//! audiod — HDA audio server.
//!
//! Pipeline: `audshim (Unix socket) → audiod → direct HDA mmap`
//!
//! audiod listens on `/tmp/audiod.sock`, accepts a 12-byte protocol header
//! (rate/channels/format), then reads raw PCM data and writes it to the
//! Intel HD-Audio controller via BAR mmap.
//!
//! Key design decisions:
//! - Direct HDA access via PCI BAR mmap — no ALSA dependency.
//! - Fixed HW configuration: S16_LE stereo.
//! - A/V sync via time-based playhead matching the shim's virtual buffer.
//! - Blocking socket reads: backpressure from socket → shim → mpv.
//! - Requires root (CAP_SYS_ADMIN) for BAR mmap + pagemap, and the kernel
//!   `snd_hda_intel` driver must be unbound from the PCI slot first.

mod backend;
mod dsp;
mod mixer;
mod player;
use audcommon::config::load_config;
use audcommon::*;
use dsp::{f32_stereo_to_s16, fold_to_stereo, Resampler};
use mixer::Mixer;
use player::Player;
use audhda::HdaPlayback;
use log::*;
use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

/// Shared server state accessible by both PCM and control handlers.
struct ServerState {
    muted: AtomicBool,
    /// Live mixed streams (maintained by the mixer thread).
    live_clients: AtomicUsize,
    total_frames: AtomicU64,
    last_delay_frames: AtomicI64,
    /// Live volume in percent (0.0..=100.0), f32 bit-stored for atomicity.
    volume: AtomicU32,
    /// Output routing for debugging: 0=speakers, 1=headphone, 2=unknown.
    output_path: AtomicU8,
}

const OUT_SPEAKERS: u8 = 0;
const OUT_HEADPHONE: u8 = 1;
const OUT_UNKNOWN: u8 = 2;

static STATE: ServerState = ServerState {
    muted: AtomicBool::new(false),
    live_clients: AtomicUsize::new(0),
    total_frames: AtomicU64::new(0),
    last_delay_frames: AtomicI64::new(-1),
    volume: AtomicU32::new(100.0f32.to_bits()),
    output_path: AtomicU8::new(OUT_UNKNOWN),
};

fn output_path_str(v: u8) -> &'static str {
    match v {
        OUT_SPEAKERS => "speakers",
        OUT_HEADPHONE => "headphone",
        _ => "unknown",
    }
}

/// Record the output route (speakers/headphone/unknown) in shared state.
pub(crate) fn store_output_path(s: &str) {
    STATE.output_path.store(
        match s {
            "speakers" => OUT_SPEAKERS,
            "headphone" => OUT_HEADPHONE,
            _ => OUT_UNKNOWN,
        },
        Ordering::Release,
    );
}

/// Current volume as a percentage (0.0..=100.0).
fn state_volume() -> f32 {
    let bits = STATE.volume.load(Ordering::Acquire);
    f32::from_bits(bits)
}

fn set_state_volume(percent: f32) {
    let clamped = percent.clamp(0.0, 100.0);
    STATE.volume.store(clamped.to_bits(), Ordering::Release);
}

/// Linear gain factor derived from the current volume percent (1.0 = 100%).
fn state_gain() -> f32 {
    state_volume() / 100.0
}

// ── WAV parser ──
struct WavFile {
    cfg: AudioCfg,
    data: Vec<u8>,
}

fn load_wav(path: &str) -> io::Result<WavFile> {
    let mut f = BufReader::new(File::open(path)?);
    let mut h = [0u8; 44];
    f.read_exact(&mut h)?;

    if &h[0..4] != b"RIFF" || &h[8..12] != b"WAVE" {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a WAV file"));
    }

    let fmt = u16::from_le_bytes([h[20], h[21]]);
    let ch = u16::from_le_bytes([h[22], h[23]]) as u32;
    let rate = u32::from_le_bytes([h[24], h[25], h[26], h[27]]);
    let bits = u16::from_le_bytes([h[34], h[35]]);

    if fmt != 1 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "only PCM WAV"));
    }
    if bits != 16 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "only 16-bit WAV"));
    }

    let dsz = u32::from_le_bytes([h[40], h[41], h[42], h[43]]) as usize;
    let mut data = vec![0u8; dsz];
    f.read_exact(&mut data)?;

    Ok(WavFile { cfg: AudioCfg::new(rate, ch, FORMAT_S16_LE), data })
}

