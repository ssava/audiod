//! Shared playback plumbing for all three driver modes (socket server, stdin
//! player, WAV player). Facade over `Backend` that owns mute/volume sync,
//! format conversion (client → HW format), and gain — so the three callers
//! don't each reimplement the same mute+convert+play loop.

use audcommon::AudioCfg;
use crate::backend::Backend;
use crate::set_mute_led;
use crate::state_gain;
use crate::{convert_to_s16, store_output_path, STATE};
use audcommon::*;
use log::*;
use std::io;
use std::sync::atomic::Ordering;

/// One playback session: a backend plus everything needed to convert a
/// client's frames into what the backend wants to hear.
pub struct Player {
    pub backend: Backend,
    /// HW frame size in bytes (what `backend.play` expects per frame).
    hw_fs: usize,
    /// HW format/channels as opened — used for the passthrough fast path.
    hw_format: u32,
    hw_channels: u32,
    /// Client frame size in bytes (from the protocol header / file header).
    client_fs: usize,
    client_channels: u32,
    client_format: u32,
    /// True when the backend mutes at the codec amp (HDA). Such backends
    /// still consume the stream while muted (no underruns); others mute by
    /// the caller dropping frames.
    hw_mute: bool,
    muted_applied: bool,
    out_buf: Vec<u8>,
}

impl Player {
    /// Open the selected backend for `cfg` and record the output route.
    pub fn open(slot: &str, cfg: &AudioCfg) -> io::Result<Self> {
        let mut backend = Backend::open(slot, cfg.rate)?;
        let hw_format = HW_FORMAT;
        let hw_channels = HW_CHANNELS;
        info!(
            "client connected (rate={} ch={} fmt={}) → HW ch={} fmt={}",
            cfg.rate, cfg.channels, cfg.format, hw_channels, hw_format
        );
        let hw_fs = (format_bytes(hw_format) * hw_channels) as usize;
        store_output_path(&backend.output_path());
        let hw_mute = backend.set_mute(false).unwrap_or(false);
        set_mute_led(false);
        Ok(Player {
            backend,
            hw_fs,
            hw_format,
            hw_channels,
            client_fs: cfg.frame_size as usize,
            client_channels: cfg.channels,
            client_format: cfg.format,
            hw_mute,
            muted_applied: false,
            out_buf: Vec::new(),
        })
    }

    /// Whether the backend mutes in hardware (HDA codec amp).
    pub fn hardware_mute(&self) -> bool {
        self.hw_mute
    }

    /// Current software mute state (shared with control clients).
    pub fn is_muted(&self) -> bool {
        STATE.muted.load(Ordering::Acquire)
    }

    /// Apply a pending mute-state change to the backend amp and the ThinkPad
    /// mute LED. No-op until the shared state actually changes.
    pub fn sync_muted(&mut self) -> io::Result<()> {
        let muted = STATE.muted.load(Ordering::Acquire);
        if muted != self.muted_applied {
            if self.hw_mute {
                self.backend.set_mute(muted)?;
            }
            set_mute_led(muted);
            self.muted_applied = muted;
        }
        Ok(())
    }

    /// Convert (as needed) then write `frames` of client PCM to the backend,
    /// applying the current volume gain. Uses the passthrough fast path when
    /// the client already speaks the HW format at HW channels with unity gain
    /// (avoids a redundant S16→S16 copy in the common case).
    pub fn play(&mut self, data: &[u8], frames: usize) -> io::Result<()> {
        if frames == 0 {
            return Ok(());
        }
        let cbytes = frames * self.client_fs;
        let gain = state_gain();
        let passthrough = self.client_format == self.hw_format
            && self.client_channels == self.hw_channels
            && (gain - 1.0).abs() < f32::EPSILON;
        if passthrough {
            return self.backend.play(self.hw_fs, &data[..cbytes]);
        }
        let out_sz = frames * self.hw_fs;
        self.out_buf.resize(out_sz, 0u8);
        convert_to_s16(
            &data[..cbytes],
            &mut self.out_buf,
            frames,
            self.client_channels,
            self.hw_channels,
            self.client_format,
            gain as f64,
        );
        self.backend.play(self.hw_fs, &self.out_buf[..out_sz])
    }
}
