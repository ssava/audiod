# audiod documentation

`audiod` is a minimal ALSA-compatible audio server written in Rust. It intercepts
`libasound.so.2` at load time via `LD_PRELOAD`, forwards PCM over a Unix socket, and
writes to hardware through a direct Intel HD-Audio (Azalia) userspace driver.

No PulseAudio. No PipeWire. No libasound on the server side.

```
mpv / Firefox
    | LD_PRELOAD
audshim (shim/)
    | Unix socket (12-byte header + length-prefixed PCM)
audiod (server/)  -- mixes concurrent clients, resamples to the mix rate
    | BAR mmap + DMA ring
Intel HDA controller + ALC269VC codec
```

## Documentation index

| Document | Contents |
|----------|----------|
| [Architecture](architecture.md) | Components, threading model, data flow, design patterns |
| [Wire protocol](protocol.md) | Socket framing: header, PCM chunks, control opcodes |
| [Usage](usage.md) | Building, running the server, clients, `audiod-ctl`, scripts, systemd, logging |
| [Configuration](configuration.md) | Full `config.toml` reference with defaults |
| [A/V sync](av-sync.md) | The time-based playhead model and its invariants |
| [HDA backend](hda-backend.md) | Userspace HD-Audio driver internals, bring-up sequence, debug flags |
| [Security](security.md) | Peer authentication (`SO_PEERCRED`), trust model, threat notes |
| [Troubleshooting](troubleshooting.md) | Common failure modes and fixes |

## Components

| Crate | Binary / artifact | Description |
|-------|-------------------|-------------|
| `common/` | `audcommon` (lib) | Shared constants, format helpers, TOML config, ring buffer |
| `server/` | `audiod` | Socket server, multi-client mixer, DSP pipeline, direct HDA backend, WAV/stdin players, control socket |
| `shim/` | `libaudshim.so` | `LD_PRELOAD` library exporting ~100 `snd_pcm_*` / `snd_config_*` symbols |
| `hda/` | `audhda` (lib) | Direct Intel HD-Audio userspace driver (controller, codec, stream, BDL) |
| `ctl/` | `audiod-ctl` | Control CLI: mute / unmute / volume / status |

## Quick start

```bash
# Build everything
cargo build --release

# Unbind the kernel driver from the PCI slot (once per boot)
echo 0000:00:1b.0 | sudo tee /sys/bus/pci/drivers/snd_hda_intel/unbind

# Start the server (root required for the HDA backend)
sudo ./target/release/audiod -d hda

# Play with any ALSA-aware application
LD_PRELOAD=./target/release/libaudshim.so mpv video.mp4

# Firefox works too (cubeb ALSA backend)
LD_PRELOAD=./target/release/libaudshim.so firefox

# Or play a WAV file directly through the server
./target/release/audiod file.wav

# Or pipe raw PCM from stdin
./target/release/audiod --stdin --rate=48000 < tone.pcm
```

See [usage.md](usage.md) for the full command reference, or
[scripts/audiod-hda.sh](../scripts/audiod-hda.sh) for a lifecycle helper
(`start` / `stop` / `log`) that handles driver unbind/rebind automatically.

## Requirements

- Linux x86_64, Rust toolchain (stable)
- Intel HD-Audio controller with a Realtek ALC269VC codec (hardcoded node map;
  other codecs fail loudly at probe — see [hda-backend.md](hda-backend.md))
- Root (`CAP_SYS_ADMIN`) for BAR mmap + pagemap PFN reads; the kernel
  `snd_hda_intel` driver must be unbound from the PCI slot while audiod runs

## License

MIT
