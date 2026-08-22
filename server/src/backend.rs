//! Playback backend abstraction.
//!
//! Single implementation:
//!  - `Hda` — direct userspace Intel HD-Audio + ALC269VC (`audhda`)
//!
//! The server code deals with `Backend` only.

use audhda::codec::Codec;
use audhda::HdaPlayback;
use log::*;
use std::io;

pub const HDA_DEFAULT_SLOT: &str = "0000:00:1b.0";

/// Parse `-d` value into an HDA PCI slot. Accepts `hda` (default slot) or
/// `hda:<pci-slot>`.
pub fn parse_slot(dev: &str) -> Result<String, String> {
    if let Some(slot) = dev.strip_prefix("hda:") {
        Ok(slot.into())
    } else if dev == "hda" {
        Ok(HDA_DEFAULT_SLOT.into())
    } else {
        Err(format!(
            "HDA-only backend: use `hda` or `hda:<pci-slot>` (got `{dev}`)"
        ))
    }
}

/// Facade over the HDA playback session.
pub struct Backend(HdaPlayback);

/// Candidate rates the HDA link can run at exactly (an SD_FORMAT
/// `base*mult/div` encoding with mult, div in 1..=8).
pub const HDA_RATES: &[u32] = &[
    192000, 176400, 96000, 88200, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000,
];

/// Nearest rate the HDA link supports for a client rate it can't encode
/// exactly (e.g. 12345 → ~12000). Prefer a supported rate so the DMA cadence
/// is a real SD_FORMAT value; the approximation is logged.
fn nearest_hda_rate(rate: u32) -> u32 {
    HDA_RATES
        .iter()
        .min_by_key(|&&r| (r.abs_diff(rate), r))
        .copied()
        .unwrap_or(48000)
}

impl Backend {
    /// Open + configure the HDA backend for `rate`.
    /// Always yields S16 stereo on the wire (matching server conversion).
    pub fn open(slot: &str, rate: u32) -> io::Result<Self> {
        if unsafe { libc::geteuid() } != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "HDA backend requires root",
            ));
        }
        let hw = if audhda::stream::rate_supported(rate) {
            rate
        } else {
            let nearest = nearest_hda_rate(rate);
            warn!(
                "client rate {} Hz not representable on the HDA link;                  approximating with {} Hz (no resampling)",
                rate, nearest
            );
            nearest
        };
        let pb = HdaPlayback::open(slot, hw)?;
        info!("HDA backend ready on {} ({} Hz S16 stereo)", slot, hw);
        Ok(Backend(pb))
    }

    pub fn play(&mut self, _frame_size: usize, data: &[u8]) -> io::Result<()> {
        const TARGET_FRAMES: i64 = audcommon::SHIM_SERVER_FRAMES as i64;
        let mut off = 0;
        while off < data.len() {
            let delay = self.0.delay_frames();
            if delay >= TARGET_FRAMES {
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            let headroom = ((TARGET_FRAMES - delay) as usize) * audhda::stream::FRAME_BYTES;
            let want = (data.len() - off).min(headroom);
            let n = self.0.play(&data[off..off + want]);
            if n == 0 {
                std::thread::sleep(std::time::Duration::from_millis(1));
            } else {
                off += n;
            }
        }
        Ok(())
    }

    /// Write straight into the physical DMA ring with NO occupancy-target
    /// check. For the mixer, which paces itself precisely (`writable =
    /// clamp(SHIM_SERVER_FRAMES − delay_frames, …)` before every tick):
    /// re-checking the target here only duplicated MMIO delay reads and,
    /// whenever the caller's budget raced the DAC clock, spun in 1 ms
    /// sleeps (~27% of mixer CPU under resampled playback).
    pub fn push(&mut self, data: &[u8]) -> io::Result<()> {
        debug_assert!(data.len().is_multiple_of(audhda::stream::FRAME_BYTES));
        let mut off = 0;
        while off < data.len() {
            let n = self.0.play(&data[off..]);
            if n == 0 {
                // Physical ring momentarily full; brief backoff (cannot spin).
                std::thread::sleep(std::time::Duration::from_micros(500));
            } else {
                off += n;
            }
        }
        Ok(())
    }

    pub fn delay_frames(&mut self) -> io::Result<i64> {
        Ok(self.0.delay_frames())
    }

    /// Mute/unmute audio output. Returns `true` when applied at the codec
    /// level (HDA); callers should still handle software mute.
    pub fn set_mute(&mut self, mute: bool) -> io::Result<bool> {
        Codec::set_mute(&mut self.0.controller, mute)?;
        Ok(true)
    }

    /// Immediately stop and reset (like ALSA DROP). For Hda this clears the
    /// ring so the stream restarts cleanly at the next seek.
    pub fn stop_immediate(&mut self) -> io::Result<()> {
        self.0.drop_all();
        Ok(())
    }

    /// Debugging aid: where audio is currently routed ("speakers",
    /// "headphone", or "unknown").
    pub fn output_path(&mut self) -> String {
        Codec::output_path(&mut self.0.controller)
    }

    pub fn drain(&mut self) -> io::Result<()> {
        while self.0.delay_frames() > 0 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        self.0.stop();
        Ok(())
    }
}
