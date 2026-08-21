#![allow(non_camel_case_types)]
// Every exported symbol is a C-callable ABI stub: `extern "C"` requires the
// `unsafe` marker on the whole function signature. The safety contract is the
// ALSA C API itself (valid opaque handles/buffers from the caller), not
// per-function Rust preconditions, so per-function `# Safety` docs would be
// noise.
#![allow(clippy::missing_safety_doc)]
//! audshim — LD_PRELOAD interception library for ALSA.
//!
//! Pipeline: `mpv/Firefox → audshim (LD_PRELOAD) → Unix socket → audiod → ioctl → /dev/snd/pcmC0D0p`
//!
//! This library exports 100+ `snd_pcm_*` symbols via `#[no_mangle]` to replace libasound.so.2
//! at load time. Real dlopen/dlsym are resolved via `dlvsym(RTLD_NEXT, ...)` to avoid recursion.
//!
//! Key design decisions:
//! - `snd_pcm_writei` pushes PCM data to a ring buffer. A drain thread writes data to a
//!   blocking Unix socket. `set_sock_buf()` limits the socket send buffer to 32768 bytes
//!   (kernel-doubled from 16384), capping ring-level oscillation at ~2 drain chunks.
//! - The shim presents a virtual device whose buffer is `BUFFER_SIZE` frames. `snd_pcm_delay`
//!   and `snd_pcm_avail_update` satisfy `delay + avail == BUFFER_SIZE` (as a real snd_pcm
//!   does), so the app can only ever write one buffer-worth ahead — preventing the ring from
//!   backlogging seconds of audio that would break mpv's A/V sync. Delay is derived from a
//!   time-based playhead (`pushed_frames − rate·elapsed + SERVER_FRAMES`) so it advances
//!   continuously with the DAC clock instead of stepping in drain-chunk quanta.
//! - Format codes are stored but conversion happens on the server side.

use audcommon::*;
use log::*;
use std::ffi::CStr;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

use audcommon::ring::AudioRingBuffer;

// Channel position constants from <sound/asound.h>
const SND_CHMAP_FL: u32 = 3;
const SND_CHMAP_FR: u32 = 4;

/// ALSA channel map struct — must match kernel ABI.
/// fields: channels:u32, pos[] (variable-length).
/// We fix stereo so pos always has 2 elements.
#[repr(C)]
struct SndPcmChmap {
    channels: u32,
    pos: [u32; 2],
}

/// Initialize env_logger lazily on first call (the shim is a cdylib, so we can't init at
/// library load time). Default filter is `warn` — set `RUST_LOG=audshim=debug` for verbose output.
fn init_logging() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = env_logger::Builder::from_env(
            env_logger::Env::default().default_filter_or("warn"),
        )
        .format_timestamp_millis()
        .try_init();
    });
}

/// Sizes of the fake `snd_pcm_hw_params_t` / `snd_pcm_sw_params_t` blobs we
/// hand back to callers. Must match the `snd_pcm_*_sizeof()` exports so the
/// caller never reads/writes past the malloc'd region.
const HW_PARAMS_SIZE: usize = 608;
const SW_PARAMS_SIZE: usize = 136;
static MUTE_CMD: AtomicU8 = AtomicU8::new(0);
static LAST_SILENT: AtomicBool = AtomicBool::new(false);
/// Set when the app dropped/paused the stream; the drain thread forwards a
/// `MSG_FLUSH` so the server can stop the HDA DMA ring (which would otherwise
/// wrap around and re-play stale audio forever).
static FLUSH_CMD: AtomicBool = AtomicBool::new(false);
/// Fake handle returned by `dlopen("libasound.so.2")` so the app thinks the library loaded.
/// When passed to `dlsym`, we return our own `snd_*` symbols via `lookup()`.
const FAKE_HANDLE: *mut libc::c_void = 0x41756400 as *mut _;

/// Real dlopen/dlsym/dlclose — obtained via `dlvsym(RTLD_NEXT, ...)` to avoid recursion
/// (we override dlopen/dlsym/dlclose, so calling the libc versions directly would recurse).
type DlFcn = unsafe extern "C" fn(*const libc::c_char, libc::c_int) -> *mut libc::c_void;
type DlSym = unsafe extern "C" fn(*mut libc::c_void, *const libc::c_char) -> *mut libc::c_void;
type DlCls = unsafe extern "C" fn(*mut libc::c_void) -> libc::c_int;

fn real_dlopen() -> DlFcn {
    static V: std::sync::OnceLock<DlFcn> = std::sync::OnceLock::new();
    *V.get_or_init(|| unsafe {
        let p = libc::dlvsym(libc::RTLD_NEXT, c"dlopen".as_ptr(), c"GLIBC_2.2.5".as_ptr());
        if p.is_null() { panic!("audshim: dlvsym(dlopen) failed"); }
        std::mem::transmute(p)
    })
}

fn real_dlsym() -> DlSym {
    static V: std::sync::OnceLock<DlSym> = std::sync::OnceLock::new();
    *V.get_or_init(|| unsafe {
        let p = libc::dlvsym(libc::RTLD_NEXT, c"dlsym".as_ptr(), c"GLIBC_2.2.5".as_ptr());
        if p.is_null() { panic!("audshim: dlvsym(dlsym) failed"); }
        std::mem::transmute(p)
    })
}

fn real_dlclose() -> DlCls {
    static V: std::sync::OnceLock<DlCls> = std::sync::OnceLock::new();
    *V.get_or_init(|| unsafe {
        let p = libc::dlvsym(libc::RTLD_NEXT, c"dlclose".as_ptr(), c"GLIBC_2.2.5".as_ptr());
        if p.is_null() { panic!("audshim: dlvsym(dlclose) failed"); }
        std::mem::transmute(p)
    })
}

/// ALSA PCM states returned by snd_pcm_state.
const SND_PCM_STATE_OPEN: i32 = 0;
const SND_PCM_STATE_SETUP: i32 = 1;
const SND_PCM_STATE_PREPARED: i32 = 2;
const SND_PCM_STATE_RUNNING: i32 = 3;
const SND_PCM_STATE_PAUSED: i32 = 6;

/// Fake PCM handle returned to the application.
/// Real ALSA uses an opaque pointer to a kernel-bound structure; we allocate this on the heap
/// and store the socket fd + ring buffer + drain thread state.
/// Poll timer cadence for poll-based clients (cubeb/Firefox). Real ALSA
/// fireable fds gate the client on hardware writability; we have no such fd,
/// so a 10 ms timer steps the client through its write loop at a rate the
/// time-based playhead can track continuously.
const POLL_TIMER_NS: i64 = 10_000_000;
/// Advertised total device buffer (frames). The server keeps its HDA/ALSA
/// ring at SERVER_FRAMES, so `delay` is clamped to `BUFFER_SIZE` and
/// `avail = BUFFER_SIZE - delay` — the same invariant a real snd_pcm
/// satisfies. This stops mpv's A/V sync engine from seeing a 2.4 s backlog
/// (and jittering 4096-frame steps) that makes it drop video to "catch up".
///
/// Values are shared with the server via `audcommon` so `delay + avail ==
/// BUFFER_SIZE` and the server's `SHIM_SERVER_FRAMES` occupancy cap can never
/// drift apart silently.
const BUFFER_SIZE: isize = audcommon::SHIM_BUFFER_FRAMES;
/// Frames the shim assumes are always buffered server-side (the +4096
/// baseline in the delay model). The server caps HDA occupancy at this value.
const SERVER_FRAMES: isize = audcommon::SHIM_SERVER_FRAMES;
/// Drain thread pops / server writes this many bytes at a time.
const DRAIN_CHUNK: usize = audcommon::DRAIN_CHUNK;

#[repr(C)]
pub struct snd_pcm_t {
    sock: Arc<AtomicI32>,
    rate: u32,
    channels: u32,
    format: i32,
    configured: bool,
    state: i32,
    ring: Arc<AudioRingBuffer>,
    drain_stop: Arc<AtomicBool>,
    total_pushed: Arc<AtomicUsize>,
    total_written: Arc<AtomicUsize>,
    /// Playhead anchor (first writei after prepare/drop). Shared with the
    /// drain thread so a socket reconnect can reset it (the new server's
    /// clock starts fresh, so the old anchor would under-report delay).
    play_start: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    /// Rate/channels/format stashed at configure time. The drain thread
    /// re-sends this 12-byte header after reconnecting to the server.
    header: Arc<std::sync::Mutex<Option<(u32, u32, u16)>>>,
    drain_handle: Option<std::thread::JoinHandle<()>>,
    /// One-shot timerfd returned to poll-based clients (cubeb/Firefox). We
    /// can't signal "writable" like a real snd_pcm (our ring is always
    /// pushable), so the timer paces the client's `poll()` instead —
    /// otherwise it would busy-loop on an always-ready fd. Re-armed after
    /// each firing.
    poll_fd: i32,
}

