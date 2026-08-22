# Direct HDA backend (`audhda`)

The `hda/` crate is a minimal userspace Intel HD-Audio (Azalia) driver. It
replaces the kernel ALSA ioctl path entirely: audiod mmaps the PCI BAR, brings
up the link, programs the codec over CORB/RIRB (PIO fallback), and feeds a
pinned DMA ring.

## Module map

| Module | Purpose |
|--------|---------|
| `regs.rs` | BAR register map (offsets, bit definitions) |
| `mmio.rs` | `resource0` mmap + typed register access |
| `pagemap.rs` | `mlock` + `/proc/self/pagemap` PFN → physical address for DMA buffers |
| `bdl.rs` | Buffer Descriptor List entries |
| `pcicfg.rs` | PCI config: bus-master enable, TCSEL, SCH NOSNOOP |
| `corb.rs` | CORB/RIRB DMA command/response rings (default codec transport) |
| `controller.rs` | Link reset, STATESTS codec detect, command engine dispatch (CORB/RIRB or PIO IC/IR/IRS fallback) |
| `stream.rs` | Playback SD setup/start/stop/SRST, 64-page DMA ring, LPIB delay |
| `codec.rs` | ALC269VC probe + playback path init (DAC→Mux→Pin), topology dump |
| `dbg.rs` | Debug options (CLI flags, legacy env fallback) |

## Bring-up sequence

1. **Open** — mmap BAR0 (`resource0`), read GCAP to learn input/output stream
   counts; fence on a non-HDA slot before touching its registers.
2. **PCI bus master** — set Command bits 1 (memory space) + 2 (bus master).
   The kernel normally does this in probe; an unbound device has it cleared,
   and without it DMA never fetches (LPIB stays 0).
3. **Link reset** — CRST cycle, wait for codec detection in STATESTS.
4. **Codec probe** — read vendor ID via codec verbs (CORB/RIRB by default,
   `--cmd-engine=pio` for the legacy immediate-command interface); validate
   the hardcoded
   ALC269VC node map (widget types must match: DACs=OUT, muxes=MIX/SEL,
   pins=PIN) and fail loudly on anything else.
5. **Stream setup** — SRST the playback SD, program format/tag/BDL/CBL/LVI.
6. **Codec init** — unmute amps along DAC 0x02 → Mux 0x0c → Pin 0x14
   (speaker) / headphone path; SET_STREAM_FORMAT + converter channel tag.

A startup sweep (open+drop) halts any stale DMA ring left by a previously
`SIGKILL`ed instance.

## Hardware facts learned the hard way

These are encoded in code comments but worth documenting:

- **Playback SD bank offset**: output streams do not start at 0x80. With
  `ISS` capture streams, SDO0 lives at `0x80 + 0x20*ISS` (e.g. 0x100 when
  ISS=4). The kernel assigns capture streams first; our `sd0_base()` derives
  this from GCAP. Stream tag = `capture_streams + 1`.
- **Byte-width SD_CTL writes**: Intel PCH requires byte RMW (`updateb`) for
  SD_CTL changes (`access_sdnctl_in_dword=0`); a 32-bit write fails to latch
  RUN.
- **SD_STS is 8-bit** at SD base + 0x03 — reading it as 32 bits spans LPIB and
  returns 0xffffffff.
- **Verb payload width**: SET/GET_AMP verbs carry gain/index/LEFT/RIGHT bits in
  payload[15:8]; truncating the payload to 8 bits silently no-ops amp-unmute
  (symptom: audio works after warm reboot, silent after link reset).
- **Parameter IDs**: `AC_PAR_AUDIO_WIDGET_CAP` = 0x09 (not 0x04),
  `AC_PAR_STREAM_FORMATS` = 0x0b (not 0x0a); conn-list presence is widget-cap
  bit 6.
- **Physical addresses**: pagemap PFNs are sanity-checked < 128 TiB; multi-GB
  ring addresses are normal on large-RAM machines.

## Runtime behavior

- The physical DMA ring stays 64 pages (256 KiB ≈ 1.4 s @48k stereo S16) for
  underrun headroom, but logical occupancy is capped at `SHIM_SERVER_FRAMES`
  (4096 frames ≈ 85 ms) for the A/V sync model — see [av-sync.md](av-sync.md).
- After all clients go dry for 250 ms the mixer stops the HDA stream
  (`stop_immediate`) so DMA doesn't wrap the ring and replay stale audio.
  Stops are sticky; `start()` re-runs SRST+setup before re-asserting RUN.
- `drop_all()` zeroes the ring so restarts begin from silence.
- Output route (speaker vs headphone jack sense) is refreshed ~once per second
  and reported in `audiod-ctl status`.

## Supported rates

Any rate expressible as `base*mult/div` with mult, div ≤ 8:

```
8000 11025 12000 16000 22050 24000 32000 44100 48000 88200 96000 176400 192000
```

The mixer runs at `mix_rate` (default 48000); clients at other rates are
resampled ([architecture.md](architecture.md#dsp-pipeline-server-srcdsprs)).

## Requirements & caveats

- Root (`CAP_SYS_ADMIN`) for BAR mmap and pagemap reads.
- `snd_hda_intel` must be unbound from the slot while audiod owns it;
  `scripts/audiod-hda.sh stop` rebinds it to restore desktop audio.
- Codec support is hardcoded to Realtek ALC269VC. Other codecs fail at probe
  with a clear error rather than programming wrong widgets.
- **Command engine**: CORB/RIRB DMA rings by default (one shared pinned page:
  CORB 256×4 B at offset 0, RIRB 256×8 B at 1 KiB; sizes negotiated 256→16→2
  with readback verification). Responses are polled from RIRBWP — no
  interrupts. Unsolicited responses and foreign-codec entries are logged and
  skipped (jack-detect groundwork). After 3 consecutive response timeouts or a
  hard ring error, audiod degrades to the legacy PIO immediate-command engine
  once, re-issues the timed-out verb through it, and stays there. PIO is also
  selectable up-front with `--cmd-engine=pio`.
- Playback only; no capture streams.
