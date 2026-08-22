//! Physical-address resolution for userspace DMA buffers.
//!
//! The HDA controller BDL must reference real physical addresses. We pin a
//! page-aligned anonymous mapping with `mlock`, then translate each virtual
//! page to its physical page via `/proc/self/pagemap`. Reading the PFN
//! requires CAP_SYS_ADMIN, so this crate must run as root.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

pub const PAGE_SIZE: usize = 4096;

/// Translate virtual address `vaddr` to its physical address using
/// `/proc/self/pagemap`. Requires CAP_SYS_ADMIN.
fn vaddr_to_phys(vaddr: u64) -> io::Result<u64> {
    const PFN_PRESENT: u64 = 1 << 63;
    const PFN_PFN_MASK: u64 = (1 << 55) - 1;

    let mut f = File::open("/proc/self/pagemap")?;
    let entry_off = (vaddr >> 12) * 8;
    f.seek(SeekFrom::Start(entry_off))?;
    let mut ent = [0u8; 8];
    f.read_exact(&mut ent)?;
    let entry = u64::from_le_bytes(ent);
    if entry & PFN_PRESENT == 0 {
        return Err(io::Error::other("page not resident"));
    }
    Ok((entry & PFN_PFN_MASK) << 12)
}

/// A page-aligned, mlocked buffer visible both to software and to the HDA
/// controller (via `to_physical(off)`).
pub struct DmaBuffer {
    ptr: *mut u8,
    len: usize,
    phys: Vec<u64>, // physical address of each 4 KiB page
}

impl DmaBuffer {
    /// Allocate `size` bytes (rounded up to whole pages), pin them in RAM,
    /// and resolve the physical address of every page.
    pub fn new(size: usize) -> io::Result<Self> {
        let len = align_up(size, PAGE_SIZE);
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if addr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr = addr as *mut u8;

        // Touch every page so it becomes resident before mlock.
        unsafe { std::ptr::write_bytes(ptr, 0, len) };
        if unsafe { libc::mlock(ptr as *const _, len) } != 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::munmap(addr, len) };
            return Err(e);
        }

        let pages = len / PAGE_SIZE;
        let mut phys = Vec::with_capacity(pages);
        for i in 0..pages {
            match vaddr_to_phys(ptr as u64 + (i * PAGE_SIZE) as u64) {
                Ok(p) => phys.push(p),
                Err(e) => {
                    unsafe { libc::munlock(ptr as *const _, len) };
                    unsafe { libc::munmap(addr, len) };
                    return Err(e);
                }
            }
        }

        // Sanity: a pinned user page must always land inside addressable RAM.
        // Anything >= 128 TiB indicates a PFN-decode regression (Linux x86-64
        // physical address space is far below this).
        const MAX_PHYS: u64 = 128 << 40;
        if let Some(&bad) = phys.iter().find(|&&p| p >= MAX_PHYS) {
            unsafe { libc::munlock(ptr as *const _, len) };
            unsafe { libc::munmap(addr, len) };
            return Err(io::Error::other(format!(
                "decoded physical address 0x{bad:x} >= 128 TiB — pagemap decode error"
            )));
        }
        log::debug!(
            "DmaBuffer: {} pages phys[0]=0x{:x} phys[last]=0x{:x}",
            pages,
            *phys.first().unwrap(),
            *phys.last().unwrap()
        );

        Ok(DmaBuffer { ptr, len, phys })
    }

    pub const fn size(&self) -> usize {
        self.len
    }

    /// Physical address of the byte at offset `off`. Panics if
    /// `off` is past the end of the buffer.
    pub fn toaddr(&self, off: usize) -> u64 {
        assert!(
            off < self.len,
            "toaddr out of bounds: off={off} len={}",
            self.len
        );
        let page = off / PAGE_SIZE;
        self.phys[page] + (off % PAGE_SIZE) as u64
    }

    /// Write `data` at byte offset `off`. Panics if the copy would
    /// extend past the end of the buffer.
    pub fn write(&self, off: usize, data: &[u8]) {
        let end = off.checked_add(data.len()).expect("write offset overflow");
        assert!(
            end <= self.len,
            "write out of bounds: off={off} len={} end={end}",
            data.len(),
        );
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr.add(off), data.len());
        }
    }

    /// Read bytes at byte offset `off` into `out`. Panics if the copy
    /// would extend past the end of the buffer.
    pub fn read(&self, off: usize, out: &mut [u8]) {
        let end = off.checked_add(out.len()).expect("read offset overflow");
        assert!(
            end <= self.len,
            "read out of bounds: off={off} len={} end={end}",
            self.len,
        );
        unsafe {
            std::ptr::copy_nonoverlapping(self.ptr.add(off), out.as_mut_ptr(), out.len());
        }
    }

    /// Zero the entire buffer.
    pub fn zero(&self) {
        unsafe { std::ptr::write_bytes(self.ptr, 0, self.len) };
    }
}

impl Drop for DmaBuffer {
    fn drop(&mut self) {
        unsafe {
            libc::munlock(self.ptr as *const _, self.len);
            libc::munmap(self.ptr as *mut _, self.len);
        }
    }
}

fn align_up(v: usize, a: usize) -> usize {
    (v + a - 1) & !(a - 1)
}