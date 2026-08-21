# Security model

## The problem

audiod runs as **root** (the HDA backend needs `CAP_SYS_ADMIN` for BAR mmap +
pagemap). Its socket at `/tmp/audiod.sock` is world-connectable by design:
audio clients (mpv, Firefox) run as the normal desktop user and must be able to
stream PCM, and `audiod-ctl` must be able to send commands. An unauthenticated
socket on a multi-user machine would let any local user play audio into your
output — or worse, drive a root process through malformed input.

## Peer authentication

Every accepted connection — PCM and control alike — is checked with
`SO_PEERCRED` before any protocol bytes are read:

- The connection is allowed if the peer uid is **0** (root) or equals the
  **launcher's uid**.
- The launcher's uid is resolved once and cached in a `OnceLock`
  (`trusted_uid()`): the first `$SUDO_UID` entry if present (i.e. "the user who
  ran sudo to start audiod"), otherwise the server's own effective uid.
- Rejected peers get their connection dropped immediately with a warning log;
  no header parsing happens.

This means: start audiod with `sudo`, and exactly your user account (plus root)
can use it. Other local users cannot connect.

## Attack surface notes

| Surface | Mitigation |
|---------|------------|
| Malformed headers/chunks | Length-prefixed framing; readers tolerate short reads and disconnect cleanly; header read retries transient WouldBlock within a deadline instead of dropping valid reconnects |
| Untrusted PCM data | Treated as audio only; conversion clamps to i16 after gain; no parsing beyond format fields from the authenticated peer |
| Control opcodes | Only reachable post-authentication; volume clamped to 0–100 |
| Socket squatting | Server unlinks/rebinds its known path at startup (`scripts/audiod-hda.sh` also removes stale sockets) |
| DMA buffers | Pinned via mlock'd pages; PFNs sanity-checked (< 128 TiB) |

## What audiod does *not* claim

- It does not sandbox itself (no seccomp/privilege drop after BAR mmap — the
  mmap must persist for the backend's lifetime).
- `/tmp` socket paths are predictable; on hostile multi-user systems prefer a
  `socket_path` under a root-owned directory (configurable, see
  [configuration.md](configuration.md)).
- The shim performs no authentication of the server; it trusts whatever answers
  on the configured socket path (same trust domain as the user running it).

## Reporting

If you find a security issue, please open a GitHub issue or contact the
maintainer privately before public disclosure.
