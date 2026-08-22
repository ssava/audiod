//! DMA codec command/response engines: CORB (verbs out) + RIRB (responses in).
//!
//! Default transport replacing the legacy PIO immediate-command interface.
//! Both rings share one pinned page: CORB at byte 0 (up to 256 x 4 B), RIRB at
//! byte 1024 (up to 256 x 8 B); page alignment trivially satisfies the spec's
//! 128-byte base alignment (Intel HDA Spec Rev 1.0a sections 3.3.19/3.3.25).
//!
//! Bring-up mirrors the kernel `snd_hdac_bus_init_cmd_io`
//! (`sound/hda/hdac_controller.c`): stop engines -> program bases ->
//! negotiate ring sizes (prefer 256 entries) -> reset pointers -> enable RIRB
//! first so a response can never land before its buffer is live -> start CORB.
//!
//! Responses are polled from RIRBWP (no interrupts, consistent with this
//! driver's polling model elsewhere). Entry semantics per spec section 4.4.2:
//! after reset the controller writes responses starting at entry 1 and WP
//! holds the index of the last written entry; software keeps its own read
//! pointer and consumes by advancing to the next index.

use crate::mmio::Mmio;
use crate::pagemap::DmaBuffer;
use crate::regs::*;
use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

/// Per-verb response timeout. Generous vs the PIO 1 ms: the response must
/// travel the link and be DMA'd back before RIRBWP moves.
const TIMEOUT_MS: u64 = 10;

/// Byte offset of the RIRB inside the shared DMA page.
const RIRB_OFF: usize = 1024;

/// A live CORB/RIRB pair sharing one pinned DMA page.
pub struct CorbRing {
    rb: DmaBuffer,
    /// Negotiated entry count (identical for both rings).
    count: usize,
    /// Software read pointer into the RIRB (hardware exposes only WP).
    sw_rp: usize,
    /// Solicited responses drained from the RIRB but not yet consumed.
    pending: VecDeque<u32>,
}

impl CorbRing {
    /// Stop, reprogram and restart both rings. Safe against stale state left
    /// by the kernel driver or a previous run: engines are halted and every
    /// pointer is reset unconditionally.
    pub fn init(bar: &Mmio) -> io::Result<Self> {
        // Stop both engines before touching bases or pointers ("this register
        // field must not be written when the DMA engine is running").
        bar.write_u8(CORBCTL as usize, 0);
        bar.write_u8(RIRBCTL as usize, 0);

        let rb = DmaBuffer::new(4096)?;
        rb.zero();
        let corb_pa = rb.toaddr(0);
        let rirb_pa = rb.toaddr(RIRB_OFF);
        bar.write_u32(CORBLBASE as usize, corb_pa as u32);
        bar.write_u32(CORBUBASE as usize, (corb_pa >> 32) as u32);
        bar.write_u32(RIRBLBASE as usize, rirb_pa as u32);
        bar.write_u32(RIRBUBASE as usize, (rirb_pa >> 32) as u32);
        log::debug!("CORB pa=0x{corb_pa:x} RIRB pa=0x{rirb_pa:x}");

        // Negotiate sizes independently; they must agree for paired operation.
        let count = program_size(bar, CORBSIZE, "CORB")?;
        let rirb_count = program_size(bar, RIRBSIZE, "RIRB")?;
        if rirb_count != count {
            return Err(io::Error::other(format!(
                "CORB({count}) / RIRB({rirb_count}) negotiated sizes differ"
            )));
        }

        bar.write_u16(CORBWP as usize, 0);
        // Pulse the CORBRP reset bit: wait for it to latch, then clear it.
        // Not all implementations latch it — warn and continue (kernel does).
        bar.write_u16(CORBRP as usize, CORBRP_RST);
        if !wait_bit(bar, CORBRP as usize, CORBRP_RST, Duration::from_millis(5)) {
            log::warn!("CORBRP reset bit did not latch — continuing");
        }
        bar.write_u16(CORBRP as usize, 0);

        bar.write_u16(RIRBWP as usize, RIRBWP_RST);
        // N=1: RINTFL updates on every new entry (we poll WP regardless).
        bar.write_u16(RINTCNT as usize, 1);
        // RIRB live before CORB: a command must not fly before its response
        // buffer is armed.
        bar.write_u8(RIRBCTL as usize, RIRBCTL_DMA_EN | RIRBCTL_IRQ_EN);
        bar.write_u8(CORBCTL as usize, CORBCTL_RUN);

        log::info!("command engine: CORB/RIRB ({count} entries)");
        Ok(CorbRing {
            rb,
            count,
            sw_rp: 0,
            pending: VecDeque::new(),
        })
    }

