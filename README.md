# audiod

A minimal ALSA-compatible audio server written in Rust. Intercepts `libasound.so.2` at load time via `LD_PRELOAD`, forwards PCM over a Unix socket, and writes to hardware through a direct Intel HD-Audio (Azalia) userspace driver.

- **Direct HDA** — userspace Intel HD-Audio + ALC269VC programming via BAR mmap

No PulseAudio. No PipeWire. No libasound on the server side.

## Pipeline

```
mpv / Firefox
    ↓ LD_PRELOAD
audshim (shim/src/lib.rs)
    ↓ Unix socket (12-byte header + length-prefix PCM)
audiod (server/src/main.rs)
    ↓ BAR mmap
HDA controller
```

## Components

| Component | Lines | Description |
|-----------|-------|-------------|
| `common/` | 68 | Shared constants, format helpers, TOML config |
| `server/` | ~1140 | Socket server, HDA backend, WAV/stdin players, control socket |
| `shim/` | 1485 | `LD_PRELOAD` library exporting 100+ `snd_pcm_*` + `snd_config_*` symbols |
| `hda/` | 163 | Direct Intel HD-Audio (Azalia) userspace driver (controller, codec, stream, BDL) |
| `ctl/` | 81 | `audiod-ctl` CLI for mute/volume/status |

## Documentation

Full documentation lives in [`docs/`](docs/README.md):

| Document | Contents |
|----------|----------|
| [Architecture](docs/architecture.md) | Components, threading model, data flow, design patterns |
| [Wire protocol](docs/protocol.md) | Socket framing: header, PCM chunks, control opcodes |
| [Usage](docs/usage.md) | Building, running, clients, `audiod-ctl`, scripts, systemd, logging |
| [Configuration](docs/configuration.md) | Full `config.toml` reference with defaults |
| [A/V sync](docs/av-sync.md) | The time-based playhead model and its invariants |
| [HDA backend](docs/hda-backend.md) | Userspace HD-Audio driver internals, bring-up, debug flags |
| [Security](docs/security.md) | Peer authentication (`SO_PEERCRED`), trust model |
| [Troubleshooting](docs/troubleshooting.md) | Common failure modes and fixes |

## Quick start

```bash
# Build everything
cargo build --release

# Unbind the kernel driver (once)
echo 0000:00:1b.0 | sudo tee /sys/bus/pci/drivers/snd_hda_intel/unbind

# Start the server (HDA backend, default slot)
sudo ./target/release/audiod

# Play with any ALSA-aware app
LD_PRELOAD=./target/release/libaudshim.so mpv video.mp4

# Or play a WAV file directly
./target/release/audiod file.wav

# Or pipe PCM from stdin
./target/release/audiod --stdin --rate=48000 < tone.pcm
```

## Backends

### Direct HDA (only backend)

Replaces the kernel ALSA ioctl path with direct BAR programming:

- Controller bring-up: link reset, PCI bus master enable, codec detect via STATESTS
- PIO command/response (IC/IR/IRS) — no CORB/RIRB
- Single playback stream descriptor (SD0) with a 64-page (256 KiB) pinned DMA ring
- Realtek ALC269VC codec init: DAC→Mux→Pin for speaker and headphone paths
- Rate encoding: any `base*mult/div` expressible rate (8 kHz–192 kHz)

Requires **root** and the kernel `snd_hda_intel` driver must be unbound from the PCI slot:

```bash
# Unbind the driver (once)
echo 0000:00:1b.0 | sudo tee /sys/bus/pci/drivers/snd_hda_intel/unbind

# Start audiod
sudo ./target/release/audiod -d hda

# Or specify a different PCI slot
sudo ./target/release/audiod -d hda:0000:00:1f.3
```

A startup sweep (open+drop) halts any stale DMA ring left by a previous `SIGKILL`ed instance.

## HDA debug flags

```bash
audiod -d hda --skip-reset          # skip link reset (re-arm IC only)
audiod -d hda --skip-codec-init     # skip codec playback init
audiod -d hda --dump-state          # dump codec register state after init
audiod -d hda --dump-topology       # walk and log the codec widget graph
audiod -d hda --dump-ring           # hexdump first DMA ring bytes fed
audiod -d hda --selftest --secs=3   # 440 Hz sine straight to DMA (no socket)
```

## Control

`audiod-ctl` talks to the server over a dedicated control socket (`/tmp/audiod.sock`, rate=0 header). The server accepts control connections on the same listener; the control handler runs in its own thread.

```bash
audiod-ctl status          # JSON: muted, volume, active, total_frames, delay_frames, output
audiod-ctl mute
audiod-ctl unmute
audiod-ctl volume 75       # 0-100%
audiod-ctl status | jq .
```

## A/V sync model

The shim presents a virtual device with `delay + avail == BUFFER_SIZE` (8192 frames). Delay is derived from a **time-based playhead**:

