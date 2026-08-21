//! Buffer Descriptor List (BDL) construction.
//!
//! Each BDLE is 16 bytes: `{ addr_lo:u32, addr_hi:u32, size:u32, ioc:u32 }`.
//! The hardware reads the list sequentially and wraps at the entry indicated
//! by the SD_LVI field. A single descriptor's data may not cross a 4 KiB
//! boundary, so we emit one entry per page.
//!
//! Entries are serialized to little-endian bytes and written straight into the
//! pinned `DmaBuffer` (no unsafe pointer casts are needed).

use crate::pagemap::{DmaBuffer, PAGE_SIZE};

/// Maximum entries the controller will DMA through before wrapping drivers
/// usually allocate up to `MAX_BDL_ENTRIES` (reference kernel value).
pub const MAX_BDL_ENTRIES: usize = crate::regs::MAX_BDL_ENTRIES; // 256

const ENTRY_SIZE: usize = 16;

/// Fill `bdl` with the first `count` pages of `ring`, one entry per page.
/// Returns the number of entries written (= min(count, bdl capacity, ring
/// pages)).
///
/// `ioc` is set on the last entry so an interrupt is raised at that point;
/// callers polling for completion may leave it zero.
pub fn fill(bdl: &DmaBuffer, ring: &DmaBuffer, count: usize, ioc_last: bool) -> usize {
    let entries = count.min(bdl.size() / ENTRY_SIZE).min(ring.size() / PAGE_SIZE);

    for i in 0..entries {
        let pa = ring.toaddr(i * PAGE_SIZE);
        let ioc = (ioc_last && i + 1 == entries) as u32;
        let entry = make_entry(pa, PAGE_SIZE as u32, ioc);
        bdl.write(i * ENTRY_SIZE, &entry);
    }
    entries
}

/// Serialize one 16-byte BDLE (little-endian).
pub fn make_entry(addr: u64, size: u32, ioc: u32) -> [u8; ENTRY_SIZE] {
    let mut e = [0u8; ENTRY_SIZE];
    e[0..4].copy_from_slice(&(addr as u32).to_le_bytes());
    e[4..8].copy_from_slice(&((addr >> 32) as u32).to_le_bytes());
    e[8..12].copy_from_slice(&size.to_le_bytes());
    e[12..16].copy_from_slice(&ioc.to_le_bytes());
    e
}