//! Bounded SPSC byte ring buffer shared by the shim (client side) and the
//! server's mixer (per-client stream queues).
//!
//! `push` blocks while full (natural backpressure), `pop` blocks while empty
//! and returns 0 once `interrupt()` fires (shutdown signal).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

struct Inner {
    buf: Vec<u8>,
    read_pos: usize,
    write_pos: usize,
}

pub struct AudioRingBuffer {
    cap: usize,
    inner: Mutex<Inner>,
    filled: AtomicUsize,
    interrupted: AtomicBool,
    space_avail: Condvar,
    data_avail: Condvar,
}

impl AudioRingBuffer {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.next_power_of_two();
        AudioRingBuffer {
            cap,
            inner: Mutex::new(Inner {
                buf: vec![0u8; cap],
                read_pos: 0,
                write_pos: 0,
            }),
            filled: AtomicUsize::new(0),
            interrupted: AtomicBool::new(false),
            space_avail: Condvar::new(),
            data_avail: Condvar::new(),
        }
    }

    #[allow(dead_code)]
    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn filled(&self) -> usize {
        self.filled.load(Ordering::Acquire)
    }

    pub fn available(&self) -> usize {
        self.cap - self.filled.load(Ordering::Acquire)
    }

    pub fn push(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        while self.filled.load(Ordering::Acquire) + data.len() > self.cap {
            inner = self.space_avail.wait(inner).unwrap();
        }
        let cap = self.cap;
        let write_pos = inner.write_pos;
        let space_to_end = cap - write_pos;
        let first = data.len().min(space_to_end);
        inner.buf[write_pos..write_pos + first].copy_from_slice(&data[..first]);
        if first < data.len() {
            let remaining = &data[first..];
            inner.buf[..remaining.len()].copy_from_slice(remaining);
            inner.write_pos = remaining.len();
        } else {
            inner.write_pos = write_pos + first;
        }
        if inner.write_pos >= cap {
            inner.write_pos -= cap;
        }
        self.filled.fetch_add(data.len(), Ordering::Release);
        self.data_avail.notify_one();
    }

    /// Interrupt any blocked pop() call, making it return empty.
    /// Used to signal the drain thread to check its stop flag on close.
    pub fn interrupt(&self) {
        self.interrupted.store(true, Ordering::Release);
        self.data_avail.notify_all();
    }

    /// Shared copy-out path for `pop`/`try_pop`; caller holds the lock and
    /// has verified `filled() > 0`.
    fn copy_out(&self, inner: &mut Inner, buf: &mut [u8]) -> usize {
        let cap = self.cap;
        let read_pos = inner.read_pos;
        let to_read = buf.len().min(self.filled.load(Ordering::Acquire));
        let space_to_end = cap - read_pos;
        let first = to_read.min(space_to_end);
        buf[..first].copy_from_slice(&inner.buf[read_pos..read_pos + first]);
        if first < to_read {
            inner.read_pos = to_read - first;
            buf[first..to_read].copy_from_slice(&inner.buf[..to_read - first]);
        } else {
            inner.read_pos = read_pos + first;
        }
        if inner.read_pos >= cap {
            inner.read_pos -= cap;
        }
        self.filled.fetch_sub(to_read, Ordering::Release);
        to_read
    }

    /// Pop up to `buf.len()` bytes from the ring into `buf`.
    /// Returns the number of bytes written, or 0 if interrupted.
    pub fn pop(&self, buf: &mut [u8]) -> usize {
        let mut inner = self.inner.lock().unwrap();
        loop {
            if self.interrupted.swap(false, Ordering::AcqRel) {
                return 0;
            }
            if self.filled.load(Ordering::Acquire) > 0 {
                break;
            }
            inner = self.data_avail.wait(inner).unwrap();
        }
        let n = self.copy_out(&mut inner, buf);
        self.space_avail.notify_one();
        n
    }

    /// Non-blocking [`pop`](Self::pop): returns 0 immediately when the ring
    /// is empty (used by the mixer, which must never stall on a dry client).
    pub fn try_pop(&self, buf: &mut [u8]) -> usize {
        let mut inner = self.inner.lock().unwrap();
        if self.interrupted.swap(false, Ordering::AcqRel) {
            return 0;
        }
        if self.filled.load(Ordering::Acquire) == 0 {
            return 0;
        }
        let n = self.copy_out(&mut inner, buf);
        self.space_avail.notify_one();
        n
    }

    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.read_pos = 0;
        inner.write_pos = 0;
        self.filled.store(0, Ordering::Release);
        self.space_avail.notify_all();
        self.data_avail.notify_all();
    }
}