/// Null-checked shared view of the fake PCM handle. The handle is allocated
/// by `snd_pcm_open` via `Box::into_raw` and accessed exclusively by the
/// owning thread, so a shared borrow is sound once the pointer is non-null.
fn pcm_ref<'a>(p: *mut snd_pcm_t) -> Option<&'a snd_pcm_t> {
    if p.is_null() { None } else { Some(unsafe { &*p }) }
}

/// Null-checked mutable view of the fake PCM handle (see `pcm_ref`).
fn pcm_mut<'a>(p: *mut snd_pcm_t) -> Option<&'a mut snd_pcm_t> {
    if p.is_null() { None } else { Some(unsafe { &mut *p }) }
}

/// Null-checked view of a caller-supplied buffer of `n` bytes.
fn input_slice<'a>(p: *const libc::c_void, n: usize) -> Option<&'a [u8]> {
    if p.is_null() { None } else { Some(unsafe { std::slice::from_raw_parts(p as *const u8, n) }) }
}

/// Null-checked mutable view of a caller-supplied buffer of `n` bytes.
fn input_slice_mut<'a>(p: *mut libc::c_void, n: usize) -> Option<&'a mut [u8]> {
    if p.is_null() { None } else { Some(unsafe { std::slice::from_raw_parts_mut(p as *mut u8, n) }) }
}

/// Set SO_SNDBUF on a socket to limit kernel buffering. The kernel doubles the value,
/// so passing 16384 results in a 32768-byte send buffer (~2 drain chunks).
fn set_sock_buf(fd: i32) {
    let sndbuf: i32 = 16384;
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &sndbuf as *const _ as *const libc::c_void,
            std::mem::size_of::<i32>() as u32,
        );
    }
}

/// Arm the one-shot poll timer to fire in 10 ms. A one-shot stays readable
/// only once (until re-armed), so draining on each fire keeps poll() paced
/// instead of chaining back-to-back wakeups.
fn arm_poll_timer(fd: i32) {
    let ts = libc::timespec { tv_sec: 0, tv_nsec: POLL_TIMER_NS };
    let spec = libc::itimerspec {
        it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
        it_value: ts,
    };
    unsafe {
        libc::timerfd_settime(fd, 0, &spec, std::ptr::null_mut());
    }
}

/// Drain any pending expirations from the poll timer (non-blocking). Call
/// this after the client's poll wakes so the fd doesn't stay readable.
fn drain_poll_timer(fd: i32) {
    let mut buf = [0u8; 8];
    unsafe {
        libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
    }
}

/// Connect to audiod's Unix socket at `/tmp/audiod.sock`.
/// The socket is left in blocking mode — the dedicated drain thread handles blocking
/// writes so `snd_pcm_writei` never blocks (it only pushes to the ring buffer).
fn connect_audiod() -> Result<i32, ()> {
    let sock = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if sock < 0 { return Err(()); }
    set_sock_buf(sock);
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let path_len = SOCKET_PATH_BYTES.len();
    assert!(path_len <= addr.sun_path.len(), "socket path too long");
    addr.sun_path[..path_len].copy_from_slice(
        &SOCKET_PATH_BYTES[..path_len].iter().map(|&b| b as libc::c_char).collect::<Vec<_>>(),
    );
    let alen = std::mem::offset_of!(libc::sockaddr_un, sun_path) + path_len;
    let r = unsafe {
        libc::connect(sock, &addr as *const _ as *const libc::sockaddr, alen as u32)
    };
    if r < 0 {
        let err = unsafe { *libc::__errno_location() };
        unsafe { libc::close(sock) };
        debug!("connect_audiod failed: errno={err}");
        return Err(());
    }
    Ok(sock)
}

/// Protocol header format (12 bytes, all little-endian):
///   [0..4]  rate       u32 — sample rate in Hz (e.g. 44100)
///   [4..6]  channels   u16 — number of channels in this stream
///   [6..8]  format     u16 — ALSA PCM format code (2=S16_LE, 14=FLOAT_LE)
///   [8..12] reserved   u32 — unused, zero
///
/// The header is sent once during `snd_pcm_hw_params()` before any PCM data.
fn send_header(sock: i32, rate: u32, channels: u32, format: u16) -> bool {
    let mut h = [0u8; HDR_SZ];
    h[0..4].copy_from_slice(&rate.to_le_bytes());
    h[4..6].copy_from_slice(&(channels as u16).to_le_bytes());
    h[6..8].copy_from_slice(&format.to_le_bytes());
    let r = unsafe {
        libc::send(sock, h.as_ptr() as *const libc::c_void, HDR_SZ, libc::MSG_NOSIGNAL)
    };
    r as usize == HDR_SZ
}

// ── Overrides ──

/// Real ALSA: Loads a shared library and returns a handle for `dlsym`.
/// Our impl: If the path contains "libasound", return `FAKE_HANDLE` without loading anything.
/// Otherwise, delegate to the real `dlopen` via `dlvsym(RTLD_NEXT)`.
#[no_mangle]
pub unsafe extern "C" fn dlopen(path: *const libc::c_char, flags: libc::c_int) -> *mut libc::c_void {
    if !path.is_null() {
        if let Ok(s) = CStr::from_ptr(path).to_str() {
            if s.contains("libasound.so") {
                return FAKE_HANDLE;
            }
        }
    }
    real_dlopen()(path, flags)
}

/// Real ALSA: Unloads a shared library.
/// Our impl: If handle is `FAKE_HANDLE`, do nothing. Otherwise delegate to real `dlclose`.
#[no_mangle]
pub unsafe extern "C" fn dlclose(handle: *mut libc::c_void) -> libc::c_int {
    if handle == FAKE_HANDLE { return 0; }
    real_dlclose()(handle)
}

/// Storage backing the `snd_config` global that cubeb dlsyms. Always NULL,
/// so cubeb's PulseAudio handle_underrun workaround
/// (`init_local_config_with_workaround`, which requires a real config tree)
/// bails out immediately and cubeb uses plain `snd_pcm_open`.
static SND_CONFIG_STORAGE: std::sync::atomic::AtomicPtr<libc::c_void> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// Real ALSA: Looks up a symbol in a loaded library handle.
/// Our impl: If handle is `FAKE_HANDLE`, look up the symbol in our internal table (`lookup()`).
/// If not found, return NULL (the standard dlsym "not found" contract) and log
/// loudly. We must NOT fall through to real libasound here: a caller that
/// resolved a real function against our fake handle would then call it with
/// one of our fake `snd_pcm_t` handles — exactly the corruption that produced
/// Firefox's "OpenCubeb() failed" before the cubeb symbol set was implemented.
#[no_mangle]
pub unsafe extern "C" fn dlsym(handle: *mut libc::c_void, symbol: *const libc::c_char) -> *mut libc::c_void {
    if handle == FAKE_HANDLE && !symbol.is_null() {
        if let Ok(name) = CStr::from_ptr(symbol).to_str() {
            if name == "snd_config" {
                // `snd_config` is a global variable, not a function: dlsym
                // must return the *address* of a `snd_config_t*`. Point at
                // our always-NULL storage.
                return &SND_CONFIG_STORAGE as *const std::sync::atomic::AtomicPtr<libc::c_void> as *mut libc::c_void
            }
            if let Some(f) = lookup(name) {
                return f;
            }
            warn!("dlsym(FAKE_HANDLE, \"{name}\") -> NULL (not in shim lookup; add it to lookup() if a client needs it)");
        }
        return ptr::null_mut();
    }
    // Real handle (or RTLD_DEFAULT/RTLD_NEXT): the app is genuinely resolving
    // against glibc/other libraries — delegate to the real dlsym.
    if !symbol.is_null() {
        real_dlsym()(handle, symbol)
    } else {
        ptr::null_mut()
    }
}



// ── Symbol lookup ──

macro_rules! sym {
    ($name:expr, $func:ident) => {
        if $name == stringify!($func) || $name == concat!(stringify!($func), "@ALSA_0.9") {
            return Some($func as *mut libc::c_void);
        }
    };
}

