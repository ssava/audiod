# audiod — Project Tracker

## Build & Test Commands
```bash
cargo build --release                    # Build server + shim
sudo scripts/audiod-hda.sh               # Unbind driver + (re)start HDA server (stop|log subcommands)
LD_PRELOAD=./target/release/libaudshim.so mpv <file>  # Test with shim
LD_PRELOAD=./target/release/libaudshim.so alsamixer    # Verify no crash
./target/release/audiod -l                            # List available devices
```

## Current Status
- **Pause-loop fix (2026-08-21)**: pausing a video replayed ~1.4 s of stale audio ("loops 1 time") before going silent. Two compounding causes: (1) the multi-client refactor regressed flush handling — `Cmd::Flush` only cleared the client ring and no longer stopped the DMA (pre-refactor `serve_client` called `Backend::stop_immediate()`); (2) the mixer's idle watchdog sat *behind* the `writable == 0` early-out, and after writes stop, LPIB passes `write_pos` so `delay_bytes()` wraps to ~RING_SIZE → `writable == 0` for a full ring traversal (~1.37 s at 48k S16 stereo), during which the dry timer could not even arm. Live log evidence: two shim `MSG_FLUSH` sends, zero server-side flush logs, single `all streams dry 250ms` stop exactly ~1.46 s after the last flush. Fix (`server/src/mixer.rs`, `server/src/main.rs`): `MixerStream::request_flush()` flag consumed by the mixer each tick — drops that stream's queue and calls `stop_immediate()` immediately when no other stream has audio pending (multi-client safe: other streams keep playing); idle watchdog refactored into `try_idle_stop()` and now runs on EVERY tick based on queue occupancy, immune to the wrap window. Resume path unchanged (`HdaPlayback::start()` re-setups after `drop_all`). Verified: build/clippy clean, 15/15 tests pass.
- **Idle-stop fix (2026-08-21, live-verified)**: after the last client disconnects, the mixer never stopped the HDA stream — `retain()` evicts the finished stream the same tick its ring empties, while `dry_since` was still `None` from the last data tick, and the `streams.is_empty()` branch only *checked* the dry timer without arming it. Result: DMA kept running and wrapped its 256 KiB ring, replaying the last ~1.4 s forever ("audio loop" with 0 clients). Fix: the empty-registry branch arms the timer itself; `stop_immediate` errors are logged + retried (was `let _ =`); stops are sticky (`stopped` flag cleared only when data flows again) so idle doesn't re-SRST every 250 ms; `STATE.last_delay_frames` zeroes on stop so STATUS reads truthfully. Verified: two 120 s tones mixed concurrently, both disconnect → exactly one "HDA stopped" line, `delay_frames:0`, no loop.
- **Multi-client mixing (2026-08-21)**: server now mixes concurrent PCM clients into one shared HDA backend. Per-client reader thread (`server/src/client.rs`) → fold to stereo f32 → resample to mix rate → bounded ring; single mixer thread (`server/src/mixer.rs`) owns the `Backend`, sums active rings as i32 with master gain + clamp, paces each stream at `writable = clamp(SHIM_SERVER_FRAMES − delay_frames, 0, CHUNK_FRAMES=1024)` to preserve the A-V sync invariant, and stops the HDA stream after 250 ms all-dry. Resampler: windowed-sinc table 256 phases × 48 taps Blackman (`server/src/dsp.rs`), position tracked as exact integer ratio so chunk splits are bit-exact; `"linear"` fallback via config. Verified by 13 dsp unit tests: passband flat ±0.5 dB to 18 kHz, image at 33.1 kHz < −50 dB, 96k→48k alias < −50 dB, DC gain unity, chunk-split invariance bit-exact, impulse response sane. **Test-harness lesson**: feed resampler tests interleaved stereo — a mono sine reinterpreted as frame pairs silently doubles every frequency and gives plausible-looking garbage.
- Normal playback A-V sync: within ±0.040 (40ms) ✓
- Seek A-V spike: recovers to 0.000 with ~1 dropped frame ✓
- No 55% speed regression, no "unexpected partial write" errors ✓
- **Zero drops (2026-08-11)**: fixed by switching the shim delay model from quantized `in_flight` counting to a **time-based playhead** (`delay = pushed_frames − rate·elapsed + SERVER_FRAMES`). Verified 45s mpv run: `Dropped: 0`, A-V max abs 0.033 (= one 30fps video frame, mpv's display granularity), mean abs 0.0007, real-time pacing (29s media in 30s wall). Live IPC seeks to 1:40 and 5:00 both hold A-V at 0.000 with 0 drops.
- `snd_pcm_writei` always returns `size`, uses ring buffer push (never blocks on socket) ✓
- `snd_pcm_delay` uses a **time-based playhead**, not `ring.filled()`: `(pushed_frames − rate·elapsed).max(0) + SERVER_FRAMES`, clamped to `BUFFER_SIZE` (8192). `play_start` anchors on first `writei` after `prepare`/`drop`. The playhead advances continuously with the DAC clock, so delay/avail move smoothly instead of jumping in drain-chunk quanta — this is what killed the last ~1-4 f/s drops.
- Shim constants: `DRAIN_CHUNK=1024` (small socket writes make ring oscillation fine-grained), `BUFFER_SIZE=8192` advertised frames, `SERVER_FRAMES=4096` baseline matching server-side HDA occupancy. `delay + avail == BUFFER_SIZE` invariant (like real snd_pcm).
- Socket buffer limited via `setsockopt(SO_SNDBUF=16384)` — kernel doubles to 32768, limits ring oscillation to ~2 drain chunks ✓
- HW_PARAMS EINVAL: fixed — always configure with 2 channels ✓
- Chmap SEGV: fixed — `snd_pcm_get_chmap` returns valid stereo chmap ✓
- `dlopen` only intercepts `libasound.so` (not plugin modules); `dlsym` on a fake handle with an unknown symbol returns NULL + warn! (no real-libasound fallthrough — that was the cubeb break), except `snd_config` (special-cased) ✓
- Server trusts only its launcher (uid 0 or `$SUDO_UID`) via SO_PEERCRED for both PCM and control connections — no more unauthenticated `/tmp/audiod.sock` access
- Fake ALSA mixer intercepts removed — alsamixer/amixer no longer segfault under LD_PRELOAD ✓
- Auto-device enumeration: `-d <name>` selects device by hw:X,Y, name substring, or path; `-l` lists all ✓

## Firefox/cubeb compatibility (2026-08-20)
- **Root cause of "OpenCubeb() failed to init cubeb"**: Firefox's cubeb ALSA backend (`cubeb_alsa.c`) `dlsym`s 34 libasound symbols and configures streams via `snd_pcm_set_params` (not mpv's hw_params path). Symbols missing from the shim's `lookup()` fell through to **real** libasound (loaded via libxul's `NEEDED libasound.so.2` for `snd_seq`/MIDI). Real `snd_pcm_set_params` then interpreted our fake `snd_pcm_t` as a real handle → error → `cubeb_stream_init` fails → Firefox plays no audio.
- **Fix**: the shim now implements the full cubeb symbol set against its fake-pcm model:
  - `snd_pcm_set_params` commits the 12-byte protocol header (shared `commit_configured` helper with `snd_pcm_hw_params`); Firefox uses FLOAT_LE which the server already converts to S16.
  - `snd_pcm_get_params` (buffer=8192/period=1024), `snd_pcm_frames_to_bytes`, `snd_pcm_open_lconf` (delegates to `snd_pcm_open`), `snd_pcm_hw_params_get_channels_max` (=2), `snd_pcm_hw_params_get_rate` (reads rate stashed in the param blob at offset 0, default 48000).
  - **Poll pacing**: `snd_pcm_poll_descriptors{,_count,_revents}` use a per-PCM **one-shot timerfd (10 ms)** re-armed on fire — real ALSA signals writability via the fd; we have no such fd, so the timer paces cubeb's write loop instead of busy-looping an always-ready fd or stalling on an inert one.
  - Config symbols (`snd_config` global + `snd_config_*`, `snd_lib_error_set_handler`) are stubs; `snd_config` returns the address of an always-NULL storage so cubeb's PulseAudio workaround short-circuits.
  - `snd_pcm_hw_params_any` now zeroes the param blob (deterministic `get_rate`).