// ── Protocol ──

/// Parse 12-byte protocol header into AudioCfg.
/// Header: [rate:u32 LE, channels:u16 LE, format:u16 LE, reserved:u32].
/// Validates rate>0, channels in [1,8], format is S16_LE or FLOAT_LE.
fn parse_header(buf: &[u8; HDR_SZ]) -> Option<AudioCfg> {
    let rate = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let ch = u16::from_le_bytes([buf[4], buf[5]]) as u32;
    let fmt_code = u16::from_le_bytes([buf[6], buf[7]]);
    if rate == 0 || ch == 0 || ch > 8 {
        return None;
    }
    let fmt = fmt_code as u32;
    if fmt != FORMAT_S16_LE && fmt != FORMAT_FLOAT_LE {
        error!("unsupported format {}", fmt);
        return None;
    }
    Some(AudioCfg::new(rate, ch, fmt))
}

/// Convert PCM data from client format to S16_LE stereo (HW format),
/// applying software gain. When `gain == 1.0` and input matches output
/// format, the caller should skip this function entirely (passthrough).
fn convert_to_s16(src: &[u8], dst: &mut [u8], frames: usize, ch_in: u32, ch_out: u32, fmt_in: u32, gain: f64) {
    let g = gain as f32;
    if fmt_in == FORMAT_S16_LE && ch_in == 1 && ch_out == 2 {
        let ss = src.as_ptr() as *const i16;
        let sd = dst.as_mut_ptr() as *mut i16;
        for i in 0..frames {
            let sample = unsafe { *ss.add(i) } as f32 * g;
            let val = sample.round().max(i16::MIN as f32).min(i16::MAX as f32) as i16;
            unsafe {
                *sd.add(i * 2) = val;
                *sd.add(i * 2 + 1) = val;
            }
        }
    } else if fmt_in == FORMAT_FLOAT_LE && ch_in == ch_out {
        let sf = src.as_ptr() as *const f32;
        let sd = dst.as_mut_ptr() as *mut i16;
        let n = frames * ch_in as usize;
        for i in 0..n {
            let sample = unsafe { *sf.add(i) * g };
            let clamped = sample.clamp(-1.0, 1.0);
            unsafe { *sd.add(i) = (clamped * 32767.0) as i16; }
        }
    } else if fmt_in == FORMAT_FLOAT_LE && ch_in == 1 && ch_out == 2 {
        let sf = src.as_ptr() as *const f32;
        let sd = dst.as_mut_ptr() as *mut i16;
        for i in 0..frames {
            let sample = unsafe { *sf.add(i) * g };
            let clamped = sample.clamp(-1.0, 1.0);
            let val = (clamped * 32767.0) as i16;
            unsafe {
                *sd.add(i * 2) = val;
                *sd.add(i * 2 + 1) = val;
            }
        }
    } else if fmt_in == FORMAT_S16_LE && ch_in == ch_out && g != 1.0 {
        let ss = src.as_ptr() as *const i16;
        let sd = dst.as_mut_ptr() as *mut i16;
        let n = frames * ch_in as usize;
        for i in 0..n {
            let sample = unsafe { *ss.add(i) } as f32 * g;
            let val = sample.round().max(i16::MIN as f32).min(i16::MAX as f32) as i16;
            unsafe { *sd.add(i) = val; }
        }
    }
}

// ── Socket server ──

/// Handle a control-only client (rate=0 in header). Reads commands and
/// acts on the shared ServerState. Returns when the client disconnects.
fn handle_control(mut stream: UnixStream) {
    if !peer_allowed(&stream) {
        warn!("control connection from uid {:?} rejected", peer_uid(&stream));
        return;
    }
    info!("control client connected");
    loop {
        let mut len_buf = [0u8; 4];
        if stream.read_exact(&mut len_buf).is_err() { break; }
        let len = u32::from_le_bytes(len_buf) as usize;
        if len != 0 { break; }
        let mut cmd = [0u8; 1];
        if stream.read_exact(&mut cmd).is_err() { break; }
        if let Some(cmd) = Cmd::read(&mut stream, cmd[0]) {
            if let Some(body) = cmd.apply() {
                write_status(&mut stream, &body);
            }
        } else {
            break;
        }
    }
    info!("control client disconnected");
}

