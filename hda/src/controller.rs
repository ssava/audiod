//! Low-level controller bring-up: link reset, codec detection, and the PIO
//! immediate-command path (IC/IR/IRS). No CORB/RIRB buffers are used — the
//! HD-Audio spec permits PIO for single command-response round trips.
//!
//! Register flow mirrors `snd_hdac_bus_reset_link` / `snd_hdac_bus_send_cmd_pio`
//! in the kernel (`common-controller.c`), verified bit-for-bit.

use crate::mmio::Mmio;
use crate::regs::*;
use crate::pcicfg;
use std::io;
use std::time::{Duration, Instant};

pub struct Controller {
    pub bar: Mmio,
    pub codec_mask: u16,
    pub gcap: u16,
    /// Codec address (CAD) selected during probe.
    pub cad: u32,
}

impl Controller {
    /// Number of capture (input) streams from GCAP. Capture streams own the
    /// first SD index range (mid kernel `azx_first_init` sets
    /// `playback_index_offset = capture_streams`).
    #[inline]
    pub fn capture_streams(&self) -> usize {
        ((self.gcap >> 8) & 0x0f) as usize
    }

    /// Global SD index of playback stream 0 (all capture streams first).
    #[inline]
    pub fn sd_index(&self) -> usize {
        self.capture_streams()
    }

    /// BAR offset of playback stream 0's SD registers:
    /// `0x80 + 0x20 * sd_index` (SDI0=0x80, SDI1=0xa0, ... SDO0=0x100 offset).
    #[inline]
    pub fn sd0_base(&self) -> u32 {
        SD_STREAM_BASE + SD_STREAM_STRIDE * self.sd_index() as u32
    }

    /// Stream tag for playback stream 0. The controller assigns tags as
    /// `i + 1` per stream, and codec DACs bind to the same tag.
    #[inline]
    pub fn sd0_tag(&self) -> u32 {
        self.sd_index() as u32 + 1
    }
}

/// Encode a 32-bit command verb:
///   [31:28] codec address, [27:20] node id, [19:8] verb id, [7:0] payload.
/// AMP verbs carry their index/direction/channel bits in payload [15:8]
/// (the kernel's `snd_hdac_make_cmd` ORs a full 16-bit parm after `verb<<8`),
/// so the payload must not be truncated to 8 bits.
pub const fn make_verb(cad: u32, nid: u32, verb: u32, payload: u32) -> u32 {
    ((cad & 0xf) << 28) | ((nid & 0xff) << 20) | ((verb & 0xfff) << 8) | (payload & 0xffff)
}

impl Controller {
    pub fn open(dev: &str) -> io::Result<Self> {
        let bar = Mmio::map(dev).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("map BAR0 {dev}: {e} (need root + kernel driver unbound)"),
            )
        })?;
        // Identity check before we touch anything: report the PCI vendor/device
        // so a stray slot is obvious in the log. The hard fence is the GCAP
        // sanity check below (a real HDA controller always advertises at least
        // one in/out stream pair).
        let (pci_vendor, pci_device) = pcicfg::vendor_device(dev).unwrap_or((0, 0));
        log::info!("PCI {dev}: vendor=0x{pci_vendor:04x} device=0x{pci_device:04x}");
        if pci_vendor == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("slot {dev} has no readable PCI vendor ID — not an HDA controller?"),
            ));
        }
        pcicfg::set_snoop(dev)
            .map_err(|e| io::Error::new(e.kind(), format!("pci snoop config: {e}")))?;
        // PCI command register: Memory Space + Bus Master. Without bus master
        // the controller cannot DMA (kernel does this in pci_set_master).
        let enabled = pcicfg::set_master(dev)
            .map_err(|e| io::Error::new(e.kind(), format!("pci bus master: {e}")))?;
        if enabled {
            log::info!("PCI bus master enabled");
        }

        let gcap = bar.read_u16(GCAP as usize);
        // Fence: GCAP must advertise at least one capture or playback stream.
        // 0 or 0xfffff means we're not looking at an HD-Audio controller
        // (wrong slot), and hammering its registers with link-reset writes
        // could corrupt an unrelated device.
        if gcap & (GCAP_ISS as u16 | GCAP_OSS as u16) == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("slot {dev} not an HDA controller (GCAP=0x{gcap:04x})"),
            ));
        }
        log::info!(
            "GCAP ISS={} OSS={} 6KoK={} NSDO={}",
            (gcap & (GCAP_ISS as u16)) >> 8,
            (gcap & (GCAP_OSS as u16)) >> 12,
            gcap & (GCAP_64OK as u16) != 0,
            (gcap & (GCAP_NSDO as u16)) >> 1
        );
        let mut c = Controller { bar, codec_mask: 0, gcap, cad: 0 };
        let dbg = crate::dbg::opts();
        if !dbg.skip_reset {
            c.reset_link()?;
            c.codec_mask = c.bar.read_u16(STATESTS as usize);
        } else {
            log::warn!("link reset skipped (--skip-reset)");
            // Without a reset, a previous run's timed-out PIO command may have
            // left the IC stuck busy. Re-arm it by clearing ICB (kernel does
            // this via a fresh IRS write when not resetting the link).
            let mut spins = 50;
            while c.bar.read_u16(IRS as usize) & IRS_BUSY != 0 {
                if spins == 0 {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "IC stuck busy (no reset)"));
                }
                c.bar.write_u16(IRS as usize, IRS_VALID);
                std::thread::sleep(Duration::from_micros(1));
                spins -= 1;
            }
            // STATESTS not refreshed; probe only the Realtek (cad 0).
            c.codec_mask = 0x1;
            log::info!("codec_mask = 0x{:x} (forced cad 0, reset skipped)", c.codec_mask);
        }
        c.int_clear();
        Ok(c)
    }

    /// Full link reset (assert then release RESET), then read STATESTS to
    /// learn which codecs are present. Mirrors the kernel with full_reset=true.
