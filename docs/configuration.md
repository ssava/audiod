# Configuration

audiod reads a single TOML file at startup. It is loaded **once** via `OnceLock`
and cached for the process lifetime — restart audiod to pick up changes.

## File location

1. `$AUDIOD_CONFIG` (path to the file), if set
2. `~/.config/audiod/config.toml` otherwise

A missing file is not an error; all defaults apply.

## Full reference

```toml
[server]
# Unix socket audiod binds and clients connect to.
socket_path = "/tmp/audiod.sock"

# Startup master volume as a gain factor (1.0 = 100%). Seeded as percent;
# change at runtime with `audiod-ctl volume <pct>`.
volume = 1.0

# Rate the mixer runs the HDA backend at; every client is resampled to it.
mix_rate = 48000

# Maximum concurrent PCM clients. Further connections are rejected
# immediately (the shim's reconnect backoff absorbs it).
max_clients = 8

# Per-client server-side ring capacity in bytes (S16LE stereo frames).
# 65536 B ≈ 170 ms of headroom per client @48 kHz.
client_ring_bytes = 65536

# Client→mix-rate resampler: "sinc" (windowed-sinc table, default)
# or "linear" (debug / A-B comparison fallback).
resampler = "sinc"

[shim]
# Per-handle ring buffer size in bytes inside libaudshim.so.
ring_buffer_size = 1048576
```

## Defaults table

| Key | Default | Meaning |
|-----|---------|---------|
| `server.socket_path` | `/tmp/audiod.sock` | Bind path |
| `server.volume` | `1.0` | Initial gain |
| `server.mix_rate` | `48000` | Mixer/HDA rate |
| `server.max_clients` | `8` | Concurrent PCM clients |
| `server.client_ring_bytes` | `65536` | ~170 ms per client @48k S16 stereo |
| `server.resampler` | `"sinc"` | `"sinc"` or `"linear"` |
| `shim.ring_buffer_size` | `1048576` | Shim-side ring (1 MiB) |

## Notes and tuning hints

- **`mix_rate`** must be a rate the HDA link can encode exactly
  (`base*mult/div`, mult/div ≤ 8): 8000, 11025, 12000, 16000, 22050, 24000,
  32000, 44100, 48000, 88200, 96000, 176400, 192000.
- **`client_ring_bytes`** absorbs scheduling jitter between reader threads and
  the mixer. Too small → underruns under load; too large → more buffered
  latency for that client's flush tail (~85 ms of DMA residue plays out after a
  seek regardless).
- **`resampler = "linear"`** halves CPU but fails the alias/imaging tests; use
  it only for debugging or A-B listening.
- The shim reads only `[shim]`; the server ignores it, and vice versa.

## Environment variables

| Variable | Used by | Purpose |
|----------|---------|---------|
| `AUDIOD_CONFIG` | both | Path to config file |
| `RUST_LOG` | both | `env_logger` filter (see [usage.md](usage.md#logging)) |
| `SUDO_UID` | server | Launcher uid recorded for peer trust (see [security.md](security.md)) |
| `AUDIOD_SLOT` | `scripts/audiod-hda.sh` | PCI slot override for the lifecycle script |

Legacy `AUDIOD_*` debug env vars are still honored inside `audhda::dbg` as a
fallback, but the CLI flags (`--skip-reset`, `--dump-topology`, …) are the
supported interface.