/// 12-byte protocol header is the intro: rate=0 signals a control client.
/// Opens the control socket world-writable so non-root audio clients (shim,
/// audiod-ctl) can connect when the server runs as root (HDA backend).
fn chmod_socket(path: &str) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(md) = std::fs::metadata(path) {
        let mut perms = md.permissions();
        perms.set_mode(0o777);
        let _ = std::fs::set_permissions(path, perms);
    }
}

/// ThinkPad mute LED (`platform::mute`, driven by ACPI). With `snd_hda_intel`
/// unbound, no kernel driver toggles it, so it can stay lit (muted) forever
/// and the physical "muted" indicator never clears. audiod (running as root
/// under the HDA backend) writes it so the LED matches the real mute state:
/// brightness 1 = muted (lit), 0 = unmuted.
fn set_mute_led(mute: bool) {
    let Some(dir) = std::fs::read_dir("/sys/class/leds").ok() else { return };
    for e in dir.filter_map(|e| e.ok()) {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !name.contains("::mute") { continue; }
        let _ = std::fs::write(e.path().join("brightness"), if mute { "1" } else { "0" });
    }
}

// ── Socket authorization ──

/// The server (root, under the HDA backend) binds a world-writable socket so
/// non-root audio clients (the shim, audiod-ctl) can connect. That means ANY
/// local user could otherwise mute/unmute/change volume of a root process, so
/// connections are checked against the uid that launched us (or root).
fn trusted_uid() -> u32 {
    static TRUSTED: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *TRUSTED.get_or_init(|| {
        if let Some(u) = std::env::var("SUDO_UID")
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
            {
                return u;
            }
        unsafe { libc::geteuid() }
    })
}

/// Peer uid of the connected socket via SO_PEERCRED; None when it can't be
/// determined (then the connection is rejected — no auth, no trust).
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if r < 0 {
        None
    } else {
        Some(cred.uid)
    }
}

/// Whether this connection may issue commands / stream: only root or the user
/// that launched the server.
fn peer_allowed(stream: &UnixStream) -> bool {
    match peer_uid(stream) {
        Some(0) => true,
        Some(uid) => uid == trusted_uid(),
        None => false,
    }
}

// ── Control commands (Command pattern) ──

/// A control command decoded from the socket (len=0 + 1-byte opcode + optional
/// payload). Parsing is separated from side effects so the PCM-stream handler
/// and the control-only handler share a single `apply`.
enum Cmd {
    Mute,
    Unmute,
    Flush,
    Volume(f32),
    Status,
}

impl Cmd {
    /// Decode the rest of a command after its 1-byte opcode has been read.
    /// Returns None on an unknown opcode or a truncated payload (caller drops
    /// the client, mirroring real ALSA's "invalid command kills the stream").
    fn read(stream: &mut UnixStream, op: u8) -> Option<Cmd> {
        match op {
            MSG_MUTE => Some(Cmd::Mute),
            MSG_UNMUTE => Some(Cmd::Unmute),
            MSG_FLUSH => Some(Cmd::Flush),
            MSG_VOLUME => {
                let mut v = [0u8; 4];
                if stream.read_exact(&mut v).is_err() {
                    return None;
                }
                Some(Cmd::Volume(f32::from_le_bytes(v)))
            }
            MSG_STATUS => Some(Cmd::Status),
            _ => {
                warn!("unknown control cmd {}", op);
                None
            }
        }
    }