    /// Publish one verb into the CORB. Fails with WouldBlock if the ring is
    /// full (the controller has not consumed up to the write position).
    pub fn send(&mut self, bar: &Mmio, cmdword: u32) -> io::Result<()> {
        let wp = ((bar.read_u16(CORBWP as usize) & 0xff) as usize) % self.count;
        let rp = ((bar.read_u16(CORBRP as usize) & 0xff) as usize) % self.count;
        let next = (wp + 1) % self.count;
        if next == rp {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "CORB full"));
        }
        self.rb.write(next * 4, &cmdword.to_le_bytes());
        bar.write_u16(CORBWP as usize, next as u16);
        Ok(())
    }

    /// Wait for the solicited response to the verb last sent to codec `cad`.
    /// Extra solicited responses are queued in arrival order; unsolicited and
    /// foreign-codec entries are logged and skipped.
    pub fn wait_response(&mut self, bar: &Mmio, cad: u32) -> io::Result<u32> {
        if let Some(r) = self.pending.pop_front() {
            return Ok(r);
        }
        let deadline = Instant::now() + Duration::from_millis(TIMEOUT_MS);
        loop {
            let hw_wp = (bar.read_u16(RIRBWP as usize) & 0xff) as usize;
            if hw_wp >= self.count {
                return Err(io::Error::other(format!(
                    "RIRBWP 0x{hw_wp:x} outside ring of {} entries",
                    self.count
                )));
            }
            if hw_wp != self.sw_rp {
                let entries = collect_entries(
                    |idx| {
                        let mut b = [0u8; 8];
                        self.rb.read(RIRB_OFF + idx * 8, &mut b);
                        (
                            u32::from_le_bytes(b[0..4].try_into().unwrap()),
                            u32::from_le_bytes(b[4..8].try_into().unwrap()),
                        )
                    },
                    self.sw_rp,
                    hw_wp,
                    self.count,
                );
                self.sw_rp = hw_wp;
                for (resp, ex) in entries {
                    match route_entry(resp, ex, cad) {
                        Route::Solicited(r) => self.pending.push_back(r),
                        Route::Unsolicited(resp, ex) => {
                            log::warn!("unsolicited RIRB resp=0x{resp:08x} ex=0x{ex:08x}");
                        }
                        Route::Foreign(seen, resp) => {
                            log::warn!(
                                "RIRB response from cad {seen} (want {cad}): 0x{resp:08x}"
                            );
                        }
                    }
                }
                if let Some(r) = self.pending.pop_front() {
                    return Ok(r);
                }
                // Drained everything new and nothing matched: the response we
                // want may still be in flight — keep polling until deadline.
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "RIRB response timeout",
                ));
            }
            std::thread::sleep(Duration::from_micros(10));
        }
    }

    /// Stop both DMA engines.
    pub fn shutdown(&self, bar: &Mmio) {
        bar.write_u8(CORBCTL as usize, 0);
        bar.write_u8(RIRBCTL as usize, 0);
    }
}

/// Program the largest supported ring size into `reg` (CORBSIZE/RIRBSIZE) and
/// verify the encoding reads back. Capability bits live at 6:4, the RW
/// programmed-size encoding at 1:0 (00=2, 01=16, 10=256 entries).
fn program_size(bar: &Mmio, reg: u32, name: &str) -> io::Result<usize> {
    const CHOICES: [(u8, u8, usize); 3] = [
        (SIZE_CAP_256, SIZE_256, 256),
        (SIZE_CAP_16, SIZE_16, 16),
        (SIZE_CAP_2, SIZE_2, 2),
    ];
    let caps = bar.read_u8(reg as usize);
    for (cap, enc, n) in CHOICES {
        if caps & cap == 0 {
            continue;
        }
        bar.write_u8(reg as usize, enc);
        if bar.read_u8(reg as usize) & 0x03 == enc {
            log::debug!("{name}: {n} entries");
            return Ok(n);
        }
        log::warn!("{name}: size {n} accepted caps but readback mismatch");
    }
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("{name}: no advertised size readable back"),
    ))
}

/// Poll register at `off` until `bit` is set or `limit` elapses (1 us steps).
fn wait_bit(bar: &Mmio, off: usize, bit: u16, limit: Duration) -> bool {
    let t0 = Instant::now();
    while bar.read_u16(off) & bit == 0 {
        if t0.elapsed() >= limit {
            return false;
        }
        std::thread::sleep(Duration::from_micros(1));
    }
    true
}

