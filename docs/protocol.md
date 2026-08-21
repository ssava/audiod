# Wire protocol

One Unix socket (`/tmp/audiod.sock` by default) carries both PCM streams and
control commands. Every connection is authenticated via `SO_PEERCRED` — see
[security.md](security.md).

## Connection lifecycle

1. Client connects to the socket.
2. Client sends the **12-byte configuration header** (below).
3. The server replies with nothing; the stream is live. For PCM connections the
   server registers a mixer slot; for control connections (rate = 0) it enters
   command mode.
4. Data flows as **length-prefixed chunks** until EOF or disconnect.

## Configuration header (12 bytes, little-endian)

| Offset | Size | Field | Notes |
|--------|------|-------|-------|
| 0 | 4 | `rate: u32` | Sample rate in Hz. `0` marks a **control** connection. |
| 4 | 2 | `channels: u16` | 1–8 for PCM |
| 6 | 2 | `format: u16` | `2` = S16_LE, `14` = FLOAT_LE (ALSA `snd_pcm_format_t` values) |
| 8 | 4 | reserved | Must be 0 |

The server folds any client format into the mix (S16LE stereo at
`mix_rate`), so no negotiation round-trip is needed.

## PCM data frames

Each chunk is:

```
[u32 LE length][length bytes of interleaved PCM]
```

- Length is the byte count of the payload (not frames).
- A chunk may contain a partial frame count only at stream end; readers fold
  whole frames.
- There is no per-chunk acknowledgement; backpressure is applied by the socket
  and the server-side bounded rings.

## Control opcodes

A **zero-length data frame** (`length == 0`) is a control opcode instead of
PCM. One opcode byte follows the length prefix:

| Opcode | Byte | Payload after opcode | Effect |
|--------|------|----------------------|--------|
| Mute | `'M'` (0x4D) | none | Master mute on |
| Unmute | `'U'` (0x55) | none | Master mute off |
| Volume | `'V'` (0x56) | `f32` percent (0–100) | Set master volume |
| Status | `'S'` (0x53) | none | Reply: `[u32 len][JSON]` |
| Flush | `'F'` (0x46) | none | Clear the sending client's ring (per-stream scope) |

STATUS reply JSON shape:

```json
{
  "muted": false,
  "volume": 1.000,
  "active": true,
  "clients": 1,
  "total_frames": 2400000,
  "delay_frames": 2048,
  "output": "speaker"
}
```

- `active` — boolean: true while at least one client is streaming
- `clients` — number of live mixer streams
- `delay_frames` — last observed HDA ring delay in frames (0 when stopped)
- `output` — `"speaker"`, `"headphone"`, or `"unknown"` (refreshed ~1 Hz)

Control opcodes can also travel inline on a PCM connection as zero-length
frames — this is how shim-side `snd_pcm_drop`/pause sends FLUSH without a
second connection.

## Shim-side framing notes

- The drain thread writes chunks of ≤ `DRAIN_CHUNK` (1024 B) payloads so the
  socket buffer oscillates finely.
- `SO_SNDBUF` is capped at 16384 (kernel doubles it to 32768) to bound
  ring oscillation to ~2 drain chunks (~42 ms).
- Writes use `send(MSG_NOSIGNAL)` so a dead peer yields `EPIPE`, never SIGPIPE.
- On reconnect the drain thread re-sends the stashed 12-byte header before
  resuming PCM.

## Example: minimal PCM client

```c
int fd = socket(AF_UNIX, SOCK_STREAM, 0);
connect(fd, ...);                       /* /tmp/audiod.sock */

uint8_t hdr[12] = {0};
uint32_t rate = 48000;
memcpy(hdr + 0, &rate, 4);              /* u32 LE */
uint16_t ch = 2, fmt = 2;               /* stereo, S16_LE */
memcpy(hdr + 4, &ch, 2);
memcpy(hdr + 6, &fmt, 2);
write(fd, hdr, 12);

uint32_t n = payload_len;
write(fd, &n, 4);                       /* length prefix */
write(fd, payload, n);                  /* interleaved S16LE stereo */
```
