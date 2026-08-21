# Usage

## Building

```bash
cargo build --release
cargo test -p audcommon -p audiod   # unit tests (ring, dsp, …)
```

Outputs:

| Artifact | Purpose |
|----------|---------|
| `target/release/audiod` | Server binary |
| `target/release/libaudshim.so` | `LD_PRELOAD` library |
| `target/release/audiod-ctl` | Control CLI |

## Starting the server

The HDA backend needs **root** and the kernel `snd_hda_intel` driver unbound
from the PCI slot:

```bash
# Unbind (once per boot)
echo 0000:00:1b.0 | sudo tee /sys/bus/pci/drivers/snd_hda_intel/unbind

# Run the socket server
sudo ./target/release/audiod            # default slot 0000:00:1b.0
sudo ./target/release/audiod -d hda:0000:00:1f.3   # explicit slot
```

Or use the lifecycle script, which kills any old instance, unbinds/rebinds the
driver and tails the log for you:

```bash
sudo scripts/audiod-hda.sh          # (re)start
sudo scripts/audiod-hda.sh stop     # stop + give the slot back to the kernel
sudo scripts/audiod-hda.sh log      # follow /tmp/audiod.log
AUDIOD_SLOT=0000:00:1f.3 sudo -E scripts/audiod-hda.sh   # different slot
```

### `audiod` command line

```
audiod [-d slot]                    Run Unix socket server
audiod [-d slot] file.wav           Play a WAV file directly
audiod [-d slot] --stdin [opts]     Play raw PCM from stdin
audiod -l                           Show HDA controller info
```

| Option | Description |
|--------|-------------|
| `-d <dev>` | `hda` (default slot `0000:00:1b.0`) or `hda:<pci-slot>` |
| `-l` | Probe and print controller/codec info (root required) |
| `--rate=N` | Rate for `--stdin` (default 44100) |
| `--channels=N` | Channels for `--stdin` (default 2) |
| `--selftest [--secs=S] [--rate=R]` | 440 Hz sine straight to DMA, bypassing socket/conversion |
| `--skip-reset` | Skip HDA link reset (re-arm IC only — recovers a stuck immediate-command engine) |
| `--skip-codec-init` | Skip codec playback init (codec assumed pre-configured) |
| `--dump-state` | Dump codec register state after init |
| `--dump-topology` | Walk and log the codec widget graph |
| `--dump-ring` | Hexdump the first DMA ring bytes fed |

## Playing audio through the shim

Any ALSA application can be pointed at the shim without configuration:

```bash
LD_PRELOAD=./target/release/libaudshim.so mpv video.mkv
LD_PRELOAD=./target/release/libaudshim.so firefox          # cubeb ALSA backend
LD_PRELOAD=./target/release/libaudshim.so alsamixer        # must not crash; no mixer intercepts
```

Multiple clients play simultaneously and are mixed by the server. Seek/pause in
one client does not disturb the others.

Direct playback modes (no shim needed):

```bash
./target/release/audiod music.wav
sox input.mp3 -t s16le -r 48000 -c 2 - | ./target/release/audiod --stdin --rate=48000
```

## Controlling playback (`audiod-ctl`)

```bash
audiod-ctl status           # JSON state summary
audiod-ctl mute
audiod-ctl unmute
audiod-ctl volume 75        # percent, 0–100
audiod-ctl -s /tmp/audiod.sock status    # explicit socket path
audiod-ctl status | jq .
```

`status` output fields are documented in [protocol.md](protocol.md#control-opcodes).

## Systemd

Install `systemd/audiod.service` (edit `ExecStart` to your build path):

```ini
[Unit]
Description=audiod audio server (ALSA ioctl or direct HDA)
After=sound.target local-fs.target
Wants=sound.target

[Service]
Type=simple
ExecStart=/path/to/audiod -d hda
User=root
Restart=on-failure
RestartSec=2
StandardOutput=journal
StandardError=journal
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now audiod
journalctl -u audiod -f
```

Note: the kernel driver unbind does not survive reboot; either run it from a
oneshot unit before audiod starts or keep using `scripts/audiod-hda.sh`.

## Logging

Both binaries use `env_logger`; defaults are quiet-ish.

```bash
RUST_LOG=info audiod              # server default
RUST_LOG=debug audiod             # verbose HW params / hex dumps
RUST_LOG=audhda=debug audiod      # HDA ring stats (wpos/lpib/delay)
RUST_LOG=audshim=debug <app>      # shim writei instrumentation
RUST_LOG=audshim=trace <app>      # periodic delay/ring stats
RUST_LOG=warn LD_PRELOAD=./target/release/libaudshim.so mpv ...   # shim silent (default)
```

Server logs land wherever you redirect them (`scripts/audiod-hda.sh` uses
`/tmp/audiod.log`; systemd uses the journal).

## Verifying a session

```bash
# 1. Devices visible?
./target/release/audiod -l

# 2. Codec + DMA alive? (audible 440 Hz tone)
sudo ./target/release/audiod -d hda --selftest --secs=3

# 3. End-to-end with sync stats
RUST_LOG=audshim=trace LD_PRELOAD=./target/release/libaudshim.so \
  mpv --no-video somefile.mkv     # watch Dropped: 0, A-V ≈ 0.000

# 4. Multi-client + control
audiod-ctl status                 # "clients":N
audiod-ctl volume 60
```