unsafe fn lookup(name: &str) -> Option<*mut libc::c_void> {
    sym!(name, snd_pcm_open); sym!(name, snd_pcm_close); sym!(name, snd_pcm_writei);
    sym!(name, snd_pcm_prepare); sym!(name, snd_pcm_drain); sym!(name, snd_pcm_drop);
    sym!(name, snd_pcm_start); sym!(name, snd_pcm_pause); sym!(name, snd_pcm_state);
    sym!(name, snd_pcm_avail_update); sym!(name, snd_pcm_avail); sym!(name, snd_pcm_rewind);
    sym!(name, snd_pcm_forward); sym!(name, snd_pcm_nonblock); sym!(name, snd_pcm_delay);
    sym!(name, snd_pcm_hw_params_malloc); sym!(name, snd_pcm_hw_params_free);
    sym!(name, snd_pcm_hw_params_any); sym!(name, snd_pcm_hw_params_set_access);
    sym!(name, snd_pcm_hw_params_set_format); sym!(name, snd_pcm_hw_params_set_channels);
    sym!(name, snd_pcm_hw_params_set_rate_near);
    sym!(name, snd_pcm_hw_params_set_buffer_size_near);
    sym!(name, snd_pcm_hw_params_set_period_size_near);
    sym!(name, snd_pcm_hw_params_set_periods_near);
    sym!(name, snd_pcm_hw_params_get_buffer_size);
    sym!(name, snd_pcm_hw_params_get_period_size);
    sym!(name, snd_pcm_hw_params_get_period_time);
    sym!(name, snd_pcm_hw_params); sym!(name, snd_pcm_hw_params_current);
    sym!(name, snd_pcm_sw_params_malloc); sym!(name, snd_pcm_sw_params_free);
    sym!(name, snd_pcm_sw_params_current);
    sym!(name, snd_pcm_sw_params_set_avail_min);
    sym!(name, snd_pcm_sw_params_set_start_threshold);
    sym!(name, snd_pcm_sw_params_set_stop_threshold);
    sym!(name, snd_pcm_sw_params_get_boundary);
    sym!(name, snd_pcm_sw_params);
    sym!(name, snd_strerror);
    sym!(name, snd_pcm_format_size); sym!(name, snd_pcm_format_physical_width);
    sym!(name, snd_pcm_format_width); sym!(name, snd_pcm_format_little_endian);
    sym!(name, snd_pcm_format_signed);
    sym!(name, snd_pcm_recover);
    // cubeb (Firefox) — implemented below against the fake-pcm model so the
    // client never reaches real libasound with one of our handles.
    sym!(name, snd_pcm_set_params); sym!(name, snd_pcm_get_params);
    sym!(name, snd_pcm_frames_to_bytes);
    sym!(name, snd_pcm_open_lconf);
    sym!(name, snd_pcm_poll_descriptors); sym!(name, snd_pcm_poll_descriptors_count);
    sym!(name, snd_pcm_poll_descriptors_revents);
    sym!(name, snd_pcm_hw_params_get_channels_max); sym!(name, snd_pcm_hw_params_get_rate);
    sym!(name, snd_config_add); sym!(name, snd_config_copy); sym!(name, snd_config_delete);
    sym!(name, snd_config_get_id); sym!(name, snd_config_get_string);
    sym!(name, snd_config_imake_integer); sym!(name, snd_config_search);
    sym!(name, snd_config_search_definition);
    sym!(name, snd_lib_error_set_handler);
    sym!(name, snd_pcm_dump); sym!(name, snd_pcm_hw_params_dump);
    sym!(name, snd_pcm_hw_params_can_pause); sym!(name, snd_pcm_hw_params_copy);
    sym!(name, snd_pcm_hw_params_get_buffer_size_max);
    sym!(name, snd_pcm_hw_params_get_period_size_min);
    sym!(name, snd_pcm_hw_params_set_buffer_time_near);
    sym!(name, snd_pcm_hw_params_set_channels_near);
    sym!(name, snd_pcm_hw_params_set_rate_resample);
    sym!(name, snd_pcm_hw_params_sizeof);
    sym!(name, snd_pcm_hw_params_test_format);
    sym!(name, snd_pcm_sw_params_set_silence_size);
    sym!(name, snd_pcm_sw_params_sizeof);
    sym!(name, snd_pcm_status); sym!(name, snd_pcm_status_get_avail);
    sym!(name, snd_pcm_status_get_delay); sym!(name, snd_pcm_status_get_state);
    sym!(name, snd_pcm_status_sizeof);
    sym!(name, snd_pcm_state_name); sym!(name, snd_pcm_readi);
    sym!(name, snd_pcm_writen); sym!(name, snd_pcm_resume);
    sym!(name, snd_pcm_stream);
    sym!(name, snd_output_buffer_open); sym!(name, snd_output_buffer_string);
    sym!(name, snd_output_close); sym!(name, snd_output_flush);
    sym!(name, snd_pcm_get_chmap); sym!(name, snd_pcm_set_chmap);
    sym!(name, snd_pcm_free_chmaps); sym!(name, snd_pcm_query_chmaps);
    sym!(name, snd_pcm_chmap_print);
    sym!(name, snd_pcm_chmap_name); sym!(name, snd_pcm_chmap_long_name);
    sym!(name, snd_pcm_chmap_type_name);
    None
}

// ── PCM impl ──

/// Write all bytes to fd (retry on EAGAIN). Returns true on success.
/// Uses `send(MSG_NOSIGNAL)` so a dead peer returns EPIPE instead of raising
/// SIGPIPE (which would kill the host app before the reconnect loop runs).
fn write_all(fd: i32, buf: &[u8]) -> bool {
    let mut pos = 0;
    while pos < buf.len() {
        let n = unsafe {
            libc::send(
                fd,
                buf[pos..].as_ptr() as *const libc::c_void,
                buf.len() - pos,
                libc::MSG_NOSIGNAL,
            )
        };
        if n < 0 {
            let err = unsafe { *libc::__errno_location() };
            if err == libc::EINTR { continue; }
            if err == libc::EAGAIN || err == libc::EWOULDBLOCK {
                std::thread::yield_now();
                continue;
            }
            return false;
        }
        pos += n as usize;
    }
    true
}

/// Background drain thread: pops data from the ring buffer and writes it to the
/// socket (blocking) with length-prefixed framing. Also handles mute/flush
/// control commands by checking MUTE_CMD/FLUSH_CMD between chunks. If the
/// socket dies (audiod restarted/crashed), reconnects automatically so audio
/// resumes instead of being silently dropped.
fn drain_thread_loop(
    sock_fd: &AtomicI32,
    ring: &AudioRingBuffer,
    stop: &AtomicBool,
    total_written: &AtomicUsize,
    total_pushed: &AtomicUsize,
    play_start: &std::sync::Mutex<Option<std::time::Instant>>,
    header: &std::sync::Mutex<Option<(u32, u32, u16)>>,
) {
    let mut buf = vec![0u8; DRAIN_CHUNK];
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let cmd = MUTE_CMD.swap(0, Ordering::AcqRel);
        if cmd != 0 {
            let fd = sock_fd.load(Ordering::Acquire);
            if fd >= 0 && !send_frame(fd, &[cmd]) {
                warn!("drain: mute command write failed");
                if !reconnect_audiod(sock_fd, stop, header, total_written, total_pushed, play_start) {
                    break;
                }
            }
        }
        if FLUSH_CMD.swap(false, Ordering::AcqRel) {
            let fd = sock_fd.load(Ordering::Acquire);
            if fd >= 0 {
                if !send_frame(fd, &[MSG_FLUSH]) {
                    warn!("drain: flush command write failed");
                    if !reconnect_audiod(sock_fd, stop, header, total_written, total_pushed, play_start) {
                        break;
                    }
                } else {
                    debug!("sending MSG_FLUSH to server");
                }
            }
        }
        let n = ring.pop(&mut buf);
        if n == 0 {
            continue;
        }
        let fd = sock_fd.load(Ordering::Acquire);
        if fd < 0 {
            continue;
        }
        if !send_frame(fd, &buf[..n]) {
            warn!("drain: PCM write failed — reconnecting to audiod");
            if !reconnect_audiod(sock_fd, stop, header, total_written, total_pushed, play_start) {
                break;
            }
            // Drop the partially-sent chunk; the new server session starts
            // clean on the next pop.
            continue;
        }
        total_written.fetch_add(n, Ordering::Release);
    }
}

/// Write one length-prefixed protocol frame (control command or PCM chunk).
fn send_frame(fd: i32, payload: &[u8]) -> bool {
    let len = (payload.len() as u32).to_le_bytes();
    write_all(fd, &len) && write_all(fd, payload)
}

/// Attempt to reconnect to audiod after a socket write failure, retrying with
/// exponential backoff until `stop` is set. On success, re-sends the stashed
/// 12-byte protocol header and resets session counters/playhead so both sides
/// start from a clean stream. Returns false if the drain thread should give
/// up (shutdown requested).
fn reconnect_audiod(
    sock_fd: &AtomicI32,
    stop: &AtomicBool,
    header: &std::sync::Mutex<Option<(u32, u32, u16)>>,
    total_written: &AtomicUsize,
    total_pushed: &AtomicUsize,
    play_start: &std::sync::Mutex<Option<std::time::Instant>>,
) -> bool {
    let old = sock_fd.swap(-1, Ordering::AcqRel);
    if old >= 0 {
        unsafe { libc::close(old) };
    }
    let mut backoff = 50u64;
    let mut attempts = 0u64;
    let mut last_warn = std::time::Instant::now();
    while !stop.load(Ordering::Acquire) {
        if let Ok(fd) = connect_audiod() {
            sock_fd.store(fd, Ordering::Release);
            if let Some((rate, channels, format)) = *header.lock().unwrap() {
                if !send_header(fd, rate, channels, format) {
                    warn!("reconnect: failed to re-send header on fd {}", fd);
                }
            }
            total_written.store(0, Ordering::Release);
            total_pushed.store(0, Ordering::Release);
            *play_start.lock().unwrap() = None;
            info!("drain: reconnected to audiod after {attempts} failed attempt(s)");
            return true;
        }
        attempts += 1;
        if last_warn.elapsed().as_secs() >= 10 {
            warn!(
                "reconnect: audiod still unreachable after {attempts} attempt(s), \
                 retrying every {}ms",
                backoff
            );
            last_warn = std::time::Instant::now();
        }
        std::thread::sleep(std::time::Duration::from_millis(backoff));
        backoff = (backoff * 2).min(2000);
    }
    false
}

