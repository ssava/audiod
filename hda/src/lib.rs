//! audhda — minimal userspace Intel HD-Audio driver.
//!
//! Replaces the kernel ALSA ioctl path in `audiod` with direct BAR
//! programming: link reset, PIO command/response, a single playback SD, a
//! pinned DMA ring, and a Realtek ALC269VC codec setup.
//!
//! Requires CAP_SYS_ADMIN (root) for BAR mmap + pagemap PFN reads, and for
//! the kernel HDA driver to be unbound from the PCI device.

pub mod bdl;
pub mod codec;
pub mod controller;
pub mod dbg;
pub mod mmio;
pub mod pagemap;
pub mod pcicfg;
pub mod regs;
pub mod stream;

use log::*;
use std::io;

pub const HW_RATE: u32 = 48000;
pub const HW_CHANNELS: u32 = 2;

/// Top-level playback session: controller + codec + running stream.
pub struct HdaPlayback {
    pub controller: controller::Controller,
    pub codec: codec::Codec,
    pub stream: stream::Stream,
    pub fmt: u16,
    pub rate: u32,
    pub running: bool,
    pub bytes_written: u64,
}

impl HdaPlayback {
    /// Bring up the HDA link at `dev` (PCI slot), detect and init the codec,
    /// then arm SD0 for playback at `rate`.
    pub fn open(dev: &str, rate: u32) -> io::Result<Self> {
        let mut controller = controller::Controller::open(dev)?;
        if controller.codec_mask == 0 {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no codec on link"));
        }

        let codec = codec::Codec::probe(&mut controller)?;
        info!("codec {:04x}/{:04x}", codec.vendor_id >> 16, codec.vendor_id & 0xffff);

        let dbg = crate::dbg::opts();
        if dbg.dump_topology {
            codec::Codec::dump_topology(&mut controller)?;
        }

        let skip_init = dbg.skip_codec_init;

        let mut s = stream::Stream::new()?;
        s.srst(&mut controller);
        s.setup(&mut controller, rate)?;
        let tag = s.tag(&controller);
        if !skip_init {
            codec::Codec::init_playback(&mut controller, stream::format_val(rate), tag)?;
            if dbg.dump_state {
                codec::Codec::dump_state(&mut controller)?;
            }
        } else {
            log::warn!("codec init skipped (--skip-codec-init) — codec assumed pre-configured");
        }

        Ok(HdaPlayback { controller, codec, stream: s, fmt: stream::format_val(rate), rate, running: false, bytes_written: 0 })
    }

    pub fn tag(&self) -> u32 {
        self.stream.tag(&self.controller)
    }

    /// Begin DMA. If the stream was previously stopped/reset (e.g. after a
    /// drop), re-run the per-SD setup first: `srst()` clears the SD registers
    /// (stream tag, CBL, LVI, BDL address), so `start()` must re-program them
    /// before re-asserting RUN. Mirrors the kernel calling
    /// `snd_hdac_stream_setup` + `snd_hdac_stream_start` on each prepare.
    pub fn start(&mut self) {
        if !self.running {
            self.stream.srst(&mut self.controller);
            if let Err(e) = self.stream.setup(&mut self.controller, self.rate) {
                log::error!("stream re-setup failed, playback will be broken: {}", e);
            }
            self.stream.start(&mut self.controller);
            self.running = true;
        }
    }

    /// Write `data` (S16_LE stereo bytes) into the ring. Copies as much as
    /// fits and returns bytes stored; caller should back-pressure when 0.
    pub fn play(&mut self, data: &[u8]) -> usize {
        self.start();
        let n = self.stream.write(&self.controller, data);
        self.bytes_written = self.bytes_written.wrapping_add(n as u64);
        if self.bytes_written % (32 * 1024) < 4 {
            log::debug!(
                "ring: wpos={} lpib={} delay={}B free={}B status=0x{:08x}",
                self.stream.write_pos(),
                self.stream.lpib(&self.controller),
                self.stream.delay_bytes(&self.controller),
                self.stream.free_bytes(&self.controller),
                self.stream.status(&self.controller)
            );
        }
        n
    }

    /// Frames delayed in the ring (write_pos - DMA pos), S16/48k stereo.
    pub fn delay_frames(&self) -> i64 {
        (self.stream.delay_bytes(&self.controller) as i64) / stream::FRAME_BYTES as i64
    }

    /// Self-test: write a pure 440 Hz sine directly into the DMA ring for
    /// `secs` seconds, bypassing the socket/conversion path entirely. Returns
    /// once the ring has been filled (DMA runs independently).
    pub fn selftest_tone(&mut self, secs: f32, volume: f32) -> io::Result<()> {
        let rate = self.rate;
        let frames = (secs * rate as f32) as usize;
        let mut buf = Vec::with_capacity(frames * stream::FRAME_BYTES);
        let mut ph: f64 = 0.0;
        let step = 2.0 * std::f64::consts::PI * 440.0 / rate as f64;
        for _ in 0..frames {
            let s = (ph.sin() * 30000.0 * volume as f64) as i16;
            buf.extend_from_slice(&s.to_le_bytes());
            buf.extend_from_slice(&s.to_le_bytes());
            ph += step;
        }
        if crate::dbg::opts().dump_state {
            codec::Codec::dump_state(&mut self.controller)?;
        }
        // Start DMA, then pump the ring.
        self.start();
        let mut written = 0usize;
        while written < buf.len() {
            let n = self.stream.write(&self.controller, &buf[written..]);
            if n == 0 {
                std::thread::sleep(std::time::Duration::from_micros(500));
                continue;
            }
            written += n;
        }
        log::warn!("selftest: wrote {} bytes of 440 Hz tone to DMA ring", written);
        Ok(())
    }

    /// Stop + full stream reset + drop, leaving the DMA ring zeroed so the
    /// stream restarts from silence instead of re-playing stale bytes.
    pub fn drop_all(&mut self) {
        self.stream.stop(&mut self.controller);
        self.stream.srst(&mut self.controller);
        self.stream.clear();
        self.stream.zero_ring();
        self.running = false;
    }

    /// Hard unlock of the controller (for graceful shutdown).
    pub fn stop(&mut self) {
        self.stream.stop(&mut self.controller);
        self.running = false;
    }
}