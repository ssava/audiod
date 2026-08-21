// Audcommon — shared constants, helpers, and config for audiod and audshim

pub mod config;
pub mod ring;

pub const SOCKET_PATH_STR: &str = "/tmp/audiod.sock";
pub const SOCKET_PATH_BYTES: &[u8] = b"/tmp/audiod.sock\0";
pub const HDR_SZ: usize = 12;

pub const MSG_MUTE: u8 = b'M';
pub const MSG_UNMUTE: u8 = b'U';
pub const MSG_STATUS: u8 = b'S';
pub const MSG_VOLUME: u8 = b'V';
pub const MSG_FLUSH: u8 = b'F';

pub const FORMAT_S16_LE: u32 = 2;
pub const FORMAT_FLOAT_LE: u32 = 14;

pub const HW_FORMAT: u32 = FORMAT_S16_LE;
pub const HW_CHANNELS: u32 = 2;

// ── A/V-sync constants ──
// These are a coupled invariant shared between the shim's virtual-buffer model
// and the server's HDA ring occupancy cap. They MUST stay in agreement:
//
//   * SHIM_BUFFER_FRAMES / SHIM_SERVER_FRAMES: the shim presents a virtual
//     device whose buffer is exactly SHIM_BUFFER_FRAMES, with a fixed
//     SHIM_SERVER_FRAMES "always buffered" baseline (`delay + avail ==
//     BUFFER_SIZE`). The server caps the HDA ring's logical occupancy at the
//     same SHIM_SERVER_FRAMES so the two models line up.
/// Virtual device buffer the shim advertises (frames). delay + avail == this.
pub const SHIM_BUFFER_FRAMES: isize = 8192;
/// Fixed number of frames the shim assumes are always buffered server-side.
/// The HDA backend caps logical ring occupancy at this same value.
pub const SHIM_SERVER_FRAMES: isize = 4096;
/// Drain thread pops / server writes this many bytes at a time.
pub const DRAIN_CHUNK: usize = 1024;

pub fn format_bytes(fmt: u32) -> u32 {
    match fmt {
        FORMAT_S16_LE => 2,
        FORMAT_FLOAT_LE => 4,
        _ => 2,
    }
}

#[derive(Clone, Copy)]
pub struct AudioCfg {
    pub rate: u32,
    pub channels: u32,
    pub frame_size: u16,
    pub format: u32,
}

impl AudioCfg {
    pub fn new(rate: u32, channels: u32, format: u32) -> Self {
        let bps = format_bytes(format);
        AudioCfg { rate, channels, frame_size: (bps * channels) as u16, format }
    }
}

pub fn pcm_format_bytes(fmt: u16) -> usize {
    match fmt {
        2 => 2,
        6 => 4,
        14 => 4,
        _ => 2,
    }
}