```
delay = max(0, pushed_frames - rate * elapsed_since_first_write) + SERVER_FRAMES
```

This advances continuously with the DAC clock instead of jumping in drain-chunk quanta. The server caps HDA logical occupancy at the same `SERVER_FRAMES` (4096), so both sides share a single source of truth via `audcommon` constants.

Socket send buffer is limited to 32 KiB (kernel-doubled from 16 KiB), keeping ring oscillation to ≤2 drain chunks (~42 ms).

## Protocol

1. Client connects to `/tmp/audiod.sock`
2. Client sends **12-byte header** (all little-endian):
   - `[0..4]` rate `u32`
   - `[4..6]` channels `u16`
   - `[6..8]` format `u16` (`2` = S16_LE, `14` = FLOAT_LE)
   - `[8..12]` reserved `u32`
3. Server replies with nothing; stream is now live
4. Each PCM chunk: **4-byte length prefix** (LE `u32`) + raw interleaved PCM frames
5. Zero-length data frames carry **control opcodes** (`M`ute, `U`nmute, `F`lush, `V`olume, `S`tatus) — this is how `snd_pcm_*` control calls travel inline on the PCM stream

## Design patterns

- **Facade**: `Player` unifies the three entry points (socket server, `--stdin`, `--wav`) into one `muted + gain + convert + play` path with a passthrough fast path when client format/channels == HW and gain ≈ 1.0
- **Command**: `Cmd` enum (`read()` + `apply(Option<&mut Backend>)`) used by both the PCM-stream "zero-length data" path and `handle_control`
- **Singleton**: config + `trusted_uid` cached in `OnceLock`

## Security

The server binds a world-writable socket so non-root audio clients (shim, `audiod-ctl`) can connect when running as root (HDA backend). Every connection is checked via `SO_PEERCRED`:

- Only **root** or the **launcher's uid** (`$SUDO_UID` / `geteuid()`) may stream PCM or issue control commands
- Unknown peers are rejected with a warning and the connection is dropped

## Firefox / cubeb compatibility

Firefox's cubeb ALSA backend (`cubeb_alsa.c`) `dlsym`s 34 libasound symbols and configures via `snd_pcm_set_params`. The shim implements the full cubeb symbol set against its fake-pcm model so no call ever reaches real libasound with a shim-allocated handle. A one-shot `timerfd` (10 ms) paces poll-based clients instead of busy-looping an always-ready fd.

Verified: mpv A/V sync unchanged; Firefox YouTube audio plays through audiod.

## Reconnect

If the server dies and restarts, the shim's drain thread reconnects with exponential backoff (50 ms → 2 s), re-sends the stashed 12-byte header, and resets counters + playhead. `snd_pcm_writei` never fails with `EBADF` during reconnect — it pushes to the ring and blocks on ring-full (real ALSA backpressure) until space frees up.

## Configuration

`~/.config/audiod/config.toml` (or path in `AUDIOD_CONFIG`):

```toml
[server]
socket_path = "/tmp/audiod.sock"
volume = 1.0

[shim]
ring_buffer_size = 1048576
```

Config is loaded once via `OnceLock`; restart audiod to pick up changes.

## Systemd

`systemd/audiod.service` — start/auto-restart on crash, logs via journal.

```ini
[Unit]
Description=audiod audio server
After=sound.target local-fs.target

[Service]
Type=simple
ExecStart=/path/to/audiod -d hda
User=root
Restart=on-failure
RestartSec=2
Environment=RUST_LOG=info
```

## Caveats

| Topic | Detail |
|-------|--------|
| Single client | One PCM client at a time; second connection queues |
| Fixed HW format | Server always writes S16_LE stereo; conversion is done in software |
| Rate must match HW | HDA backend supports 8 kHz–192 kHz (base×mult/div) |
| No resampling | Client rate must be representable on the hardware |
| LD_PRELOAD fragility | Apps that call `dlsym(RTLD_DEFAULT, ...)` on ALSA symbols or use non-standard ALSA entry points may bypass the shim |
| HDA requires root | `CAP_SYS_ADMIN` for BAR mmap + pagemap; kernel `snd_hda_intel` must be unbound |

## Building

```bash
cargo build --release
```

Outputs:
- `target/release/audiod` — server binary
- `target/release/libaudshim.so` — `LD_PRELOAD` library
- `target/release/audiod-ctl` — control CLI

## Logging

```bash
RUST_LOG=info audiod              # server default
RUST_LOG=debug audiod             # verbose HW params / hex dumps
RUST_LOG=audhda=debug audiod      # HDA ring stats
RUST_LOG=audshim=debug audiod     # shim writei instrumentation
RUST_LOG=audshim=trace audiod     # periodic delay / ring stats
```

## License

MIT