    /// Apply the command's side effects. Stream-scoped commands (FLUSH) are
    /// handled by the sending stream's reader; a FLUSH arriving from a
    /// control-only client is a no-op. Returns a JSON status body for STATUS,
    /// otherwise None.
    fn apply(self) -> Option<String> {
        match self {
            Cmd::Mute => {
                STATE.muted.store(true, Ordering::Release);
                set_mute_led(true);
                info!("muted");
                None
            }
            Cmd::Unmute => {
                STATE.muted.store(false, Ordering::Release);
                set_mute_led(false);
                info!("unmuted");
                None
            }
            Cmd::Flush => {
                debug!("flush from control-only client: no-op");
                None
            }
            Cmd::Volume(g) => {
                set_state_volume(g);
                info!("volume (via ctl) = {:.2}", state_volume());
                None
            }
            Cmd::Status => {
                let muted = STATE.muted.load(Ordering::Acquire);
                let volume = state_volume();
                let frames = STATE.total_frames.load(Ordering::Acquire);
                let delay = STATE.last_delay_frames.load(Ordering::Acquire);
                let clients = STATE.live_clients.load(Ordering::Acquire);
                let out = output_path_str(STATE.output_path.load(Ordering::Acquire));
                Some(format!(
                    r#"{{"muted":{},"volume":{:.3},"active":{},"clients":{},"total_frames":{},"delay_frames":{},"output":"{}"}}"#,
                    muted,
                    volume,
                    clients > 0,
                    clients,
                    frames,
                    delay,
                    out,
                ))
            }
        }
    }
}

/// Write a length-prefixed response (used by STATUS) to the client.
fn write_status(stream: &mut UnixStream, body: &str) {
    let b = body.as_bytes();
    let _ = stream.write_all(&(b.len() as u32).to_le_bytes());
    let _ = stream.write_all(b);
}

/// Accept control-only clients (rate=0 header) on the control socket in a
/// background thread. Used by the CLI players so `audiod-ctl` can reach them.
fn spawn_control_socket() {
    let _ = std::fs::remove_file(SOCKET_PATH_STR);
    let listener = match UnixListener::bind(SOCKET_PATH_STR) {
        Ok(l) => l,
        Err(e) => { warn!("control socket bind {}: {}", SOCKET_PATH_STR, e); return; }
    };
    chmod_socket(SOCKET_PATH_STR);
    if listener.set_nonblocking(true).is_err() { warn!("control socket nonblock"); }
    std::thread::Builder::new()
        .stack_size(65536)
        .name("audiod-ctrl-accept".into())
        .spawn(move || {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let mut hdr = [0u8; HDR_SZ];
                        if read_header_tolerant(&stream, &mut hdr).is_err() { continue; }
                        let rate = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
                        if rate == 0 {
                            std::thread::Builder::new()
                                .stack_size(65536)
                                .name("audiod-ctrl".into())
                                .spawn(move || handle_control(stream))
                                .expect("spawn control handler");
                        }
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => { warn!("control accept: {}", e); }
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        })
        .expect("spawn control accept");
}

