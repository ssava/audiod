//! PCI config-space helpers for the HDA device.
//!
//! The controller exposes its PCI config via sysfs (`.../config`). The kernel
//! driver performs two tweaks in `azx_init_pci` that we replicate:
//!  - clear bits 0-2 of TCSEL (offset 0x44) — fixes speaker static
//!  - clear the SCH/PCH NOSNOOP bit (DEVC offset 0x78, bit 11) so snooping is
//!    enabled, required for coherent DMA to the ring/BDL

use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Read};

const TCSEL_OFF: u64 = 0x44;
const DEVC_OFF: u64 = 0x78;
const DEVC_NOSNOOP_BIT: u16 = 0x0800; // bit 11

// PCI config command register (offset 0x04).
const PCI_CMD_OFF: u64 = 0x04;
const PCI_CMD_MEM_SPACE: u16 = 0x0002; // bit 1
const PCI_CMD_BUS_MASTER: u16 = 0x0004; // bit 2

// PCI header: vendor ID (offset 0x00, 16-bit), device ID (offset 0x02).
const PCI_VENDOR_OFF: u64 = 0x00;
const PCI_DEVICE_OFF: u64 = 0x02;

/// Read the PCI vendor (0x00) and device (0x02) IDs of the device. Used as an
/// identity check before we start writing controller registers.
pub fn vendor_device(dev: &str) -> io::Result<(u16, u16)> {
    let mut f = open_config(dev)?;
    Ok((read_u16(&mut f, PCI_VENDOR_OFF)?, read_u16(&mut f, PCI_DEVICE_OFF)?))
}

fn open_config(dev: &str) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("/sys/bus/pci/devices/{}/config", dev))
}

fn read_u16(f: &mut File, off: u64) -> io::Result<u16> {
    let mut buf = [0u8; 2];
    f.seek(SeekFrom::Start(off))?;
    f.read_exact(&mut buf)?;
    Ok(u16::from_le_bytes(buf))
}

fn write_u16(f: &mut File, off: u64, v: u16) -> io::Result<()> {
    use std::io::Write;
    f.seek(SeekFrom::Start(off))?;
    f.write_all(&v.to_le_bytes())
}

fn read_u8(f: &mut File, off: u64) -> io::Result<u8> {
    let mut buf = [0u8; 1];
    f.seek(SeekFrom::Start(off))?;
    f.read_exact(&mut buf)?;
    Ok(buf[0])
}

fn write_u8(f: &mut File, off: u64, v: u8) -> io::Result<()> {
    use std::io::Write;
    f.seek(SeekFrom::Start(off))?;
    f.write_all(&[v])
}

/// Apply the kernel-style `azx_init_pci` tweaks for Intel HDA.
pub fn set_snoop(dev: &str) -> io::Result<()> {
    let mut f = open_config(dev)?;

    // TCSEL[2:0] = 0.
    let t = read_u8(&mut f, TCSEL_OFF)?;
    write_u8(&mut f, TCSEL_OFF, t & !0x07)?;

    // Snoop on: clear NOSNOOP bit.
    let s = read_u16(&mut f, DEVC_OFF)?;
    if s & DEVC_NOSNOOP_BIT != 0 {
        write_u16(&mut f, DEVC_OFF, s & !DEVC_NOSNOOP_BIT)?;
    }

    Ok(())
}

/// Set the PCI Command register bits required for DMA: memory space (bit 1)
/// plus bus master (bit 2). Mirrors `pci_set_master()`, which the kernel
/// calls in `azx_probe_continue`. Without bus mastering the controller cannot
/// fetch the BDL or the playback ring, so LPIB never advances.
pub fn set_master(dev: &str) -> io::Result<bool> {
    let mut f = open_config(dev)?;
    let cmd = read_u16(&mut f, PCI_CMD_OFF)?;
    let enable = PCI_CMD_MEM_SPACE | PCI_CMD_BUS_MASTER;
    if cmd & enable == enable {
        return Ok(false); // already enabled
    }
    write_u16(&mut f, PCI_CMD_OFF, cmd | enable)?;
    Ok(true)
}