/// Real ALSA: Returns a human-readable error string for an ALSA error code (positive or negative).
/// Our impl: Handles common errno values with descriptive C strings. For unknown
/// codes, formats into a thread-local buffer (static mut would be a data race).
#[no_mangle]
pub unsafe extern "C" fn snd_strerror(errnum: libc::c_int) -> *const libc::c_char {
    thread_local! {
        static ERR_BUF: std::cell::RefCell<[u8; 64]> = const { std::cell::RefCell::new([0u8; 64]) };
    }
    let s: *const libc::c_char = match errnum {
        0 => c"Success".as_ptr(),
        -1 | 1 => c"Operation not permitted".as_ptr(),
        -2 | 2 => c"No such file or directory".as_ptr(),
        -5 | 5 => c"Input/output error".as_ptr(),
        -6 | 6 => c"No such device or address".as_ptr(),
        -9 | 9 => c"Bad file descriptor".as_ptr(),
        -11 | 11 => c"Resource temporarily unavailable".as_ptr(),
        -12 | 12 => c"Cannot allocate memory".as_ptr(),
        -13 | 13 => c"Permission denied".as_ptr(),
        -16 | 16 => c"Device or resource busy".as_ptr(),
        -19 | 19 => c"No such device".as_ptr(),
        -22 | 22 => c"Invalid argument".as_ptr(),
        -32 | 32 => c"Broken pipe".as_ptr(),
        -77 | 77 => c"Function not implemented".as_ptr(),
         -95 | 95 => c"Operation not supported".as_ptr(),
        -112 | 112 => c"Host is down".as_ptr(),
        _ => {
            use std::fmt::Write;
            let mut s = String::new();
            write!(s, "errno={}", errnum).ok();
            ERR_BUF.with(|cell| {
                let mut buf = cell.borrow_mut();
                let b = s.as_bytes();
                let n = b.len().min(buf.len() - 1);
                buf[..n].copy_from_slice(&b[..n]);
                buf[n] = 0;
                buf.as_ptr() as *const libc::c_char
            })
        }
    };
    s
}

/// Shared delay/avail model: the shim presents a virtual device whose total
/// buffer is exactly `BUFFER_SIZE` frames. `delay` is derived from a
/// *time-based playhead* (frames pushed by the app minus frames the DAC has
/// consumed, `rate` per wall-clock second) plus a fixed `SERVER_FRAMES`
/// baseline for the server/HDA pipeline. Because the playhead advances
/// continuously (frame by frame), `delay`/`avail` move smoothly instead of
/// jumping in drain-chunk quanta — mirroring how a real snd_pcm's delay
/// decreases as hardware consumes. `delay + avail == BUFFER_SIZE`, so an app
/// can never write more than the advertised buffer ahead (no multi-second
/// backlog, which made mpv's A/V sync engine drop video).
pub fn pcm_delay_avail(h: &snd_pcm_t) -> (isize, isize) {
    let bps = pcm_format_bytes(h.format as u16);
    let fb = (bps * h.channels as usize) as isize;
    if fb == 0 {
        return (SERVER_FRAMES, BUFFER_SIZE - SERVER_FRAMES);
    }
    let pushed_frames = (h.total_pushed.load(Ordering::Acquire) as isize) / fb;
    let consumed_frames = match *h.play_start.lock().unwrap() {
        Some(t0) => (h.rate as f64 * t0.elapsed().as_secs_f64()) as isize,
        None => 0,
    };
    let unplayed = (pushed_frames - consumed_frames).max(0) + SERVER_FRAMES;
    let delay = unplayed.min(BUFFER_SIZE);
    (delay, BUFFER_SIZE - delay)
}

/// Real ALSA: Returns the number of frames buffered but not yet played (DMA + kernel buffer).
/// Our impl: `ring` backlog + the fixed server-side baseline, clamped to BUFFER_SIZE.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_delay(p: *mut snd_pcm_t, d: *mut isize) -> libc::c_int {
    if d.is_null() { return -libc::EINVAL; }
    let h = match pcm_ref(p) {
        Some(h) => h,
        None => return -libc::EINVAL,
    };
    *d = pcm_delay_avail(h).0;
    0
}

/// Real ALSA: Returns the number of frames that can be written without blocking.
/// Our impl: `BUFFER_SIZE - delay` (never more than the advertised buffer).
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_avail_update(p: *mut snd_pcm_t) -> isize {
    let h = match pcm_ref(p) {
        Some(h) => h,
        None => return 0,
    };
    pcm_delay_avail(h).1
}

/// Real ALSA: Same as `snd_pcm_avail_update`, but may block. We just delegate.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_avail(p: *mut snd_pcm_t) -> isize { snd_pcm_avail_update(p) }

/// Real ALSA: Opens a PCM device, returns an opaque handle.
/// Our impl: Connects to audiod, allocates a ring buffer, spawns the drain thread.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_open(
    pcm: *mut *mut snd_pcm_t, _name: *const libc::c_char,
    _stream: libc::c_int, _mode: libc::c_int,
) -> libc::c_int {
    init_logging();
    let sock = match connect_audiod() {
        Ok(s) => s,
        Err(_) => {
            error!("connect_audiod failed — is audiod running?");
            return -libc::ENOENT;
        }
    };
    let cfg = audcommon::config::load_config();
    let ring = Arc::new(AudioRingBuffer::new(cfg.shim.ring_buffer_size));
    let sock_fd = Arc::new(AtomicI32::new(sock));
    let drain_stop = Arc::new(AtomicBool::new(false));
    let total_pushed = Arc::new(AtomicUsize::new(0));
    let total_written = Arc::new(AtomicUsize::new(0));
    let play_start = Arc::new(std::sync::Mutex::new(None));
    let header = Arc::new(std::sync::Mutex::new(None));
    let ring_for_drain = ring.clone();
    let sock_for_drain = sock_fd.clone();
    let stop_for_drain = drain_stop.clone();
    let pushed_for_drain = total_pushed.clone();
    let written_for_drain = total_written.clone();
    let play_for_drain = play_start.clone();
    let header_for_drain = header.clone();
    let drain_handle = std::thread::Builder::new()
        .stack_size(65536)
        .name("audshim-drain".into())
        .spawn(move || {
            drain_thread_loop(
                &sock_for_drain,
                &ring_for_drain,
                &stop_for_drain,
                &written_for_drain,
                &pushed_for_drain,
                &play_for_drain,
                &header_for_drain,
            )
        })
        .expect("spawn drain thread");
    let poll_fd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_NONBLOCK | libc::TFD_CLOEXEC) };
    if poll_fd >= 0 {
        arm_poll_timer(poll_fd);
    }
    *pcm = Box::into_raw(Box::new(snd_pcm_t {
        sock: sock_fd, rate: 0, channels: 0, format: 2, configured: false,
        state: SND_PCM_STATE_SETUP,
        ring, drain_stop,
        total_pushed, total_written, play_start, header,
        drain_handle: Some(drain_handle),
        poll_fd,
    }));
    info!("opening PCM (sock={})", sock);
    0
}

/// Real ALSA: Closes the PCM device, frees kernel resources.
/// Our impl: Signals drain thread to stop, waits for it, then closes socket.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_close(pcm: *mut snd_pcm_t) -> libc::c_int {
    if pcm.is_null() { return -libc::EINVAL; }
    let h = Box::from_raw(pcm);
    h.drain_stop.store(true, Ordering::Release);
    h.ring.interrupt();
    if let Some(handle) = h.drain_handle {
        let _ = handle.join();
    }
    let fd = h.sock.load(Ordering::Acquire);
    if fd >= 0 {
        libc::close(fd);
    }
    if h.poll_fd >= 0 {
        libc::close(h.poll_fd);
    }
    info!("closing PCM");
    0
}

/// Real ALSA: Prepares the PCM for playback, resetting the DMA buffer and state.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_prepare(p: *mut snd_pcm_t) -> libc::c_int {
    let h = match pcm_mut(p) {
        Some(h) => h,
        None => return -libc::EINVAL,
    };
    h.state = SND_PCM_STATE_PREPARED;
    *h.play_start.lock().unwrap() = None;
    0
}

/// Real ALSA: Drains the PCM playback buffer, blocking until all remaining frames
/// have been played by the hardware.
/// Our impl: Let existing data drain naturally through the pipeline. Keep the
/// socket and ring alive — the server continues playing whatever is buffered.
/// Returns immediately (non-blocking); close() will clean up remaining data.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_drain(p: *mut snd_pcm_t) -> libc::c_int {
    let h = match pcm_mut(p) {
        Some(h) => h,
        None => return -libc::EINVAL,
    };
    h.state = SND_PCM_STATE_PREPARED;
    0
}