- Verified: mpv A-V sync unchanged; Firefox YouTube audio now plays through audiod.

## Robustness & Security Pass (2026-08-20, "Implement all" of 9 review items)
- **Design patterns now explicit**: `Player` is a **Facade** (server/src/player.rs) unifying the three players (socket/`--stdin`/`--wav`) into one muted+gain+convert+play path with a passthrough fast path when client format/channels == HW and gain≈1; control opcodes are the **Command** pattern (`Cmd` enum with `read()` + `apply(Some(&mut Backend)) -> Option<String>`), read by both the PCM-read "zero-length-opcode" path and `handle_control`; `Backend`/`Controller` are **Strategy/Adapter**; config + `trusted_uid` are **Singleton** via `OnceLock`.
- **Security**: `/tmp/audiod.sock` previously accepted any local user. `trusted_uid()` (first `$SUDO_UID`, else `geteuid()`, cached in `OnceLock`) + `peer_uid()` via `SO_PEERCRED` now gate both PCM and control connections (`peer_allowed`: uid 0 or the launcher's uid, reject otherwise, warn log).
- **A-V sync constants centralized** in `audcommon` (single source of truth, no drift): `SHIM_BUFFER_FRAMES=8192`, `SHIM_SERVER_FRAMES=4096`, `DRAIN_CHUNK=1024`, `HW_PERIOD_FRAMES=1024`, `HW_BUFFER_FRAMES=4096`. `backend.rs::TARGET_FRAMES` and `alsa_hw.rs::setup_pcm_ex` reference them.
- **Drain reconnect** (was "Reconnection on server restart" in Missing Features): `snd_pcm_drop`/server death no longer kills the shim. The drain thread's `reconnect_audiod()` (50 ms → 2 s exponential backoff) re-connects, re-sends the stashed 12-byte header, and resets counters + playhead on reconnect; `FLUSH_CMD` handling moved into the drain thread. `send_frame()` detects the dead socket.
- **Server-restart reconnect verified live (2026-08-20)**: SIGKILL of the HDA server mid-120s-playback + restart now re-establishes the session cleanly — mpv exits 0, `backend ready: 4` (2 sweeps + 2 client opens), `client connects: 2`, `done: 1`, `shim reconnected: 1`, **0** mpv write errors. Two fixes made this possible: (1) `snd_pcm_writei` **no longer returns EBADF when `h.sock` is transiently -1** — a server restart previously made every mpv write fail with `pcm write error: Bad file descriptor` (thousands of lines, audio silent to EOF); writei now just pushes to the ring and blocks on ring-full (real ALSA backpressure) until the drain reconnects and frees space; (2) the server's accept header-read (`read_header_tolerant`) retries on WouldBlock/EAGAIN (2 s deadline) instead of dropping a reconnect whose 12-byte header hasn't landed yet — the old nonblocking `read_exact` raced the connect. Also instrumented: `connect_audiod` logs its failing errno at `debug!`, `reconnect_audiod` warns every 10 s of failure, all visible with `RUST_LOG=audshim=info`.
- **Truthful format widths**: `snd_pcm_format_width` / `snd_pcm_format_physical_width` now return spec values from `format_widths()` (S16=16/16, S24_LE=24/32, S32_LE=32/32, FLOAT_LE=32/32, FLOAT64_LE=64/64) instead of lying to Firefox/PulseAudio.
- **Real PCM fd in STATUS**: `STATE.pcm_fd` is the actual client fd while streaming (resets to -1 after), so `audiod-ctl status` reports correct `"active"` instead of a stale -1.
- **HDA bring-up guards**: `Controller::open` logs PCI vendor/device and fences on GCAP (rejects a non-HDA slot before hammering its registers); `--skip-reset` re-arms a stuck IC instead of assuming clean state. `Codec::check_widget` validates the hardcoded ALC269 node map (DACs=OUT, muxes=MIX/SEL, pins=PIN) and fails loudly instead of programming wrong widgets on a non-ALC269 codec.
- **Ah — param-id bug found & fixed during bring-up**: `AC_PAR_AUDIO_WIDGET_CAP` was 0x04 (that's NODE_COUNT) — widget caps are **0x09**; `AC_PAR_STREAM_FORMATS` was 0x0a (that's PCM) — stream formats are **0x0b**; conn-list reader used bit 7 instead of bit 6 (`AC_WCAP_CONN_LIST`). Symptom: `backend open failed: MUX_SPK ... widget type 0x0` after a link reset + `--dump-topology` showing only nid=0x01. Verified fixed: full 0x01–0x20 topology walk with correct types, clean playback, no reconnects.
- **Debug flags replace env vars**: `-d hda` now accepts `--skip-reset`, `--skip-codec-init`, `--dump-state`, `--dump-ring`, `--dump-topology` (an env-var fallback is kept in `audhda::dbg`). `-l` still lists devices.
- Build/test/clippy all clean; mpv regression re-verified against HDA backend.

- **Systemd service**: `systemd/audiod.service` — start/auto-restart on crash, logs via journal. HDA requires root + `snd_hda_intel` driver unbound; ALSA backend works without root.

## Direct HDA Backend (branch work, new crate `hda`/`audhda`)
- New workspace member `hda/` — minimal userspace Intel HD-Audio (Azalia) controller driver replacing the kernel ioctl path when `-d hda[:slot]`.
- `server/src/backend.rs` — `Select` (Alsa/Hda) + `Backend` enum; server main now deals with `Backend` only.
- `hda/src/` modules: `regs` (BAR register map), `mmio` (resource0 mmap), `pagemap` (mlock + /proc/self/pagemap PFN → phys), `bdl` (BDL entries), `pcicfg` (TCSEL + SCH NOSNOOP tweaks), `controller` (link reset, STATESTS detect, PIO cmd via IC/IR/IRS), `stream` (SD0 setup/start/stop/SRST, 64-page DMA ring, LPIB delay), `codec` (ALC269VC probe + init_playback: DAC0x02→Mux0x0c→Pin0x14 + HP path).
- Requires root (CAP_SYS_ADMIN) for BAR mmap + pagemap, and the kernel `snd_hda_intel` driver must be unbound from the PCI slot first.
- **Playback SD bank (fix for silent DMA)**: SD registers for OUTPUT streams are NOT at 0x80. Kernel `azx_init_streams` assigns streams global indexes `0..num-1`, capture first (`playback_index_offset = capture_streams`), and `snd_hdac_stream_init` computes `sd_addr = 0x80 + 0x20*idx`. So on a chip with ISS>0 capture streams, SDO0 lives at `0x80 + 0x20*capture_streams` (e.g. 0x100 for ISS=4). Header comment: *SDI0=0x80, SDI1=0xa0, ... SDO3=0x160*. Our code now derives `sd0_base()` from GCAP ISS; stream tag = `capture_streams + 1` (kernel assigns `tag = i+1`); SIE bit = `1 << global_idx`.
- **PCI bus master required (fix for LPIB stuck)**: `lspci -vv` showed `BusMaster-` on the unbound device. The kernel enables it via `pci_set_master()` during probe; being unbound clears it, so the controller can never fetch the BDL/ring and LPIB stays 0. `pcicfg::set_master` now sets PCI Command (off 0x04) bits 1 (memory space) + 2 (bus master), called from `Controller::open`.
- **Pagemap PFN decode is fine**: previously logged `BDLpa=0x133b3e000` and misread as ~1.3 TB; it is 4.8 GB, and `ringpa0=0x30e2d8000` is ~12.2 GB — both within 16 GB RAM. The pagemap decode was correct; the DMA silence was the missing bus-master bit. `DmaBuffer::new` now sanity-checks each PFN `< 128 TiB` (would indicate a decode regression).
- **Byte-width SD_CTL writes on start**: `snd_hdac_stream_start` sets `SD_CTL_DMA_START|SD_INT_MASK` via `updateb` (byte RMW) since Intel PCH has `access_sdnctl_in_dword=0` (only Loongson/Zhaoxin set it to 1). A 32-bit word write previously failed to latch RUN (readback showed bit18 TRAFFIC_PRIO but no bit1 DMA_START).
- `SD_STS` is an 8-bit register at SD base + 0x03 (do not read it as 32-bit — readback spans LPIB and yields 0xffffffff).
- **Live bring-up verified 2026-08-08**: with `snd_hda_intel` unbound and `-d hda --stdin --rate=48000`, SD0 (0x100, tag 5) arms cleanly, all codec verbs ACK, and DMA now **advances** (`SD0 started: CTL=0x2054001e STS=0x20 LPIB=0x20`, then LPIB increments through the run, FIFO_READY=0x20 set) — the previous frozen-LPIB stall was the missing **PCI bus master** bit. `drain()` waits out the ring, so a 288 KB / 1.5 s tone plays at real-time after stdin EOF. HDA backend works as root: `sudo ./target/release/audiod -d hda --stdin --rate=48000 < /tmp/audiod_sine.pcm`.
- **HDA ring occupancy capped at 4096 frames for A/V sync (fixed 2026-08-09)**: the server's HDA `play` loop previously wrote the whole 256 KiB DMA ring (~1.4 s of audio). That buffered data was invisible to the shim's `snd_pcm_delay = ring.filled()/fb + 4096` estimate, so mpv under-estimated audio delay by ~1.3 s, thought audio was far ahead, and dropped video frames to chase the phantom clock ("a lot of frame drop"); stale DMA audio also kept playing ~1.4 s after seeks. `Backend::play` for HDA now caps *logical* DMA occupancy at `TARGET_FRAMES=4096` (matching the shim's +4096 model and the ALSA kernel buffer), sleeping 1 ms when full — the physical ring stays 64 pages for underrun headroom. This restores the ALSA-verified ±40 ms sync model.
- **Playback pause idle-loop (fixed 2026-08-10)**: with a push AO (mpv), pause calls `snd_pcm_drop` but the server never learned of it, so the HDA DMA kept running and `SD_LPIB` wrapped the 256 KiB ring, re-playing stale audio forever. Now `snd_pcm_drop`/`snd_pcm_pause(1)` set `FLUSH_CMD` → the drain thread sends `MSG_FLUSH` (len=0 + `b'F'`) → `serve_client` calls `Backend::stop_immediate()` (HDA only). A server-side `poll` watchdog (250 ms on the length-prefix read) additionally stops an idle HDA stream in cases mpv drops nothing (`--keep-open` EOF). `HdaPlayback::start()` re-runs `srst()+setup()` when `!running`, and `drop_all()` now zeroes the DMA ring, so a restart is clean (LPIB=0, silence until fresh data). `SND_PCM_STATE_PAUSED` const + `common::MSG_FLUSH` added.
- **Silent-after-reset root cause (fixed 2026-08-09)**: `make_verb` truncated the verb payload to 8 bits (`payload & 0xff`), but the kernel's `snd_hdac_make_cmd` ORs a **16-bit** parm after `verb<<8` — SET/GET_AMP verbs carry `OUTPUT|LEFT|RIGHT` and index bits in payload[15:8]. Dropping those bits made the amp-unmute verbs (b044 etc.) no-ops. Skip-reset happened to work because the kernel had already unmuted the amps; a link reset returns the codec amps to their **muted** power-on default, so playback was silent. `make_verb` now uses `payload & 0xffff`. Also corrected readback verbs: `GET_STREAM_FORMAT` is `0x0a00` (not `0xf02`=GET_CONNECT_LIST) and GET_AMP uses index in bits 0-3. Selftest tone is now audible with the full reset path.

## Memory Optimizations (branch `memory-optimize`, merged master 2026-07-12)
- Ring buffer default: 2MB → 1MB (-50%)
- Drain thread stack: 2MB (Rust default) → 64KB via `Builder::new().stack_size(65536)`
- `ring.pop()` API changed: `pop(max) -> Vec<u8>` → `pop(&mut [u8]) -> usize`. Pre-allocated reusable buffer eliminates per-chunk heap alloc/free.
- Server conversion buffer: `reserve`+`set_len` instead of `resize(0)` skips zero-fill.
- Server data buffer: `reserve(len)` before read loop avoids repeated growth.

## Architecture
- `common/` — Shared constants (SOCKET_PATH, format codes, format_bytes helpers) + TOML config via `serde` + `AudioRingBuffer` (`ring.rs`, shared by shim drain and server mixer).
- `server/` — Binary. Direct HDA backend only (`-d hda[:slot]`, root required). **Multi-threaded**: one reader thread per PCM client (`client.rs`: SO_PEERCRED gate → 12-byte header → fold to stereo f32 → resample → bounded ring push for backpressure; stream-scoped `Cmd::Flush`) + a single **mixer thread** (`mixer.rs`) that owns the `Backend` (opened inside the thread — `HdaPlayback` is `!Send`), sums active client rings as i32 with master gain + clamp, paces at `writable = clamp(SHIM_SERVER_FRAMES − delay, 0, CHUNK_FRAMES)` and idles the HDA stream after 250 ms all-dry. DSP pipeline in `dsp.rs`.
- `shim/` — cdylib. LD_PRELOAD intercept. Each handle has an `AudioRingBuffer` + a drain thread that does blocking socket writes. `snd_pcm_writei` only pushes to ring buffer, returns `size` immediately.
- Protocol: 12-byte header (rate:u32, channels:u16, format:u16, reserved:u32) + 4-byte length prefix + PCM data frames.
- Server always runs HW as S16_LE stereo at the mix rate (default 48000); clients of any rate/channels 1–8/S16|FLOAT are folded + resampled into the mix.
- Socket buffer limited via `setsockopt(SO_SNDBUF=16384)` — kernel doubles to 32768. Prevents drain thread from overfilling the socket buffer, keeping ring oscillation to ≤2 drain chunks (~42ms).

## A/V Sync Design
- **snd_pcm_writei** → pushes to ring buffer, returns `size` (never blocks unless ring full)
- **Drain thread** (per handle) → pops from ring, blocking socket write (stack 64KB)
- **Server** (single-threaded) → blocking socket read, converts, WRITEI ioctl. The ~21ms ioctl block causes only ~8KB to accumulate in the kernel socket buffer (425KB default), so drain thread never stalls.
- **snd_pcm_delay** → time-based playhead: `(pushed_frames − rate·elapsed).max(0) + SERVER_FRAMES(4096)`, clamped to `BUFFER_SIZE(8192)`. `play_start` anchors on first writei after prepare/drop. Advances continuously with wall clock, so no drain-chunk quantization.
- **snd_pcm_avail_update** → `BUFFER_SIZE(8192) − delay` (delay + avail == BUFFER_SIZE, as real snd_pcm)
- **snd_pcm_close** → signals drain thread via `interrupt()`, joins, closes socket
- **snd_pcm_drop** → clears ring, resets pushed/written/play_start, sends MSG_FLUSH to server

## Config (`common/src/config.rs`)
- `~/.config/audiod/config.toml` with `[shim] ring_buffer_size` (default 1MB) and `socket_path`
- `[server]` knobs: `mix_rate` (48000), `max_clients` (8), `client_ring_bytes` (65536), `resampler` ("sinc" | "linear")
- `AUDIOD_CONFIG` env var override
- Loaded once via `OnceLock`, cached for lifetime

## Line Counts
```
common/src/lib.rs:       69
common/src/config.rs:   102
common/src/ring.rs:     152
server/src/main.rs:     876
server/src/mixer.rs:    222
server/src/dsp.rs:      453
server/src/backend.rs:  126
server/src/player.rs:   118
shim/src/lib.rs:       1484
hda/src/lib.rs:         163
hda/src/regs.rs:        105
hda/src/mmio.rs:         95
hda/src/pagemap.rs:     147
hda/src/bdl.rs:          44
hda/src/pcicfg.rs:       94
hda/src/controller.rs:  284
hda/src/stream.rs:      307
hda/src/codec.rs:       345
hda/src/dbg.rs:          31
ctl/src/main.rs:         81
Total:                5298

## Logging
- `log` + `env_logger` integrated in both server and shim
- Server: `RUST_LOG=info audiod` (default), `RUST_LOG=debug audiod` for verbose
- Shim: `RUST_LOG=warn` (default, silent), `RUST_LOG=audshim=debug` for writei instrumentation, `RUST_LOG=audshim=trace` for periodic delay/ring stats
- Log levels: `info!` for connection/setup/drain lifecycle, `debug!` for HW params/hex dumps/writei stats, `warn!` for partial frames/EPIPE, `error!` for failures
- HDA ring stats (`ring: wpos=... lpib=...`) logged at `debug!`; enable with `RUST_LOG=audhda=debug`
- Shim init: lazy `Once`-based init (cdylib can't init at load time)

## Notable Missing Features (Nice-to-Have)

### Control & Monitoring
- **Control via `audiod-ctl`**: `audiod-ctl status|mute|unmute|volume <pct>` works over the control socket (SO_PEERCRED-gated). Volume maps to `Cmd::Volume` → `set_state_volume` (gain applied per-`play`). Per-stream latency stats are still unimplemented.
- **Latency stats endpoint**: Expose `snd_pcm_delay`, `ring.filled()`, `total_written`/`total_pushed` via socket for real-time A-V health monitoring.

### Server Hardening
- **Graceful shutdown**: Server ignores SIGTERM/SIGINT — should flush pending data before exit.

### Config & Deployment
- **Runtime config reload**: Config loaded once via `OnceLock`. Must restart audiod to change socket path or ring buffer size.
- **Systemd service**: `.service` file so audiod is managed by systemd (auto-start, restart on crash, logging via journal).
- **Format negotiation**: Server folds any client format into the mix, but doesn't query clients for optimal format.

### Shim Improvements
- **Seamless reconnect**: Instead of dropping PCM frames and reconnecting socket on seek, the shim could buffer through seeks.
- **Mixer pass-through**: If alsamixer integration is needed, re-add mixer intercepts as pass-through to real libasound with mute bridging (previously removed to fix segfault).
