# A/V sync model

audiod's primary consumer is a video player (mpv), which uses `snd_pcm_delay()`
to decide how audio and video clocks line up. The delay estimate must be
smooth, truthful, and consistent with what the server actually buffers —
otherwise the player drops frames chasing a phantom clock.

## The virtual device

The shim advertises a device with:

- buffer size = `SHIM_BUFFER_FRAMES` = **8192** frames
- invariant: `delay + avail == 8192` at all times (like real snd_pcm)
- baseline server occupancy = `SHIM_SERVER_FRAMES` = **4096** frames

These constants live in `common/src/lib.rs` as a single source of truth shared
by shim and server; they must never be changed on one side only.

## Time-based playhead

`delay` is **not** derived from ring fill level (`ring.filled()`), which moves
in drain-chunk quanta and caused 1–4 dropped frames per second. Instead:

```
delay = clamp(pushed_frames − rate · elapsed_since_play_start, 0, BUFFER_SIZE)
        + SERVER_FRAMES
```

- `play_start` anchors on the first `writei` after prepare/drop.
- The playhead advances continuously with wall-clock time, so `delay` and
  `avail_update` move smoothly instead of jumping in ~21 ms steps.
- `SERVER_FRAMES` accounts for data buffered server-side (the mixer's HDA
  occupancy cap is set to exactly this value).

Measured results (45 s mpv run): `Dropped: 0`, max |A−V| = 0.033 s (one 30 fps
video frame — mpv's display granularity), mean |A−V| ≈ 0.0007. Seeks recover
to A−V = 0.000 within ~1 dropped frame.

## Server-side pacing

The mixer preserves the same invariant from the other side:

```
writable = clamp(SHIM_SERVER_FRAMES − backend.delay_frames(), 0, CHUNK_FRAMES)
```

with `CHUNK_FRAMES = 1024` (~21 ms @48 kHz). Each tick it writes at most
`writable` frames into the HDA DMA ring, so logical HDA occupancy never exceeds
4096 frames even though the physical ring is 256 KiB. This keeps the physical
ring available as underrun headroom while making the shim's +4096 assumption
true.

## Buffering chain

```
app → shim ring (1 MiB) → socket buf (≤32 KiB) → client ring (64 KiB)
    → HDA logical occupancy (≤4096 fr) → HDA physical ring (256 KiB)
```

- Socket `SO_SNDBUF` capped at 16384 (kernel doubles to 32768): bounds ring
  oscillation to ≤2 drain chunks (~42 ms).
- Drain chunks of 1024 B keep oscillation fine-grained.
- Per-client rings absorb scheduling jitter only; steady-state pacing comes
  from real-time `delay_frames()`.

## Idle / pause behavior

- `snd_pcm_drop`/pause sends FLUSH; the server stops HDA DMA immediately for
  that stream's contribution (per-stream scope in multi-client mode). Up to
  ~85 ms (4096 frames) already in DMA still plays out — the same tradeoff
  PulseAudio makes.
- After the last client disconnects, the mixer arms a dry timer itself and
  stops the stream once; stops are sticky so an idle stream isn't re-reset
  every tick. `STATE.last_delay_frames` zeroes on stop so STATUS reads
  truthfully.

## Why not other designs?

- **Ring-fill delay**: quantized by drain chunks → periodic small drops.
- **In-flight frame counting**: same quantization problem, worse under seeks.
- **Bigger server buffer than advertised**: mpv under-estimates delay by the
  difference and drops video to "catch up" (the original 55 %-speed bug class);
  stale DMA also kept playing ~1.4 s after seeks before the occupancy cap.