/// Real ALSA: Drops the PCM stream immediately, discarding any buffered data.
/// Our impl: Clears the ring buffer (discards pending data) but keeps the socket
/// connection alive. The ~85ms of data already in the kernel DMA buffer plays
/// out naturally, then the server blocks on read waiting for new data from the
/// next `snd_pcm_writei`. No reconnect overhead, no PCM reconfiguration.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_drop(p: *mut snd_pcm_t) -> libc::c_int {
    let h = match pcm_mut(p) {
        Some(h) => h,
        None => return -libc::EINVAL,
    };
    h.ring.clear();
    h.total_pushed.store(0, Ordering::Release);
    h.total_written.store(0, Ordering::Release);
    *h.play_start.lock().unwrap() = None;
    let was_running = h.state == SND_PCM_STATE_RUNNING;
    h.state = SND_PCM_STATE_PREPARED;
    if was_running || h.configured {
        // Tell the server to stop its backend so a pending HDA DMA ring
        // can't wrap around and re-play stale audio. Only meaningful on the
        // HDA backend, but harmless to always send.
        FLUSH_CMD.store(true, Ordering::Release);
    }
    0
}

/// Real ALSA: Starts the PCM stream.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_start(p: *mut snd_pcm_t) -> libc::c_int {
    let h = match pcm_mut(p) {
        Some(h) => h,
        None => return -libc::EINVAL,
    };
    h.state = SND_PCM_STATE_RUNNING;
    0
}

#[no_mangle]
pub unsafe extern "C" fn snd_pcm_pause(p: *mut snd_pcm_t, e: libc::c_int) -> libc::c_int {
    let h = match pcm_mut(p) {
        Some(h) => h,
        None => return -libc::EINVAL,
    };
    if e != 0 {
        h.state = SND_PCM_STATE_PAUSED;
        FLUSH_CMD.store(true, Ordering::Release);
    } else {
        h.state = SND_PCM_STATE_RUNNING;
    }
    0
}

/// Real ALSA: Returns the current PCM state.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_state(p: *mut snd_pcm_t) -> libc::c_int {
    match pcm_ref(p) {
        Some(h) => h.state,
        None => SND_PCM_STATE_OPEN,
    }
}
#[no_mangle] pub unsafe extern "C" fn snd_pcm_rewind(_: *mut snd_pcm_t, _f: isize) -> isize { 0 }
#[no_mangle] pub unsafe extern "C" fn snd_pcm_forward(_: *mut snd_pcm_t, _f: isize) -> isize { 0 }
#[no_mangle] pub unsafe extern "C" fn snd_pcm_nonblock(_: *mut snd_pcm_t, _n: libc::c_int) -> libc::c_int { 0 }
#[no_mangle] pub unsafe extern "C" fn snd_pcm_recover(_: *mut snd_pcm_t, _e: libc::c_int, _s: libc::c_int) -> libc::c_int { 0 }

// HW params
#[no_mangle] pub unsafe extern "C" fn snd_pcm_hw_params_malloc(p: *mut *mut libc::c_void) -> libc::c_int {
    if p.is_null() { return -libc::EINVAL; }
    *p = libc::malloc(HW_PARAMS_SIZE);
    if (*p).is_null() { return -libc::ENOMEM; }
    0
}
#[no_mangle] pub unsafe extern "C" fn snd_pcm_hw_params_free(p: *mut libc::c_void) {
    if !p.is_null() { unsafe { libc::free(p) }; }
}
/// Real ALSA: Fills params with the current device configuration.
/// Our impl: Zeroes the opaque blob and stashes our default rate at offset 0,
/// so `snd_pcm_hw_params_get_rate` reads a deterministic value.
#[no_mangle] pub unsafe extern "C" fn snd_pcm_hw_params_any(_: *mut snd_pcm_t, p: *mut libc::c_void) -> libc::c_int {
    if !p.is_null() {
        let slice = std::slice::from_raw_parts_mut(p as *mut u8, HW_PARAMS_SIZE);
        slice.fill(0);
        slice[0..4].copy_from_slice(&48000u32.to_le_bytes());
        slice[6..8].copy_from_slice(&2u16.to_le_bytes());
    }
    0
}
#[no_mangle] pub unsafe extern "C" fn snd_pcm_hw_params_current(_: *mut snd_pcm_t, _p: *mut libc::c_void) -> libc::c_int { 0 }
#[no_mangle] pub unsafe extern "C" fn snd_pcm_hw_params_set_access(_: *mut snd_pcm_t, _p: *mut libc::c_void, _a: libc::c_int) -> libc::c_int { 0 }

/// Real ALSA: Proposes a sample format.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_hw_params_set_format(
    p: *mut snd_pcm_t, _: *mut libc::c_void, fmt: libc::c_int,
) -> libc::c_int {
    if let Some(h) = pcm_mut(p) { h.format = fmt; }
    0
}

/// Real ALSA: Proposes channel count.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_hw_params_set_channels(
    p: *mut snd_pcm_t, _: *mut libc::c_void, val: libc::c_uint,
) -> libc::c_int {
    if let Some(h) = pcm_mut(p) { h.channels = val; }
    0
}

/// Real ALSA: Proposes a sample rate.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_hw_params_set_rate_near(
    p: *mut snd_pcm_t, params: *mut libc::c_void, val: *mut libc::c_uint, _d: *mut libc::c_int,
) -> libc::c_int {
    if !val.is_null() {
        if let Some(h) = pcm_mut(p) { h.rate = *val; }
        if !params.is_null() {
            let slice = std::slice::from_raw_parts_mut(params as *mut u8, HW_PARAMS_SIZE);
            slice[0..4].copy_from_slice(&(*val).to_le_bytes());
        }
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn snd_pcm_hw_params_set_buffer_size_near(
    _: *mut snd_pcm_t, _: *mut libc::c_void, _: *mut libc::c_uint,
) -> libc::c_int { 0 }

#[no_mangle]
pub unsafe extern "C" fn snd_pcm_hw_params_set_buffer_time_near(
    _: *mut snd_pcm_t, _: *mut libc::c_void, _: *mut libc::c_uint, _: *mut libc::c_int,
) -> libc::c_int { 0 }

#[no_mangle]
pub unsafe extern "C" fn snd_pcm_hw_params_set_period_size_near(
    _: *mut snd_pcm_t, _: *mut libc::c_void, _: *mut libc::c_uint, _: *mut libc::c_int,
) -> libc::c_int { 0 }

#[no_mangle]
pub unsafe extern "C" fn snd_pcm_hw_params_set_periods_near(
    _: *mut snd_pcm_t, _: *mut libc::c_void, _: *mut libc::c_uint, _: *mut libc::c_int,
) -> libc::c_int { 0 }

#[no_mangle]
pub unsafe extern "C" fn snd_pcm_hw_params_get_buffer_size(
    _: *mut libc::c_void, val: *mut isize,
) -> libc::c_int { if !val.is_null() { *val = BUFFER_SIZE; } 0 }

#[no_mangle]
pub unsafe extern "C" fn snd_pcm_hw_params_get_period_size(
    _: *mut libc::c_void, val: *mut isize, _d: *mut libc::c_int,
) -> libc::c_int { if !val.is_null() { *val = 1024; } 0 }

#[no_mangle]
pub unsafe extern "C" fn snd_pcm_hw_params_get_period_time(
    _: *mut libc::c_void, val: *mut libc::c_uint, _d: *mut libc::c_int,
) -> libc::c_int { if !val.is_null() { *val = 23219; } 0 }

/// Commit the currently-proposed rate/channels/format to the server: sends
/// the 12-byte protocol header once. Shared by `snd_pcm_hw_params` and
/// `snd_pcm_set_params` (cubeb's one-call config path).
fn commit_configured(h: &mut snd_pcm_t) -> libc::c_int {
    if h.rate == 0 || h.channels == 0 { return -libc::EINVAL; }
    if !h.configured {
        let fd = h.sock.load(Ordering::Acquire);
        if fd < 0 { return -libc::EPIPE; }
        // Stash the config so the drain thread can re-send the 12-byte header
        // after reconnecting to a restarted audiod.
        *h.header.lock().unwrap() = Some((h.rate, h.channels, h.format as u16));
        if !send_header(fd, h.rate, h.channels, h.format as u16) { return -libc::EIO; }
        h.configured = true;
        h.state = SND_PCM_STATE_PREPARED;
    }
    0
}

/// Real ALSA: Commits hardware parameters to the kernel.
/// Our impl: Sends the protocol header to audiod once.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_hw_params(
    p: *mut snd_pcm_t, _: *mut libc::c_void,
) -> libc::c_int {
    let h = match pcm_mut(p) {
        Some(h) => h,
        None => return -libc::EINVAL,
    };
    commit_configured(h)
}

/// Real ALSA: Configures the PCM in one call — format, access, channels,
/// rate, resampling and a latency target — then prepares it.
/// Our impl: Stashes the values and commits the protocol header, like
/// `snd_pcm_hw_params`. This is cubeb's configuration path; it must never
/// reach real libasound, which would interpret our fake handle as a real
/// snd_pcm and fail (the Firefox "OpenCubeb failed" symptom).
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_set_params(
    p: *mut snd_pcm_t,
    format: libc::c_int,
    _access: libc::c_int,
    channels: libc::c_uint,
    rate: libc::c_uint,
    _soft_resample: libc::c_int,
    _latency_us: libc::c_uint,
) -> libc::c_int {
    let h = match pcm_mut(p) {
        Some(h) => h,
        None => return -libc::EINVAL,
    };
    h.format = format;
    h.channels = channels;
    h.rate = rate;
    commit_configured(h)
}

// SW params
#[no_mangle] pub unsafe extern "C" fn snd_pcm_sw_params_malloc(p: *mut *mut libc::c_void) -> libc::c_int {
    if p.is_null() { return -libc::EINVAL; }
    *p = libc::malloc(SW_PARAMS_SIZE);
    if (*p).is_null() { return -libc::ENOMEM; }
    0
}
#[no_mangle] pub unsafe extern "C" fn snd_pcm_sw_params_free(p: *mut libc::c_void) {
    if !p.is_null() { unsafe { libc::free(p) }; }
}
#[no_mangle] pub unsafe extern "C" fn snd_pcm_sw_params_current(_: *mut snd_pcm_t, _p: *mut libc::c_void) -> libc::c_int { 0 }
#[no_mangle] pub unsafe extern "C" fn snd_pcm_sw_params_set_avail_min(_: *mut snd_pcm_t, _p: *mut libc::c_void, _v: isize) -> libc::c_int { 0 }
#[no_mangle] pub unsafe extern "C" fn snd_pcm_sw_params_set_start_threshold(_: *mut snd_pcm_t, _p: *mut libc::c_void, _v: isize) -> libc::c_int { 0 }
#[no_mangle] pub unsafe extern "C" fn snd_pcm_sw_params_set_stop_threshold(_: *mut snd_pcm_t, _p: *mut libc::c_void, _v: isize) -> libc::c_int { 0 }
#[no_mangle] pub unsafe extern "C" fn snd_pcm_sw_params_get_boundary(_: *mut libc::c_void, v: *mut isize) -> libc::c_int {
    if !v.is_null() { *v = isize::MAX; } 0
}
#[no_mangle] pub unsafe extern "C" fn snd_pcm_sw_params(_: *mut snd_pcm_t, _p: *mut libc::c_void) -> libc::c_int { 0 }