/// Handle one PCM client: register with the mixer, then loop reading
/// length-prefixed chunks, converting them to mix-rate S16LE stereo and
/// pushing into this stream's bounded ring. Blocking on a full ring is the
/// backpressure path (socket → shim drain → mpv writei), same as before.
fn reader_client(mut stream: UnixStream, cfg: AudioCfg, mixer: Mixer, max_clients: usize) {
    if !peer_allowed(&stream) {
        warn!("PCM connection from uid {:?} rejected", peer_uid(&stream));
        return;
    }
    if mixer.live_clients() >= max_clients {
        warn!(
            "client rejected: {} streams active (max {}) — closing",
            mixer.live_clients(),
            max_clients
        );
        return; // dropping `stream` closes it; the shim reconnects with backoff
    }

    let srv = load_config();
    let mut resampler = Resampler::new(cfg.rate, srv.server.mix_rate, &srv.server.resampler);
    info!(
        "client connected (rate={} ch={} fmt={}) → mix {} Hz S16 stereo{}",
        cfg.rate,
        cfg.channels,
        cfg.format,
        srv.server.mix_rate,
        if resampler.is_some() { ", resampled" } else { "" }
    );

    let mstream = mixer.register();

    let mut len_buf = [0u8; 4];
    let read_buf_sz = 8192;
    let mut buf = vec![0u8; read_buf_sz];
    let mut data = Vec::new();
    let mut folded: Vec<f32> = Vec::new();
    let mut resampled: Vec<f32> = Vec::new();
    let mut s16 = Vec::new();

    loop {
        // Poll so SIGTERM shutdown isn't stuck behind a silent client. Idle
        // time needs no action here: dry rings contribute silence and the
        // mixer stops the HDA stream after its own idle watchdog.
        const POLL_MS: i32 = 250;
        let mut pfd = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let pr = unsafe { libc::poll(&mut pfd, 1, POLL_MS) };
        if SHUTDOWN.load(Ordering::Acquire) {
            break;
        }
        if pr == 0 {
            continue;
        }
        if pr < 0 {
            if unsafe { *libc::__errno_location() } == libc::EINTR {
                continue;
            }
            break;
        }
        if stream.read_exact(&mut len_buf).is_err() {
            break;
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        if len == 0 {
            let mut cmd = [0u8; 1];
            if stream.read_exact(&mut cmd).is_err() {
                break;
            }
            match Cmd::read(&mut stream, cmd[0]) {
                Some(Cmd::Flush) => {
                    // Scoped flush (seek/pause): forward to the mixer, which
                    // drops THIS stream's queued audio and halts the DMA tail
                    // on its next tick (LPIB-wrap guard — stale ring content
                    // must never replay after a pause).
                    mstream.request_flush();
                    debug!("flush: forwarded to mixer");
                }
                Some(cmd) => {
                    if let Some(body) = cmd.apply() {
                        write_status(&mut stream, &body);
                    }
                }
                None => break,
            }
            continue;
        }
        data.clear();
        data.reserve(len);
        let mut to_read = len;
        while to_read > 0 {
            let chunk = std::cmp::min(to_read, read_buf_sz);
            let n = match stream.read(&mut buf[..chunk]) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    error!("read error: {}", e);
                    break;
                }
            };
            data.extend_from_slice(&buf[..n]);
            to_read -= n;
        }
        if data.len() != len {
            break;
        }

        // Client bytes → f32 stereo → mix rate → S16LE stereo.
        folded.clear();
        fold_to_stereo(&data, cfg.format, cfg.channels, &mut folded);
        match resampler.as_mut() {
            Some(r) => {
                resampled.clear();
                r.resample(&folded, &mut resampled);
            }
            None => std::mem::swap(&mut resampled, &mut folded),
        }
        s16.clear();
        f32_stereo_to_s16(&resampled, &mut s16);
        let frames = s16.len() / 4;
        if frames == 0 {
            continue;
        }
        mstream.ring.push(&s16);
        STATE.total_frames.fetch_add(frames as u64, Ordering::Release);
    }

    mstream.finish();
    info!("client disconnected");
}

