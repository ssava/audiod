//! Multi-client mixer.
//!
//! Owns the single `Backend` (opened once at the mix rate) and pulls from
//! per-client bounded rings at hardware pace: `writable = clamp(
//! SHIM_SERVER_FRAMES − delay_frames, 0, CHUNK_FRAMES)` — the same pacing
//! math the single-client path used, so the shim's A-V playhead model is
//! preserved exactly. Streams with no data contribute silence; when every
//! stream has been dry for `IDLE_MS` the HDA stream is stopped (LPIB-wrap
//! guard). Clients join via an mpsc channel and leave passively: a slot is
//! dropped once its `done` flag is set and its ring has drained.
//!
//! A stream-scoped flush (seek/pause) drops that stream's queued audio on
//! the next mixer tick and halts the DMA right away when no other stream
//! has audio pending — otherwise the LPIB would wrap the physical ring and
//! replay stale content (~1.4 s) before any dry timer fired.

use crate::backend::Backend;
use crate::{state_gain, store_output_path, STATE};
use audcommon::config::load_config;
use audcommon::ring::AudioRingBuffer;
use audcommon::{SHIM_BUFFER_FRAMES, SHIM_SERVER_FRAMES};
use log::*;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// Frames mixed per backend write tick (~21 ms at 48 kHz).
const CHUNK_FRAMES: usize = 1024;
/// All-dry period before the HDA stream is stopped.
const IDLE_MS: u64 = 250;

enum Msg {
    Add(Arc<MixerStream>),
}

pub struct MixerStream {
    pub ring: Arc<AudioRingBuffer>,
    /// Set by the client's reader thread on FLUSH (seek/pause); consumed by
    /// the mixer, which drops the queued audio and halts the DMA tail.
    flush: AtomicBool,
    done: AtomicBool,
}

impl MixerStream {
    /// Request a stream-scoped flush. Non-blocking; applied by the mixer on
    /// its next tick (≤ a few ms).
    pub fn request_flush(&self) {
        self.flush.store(true, Ordering::Release);
    }

    /// Mark the client gone; the mixer drops the slot once its ring drains.
    pub fn finish(&self) {
        self.done.store(true, Ordering::Release);
    }
}

/// Handle to the running mixer thread. Cloneable; registration never blocks.
#[derive(Clone)]
pub struct Mixer {
    tx: mpsc::Sender<Msg>,
    live: Arc<AtomicUsize>,
}

impl Mixer {
    /// Spawn the mixer thread, which opens the shared `Backend` itself
    /// (`HdaPlayback` wraps an mmap'd BAR raw pointer and is `!Send`, so the
    /// open must happen on the owning thread). Readiness is reported back
    /// over a channel so startup errors surface to the caller.
    pub fn spawn(slot: String) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let live = Arc::new(AtomicUsize::new(0));
        let live2 = live.clone();
        std::thread::Builder::new()
            .name("audiod-mixer".into())
            .stack_size(65536)
            .spawn(move || match Backend::open(&slot, load_config().server.mix_rate) {
                Ok(b) => {
                    let _ = ready_tx.send(Ok(()));
                    mix_loop(b, rx, live2);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            })?;
        ready_rx
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::NotConnected, "mixer thread died"))??;
        Ok(Mixer { tx, live })
    }

    /// Register a new client stream with its own bounded ring.
    pub fn register(&self) -> Arc<MixerStream> {
        let ring_bytes = load_config().server.client_ring_bytes;
        let stream = Arc::new(MixerStream {
            ring: Arc::new(AudioRingBuffer::new(ring_bytes)),
            flush: AtomicBool::new(false),
            done: AtomicBool::new(false),
        });
        let _ = self.tx.send(Msg::Add(stream.clone()));
        stream
    }

    pub fn live_clients(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }
}

/// Shared idle-stop path: fires `stop_immediate` once `dry_since` has been
/// armed for `IDLE_MS`. Sticky via `stopped` so a quiet mixer doesn't re-SRST
/// the stream every IDLE_MS. Used by both the no-streams branch and the
/// all-dry watchdog.
fn try_idle_stop(
    backend: &mut Backend,
    dry_since: &mut Option<Instant>,
    stopped: &mut bool,
    reason: &str,
) {
    if *stopped {
        return;
    }
    let t = dry_since.get_or_insert_with(Instant::now);
    if t.elapsed() < Duration::from_millis(IDLE_MS) {
        return;
    }
    match backend.stop_immediate() {
        Ok(()) => {
            *dry_since = None;
            *stopped = true;
            STATE.last_delay_frames.store(0, Ordering::Release);
            info!("mixer: {reason}, HDA stopped");
        }
        Err(e) => {
            // Retry after another quiet period instead of spinning.
            warn!("mixer: stop_immediate failed: {}", e);
            *dry_since = Some(Instant::now());
        }
    }
}