fn reset_link(&mut self) -> io::Result<()> {
        // Mirror kernel reset_link(full_reset=true): clear STATESTS only when
        // currently running (out of reset), so fresh codec present bits appear.
        let was_running = self.bar.read_u32(GCTL as usize) & GCTL_RESET != 0;
        if was_running {
            self.bar.write_u16(STATESTS as usize, STATESTS_INT_MASK);
        }
        // Assert reset: clear GCTL.RESET (32-bit RMW, mirrors updatel).
        {
            let g = self.bar.read_u32(GCTL as usize) & !GCTL_RESET;
            self.bar.write_u32(GCTL as usize, g);
        }
        let t0 = Instant::now();
        loop {
            let g = self.bar.read_u32(GCTL as usize);
            if g & GCTL_RESET == 0 {
                break;
            }
            if t0.elapsed() > Duration::from_millis(100) {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "link reset assert"));
            }
            std::thread::sleep(Duration::from_micros(500));
        }
        std::thread::sleep(Duration::from_micros(500)); // codec PLL settle (>=100us spec)

        // Release: set GCTL.RESET.
        {
            let g = self.bar.read_u32(GCTL as usize) | GCTL_RESET;
            self.bar.write_u32(GCTL as usize, g);
        }
        let t0 = Instant::now();
        loop {
            let g = self.bar.read_u32(GCTL as usize);
            if g & GCTL_RESET != 0 {
                break;
            }
            if t0.elapsed() > Duration::from_millis(100) {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "link reset release"));
            }
            std::thread::sleep(Duration::from_micros(500));
        }
        std::thread::sleep(Duration::from_millis(1)); // codecs need >=540us after

        self.codec_mask = self.bar.read_u16(STATESTS as usize);
        log::info!("codec_mask = 0x{:x}", self.codec_mask);
        Ok(())
    }

    fn int_clear(&mut self) {
        self.bar.write_u32(INTSTS as usize, INT_CTRL_EN | INT_ALL_STREAM);
    }

    /// Send a verb to codec `cad` via PIO, returning the raw immediate response.
    /// Follows the kernel `snd_hdac_bus_send_cmd_pio` sequence.
    /// `log_level` controls per-command logging (info for bring-up probing).
    pub fn cmd(&mut self, cad: u32, nid: u32, verb: u32, payload: u32) -> io::Result<u32> {
        let cmdword = make_verb(cad, nid, verb, payload);

        // Wait for ICB to become clear (currently busy).
        let mut spins = 50;
        while self.bar.read_u16(IRS as usize) & IRS_BUSY != 0 {
            if spins == 0 {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "IC busy"));
            }
            std::thread::sleep(Duration::from_micros(1));
            spins -= 1;
        }

        {
            // 1. clear IRV (set for acknowledgment per kernel)
            self.bar.write_u16(IRS as usize, IRS_VALID);
            // 2. write IC last so the command is latched
            self.bar.write_u32(IC as usize, cmdword);
            // 3. set ICB
            let irs = self.bar.read_u16(IRS as usize) | IRS_BUSY;
            self.bar.write_u16(IRS as usize, irs);
        }

        // Poll IRV: response ready.
        let t0 = Instant::now();
        loop {
            let irs = self.bar.read_u16(IRS as usize);
            if irs & IRS_VALID != 0 {
                let r = self.bar.read_u32(IR as usize);
                log::debug!(
                    "PIO cad={} nid={:02x} verb={:03x} payload={:02x} -> 0x{:08x}",
                    cad,
                    nid,
                    verb,
                    payload,
                    r
                );
                return Ok(r);
            }
            if t0.elapsed() > Duration::from_millis(1) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("PIO timeout for verb 0x{cmdword:08x}"),
                ));
            }
            std::thread::sleep(Duration::from_micros(1));
        }
    }

    /// Same as [`cmd`](Self::cmd) but logs nothing on success — used for
    /// frequently-polled reads like pin sense where info-level logging would
    /// flood the log at sense-query intervals.
    pub fn cmd_quiet(&mut self, cad: u32, nid: u32, verb: u32, payload: u32) -> io::Result<u32> {
        let cmdword = make_verb(cad, nid, verb, payload);

        // Wait for ICB to become clear (currently busy).
        let mut spins = 50;
        while self.bar.read_u16(IRS as usize) & IRS_BUSY != 0 {
            if spins == 0 {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "IC busy"));
            }
            std::thread::sleep(Duration::from_micros(1));
            spins -= 1;
        }

        {
            // 1. clear IRV (set for acknowledgment per kernel)
            self.bar.write_u16(IRS as usize, IRS_VALID);
            // 2. write IC last so the command is latched
            self.bar.write_u32(IC as usize, cmdword);
            // 3. set ICB
            let irs = self.bar.read_u16(IRS as usize) | IRS_BUSY;
            self.bar.write_u16(IRS as usize, irs);
        }

        // Poll IRV: response ready.
        let t0 = Instant::now();
        loop {
            let irs = self.bar.read_u16(IRS as usize);
            if irs & IRS_VALID != 0 {
                return Ok(self.bar.read_u32(IR as usize));
            }
            if t0.elapsed() > Duration::from_millis(1) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("PIO timeout for verb 0x{cmdword:08x}"),
                ));
            }
            std::thread::sleep(Duration::from_micros(1));
        }
    }
}