/// Read the 12-byte protocol header from an accepted stream, tolerating the
/// WouldBlock/EAGAIN that a nonblocking (listener-inherited) socket raises
/// when the header hasn't fully arrived yet. A plain `read_exact` would drop
/// the connection there — fatal for the shim's reconnect, which observes a
/// race between `connect()` completing and the first header bytes landing.
fn read_header_tolerant(mut stream: &UnixStream, buf: &mut [u8]) -> io::Result<()> {
    let mut got = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
    while got < buf.len() {
        match stream.read(&mut buf[got..]) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof on header")),
            Ok(n) => got += n,
            Err(ref e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::Interrupted =>
            {
                if std::time::Instant::now() > deadline {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "header read timeout"));
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_signal(_: i32) {
    SHUTDOWN.store(true, Ordering::Release);
}

fn run_server(slot: &str) -> io::Result<()> {
    unsafe {
        libc::signal(libc::SIGTERM, handle_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, handle_signal as *const () as libc::sighandler_t);
    }

    // One shared backend for ALL clients, opened once at the mix rate inside
    // the mixer thread. Controller::open's link reset also halts stale DMA
    // left behind by a SIGKILLed previous instance (the old "startup sweep").
    let srv = load_config();
    let max_clients = srv.server.max_clients;
    let mixer = Mixer::spawn(slot.to_string())?;
    info!(
        "mixer ready (mix_rate={} max_clients={})",
        srv.server.mix_rate, max_clients
    );

    let _ = std::fs::remove_file(SOCKET_PATH_STR);
    let listener = UnixListener::bind(SOCKET_PATH_STR)?;
    chmod_socket(SOCKET_PATH_STR);
    listener.set_nonblocking(true)?;

    let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            break;
        }

        match listener.accept() {
            Ok((stream, _)) => {
                // Read 12-byte header; rate=0 signals a control client.
                let mut hdr = [0u8; HDR_SZ];
                if read_header_tolerant(&stream, &mut hdr).is_err() { continue; }
                let rate = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
                if rate == 0 {
                    std::thread::Builder::new()
                        .stack_size(65536)
                        .name("audiod-ctrl".into())
                        .spawn(move || handle_control(stream))
                        .expect("spawn control handler");
                } else if let Some(cfg) = parse_header(&hdr) {
                    // Every PCM client gets its own reader thread immediately;
                    // the mixer merges them into the shared backend.
                    let mixer = mixer.clone();
                    handles.retain(|h| !h.is_finished());
                    handles.push(std::thread::Builder::new()
                        .stack_size(65536)
                        .name("audiod-client".into())
                        .spawn(move || reader_client(stream, cfg, mixer, max_clients))
                        .expect("spawn client handler"));
                } else {
                    error!("bad header from new client, dropping");
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => error!("accept: {}", e),
        }

        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    for h in handles.drain(..) {
        let _ = h.join();
    }

    info!("shutting down");
    let _ = std::fs::remove_file(SOCKET_PATH_STR);
    Ok(())
}

// ── CLI players ──

/// Standalone stdin-to-PCM player. Reads S16_LE PCM from stdin.
fn play_stdin(slot: &str, rate: u32, channels: u32) -> io::Result<()> {
    let cfg = AudioCfg::new(rate, channels, FORMAT_S16_LE);
    let mut player = Player::open(slot, &cfg)?;
    spawn_control_socket();
    let frame_size = cfg.frame_size as usize;
    let mut stdin = io::stdin();
    let mut buf = vec![0u8; 8192];
    loop {
        if let Err(e) = player.sync_muted() {
            error!("set_mute: {}", e);
        }
        match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if !player.hardware_mute() && player.is_muted() {
                    continue;
                }
                let frames = n / frame_size;
                let cbytes = frames * frame_size;
                if cbytes > 0 {
                    player.play(&buf[..cbytes], frames)?;
                }
            }
            Err(_) => break,
        }
    }
    player.backend.drain()
}

/// Standalone WAV file player. Only 16-bit PCM WAV.
fn play_wav(slot: &str, path: &str) -> io::Result<()> {
    let wav = load_wav(path)?;
    let mut player = Player::open(slot, &wav.cfg)?;
    spawn_control_socket();
    let frame_size = wav.cfg.frame_size as usize;
    let mut off = 0;
    while off < wav.data.len() {
        if let Err(e) = player.sync_muted() {
            error!("set_mute: {}", e);
        }
        if !player.hardware_mute() && player.is_muted() {
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }
        let bytes_left = wav.data.len() - off;
        let frames = bytes_left / frame_size;
        if frames == 0 { break; }
        let cbytes = frames * frame_size;
        player.play(&wav.data[off..off + cbytes], frames)?;
        off += cbytes;
    }
    player.backend.drain()
}

// ── Entry point ──

fn usage() {
    eprintln!("audiod – HDA audio server");
    eprintln!("Usage:");
    eprintln!("  audiod [-d slot]                 Run Unix socket server");
    eprintln!("  audiod [-d slot] file.wav         Play a WAV file");
    eprintln!("  audiod [-d slot] --stdin [opts]   Play PCM from stdin");
    eprintln!("  audiod -l                         Show HDA controller info");
    eprintln!("Options:");
    eprintln!("  -d <slot>     HDA PCI slot (default: 0000:00:1b.0)");
    eprintln!("  -l            Show HDA controller info (requires root)");
    eprintln!("  --rate=N      Sample rate (default 44100, for --stdin)");
    eprintln!("  --channels=N  Channels (default 2, for --stdin)");
    eprintln!("Debug (HDA backend):");
    eprintln!("  --skip-reset       Skip the HDA link reset");
    eprintln!("  --skip-codec-init  Skip codec playback init");
    eprintln!("  --dump-state       Dump codec register state after init");
    eprintln!("  --dump-topology    Dump the codec widget graph");
    eprintln!("  --dump-ring        Hexdump the first DMA ring bytes fed");
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    // Config `volume` is a gain factor (default 1.0) → seed as percent.
    set_state_volume((load_config().server.volume as f32) * 100.0);

    let args: Vec<String> = std::env::args().collect();

    // Parse -d, -l and debug flags from the argument list (they may appear
    // anywhere before the subcommand).
    let mut dev_name = "hda".to_string();
    let mut list_only = false;
    let mut dbg = audhda::dbg::DebugOpts::default();
    let mut positional: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-d" => {
                i += 1;
                dev_name = args.get(i).cloned().unwrap_or_else(|| "hda".into());
            }
            "-l" => list_only = true,
            "--skip-reset" => dbg.skip_reset = true,
            "--skip-codec-init" => dbg.skip_codec_init = true,
            "--dump-state" => dbg.dump_state = true,
            "--dump-ring" => dbg.dump_ring = true,
            "--dump-topology" => dbg.dump_topology = true,
            a if a.starts_with('-') => positional.push(args[i].clone()),
            _ => positional.push(args[i].clone()),
        }
        i += 1;
    }
    // HDA debug behavior is now CLI-driven; the legacy AUDIOD_* env vars still
    // work as a fallback inside audhda::dbg.
    audhda::dbg::configure(dbg);

    let slot = match backend::parse_slot(&dev_name) {
        Ok(s) => s,
        Err(e) => {
            error!("{}", e);
            std::process::exit(1);
        }
    };

    if list_only {
        println!("Direct HDA backend: slot hda:{}", slot);
        let (pci_vendor, pci_device) = audhda::pcicfg::vendor_device(&slot).unwrap_or((0, 0));
        match HdaPlayback::open(&slot, audhda::HW_RATE) {
            Ok(mut pb) => {
                println!("  PCI vendor:device = {:04x}:{:04x}", pci_vendor, pci_device);
                println!("  codec vendor:subsystem = {:04x}:{:04x}", pb.codec.vendor_id >> 16, pb.codec.vendor_id & 0xffff);
                println!("  supported rates: {:?}", backend::HDA_RATES);
                println!("  output: {}", audhda::codec::Codec::output_path(&mut pb.controller));
            }
            Err(e) => {
                println!("  (requires root and snd_hda_intel unbound: {e})");
            }
        }
        return;
    }

    info!("using backend: hda:{} (from \"{}\")", slot, dev_name);
    if unsafe { libc::geteuid() } != 0 {
        error!("HDA backend requires root (euid 0)");
        std::process::exit(1);
    }

    if positional.is_empty() {
        if let Err(e) = run_server(&slot) {
            error!("{}", e);
            std::process::exit(1);
        }
        return;
    }

    match positional[0].as_str() {
        "--help" | "-h" => usage(),
        "--stdin" | "-" => {
            let mut rate = 44100u32;
            let mut channels = 2u32;
            for arg in positional.iter().skip(1) {
                if let Some(v) = arg.strip_prefix("--rate=") {
                    rate = v.parse().unwrap_or(44100);
                } else if let Some(v) = arg.strip_prefix("--channels=") {
                    channels = v.parse().unwrap_or(2);
                }
            }
            if let Err(e) = play_stdin(&slot, rate, channels) {
                error!("{}", e);
                std::process::exit(1);
            }
        }
        "--selftest" => {
            // Direct HDA ring selftest: 440 Hz sine straight to DMA, bypassing
            // the socket/conversion path. Isolates codec+DMA from data path.
            let mut secs = 3.0f32;
            let mut rate = 48000u32;
            for arg in positional.iter().skip(1) {
                if let Some(v) = arg.strip_prefix("--secs=") {
                    secs = v.parse().unwrap_or(3.0);
                } else if let Some(v) = arg.strip_prefix("--rate=") {
                    rate = v.parse().unwrap_or(48000);
                }
            }
            let mut pc = match HdaPlayback::open(&slot, rate) {
                Ok(p) => p,
                Err(e) => {
                    error!("{}", e);
                    std::process::exit(1);
                }
            };
            if let Err(e) = pc.selftest_tone(secs, 1.0) {
                error!("selftest: {}", e);
                std::process::exit(1);
            }
            info!("selftest: holding for {:.1}s", secs);
            std::thread::sleep(std::time::Duration::from_secs_f32(secs));
        }
        path => {
            if let Err(e) = play_wav(&slot, path) {
                error!("{}: {}", path, e);
                std::process::exit(1);
            }
        }
    }
}