// ── cubeb (Firefox) plumbing ──
// cubeb's ALSA backend dlsyms these from libasound and calls them on the
// handle we return from `snd_pcm_open`. They must be implemented against our
// fake-pcm model — if any of them resolves to real libasound, the real
// function reads our Rust struct as a real snd_pcm_t and corrupts/fails.

/// Real ALSA: Returns the configured buffer and period sizes in frames.
/// Our impl: Fixed 8192-frame buffer, 1024-frame period (matches BUFFER_SIZE
/// and DRAIN_CHUNK-modeled pacing). cubeb asserts this succeeds.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_get_params(
    _: *mut snd_pcm_t, buffer_size: *mut isize, period_size: *mut isize,
) -> libc::c_int {
    if !buffer_size.is_null() { *buffer_size = BUFFER_SIZE; }
    if !period_size.is_null() { *period_size = 1024; }
    0
}

/// Real ALSA: Bytes consumed by `frames` frames of the PCM's current format.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_frames_to_bytes(p: *mut snd_pcm_t, frames: isize) -> usize {
    if frames <= 0 { return 0; }
    let fb = match pcm_ref(p) {
        Some(h) => pcm_format_bytes(h.format as u16) * h.channels.max(1) as usize,
        None => return 0,
    };
    (frames as usize) * fb
}

/// Real ALSA: Opens a PCM using an explicit local config tree.
/// Our impl: Ignores the config (we always route to audiod) and delegates to
/// `snd_pcm_open`. cubeb only passes a non-NULL config for the PulseAudio
/// workaround, which our NULL `snd_config` global prevents from triggering.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_open_lconf(
    pcm: *mut *mut snd_pcm_t, name: *const libc::c_char,
    stream: libc::c_int, mode: libc::c_int, _lconf: *mut libc::c_void,
) -> libc::c_int {
    snd_pcm_open(pcm, name, stream, mode)
}

/// Real ALSA: Number of poll descriptors for this PCM.
/// Our impl: Always one — the poll timer fd.
#[no_mangle] pub unsafe extern "C" fn snd_pcm_poll_descriptors_count(_: *mut snd_pcm_t) -> libc::c_int { 1 }

/// Real ALSA: Fills `pfds` with the fds cubeb polls to know when the PCM is
/// writable. Our impl: returns the timerfd with POLLIN. We can't signal
/// "writable" like hardware, so the timer paces cubeb's write loop at a rate
/// the time-based playhead tracks continuously.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_poll_descriptors(
    p: *mut snd_pcm_t, pfds: *mut libc::pollfd, nfds: libc::c_uint,
) -> libc::c_int {
    if nfds == 0 || pfds.is_null() { return 0; }
    let fd = match pcm_ref(p) {
        Some(h) => h.poll_fd,
        None => return 0,
    };
    if fd < 0 { return 0; }
    (*pfds).fd = fd;
    (*pfds).events = libc::POLLIN;
    (*pfds).revents = 0;
    1
}

/// Real ALSA: Translates raw poll events into snd_pcm readable/writable
/// events. Our impl: passes the polled revents through, and drains/re-arms
/// the one-shot poll timer so `poll()` stays paced at 10 ms.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_poll_descriptors_revents(
    p: *mut snd_pcm_t, pfds: *mut libc::pollfd, nfds: libc::c_uint, revents: *mut u16,
) -> libc::c_int {
    if revents.is_null() { return -libc::EINVAL; }
    *revents = 0;
    if nfds == 0 || pfds.is_null() { return 0; }
    let f = &*pfds;
    *revents = f.revents as u16;
    if let Some(h) = pcm_ref(p) {
        if h.poll_fd >= 0 && (f.revents & libc::POLLIN) != 0 {
            drain_poll_timer(h.poll_fd);
            arm_poll_timer(h.poll_fd);
        }
    }
    0
}

/// Real ALSA: Maximum channel count for the configured PCM.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_hw_params_get_channels_max(
    _: *mut libc::c_void, max: *mut libc::c_uint,
) -> libc::c_int {
    if max.is_null() { return -libc::EINVAL; }
    *max = 2;
    0
}

/// Real ALSA: Current rate for the configured PCM. Reads the rate stashed in
/// the param blob by `snd_pcm_hw_params_any`/`_set_rate_near` (default
/// 48000). cubeb uses this only for device enumeration.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_hw_params_get_rate(
    params: *mut libc::c_void, rate: *mut libc::c_uint, dir: *mut libc::c_int,
) -> libc::c_int {
    if rate.is_null() { return -libc::EINVAL; }
    let mut r = 48000u32;
    if !params.is_null() {
        let slice = std::slice::from_raw_parts(params as *const u8, HW_PARAMS_SIZE);
        r = u32::from_le_bytes(slice[0..4].try_into().unwrap());
        if r == 0 { r = 48000; }
    }
    *rate = r;
    if !dir.is_null() { *dir = 0; }
    0
}

// snd_config plumbing — cubeb LOADs these before it will use the backend.
// They are only exercised by the PulseAudio workaround, which our NULL
// `snd_config` global short-circuits, so they are harmless no-op/error stubs.
#[no_mangle] pub unsafe extern "C" fn snd_config_add(_: *mut libc::c_void, _: *mut libc::c_void) -> libc::c_int { -libc::ENOENT }
#[no_mangle] pub unsafe extern "C" fn snd_config_copy(out: *mut *mut libc::c_void, _src: *mut libc::c_void) -> libc::c_int {
    if out.is_null() { return -libc::EINVAL; }
    *out = std::ptr::null_mut();
    0
}
#[no_mangle] pub unsafe extern "C" fn snd_config_delete(_: *mut libc::c_void) -> libc::c_int { 0 }
#[no_mangle] pub unsafe extern "C" fn snd_config_get_id(_: *mut libc::c_void, _id: *mut *const libc::c_char) -> libc::c_int { -libc::ENOENT }
#[no_mangle] pub unsafe extern "C" fn snd_config_get_string(_: *mut libc::c_void, _s: *mut *const libc::c_char) -> libc::c_int { -libc::ENOENT }
#[no_mangle] pub unsafe extern "C" fn snd_config_imake_integer(_n: *mut *mut libc::c_void, _id: *const libc::c_char, _v: libc::c_long) -> libc::c_int { -libc::ENOENT }
#[no_mangle] pub unsafe extern "C" fn snd_config_search(_: *mut libc::c_void, _: *const libc::c_char, _: *mut *mut libc::c_void) -> libc::c_int { -libc::ENOENT }
#[no_mangle] pub unsafe extern "C" fn snd_config_search_definition(_: *mut libc::c_void, _: *const libc::c_char, _: *const libc::c_char, _: *mut *mut libc::c_void) -> libc::c_int { -libc::ENOENT }
#[no_mangle] pub unsafe extern "C" fn snd_lib_error_set_handler(_: *const libc::c_void) -> libc::c_int { 0 }

