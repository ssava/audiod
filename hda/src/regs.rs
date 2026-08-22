//! Intel HDA (Azalia) controller register map.
//!
//! Ported from `include/sound/hda_register.h` (Linux). Offsets are
//! relative to BAR0 (0x80-0x9f stream region: + 0x20 per stream index).

pub const GCAP: u32 = 0x00;
pub const VMIN: u32 = 0x02;
pub const VMAJ: u32 = 0x03;
pub const OUTPAY: u32 = 0x04;
pub const INPAY: u32 = 0x06;
pub const GCTL: u32 = 0x08;
pub const WAKEEN: u32 = 0x0c;
pub const STATESTS: u32 = 0x0e;
pub const GSTS: u32 = 0x10;
pub const INTCTL: u32 = 0x20;
pub const INTSTS: u32 = 0x24;
pub const WALLCLK: u32 = 0x30;
pub const SSYNC: u32 = 0x38;
pub const CORBLBASE: u32 = 0x40;
pub const CORBUBASE: u32 = 0x44;
pub const CORBWP: u32 = 0x48;
pub const CORBRP: u32 = 0x4a;
pub const CORBCTL: u32 = 0x4c;
pub const CORBSIZE: u32 = 0x4e;
pub const RIRBLBASE: u32 = 0x50;
pub const RIRBUBASE: u32 = 0x54;
pub const RIRBWP: u32 = 0x58;
pub const RINTCNT: u32 = 0x5a;
pub const RIRBCTL: u32 = 0x5c;
pub const RIRBSTS: u32 = 0x5d;
pub const RIRBSIZE: u32 = 0x5e;
pub const IC: u32 = 0x60;
pub const IR: u32 = 0x64;
pub const IRS: u32 = 0x68;
pub const DPLBASE: u32 = 0x70;
pub const DPUBASE: u32 = 0x74;

pub const GCAP_64OK: u32 = 1 << 0;
pub const GCAP_NSDO: u32 = 3 << 1;
pub const GCAP_ISS: u32 = 15 << 8;
pub const GCAP_OSS: u32 = 15 << 12;

pub const GCTL_RESET: u32 = 1 << 0;
pub const GCTL_FCNTRL: u32 = 1 << 1;
pub const GCTL_UNSOL: u32 = 1 << 8;

/// STATESTS: one present/change bit per codec (bits 0-3), W1C to clear.
pub const STATESTS_INT_MASK: u16 = 0x000f;

pub const CORBRP_RST: u16 = 1 << 15;
pub const CORBCTL_RUN: u8 = 1 << 1;
pub const RIRBWP_RST: u16 = 1 << 15;
pub const RIRBCTL_IRQ_EN: u8 = 1 << 0;
pub const RIRBCTL_DMA_EN: u8 = 1 << 1;
pub const RINTERRUPT_MASK: u8 = 0x05;

/// CORBSTS (0x4d): bit 0 = CORB memory error indication (W1C).
pub const CORBSTS_MEI: u8 = 1 << 0;

/// CORBSIZE/RIRBSIZE (bits 6:4): which ring sizes the controller supports.
pub const SIZE_CAP_2: u8 = 1 << 4;
pub const SIZE_CAP_16: u8 = 1 << 5;
pub const SIZE_CAP_256: u8 = 1 << 6;
/// Programmed size encoding (bits 1:0).
pub const SIZE_2: u8 = 0b00;
pub const SIZE_16: u8 = 0b01;
pub const SIZE_256: u8 = 0b10;

/// RIRB response-ex flags (upper dword of each entry).
pub const RIRB_EX_UNSOL: u32 = 1 << 4;
pub const RIRB_EX_CAD_MASK: u32 = 0xf;

pub const IRS_VALID: u16 = 1 << 1;
pub const IRS_BUSY: u16 = 1 << 0;

pub const DPLBASE_ENABLE: u32 = 0x1;

pub const SD_CTL: u32 = 0x00;
pub const SD_CTL_3B: u32 = 0x02;
pub const SD_STS: u32 = 0x03;
pub const SD_LPIB: u32 = 0x04;
pub const SD_CBL: u32 = 0x08;
pub const SD_LVI: u32 = 0x0c;
pub const SD_FIFOW: u32 = 0x0e;
pub const SD_FIFOSIZE: u32 = 0x10;
pub const SD_FORMAT: u32 = 0x12;
pub const SD_FIFOL: u32 = 0x14;
pub const SD_BDLPL: u32 = 0x18;
pub const SD_BDLPU: u32 = 0x1c;

pub const SD_STREAM_BASE: u32 = 0x80;
pub const SD_STREAM_STRIDE: u32 = 0x20;

pub const STREAM_RESET: u32 = 0x01;
pub const DMA_START: u32 = 0x02;
pub const INT_DESC_ERR: u32 = 0x10;
pub const INT_FIFO_ERR: u32 = 0x08;
pub const INT_COMPLETE: u32 = 0x04;
pub const INT_MASK: u32 = INT_DESC_ERR | INT_FIFO_ERR | INT_COMPLETE;

pub const STRIPE_MASK: u32 = 0x3;
pub const TRAFFIC_PRIO: u32 = 1 << 18;
pub const DIR: u32 = 1 << 19;
pub const STREAM_TAG_MASK: u32 = 0xf << 20;
pub const STREAM_TAG_SHIFT: u32 = 20;

pub const FIFO_READY: u8 = 0x20;

pub const INT_ALL_STREAM: u32 = 0x3fffffff;
pub const INT_CTRL_EN: u32 = 0x40000000;
pub const INT_GLOBAL_EN: u32 = 0x80000000;

pub const MAX_BDL_ENTRIES: usize = 4096 / 16;
pub const MAX_BUF_SIZE: usize = 4 * 1024 * 1024;

/// PCI space
pub const PCIREG_TCSEL: usize = 0x44;
pub const INTEL_SCH_HDA_DEVC: usize = 0x78;
pub const INTEL_SCH_HDA_DEVC_NOSNOOP: u16 = 0x1 << 11;

/// BDL page size / entry layout
pub const PAGE_SIZE: usize = 4096;