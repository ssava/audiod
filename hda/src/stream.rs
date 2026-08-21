//! Stream descriptor (SD) management: setup, format encoding, start/stop,
//! and the DMA ring that the server feeds.
//!
//! Mirrors `snd_hdac_stream_setup` / `snd_hdac_stream_start` (kernel
//! `stream.c`). We use a single playback stream (SD0) with a pinned,
//! page-scattered ring buffer referenced by a BDL.

use crate::bdl;
use crate::controller::Controller;
use crate::pagemap::DmaBuffer;
use crate::regs::*;
use std::io;
use std::time::Duration;

pub const FRAME_BYTES: usize = 4; // S16_LE stereo

/// Number of pages in the DMA ring buffer (256 KiB @ 48k = ~0.67 s).
pub const RING_PAGES: usize = 64;
const RING_SIZE: usize = RING_PAGES * crate::pagemap::PAGE_SIZE;

pub struct Stream {
    ring: DmaBuffer,
    bdl: DmaBuffer,
    entries: usize,
    write_pos: usize, // byte offset in ring where the next data goes
    written: usize,   // total bytes written (diagnostics)
}

/// Map any expressible `rate` to HDA `(base, mult, div)` where
/// `rate == base * mult / div`. `base` is the SD_FORMAT base bit
/// (0 = 48k family / 24.576 MHz, 1 = 44.1k family / 22.5792 MHz).
/// `mult` and `div` are 3-bit counters, so each is 1..=8.
///
/// Covers 8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000,
/// 88200, 96000, 176400, 192000, ... (any `base*mult/div` ratio that is
/// integral). Returns `None` for rates no such ratio fits (e.g. 12345).
pub fn encode_rate(rate: u32) -> Option<(u32, u32, u32)> {
    // Iterate the 48k family first so 48000 maps to the canonical (0,1,1).
    for (freq_base, base) in [(48000u32, 0u32), (44100, 1)] {
        for mult in 1..=8u32 {
            for div in 1..=8u32 {
                if freq_base * mult == rate * div {
                    return Some((base, mult, div));
                }
            }
        }
    }
    None
}

/// Whether the HDA link can run SD at exactly `rate`.
pub fn rate_supported(rate: u32) -> bool {
    encode_rate(rate).is_some()
}

/// Encode the HDA SD_FORMAT value for PCM S16 stereo at `rate`.
/// Bits: [3:0] chan-1, [6:4] bits(16=1), [10:8] div, [13:11] mult,
/// [14] base(48k=0/44.1k=1), [15] type(PCM=0).
pub fn format_val(rate: u32) -> u16 {
    let (base, mult, div) = encode_rate(rate).unwrap_or_else(|| {
        log::warn!(
            "rate {rate} Hz not representable as 48k/44.1k * mult/div (mult, div in 1..=8); using 48000"
        );
        (0, 1, 1) // 48000
    });
    let mut f = 1u32; // channels-1 (stereo)
    f |= 1 << 4; // 16-bit
    f |= (div - 1) << 8;
    f |= (mult - 1) << 11;
    f |= base << 14;
    f as u16
}

impl Stream {
    pub fn new() -> io::Result<Self> {
        let ring = DmaBuffer::new(RING_SIZE)?;
        // BDL lives in its own pinned page(s); one 16-byte entry per page.
        let bdl_bytes = (RING_PAGES * 16).max(crate::pagemap::PAGE_SIZE);
        let bdl = DmaBuffer::new(bdl_bytes)?;
        Ok(Stream {
            ring,
            bdl,
            entries: 0,
            write_pos: 0,
            written: 0,
        })
    }

    pub fn ring_size(&self) -> usize {
        RING_SIZE
    }