/// Real ALSA: Writes interleaved PCM frames to the device.
/// Our impl: Pushes data to the ring buffer. The drain thread writes it to the socket.
/// Never blocks unless the ring buffer is full (which absorbs bursts).
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_writei(
    pcm: *mut snd_pcm_t, buffer: *const libc::c_void, size: isize,
) -> isize {
    let h = match pcm_mut(pcm) {
        Some(h) => h,
        None => return -libc::EINVAL as isize,
    };
    if buffer.is_null() { return -libc::EINVAL as isize; }
    if !h.configured { return -libc::EPIPE as isize; }
    if h.channels == 0 { return -libc::EINVAL as isize; }
    // Anchor the playback playhead on the first write after (re)start. From
    // here, delay = pushed_frames - rate*elapsed, advancing continuously.
    {
        let mut ps = h.play_start.lock().unwrap();
        if ps.is_none() {
            *ps = Some(std::time::Instant::now());
        }
    }
    // NO socket liveness check here: the drain thread owns reconnects
    // asynchronously, so during an audiod restart h.sock is transiently -1.
    // Failing writei then (EBADF) makes mpv spam "pcm write error: Bad file
    // descriptor" and lose audio instead of stalling gracefully — writei must
    // push into the ring and block on ring-full (real ALSA backpressure)
    // until the drain reconnects and frees space.
    let bps = pcm_format_bytes(h.format as u16);
    let fb = bps * h.channels as usize;
    let nbytes = size as usize * fb;
    let slice = match input_slice(buffer, nbytes) {
        Some(s) => s,
        None => return -libc::EINVAL as isize,
    };
    let silent = slice.iter().all(|&b| b == 0);
    let prev = LAST_SILENT.swap(silent, Ordering::AcqRel);
    if silent && !prev {
        MUTE_CMD.store(MSG_MUTE, Ordering::Release);
    } else if !silent && prev {
        MUTE_CMD.store(MSG_UNMUTE, Ordering::Release);
    }
    let filled_before = h.ring.filled();
    let avail_before = h.ring.available();
    let push_start = std::time::Instant::now();
    h.ring.push(slice);
    let push_elapsed = push_start.elapsed();
    let pushed_total = h.total_pushed.fetch_add(nbytes, Ordering::Release) + nbytes;
    // Pull the poll timer forward so poll-based clients (cubeb) that are
    // mid-write don't spin on a stale readable expiration.
    if h.poll_fd >= 0 {
        drain_poll_timer(h.poll_fd);
        arm_poll_timer(h.poll_fd);
    }
    if push_elapsed > std::time::Duration::from_millis(10) {
        warn!("writei BLOCKED for {:?} (ring filled={} avail={} needed={})",
            push_elapsed, filled_before, avail_before, nbytes);
    }
    if pushed_total % (32 * 1024) < nbytes {
        let written_total = h.total_written.load(Ordering::Relaxed);
        let (delay, avail) = pcm_delay_avail(h);
        trace!("writei: ring_filled={} pushed_total={} written_total={} in_flight={} delay={} avail={}",
            h.ring.filled(), pushed_total, written_total,
            (pushed_total as isize - written_total as isize), delay, avail);
    }
    size
}

/// Real ALSA: Queries the channel map from the PCM device, returning it as a pointer.
/// Our impl: Allocates and returns a default stereo FL+FR chmap, heap-allocated.
/// mpv calls this with ONE argument (pcm handle) and expects a pointer return.
/// Must NOT go to libasound, which uses a different ABI (int return, out-param).
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_get_chmap(_: *mut snd_pcm_t) -> *mut libc::c_void {
    let sz = std::mem::size_of::<SndPcmChmap>();
    let p = libc::malloc(sz) as *mut SndPcmChmap;
    if p.is_null() {
        return std::ptr::null_mut();
    }
    (*p).channels = 2;
    (*p).pos[0] = SND_CHMAP_FL;
    (*p).pos[1] = SND_CHMAP_FR;
    debug!("snd_pcm_get_chmap -> allocated stereo chmap at {:?}", p);
    p as *mut libc::c_void
}

/// Real ALSA: Frees a channel map array (result of snd_pcm_query_chmaps).
/// Our impl: No-op — we never return query_chmaps results.
/// Must NOT go to libasound since the argument may be our fake pcm handle.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_free_chmaps(_maps: *mut libc::c_void) {
    // no-op
}

/// Real ALSA: Queries all available channel maps for a PCM device.
/// Our impl: Returns NULL — we only support fixed stereo.
/// Must NOT go to libasound since arg is our fake pcm handle.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_query_chmaps(_: *mut snd_pcm_t) -> *mut libc::c_void {
    std::ptr::null_mut()
}

/// Real ALSA: Sets the channel map on the PCM device.
/// Our impl: Returns -ENOTSUP — server always uses fixed stereo.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_set_chmap(_: *mut snd_pcm_t, _: *mut libc::c_void) -> libc::c_int {
    -libc::ENOTSUP
}

/// Real ALSA: Prints a channel map to a string buffer.
/// Our impl: Returns -ENOTSUP.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_chmap_print(_: *const libc::c_void, _: usize, _: *mut libc::c_char) -> libc::c_int {
    -libc::ENOTSUP
}

/// Real ALSA: Returns a short human-readable name for a channel position.
/// Our impl: Returns "Unknown".
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_chmap_name(_: libc::c_uint) -> *const libc::c_char {
    c"Unknown".as_ptr()
}

/// Real ALSA: Returns a long human-readable name for a channel position.
/// Our impl: Returns "Unknown".
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_chmap_long_name(_: libc::c_uint) -> *const libc::c_char {
    c"Unknown".as_ptr()
}

/// Real ALSA: Returns a name for a channel map type.
/// Our impl: Returns "Unknown".
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_chmap_type_name(_: libc::c_uint) -> *const libc::c_char {
    c"Unknown".as_ptr()
}

/// Real ALSA: Converts a channel position string to a numeric ID.
/// Our impl: Returns -ENOTSUP.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_chmap_from_string(_: *const libc::c_char) -> libc::c_int {
    -libc::ENOTSUP
}

/// Real ALSA: Parses a channel map string, allocating and returning a chmap.
/// Our impl: Returns -ENOTSUP.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_chmap_parse_string(_: *const libc::c_char, _: *mut *mut libc::c_void) -> libc::c_int {
    -libc::ENOTSUP
}

/// Real ALSA: Returns the byte size of `count` frames in the given format.
/// Our impl: Delegates to `snd_pcm_format_physical_width`.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_format_size(fmt: libc::c_int, count: isize) -> isize {
    let w = snd_pcm_format_physical_width(fmt);
    if w <= 0 { 0 } else { count * (w / 8) as isize }
}

/// Per-format widths matching the PCM format codes in <sound/asound.h>:
/// returns `(sample_bits, physical_bits)`.
///   2  S16_LE    (16, 16)
///   6  S24_LE    (24, 32) — 24 useful bits packed in 32
///   10 S32_LE    (32, 32)
///   14 FLOAT_LE  (32, 32)
///   16 FLOAT64_LE(64, 64)
/// Everything else resolves to 16-bit — formats we don't see in practice.
fn format_widths(fmt: libc::c_int) -> (libc::c_int, libc::c_int) {
    match fmt {
        6 => (24, 32),
        10 => (32, 32),
        14 => (32, 32),
        16 => (64, 64),
        _ => (16, 16),
    }
}

/// Real ALSA: Returns the physical width in bits for a PCM format.
/// Our impl: truthful per-format widths (previously we claimed 16 for every
/// format, which misled clients sizing buffers for FLOAT/S32).
#[no_mangle] pub unsafe extern "C" fn snd_pcm_format_physical_width(fmt: libc::c_int) -> libc::c_int { format_widths(fmt).1 }
/// Real ALSA: Returns the sample width in bits (may differ from physical width for padding).
/// Our impl: per-format sample width (e.g. S24_LE samples are 24 bits).
#[no_mangle] pub unsafe extern "C" fn snd_pcm_format_width(fmt: libc::c_int) -> libc::c_int { format_widths(fmt).0 }
/// Real ALSA: Returns whether the format is little-endian.
/// Our impl: Always true — audiod only supports LE formats.
#[no_mangle] pub unsafe extern "C" fn snd_pcm_format_little_endian(_: libc::c_int) -> libc::c_int { 1 }
/// Real ALSA: Returns whether the format is signed.
/// Our impl: Always true — S16_LE is signed.
#[no_mangle] pub unsafe extern "C" fn snd_pcm_format_signed(_: libc::c_int) -> libc::c_int { 1 }

// ── Additional stubs for mpv direct imports ──