/// Routing verdict for one drained RIRB entry.
#[derive(Debug, PartialEq)]
enum Route {
    /// Response to our own command.
    Solicited(u32),
    /// Codec-initiated event (unsolicited bit set).
    Unsolicited(u32, u32),
    /// Solicited, but from another codec address.
    Foreign(u32, u32),
}

fn route_entry(resp: u32, ex: u32, cad: u32) -> Route {
    let seen = ex & RIRB_EX_CAD_MASK;
    if ex & RIRB_EX_UNSOL != 0 {
        Route::Unsolicited(resp, ex)
    } else if seen != cad {
        Route::Foreign(seen, resp)
    } else {
        Route::Solicited(resp)
    }
}

/// Collect entries between software RP (exclusive) and hardware WP (inclusive)
/// from a ring image accessed via `read(idx)`, wrapping at `count`.
/// Consumption is 1-based after reset: the first post-reset response lands in
/// entry 1 (spec section 4.4.2 pointer example; mirrors the kernel's
/// `snd_hdac_bus_get_response` increment-before-read).
fn collect_entries(
    read: impl Fn(usize) -> (u32, u32),
    sw_rp: usize,
    hw_wp: usize,
    count: usize,
) -> Vec<(u32, u32)> {
    assert!(hw_wp < count, "hardware WP {hw_wp} outside ring of {count}");
    let mut out = Vec::new();
    let mut rp = sw_rp;
    while rp != hw_wp {
        rp = (rp + 1) % count;
        out.push(read(rp));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize (resp, ex) pairs into a little-endian RIRB image.
    fn image(entries: &[(u32, u32)]) -> Vec<u8> {
        let mut v = Vec::new();
        for (r, ex) in entries {
            v.extend_from_slice(&r.to_le_bytes());
            v.extend_from_slice(&ex.to_le_bytes());
        }
        v
    }

    fn reader(img: &[u8]) -> impl Fn(usize) -> (u32, u32) + '_ {
        move |idx| {
            let o = idx * 8;
            (
                u32::from_le_bytes(img[o..o + 4].try_into().unwrap()),
                u32::from_le_bytes(img[o + 4..o + 8].try_into().unwrap()),
            )
        }
    }

    #[test]
    fn drain_in_order_no_wrap() {
        let img = image(&[(0, 0), (0x111, 0), (0x222, 0), (0x333, 0)]);
        let got = collect_entries(reader(&img), 0, 3, 256);
        assert_eq!(
            got,
            vec![(0x111, 0), (0x222, 0), (0x333, 0)],
            "entries 1..=wp in arrival order"
        );
    }

    #[test]
    fn drain_wraps_255_to_0() {
        let mut ents = vec![(0, 0); 256];
        ents[0] = (0xaaaa, 0);
        ents[1] = (0xbbbb, 0);
        let img = image(&ents);
        let got = collect_entries(reader(&img), 255, 1, 256);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], (0xaaaa, 0));
        assert_eq!(got[1], (0xbbbb, 0));
    }

    #[test]
    fn drain_small_ring_wraps() {
        // count=16 negotiation: indices stay below 16 and wrap there.
        let mut ents = vec![(0, 0); 16];
        ents[15] = (0x0f0f, 0);
        ents[0] = (0x0101, 0);
        let img = image(&ents);
        let got = collect_entries(reader(&img), 15, 0, 16);
        assert_eq!(got, vec![(0x0101, 0)]);
    }

    #[test]
    fn drain_empty_when_caught_up() {
        let img = image(&[(0, 0)]);
        assert!(collect_entries(reader(&img), 0, 0, 256).is_empty());
    }

    #[test]
    #[should_panic(expected = "outside ring")]
    fn drain_rejects_out_of_range_wp() {
        let img = image(&[(0, 0); 16]);
        let _ = collect_entries(reader(&img), 0, 200, 16);
    }

    #[test]
    fn routing_unsolicited_wins_over_cad() {
        let ex = RIRB_EX_UNSOL | 3; // unsolicited from cad 3
        assert_eq!(route_entry(0x42, ex, 0), Route::Unsolicited(0x42, ex));
    }

    #[test]
    fn routing_foreign_cad_dropped() {
        assert_eq!(route_entry(0x42, 3, 0), Route::Foreign(3, 0x42));
    }

    #[test]
    fn routing_solicited_match() {
        assert_eq!(route_entry(0x42, 0, 0), Route::Solicited(0x42));
    }
}