    /// Fill the BDL (one entry per ring page) and program SD0 registers.
    /// Mirrors `snd_hdac_stream_setup`. Stream must not be running.
    pub fn setup(&mut self, c: &mut Controller, rate: u32) -> io::Result<()> {
        let entries = bdl::fill(&self.bdl, &self.ring, RING_PAGES, true);
        self.entries = entries;

        let base = c.sd0_base();
        let bar = &c.bar;
        // Stream tag in bits 23:20; playback => DIR=0, no stripes.
        let tag = c.sd0_tag();
        let ctl = (tag << STREAM_TAG_SHIFT) & STREAM_TAG_MASK;
        bar.write_u32((base + SD_CTL) as usize, ctl);
        bar.write_u32((base + SD_CBL) as usize, RING_SIZE as u32);
        bar.write_u16((base + SD_FORMAT) as usize, format_val(rate));
        bar.write_u16((base + SD_LVI) as usize, (entries - 1) as u16);
        let pa = self.bdl.toaddr(0);
        bar.write_u32((base + SD_BDLPL) as usize, pa as u32);
        bar.write_u32((base + SD_BDLPU) as usize, (pa >> 32) as u32);
        // Enable descriptor interrupts for IOC on last entry.
        bar.write_u32((base + SD_CTL) as usize, ctl | INT_MASK);
        log::info!(
            "SD0 setup @0x{:x} tag={}: CBL=0x{:x} LVI={} fmt=0x{:04x} BDLpa=0x{:x} ringpa0=0x{:x}",
            c.sd0_base(),
            c.sd0_tag(),
            RING_SIZE,
            entries - 1,
            format_val(rate),
            self.bdl.toaddr(0),
            self.ring.toaddr(0)
        );
        Ok(())
    }

    /// The stream tag assigned to this SD (used by the codec DAC binding).
    pub fn tag(&self, c: &Controller) -> u32 {
        c.sd0_tag()
    }

    /// Start DMA. Mirrors `snd_hdac_stream_start`: SIE + DMA_START.
    /// RUN + int-enable live in SD_CTL byte 0; kernel uses byte writes for
    /// these bits when `access_sdnctl_in_dword` is 0 (the default).
    pub fn start(&self, c: &mut Controller) {
        let base = c.sd0_base();
        let sie = 1u32 << c.sd_index();
        let bar = &c.bar;
        let int = bar.read_u32(INTCTL as usize);
        bar.write_u32(INTCTL as usize, int | sie); // SIE
        let ctl = bar.read_u32((base + SD_CTL) as usize);
        bar.write_u32((base + SD_CTL) as usize, ctl | INT_MASK);
        let low = bar.read_u8((base + SD_CTL) as usize) | DMA_START as u8;
        bar.write_u8((base + SD_CTL) as usize, low);
        std::thread::sleep(std::time::Duration::from_micros(100));
        log::info!(
            "SD0 started @0x{:x}: CTL=0x{:08x} STS=0x{:02x} LPIB=0x{:x}",
            base,
            bar.read_u32((base + SD_CTL) as usize),
            bar.read_u8((base + SD_STS) as usize),
            bar.read_u32((base + SD_LPIB) as usize)
        );
    }

    /// Stop: clear DMA start + int mask, clear status, disable SIE.
    pub fn stop(&self, c: &mut Controller) {
        let base = c.sd0_base();
        let bar = &c.bar;
        let ctl = bar.read_u32((base + SD_CTL) as usize) & !(DMA_START | INT_MASK);
        bar.write_u32((base + SD_CTL) as usize, ctl);
        // SD_STS is an 8-bit register at base + 0x03.
        bar.write_u8((base + SD_STS) as usize, INT_MASK as u8); // W1C
        let int = bar.read_u32(INTCTL as usize) & !(1u32 << c.sd_index());
        bar.write_u32(INTCTL as usize, int);
    }

    /// SD stream reset strobe (SRST). Call before setup/after stop.
    pub fn srst(&self, c: &mut Controller) {
        let base = c.sd0_base();
        let bar = &c.bar;
        bar.write_u32((base + SD_CTL) as usize, STREAM_RESET);
        for _ in 0..100 {
            if bar.read_u32((base + SD_CTL) as usize) & STREAM_RESET != 0 {
                break;
            }
            std::thread::sleep(Duration::from_micros(3));
        }
        bar.write_u32((base + SD_CTL) as usize, 0);
        for _ in 0..100 {
            if bar.read_u32((base + SD_CTL) as usize) & STREAM_RESET == 0 {
                break;
            }
            std::thread::sleep(Duration::from_micros(3));
        }
    }