fn mix_loop(mut backend: Backend, rx: mpsc::Receiver<Msg>, live: Arc<AtomicUsize>) {
    info!("mixer started");
    store_output_path(&backend.output_path());
    let mix_rate = load_config().server.mix_rate.max(1) as i64;
    let mut streams: Vec<Arc<MixerStream>> = Vec::new();
    let mut acc = vec![0i32; CHUNK_FRAMES * 2];
    let mut out = vec![0u8; CHUNK_FRAMES * 4];
    let mut pop = vec![0u8; CHUNK_FRAMES * 4];
    let mut dry_since: Option<Instant> = None;
    // Sticky "HDA stream halted" flag: once stopped, stay stopped until real
    // data flows again — otherwise the dry timer re-arms every tick and we
    // hammer stop_immediate (SRST + ring zeroing) every IDLE_MS forever.
    let mut stopped = false;
    let mut path_timer = Instant::now();

    loop {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::Add(s) => {
                    debug!("mixer: stream added");
                    streams.push(s);
                }
            }
        }

        // Stream-scoped flush (seek/pause): drop the queued audio NOW and,
        // when no other stream has audio pending, halt the DMA immediately —
        // frames already written would otherwise keep the LPIB running into
        // stale ring content (the "pause replays ~1.4 s" bug).
        let mut flushed = false;
        for s in &streams {
            if s.flush.swap(false, Ordering::AcqRel) {
                s.ring.clear();
                flushed = true;
            }
        }
        if flushed {
            debug!("mixer: flush: cleared stream ring(s)");
            if streams.iter().all(|s| s.ring.filled() == 0) && !stopped {
                match backend.stop_immediate() {
                    Ok(()) => {
                        stopped = true;
                        STATE.last_delay_frames.store(0, Ordering::Release);
                        info!("mixer: flush → HDA stopped");
                    }
                    Err(e) => warn!("mixer: flush: stop_immediate failed: {}", e),
                }
            }
            dry_since = None;
        }

        streams.retain(|s| !(s.done.load(Ordering::Acquire) && s.ring.filled() == 0));
        live.store(streams.len(), Ordering::Release);
        STATE.live_clients.store(streams.len(), Ordering::Release);

        // Refresh the output route about once a second so STATUS reflects
        // jack plug/unplug even while idle.
        if path_timer.elapsed().as_secs() >= 1 {
            store_output_path(&backend.output_path());
            path_timer = Instant::now();
        }

        if streams.is_empty() {
            // The last stream is usually retained away in the same tick its
            // ring empties, so arm/fire the dry timer here too.
            try_idle_stop(&mut backend, &mut dry_since, &mut stopped, "no streams");
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }

        // Idle watchdog — must run on EVERY tick. It used to sit behind the
        // `writable == 0` early-out below, which is exactly the state during
        // an LPIB wrap (delay_bytes jumps to ~RING_SIZE once the DMA passes
        // write_pos), so a missed stop could keep stale audio looping for a
        // full ring traversal (~1.4 s) before the timer even armed.
        if !streams.iter().any(|s| s.ring.filled() > 0) {
            try_idle_stop(&mut backend, &mut dry_since, &mut stopped, "all streams dry");
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        dry_since = None;

        let delay = backend.delay_frames().unwrap_or(0);
        let writable =
            (SHIM_SERVER_FRAMES as i64 - delay).clamp(0, CHUNK_FRAMES as i64) as usize;
        if writable == 0 {
            // Ring is at the A-V occupancy target. Sleep until the DAC clock
            // should have freed a full chunk again instead of polling at
            // 1 kHz (~900 wakeups/s measured): wait for LPIB to advance by
            // `delay − (TARGET − CHUNK)` frames, capped to stay responsive
            // to flushes and newly registered streams.
            let behind = delay - (SHIM_SERVER_FRAMES as i64 - CHUNK_FRAMES as i64);
            let ms = ((behind * 1000 / mix_rate).clamp(0, 10)) as u64;
            std::thread::sleep(Duration::from_millis(ms.saturating_sub(1)));
            continue;
        }

        acc.iter_mut().for_each(|v| *v = 0);
        let mut had_data = false;
        for s in &streams {
            let want = (writable * 4).min(s.ring.filled() / 4 * 4);
            if want == 0 {
                continue;
            }
            let got = s.ring.try_pop(&mut pop[..want]);
            had_data = true;
            for i in 0..got / 2 {
                acc[i] += i16::from_le_bytes([pop[i * 2], pop[i * 2 + 1]]) as i32;
            }
        }

        if !had_data {
            // Raced a concurrent clear; re-evaluate on the next tick.
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        stopped = false;

        let gain = state_gain();
        let muted = STATE.muted.load(Ordering::Acquire);
        let n = writable * 2;
        for (i, v) in acc[..n].iter().enumerate() {
            let s = if muted { 0.0f32 } else { *v as f32 * gain };
            let c = s.clamp(-32768.0, 32767.0) as i16;
            out[i * 2..i * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }
        if let Err(e) = backend.push(&out[..n * 2]) {
            error!("mixer: backend play: {}", e);
        }
        if let Ok(d) = backend.delay_frames() {
            STATE.last_delay_frames.store(d, Ordering::Release);
        }
    }
}

/// Sanity: the mixer's pacing target must match the shim's virtual-buffer
/// model (compile-time guard against constant drift).
const _: () = assert!(SHIM_SERVER_FRAMES <= SHIM_BUFFER_FRAMES);
