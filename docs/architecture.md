# Architecture

## Overview

```
                       user space
┌──────────────────────────────────────────────────────────────────┐
│  mpv / Firefox / any ALSA app                                    │
│      │ LD_PRELOAD=libaudshim.so                                  │
│  ┌───▼─────────────────────────────┐                             │
│  │ audshim                         │                             │
│  │  snd_pcm_writei → ring push     │   per-handle drain thread   │
│  │  snd_pcm_delay  → playhead math │   (blocking socket writes)  │
│  └───┬─────────────────────────────┘                             │
│      │ Unix socket: 12-byte header + length-prefixed PCM         │
│  ┌───▼──────────────────────────────────────────────┐            │
│  │ audiod                                           │            │
│  │  reader thread per client (client fold+resample) │            │
│  │        → per-client bounded ring                 │            │
│  │  mixer thread (owns the single Backend)          │            │
│  │        sum i32 → master gain → clamp → HDA       │            │
│  └───┬──────────────────────────────────────────────┘            │
│  ┌───▼──────────────────────────────┐                            │
│  │ audhda                           │                            │
│  │  BAR mmap · PIO verbs · BDL ring │                            │
│  └───┬──────────────────────────────┘                            │
└──────┼───────────────────────────────────────────────────────────┘
       │ DMA (bus master)
┌──────▼─────────────────┐
│ Intel HDA controller   │
│ + ALC269VC codec       │
└────────────────────────┘
```

## Crates

| Crate | Role |
|-------|------|
| `common/` (`audcommon`) | Socket path, protocol constants, format helpers, the A/V-sync frame constants (`SHIM_BUFFER_FRAMES`, `SHIM_SERVER_FRAMES`, `DRAIN_CHUNK`), TOML config, `AudioRingBuffer` shared by shim drain and server mixer. Single source of truth so shim and server cannot drift. |
| `server/` (`audiod`) | Accepts PCM clients, folds/resamples into the mix, drives the HDA backend, serves control commands, plays WAV/stdin directly. |
| `shim/` (`libaudshim.so`) | Intercepts ~100 ALSA symbols via `LD_PRELOAD`; models a virtual PCM device with a ring buffer and a time-based playhead; implements the full cubeb symbol set for Firefox. |
| `hda/` (`audhda`) | Minimal userspace Intel HD-Audio driver: controller bring-up, PIO verb engine, playback stream descriptor, pinned BDL/DMA ring, ALC269VC codec init. |
| `ctl/` (`audiod-ctl`) | CLI that sends control opcodes over the same socket (rate=0 header). |

## Server threading model

- **Accept loop** (`server/src/main.rs::run_server`): binds `/tmp/audiod.sock`,
  gates every connection through `SO_PEERCRED` (see [security.md](security.md)),
  spawns one **reader thread per PCM client** and one control-handler thread.
- **Reader threads** (`server/src/main.rs`): read the 12-byte header, then loop
  on length-prefixed chunks: bytes → `fold_to_stereo` (any format/channels 1–8)
  → resample to the mix rate → S16LE stereo bytes → bounded per-client ring.
  The ring's block-on-full push is the backpressure chain
  socket → shim → application.
- **Mixer thread** (`server/src/mixer.rs`): owns the single `Backend`
  (opened inside the thread — `HdaPlayback` wraps an mmap'd BAR raw pointer and
  is `!Send`). Each tick it:
  1. drains the registration channel,
  2. computes `writable = clamp(SHIM_SERVER_FRAMES − delay_frames, 0, CHUNK_FRAMES)`,
  3. pops ≤ `writable` frames from each live stream (missing data = silence),
     accumulates in **i32**, applies master gain/mute, saturates to i16,
  4. issues one `backend.play()` per tick,
  5. stops the HDA stream after 250 ms of all-dry rings (LPIB-wrap guard).
- **Control handler**: parses zero-length opcodes (`M`/`U`/`V`/`S`) and replies
  to STATUS with a JSON document.

## DSP pipeline (`server/src/dsp.rs`)

- `fold_to_stereo(src, fmt, ch)` — interleaved f32 stereo from S16LE/FLOATLE;
  mono duplicates; >2 channels average into L/R.
- `Resampler` — windowed-sinc coefficient table: 256 phases × 48 taps,
  Blackman-windowed sinc with cutoff `fc = 0.475·min(1, dst/src)` (anti-aliasing
  on downsample), each phase row normalized for unity DC gain. Position is
  tracked as an exact integer ratio so chunk splits are bit-exact. `"linear"`
  fallback selectable via config. Group delay ≈ `(TAPS/2)/src_rate` ≈ 0.5 ms,
  constant, absorbed by the buffering budget.
- `f32_stereo_to_s16` — final conversion before the mixer sums.

Verified by unit tests: passband flat ±0.5 dB to 18 kHz, image at 33.1 kHz
< −50 dB, 96k→48k alias < −50 dB, DC gain unity, chunk-split invariance
bit-exact.

## Shim model (`shim/src/lib.rs`)

Each opened "PCM handle" is a plain Rust struct with:

- an `AudioRingBuffer` (default 1 MiB),
- a **drain thread** (64 KiB stack) doing blocking length-prefixed socket writes,
- a **time-based playhead** used by `snd_pcm_delay` /
  `snd_pcm_avail_update` (see [av-sync.md](av-sync.md)).

`snd_pcm_writei` only pushes to the ring and returns immediately (blocking only
on ring-full), so the application never stalls on a socket write in the audio
callback path. The drain thread reconnects with exponential backoff
(50 ms → 2 s) if the server dies, re-sending the stashed header.

The shim resolves symbols from its own table via a fake `dlsym` handle;
unknown symbols return NULL with a warning instead of falling through to real
libasound (which would misinterpret shim handles). Config globals
(`snd_config*`) are stubs so cubeb's PulseAudio workaround short-circuits.

## Design patterns

| Pattern | Where |
|---------|-------|
| **Facade** | `Player` (`server/src/player.rs`) unifies the three entry points (socket server, `--stdin`, WAV file) into one muted + gain + convert + play path, with a passthrough fast path when client format == HW format and gain ≈ 1. |
| **Command** | `Cmd` enum (`read()` + `apply(Option<&mut Backend>)`) used by both the PCM-stream zero-length-opcode path and the control handler. |
| **Strategy / Adapter** | `Backend` (`server/src/backend.rs`) abstracts the playback device behind one interface. |
| **Singleton** | Config and `trusted_uid` cached in `OnceLock` for process lifetime. |

## Data flow summary

1. App calls `snd_pcm_writei` → bytes pushed to the shim ring, `size` returned.
2. Drain thread pops ≤ `DRAIN_CHUNK` (1024 B) at a time, writes
   `[len u32][pcm]` frames to the socket.
3. Reader thread converts/resamples and pushes S16LE stereo into its mixer ring.
4. Mixer paces off real HDA delay, sums all clients, writes to the DMA ring.
5. The HDA controller DMAs the BDL ring to the codec; LPIB tracks progress.

## Known limitations

- Fixed HW format: the mix always runs S16LE stereo at `mix_rate` (default 48000).
- Codec support is hardcoded to the ALC269VC node map (validated at probe).
- HDA backend requires root and an unbound kernel driver.
- No capture (playback only), no PAUSE via DMA pause register (drop/restart instead).