#[no_mangle] pub unsafe extern "C" fn snd_pcm_dump(_: *mut snd_pcm_t, _: *mut libc::c_void) -> libc::c_int { 0 }
#[no_mangle] pub unsafe extern "C" fn snd_pcm_hw_params_dump(_: *mut libc::c_void, _: *mut libc::c_void) -> libc::c_int { 0 }
#[no_mangle] pub unsafe extern "C" fn snd_pcm_hw_params_can_pause(_: *mut libc::c_void) -> libc::c_int { 0 }
#[no_mangle] pub unsafe extern "C" fn snd_pcm_hw_params_copy(dst: *mut libc::c_void, src: *mut libc::c_void) -> libc::c_int {
    let d = match input_slice_mut(dst, HW_PARAMS_SIZE) { Some(d) => d, None => return -libc::EINVAL };
    let s = match input_slice(src, HW_PARAMS_SIZE) { Some(s) => s, None => return -libc::EINVAL };
    d.copy_from_slice(s);
    0
}
#[no_mangle] pub unsafe extern "C" fn snd_pcm_hw_params_get_buffer_size_max(_: *mut libc::c_void, v: *mut isize) -> libc::c_int {
    if !v.is_null() { *v = 16384; } 0
}
#[no_mangle] pub unsafe extern "C" fn snd_pcm_hw_params_get_period_size_min(_: *mut libc::c_void, v: *mut isize, _d: *mut libc::c_int) -> libc::c_int {
    if !v.is_null() { *v = 256; } 0
}
#[no_mangle] pub unsafe extern "C" fn snd_pcm_hw_params_set_channels_near(p: *mut snd_pcm_t, _: *mut libc::c_void, v: *mut libc::c_uint) -> libc::c_int {
    if !v.is_null() {
        if let Some(h) = pcm_mut(p) { h.channels = *v; }
    }
    0
}
#[no_mangle] pub unsafe extern "C" fn snd_pcm_hw_params_set_rate_resample(_: *mut snd_pcm_t, _: *mut libc::c_void, _: libc::c_uint) -> libc::c_int { 0 }
#[no_mangle] pub unsafe extern "C" fn snd_pcm_hw_params_sizeof() -> isize { 608 }
#[no_mangle] pub unsafe extern "C" fn snd_pcm_hw_params_test_format(_: *mut snd_pcm_t, _: *mut libc::c_void, _: libc::c_int) -> libc::c_int { 0 }
#[no_mangle] pub unsafe extern "C" fn snd_pcm_sw_params_set_silence_size(_: *mut snd_pcm_t, _: *mut libc::c_void, _: isize) -> libc::c_int { 0 }
#[no_mangle] pub unsafe extern "C" fn snd_pcm_sw_params_sizeof() -> isize { 136 }
#[no_mangle] pub unsafe extern "C" fn snd_pcm_state_name(s: libc::c_int) -> *const libc::c_char {
    match s { 0 => c"SND_PCM_STATE_OPEN", 1 => c"SETUP", 2 => c"PREPARED", 3 => c"RUNNING", 4 => c"XRUN", 5 => c"DRAINING", 6 => c"PAUSED", 7 => c"SUSPENDED", 8 => c"DISCONNECTED", _ => c"UNKNOWN" }.as_ptr()
}
#[no_mangle] pub unsafe extern "C" fn snd_pcm_status_sizeof() -> isize { 128 }
/// Status buffer view: caller provides a 128-byte `snd_pcm_status_t`
/// (see `snd_pcm_status_sizeof`). Field offsets match the ALSA uapi layout
/// (state@0, delay@56, avail@64) and are written little-endian.
struct StatusBuf<'a>(&'a mut [u8]);

impl<'a> StatusBuf<'a> {
    const SIZE: usize = 128;

    fn from_ptr(p: *mut libc::c_void) -> Option<Self> {
        if p.is_null() { return None; }
        Some(StatusBuf(unsafe { std::slice::from_raw_parts_mut(p as *mut u8, Self::SIZE) }))
    }

    fn read_ref<'b>(p: *const libc::c_void) -> Option<&'b [u8]> {
        if p.is_null() { return None; }
        Some(unsafe { std::slice::from_raw_parts(p as *const u8, Self::SIZE) })
    }

    fn set_state(&mut self, v: i32) {
        self.0[0..4].copy_from_slice(&v.to_le_bytes());
    }

    fn set_delay(&mut self, v: i64) {
        self.0[56..64].copy_from_slice(&v.to_le_bytes());
    }

    fn set_avail(&mut self, v: u64) {
        self.0[64..72].copy_from_slice(&v.to_le_bytes());
    }
}

/// Real ALSA: Fills a snd_pcm_status_t with current PCM state, timestamps, pointers, etc.
/// Our impl: Writes the current state (from the shim handle) as the first i32 at offset 0,
/// matching `snd_pcm_status_t.state`. Other fields are zeroed — not needed by mpv.
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_status(p: *mut snd_pcm_t, s: *mut libc::c_void) -> libc::c_int {
    let h = match pcm_ref(p) {
        Some(h) => h,
        None => return -libc::EINVAL,
    };
    let mut st = match StatusBuf::from_ptr(s) {
        Some(st) => st,
        None => return -libc::EINVAL,
    };
    st.0.fill(0);
    st.set_state(h.state);
    let (delay, avail) = pcm_delay_avail(h);
    st.set_delay(delay as i64);
    st.set_avail(avail as u64);
    0
}
#[no_mangle] pub unsafe extern "C" fn snd_pcm_status_get_avail(s: *mut libc::c_void) -> isize {
    let b = match StatusBuf::read_ref(s) { Some(b) => b, None => return BUFFER_SIZE - SERVER_FRAMES };
    u64::from_le_bytes(b[64..72].try_into().unwrap()) as isize
}
#[no_mangle] pub unsafe extern "C" fn snd_pcm_status_get_delay(s: *mut libc::c_void) -> isize {
    let b = match StatusBuf::read_ref(s) { Some(b) => b, None => return 0 };
    i64::from_le_bytes(b[56..64].try_into().unwrap()) as isize
}
/// Real ALSA: Reads the state field from a snd_pcm_status_t (offset 0, i32).
#[no_mangle]
pub unsafe extern "C" fn snd_pcm_status_get_state(s: *mut libc::c_void) -> libc::c_int {
    let b = match StatusBuf::read_ref(s) { Some(b) => b, None => return SND_PCM_STATE_OPEN };
    i32::from_le_bytes(b[0..4].try_into().unwrap())
}
#[no_mangle] pub unsafe extern "C" fn snd_pcm_readi(_: *mut snd_pcm_t, _: *mut libc::c_void, _: isize) -> isize { -libc::ENOTSUP as isize }
#[no_mangle] pub unsafe extern "C" fn snd_pcm_writen(_: *mut snd_pcm_t, _: *mut *mut libc::c_void, _: isize) -> isize { -libc::ENOTSUP as isize }
#[no_mangle] pub unsafe extern "C" fn snd_pcm_resume(_: *mut snd_pcm_t) -> libc::c_int { 0 }
#[no_mangle] pub unsafe extern "C" fn snd_pcm_stream(_: *mut snd_pcm_t) -> libc::c_int { 0 }



/// SndOutput: A buffer that captures ALSA debug output (used by mpv's --ao=alsa debug logging).
/// The implementation allocates a 4096-byte buffer and writes accumulate via `snd_output_*`
/// functions. The buffer is printed or inspected via `snd_output_buffer_string`.
struct SndOutput {
    buf: Box<[u8; 4096]>,
    pos: usize,
}

impl SndOutput {
    fn new() -> Self {
        SndOutput { buf: Box::new([0u8; 4096]), pos: 0 }
    }

    fn clear(&mut self) {
        self.pos = 0;
    }
}

#[no_mangle]
pub unsafe extern "C" fn snd_output_buffer_open(out: *mut *mut libc::c_void) -> libc::c_int {
    if out.is_null() { return -libc::EINVAL; }
    let o = Box::into_raw(Box::new(SndOutput::new()));
    *out = o as *mut libc::c_void;
    0
}

#[no_mangle]
pub unsafe extern "C" fn snd_output_buffer_string(out: *mut libc::c_void, buf: *mut *const libc::c_char) -> isize {
    if out.is_null() || buf.is_null() { return -libc::EINVAL as isize; }
    let o = &*(out as *const SndOutput);
    *buf = o.buf.as_ptr() as *const libc::c_char;
    o.pos as isize
}

#[no_mangle]
pub unsafe extern "C" fn snd_output_close(out: *mut libc::c_void) -> libc::c_int {
    if out.is_null() { return -libc::EINVAL; }
    let o = Box::from_raw(out as *mut SndOutput);
    drop(o);
    0
}

#[no_mangle]
pub unsafe extern "C" fn snd_output_flush(out: *mut libc::c_void) -> libc::c_int {
    if out.is_null() { return -libc::EINVAL; }
    let o = &mut *(out as *mut SndOutput);
    o.clear();
    0
}

