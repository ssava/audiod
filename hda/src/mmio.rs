//! Volatile MMIO access to the HDA controller's PCI BAR.

use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;

/// A read/write mapping of the controller's BAR0 memory region.
pub struct Mmio {
    ptr: *mut u8,
    len: usize,
}

impl Mmio {
    /// Map sysfs `resource0` of the device at PCI slot `dev` (e.g.
    /// "0000:00:1b.0"). Requires CAP_SYS_ADMIN (root).
    pub fn map(dev: &str) -> io::Result<Self> {
        let path = format!("/sys/bus/pci/devices/{}/resource0", dev);
        // Must open O_RDWR: mmap(PROT_WRITE, MAP_SHARED) requires FMODE_WRITE.
        let f = File::options().read(true).write(true).open(&path)?;
        let len = 16 * 1024; // BAR0 size fixed for HDA (16k covered here; whole BAR is a small window)
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                f.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Mmio { ptr: ptr as *mut u8, len })
    }

    /// Validate an access of `size` bytes at `off` lies fully within the
    /// mapping and is naturally aligned for `width`. Panics on programmer
    /// error — the safer alternative to undefined behavior on a wild offset.
    #[inline]
    fn check(&self, off: usize, size: usize, width: usize) {
        assert!(
            off.is_multiple_of(width),
            "MMIO access misaligned: off=0x{off:x} width={width}"
        );
        assert!(
            off.checked_add(size).is_some_and(|end| end <= self.len),
            "MMIO access out of bounds: off=0x{off:x} +{size} > len 0x{:x}",
            self.len
        );
    }

    #[inline]
    pub fn read_u8(&self, off: usize) -> u8 {
        self.check(off, 1, 1);
        unsafe { std::ptr::read_volatile(self.ptr.add(off) as *const u8) }
    }

    #[inline]
    pub fn read_u16(&self, off: usize) -> u16 {
        self.check(off, 2, 2);
        unsafe { std::ptr::read_volatile(self.ptr.add(off) as *const u16) }
    }

    #[inline]
    pub fn read_u32(&self, off: usize) -> u32 {
        self.check(off, 4, 4);
        unsafe { std::ptr::read_volatile(self.ptr.add(off) as *const u32) }
    }

    #[inline]
    pub fn write_u8(&self, off: usize, v: u8) {
        self.check(off, 1, 1);
        unsafe { std::ptr::write_volatile(self.ptr.add(off), v) };
    }

    #[inline]
    pub fn write_u16(&self, off: usize, v: u16) {
        self.check(off, 2, 2);
        unsafe { std::ptr::write_volatile(self.ptr.add(off) as *mut u16, v) };
    }

    #[inline]
    pub fn write_u32(&self, off: usize, v: u32) {
        self.check(off, 4, 4);
        unsafe { std::ptr::write_volatile(self.ptr.add(off) as *mut u32, v) };
    }
}

impl Drop for Mmio {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr as *mut _, self.len);
        }
    }
}