    /// Current DMA byte position in the ring (SD_LPIB), clamped to ring size.
    pub fn lpib(&self, c: &Controller) -> usize {
        let base = c.sd0_base();
        let v = c.bar.read_u32((base + SD_LPIB) as usize) as usize;
        v % RING_SIZE
    }

    /// Bytes outstanding (written but not yet consumed by DMA).
    pub fn delay_bytes(&self, c: &Controller) -> usize {
        let rd = self.lpib(c);
        if self.write_pos >= rd {
            self.write_pos - rd
        } else {
            RING_SIZE - rd + self.write_pos
        }
    }

    /// Free space in the ring given the current DMA read position.
    fn free_space(&self, rd: usize) -> usize {
        if self.write_pos >= rd {
            RING_SIZE - (self.write_pos - rd)
        } else {
            rd - self.write_pos
        }
    }

    /// Free bytes available for writing (for diagnostics).
    pub fn free_bytes(&self, c: &Controller) -> usize {
        let rd = self.lpib(c);
        self.free_space(rd)
    }

    /// Ring byte offset where the next write lands (for diagnostics).
    pub fn write_pos(&self) -> usize {
        self.write_pos
    }

    /// Raw SD_STS status register (for diagnostics).
    pub fn status(&self, c: &Controller) -> u32 {
        let base = c.sd0_base();
        c.bar.read_u8((base + SD_STS) as usize) as u32
    }

    /// Copy up to the available space from `data` into the ring, advancing
    /// `write_pos`. Returns bytes copied (may be < `data.len()` when full).
    pub fn write(&mut self, c: &Controller, data: &[u8]) -> usize {
        let rd = self.lpib(c);
        let free = self.free_space(rd);
        let n = data.len().min(free);
        if n == 0 {
            return 0;
        }
        if crate::dbg::opts().dump_ring && self.write_pos < 64 {
            let dump: Vec<u8> = data[..n.min(64)].to_vec();
            let hex = dump.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ");
            log::info!("ring first data (+{}): wpos={} n={} bytes: {}", self.write_pos, self.write_pos, n.min(64), hex);
            self.write_pos += 0; // no-op, keep write_pos untouched here
        }
        let first = n.min(RING_SIZE - self.write_pos);
        self.ring.write(self.write_pos, &data[..first]);
        if first < n {
            self.ring.write(0, &data[first..n]);
        }
        let wp = self.write_pos;
        self.write_pos = (self.write_pos + n) % RING_SIZE;
        self.written = wp + n;
        n
    }

    /// Reset ring accounting (e.g. after a drop/seek).
    pub fn clear(&mut self) {
        self.write_pos = 0;
    }

    /// Zero every byte of the DMA ring so a restart after a drop never
    /// re-plays stale audio while fresh data is still in transit.
    pub fn zero_ring(&self) {
        self.ring.zero();
    }
}

impl Default for Stream {
    fn default() -> Self {
        Stream::new().expect("alloc stream")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_common_rates_exactly() {
        for (rate, base, mult, div) in [
            (48000, 0, 1, 1),
            (44100, 1, 1, 1),
            (32000, 0, 2, 3),
            (24000, 0, 1, 2),
            (22050, 1, 1, 2),
            (16000, 0, 1, 3),
            (12000, 0, 1, 4),
            (11025, 1, 1, 4),
            (8000, 0, 1, 6),
            (96000, 0, 2, 1),
            (88200, 1, 2, 1),
            (192000, 0, 4, 1),
        ] {
            assert_eq!(encode_rate(rate), Some((base, mult, div)), "rate {rate}");
            assert!(rate_supported(rate), "rate {rate}");
        }
    }

    #[test]
    fn rejects_unrepresentable_rates() {
        for rate in [12_345, 44_500, 47_123, 33_333] {
            assert!(encode_rate(rate).is_none(), "rate {rate}");
            assert!(!rate_supported(rate), "rate {rate}");
        }
        // Format still degenerates to a safe 48k encoding.
        assert_eq!(format_val(12_345), format_val(48_000));
    }
}
