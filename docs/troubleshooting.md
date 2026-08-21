# Troubleshooting

## Server won't start

| Symptom | Cause | Fix |
|---------|-------|-----|
| `HDA backend requires root (euid 0)` | Not running as root | `sudo ./target/release/audiod -d hda` |
| `open failed` / BAR mmap error | Kernel driver still bound to the slot | `echo 0000:00:1b.0 \| sudo tee /sys/bus/pci/drivers/snd_hda_intel/unbind` (adjust slot; `lspci -nn \| grep -i audio`) |
| `no codec on link` | Link reset raced or wrong slot | Retry; try `--skip-reset`; verify slot with `audiod -l` |
| `MUX_SPK ... widget type 0x0` / probe errors | Codec is not an ALC269VC, or link state is stale | Run `sudo audiod -d hda --dump-topology` to see what's actually there; full reset without `--skip-reset` |
| Socket bind fails / address in use | Stale `/tmp/audiod.sock` or old instance | `sudo scripts/audiod-hda.sh` handles kill+cleanup; or `rm /tmp/audiod.sock` |

## No sound

1. **Isolate the hardware path**:
   ```bash
   sudo ./target/release/audiod -d hda --selftest --secs=3
   ```
   No tone → the problem is below the socket path (codec/DMA). Check:
   - LPIB advancing with `RUST_LOG=audhda=debug` (frozen LPIB = bus master
     missing — should not happen; `pcicfg::set_master` runs at open)
   - Amp unmutes: after a link reset, codec amps power up muted. If you ran
     with `--skip-codec-init`, that's expected.
2. **Isolate the data path**: play a WAV directly
   (`./target/release/audiod file.wav`). Works but shim doesn't → shim/app
   issue.
3. **Check routing**: `audiod-ctl status` shows `"output"`; headphone jack
   sense switches routes within ~1 s.

## Audio plays forever after pause / app exit ("audio loop")

Stale DMA wrapping the 256 KiB ring replays the last ~1.4 s. Fixed by the
idle-stop + sticky-stop logic (server stops HDA after 250 ms all-dry; FLUSH
stops immediately on drop/pause). If you see it on a custom build, check that
`stop_immediate` errors are logged and retried, and that the empty-registry
branch arms the dry timer.

## mpv drops video frames / A-V drift

- Confirm constants agree: `SHIM_BUFFER_FRAMES`/`SHIM_SERVER_FRAMES` come from
  `common/src/lib.rs`; a mismatch between shim and server builds breaks the
  model. Rebuild everything together.
- Watch stats: `RUST_LOG=audshim=trace LD_PRELOAD=./target/release/libaudshim.so mpv …`
  Expect `Dropped: 0` steady-state; max |A−V| ≈ one video frame.
- Seek spikes are normal for ~1 frame; persistent offset is not.

## App can't connect / permission denied

The server only accepts root or the launching user's uid
([security.md](security.md)). If you started audiod as user A via sudo of user
B, clients must run as B (or root). Check `$SUDO_UID` was set at server start.

## Firefox: "OpenCubeb() failed to init cubeb"

The shim must resolve every symbol cubeb `dlsym`s; unknown symbols returning
NULL *and falling through to real libasound* causes exactly this failure. The
shim implements the full cubeb set — if you added code paths, run:

```bash
RUST_LOG=audshim=info LD_PRELOAD=./target/release/libaudshim.so firefox
```

and look for `dlsym(FAKE_HANDLE, "...") -> NULL` warnings naming the missing
symbol.

## alsamixer/amixer crash

Mixer intercepts were deliberately removed (they caused segfaults).
`LD_PRELOAD=./target/release/libaudshim.so alsamixer` must start cleanly and
show your real cards via pass-through dlopen; volume control goes through
`audiod-ctl` instead.

## Server killed mid-playback

Supported: the shim's drain thread reconnects with backoff (50 ms → 2 s),
re-sends its header, and playback resumes. `snd_pcm_writei` never returns EBADF
during the gap — it blocks on ring-full like real ALSA backpressure. If you see
write-error spam, your build predates the fix.

## Debug toolchain quick reference

```bash
sudo audiod -d hda --dump-topology      # widget graph walk
sudo audiod -d hda --dump-state         # codec register dump post-init
sudo audiod -d hda --dump-ring          # first DMA bytes hexdump
sudo audiod -d hda --skip-reset         # recover stuck IC without full reset
RUST_LOG=audhda=debug audiod            # ring wpos/lpib/delay stats
RUST_LOG=audshim=trace <app>            # shim delay/ring periodic stats
scripts/audiod-hda.sh log               # follow server log
```
