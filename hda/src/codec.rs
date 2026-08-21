//! Codec (Realtek ALC269VC) playback path setup.
//!
//! Nodes discovered live from `/proc/asound/card0/codec#0` on the target
//! machine and cross-checked against the ALC269 parser:
//!
//!   Node 0x01  Audio Function Group (AFG)
//!   0x02      DAC for Speaker    (stream capable, output amp)
//!   0x03      DAC for Headphone
//!   0x0c      Mixer/Src before speaker pin (connection: 0x02/0x0b)
//!   0x0d      Mixer/Src before headphone pin (connection: 0x0c/0x0d)
//!   0x14      Pin Speaker (OUT + EAPD)
//!   0x15      Pin Headphone (OUT + HP + EAPD)
//!
//! Path used by our driver:
//!   DAC0x02 → Mux0x0c → Pin0x14 (speaker)
//!   DAC0x03 → Mux0x0d → Pin0x15 (headphone)
//! Stream: programmer binds the DAC to the controller's stream tag, sets
//! format, unmutes the output amp, and enables the pin.

use crate::controller::Controller;
use std::io;

/// Vendor/subsys/rev live at the codec's root node (mirrors kernel
/// `hdac_device.c` `snd_hdac_device_init`).
pub const NID_ROOT: u32 = 0x00;
pub const NID_AFG: u32 = 0x01;

/// Param ids (hda_verbs.h)
const AC_PAR_VENDOR_ID: u32 = 0x00;
const AC_PAR_SUBSYSTEM_ID: u32 = 0x01;
const AC_PAR_REV_ID: u32 = 0x02;
const AC_PAR_STREAM_FORMATS: u32 = 0x0b; // non-zero => widget is stream (converter) capable
const AC_PAR_PIN_CAP: u32 = 0x0c;
const AC_PAR_CONNLIST_LEN: u32 = 0x0e; // low 7 bits = number of connections
const AC_PAR_AUDIO_WIDGET_CAP: u32 = 0x09;

// Widget types (AC_WID_*, from hdaudio.h). Distinguishing them lets us
// validate the hardcoded ALC269 node map instead of programming a wrong
// widget (the failure mode on non-ALC269-class codecs).
const AC_WID_AUD_OUT: u32 = 0x0;
const AC_WID_AUD_MIX: u32 = 0x2;
const AC_WID_AUD_SEL: u32 = 0x3;
const AC_WID_PIN: u32 = 0x4;
pub const NID_DAC_SPK: u32 = 0x02;
pub const NID_DAC_HP: u32 = 0x03;
pub const NID_MUX_SPK: u32 = 0x0c;
pub const NID_MUX_HP: u32 = 0x0d;
pub const NID_PIN_SPK: u32 = 0x14;
pub const NID_PIN_HP: u32 = 0x15;

// Verb ids (hda_verbs.h)
const VERB_PARAM: u32 = 0xf00;
const VERB_GET_SUBSYSTEM_ID: u32 = 0xf20;
const VERB_SET_AMP_GAIN_MUTE: u32 = 0x300;
const VERB_GET_AMP_GAIN_MUTE: u32 = 0xb00;
const VERB_GET_PIN_SENSE: u32 = 0x709;
const VERB_GET_CONNECT_SEL: u32 = 0xf01;
const VERB_SET_CONNECT_SEL: u32 = 0x701;
const VERB_GET_POWER_STATE: u32 = 0xf05;
const VERB_SET_POWER_STATE: u32 = 0x705;
const VERB_GET_CHANNEL_STREAMID: u32 = 0xf06;
const VERB_SET_CHANNEL_STREAMID: u32 = 0x706;
const VERB_GET_PIN_WIDGET_CONTROL: u32 = 0xf07;
const VERB_SET_PIN_WIDGET_CONTROL: u32 = 0x707;
const VERB_GET_STREAM_FORMAT: u32 = 0xa00;
const VERB_SET_STREAM_FORMAT: u32 = 0x200;
const VERB_GET_EAPD_BTLENABLE: u32 = 0xf0c;
const VERB_SET_EAPD_BTLENABLE: u32 = 0x70c;
const VERB_GET_CONNECT_LIST: u32 = 0xf02;

/// Pin sense bit 31: jack present / device detected.
const AC_PINSENSE_PRESENCE: u32 = 1 << 31;

const AC_PWRST_D0: u32 = 0x00;

const AC_AMP_MUTE: u32 = 1 << 7;
const AC_AMP_INDEX_SHIFT: u32 = 8;
const AC_AMP_RIGHT: u32 = 1 << 12;
const AC_AMP_LEFT: u32 = 1 << 13;
const AC_AMP_OUTPUT: u32 = 1 << 15;
// GET_AMP_GAIN_MUTE payload differs from SET: index sits in bits 0-3.
const AC_AMP_GET_LEFT: u32 = 1 << 13;
const AC_AMP_GET_OUTPUT: u32 = 1 << 15;
const AC_EAPD: u32 = 1 << 1;
const AC_PIN_OUT: u32 = 1 << 6;
const AC_PIN_HP: u32 = 1 << 7;

/// Default unmuted output amp gain for ALC269 (0x44 ≈ 0dB at ofs 0x57).
const DEFAULT_GAIN: u32 = 0x44;

pub struct Codec {
    pub vendor_id: u32,
    pub subsystem_id: u32,
}

impl Codec {
    /// Scan every codec present in `codec_mask` and pick the Realtek analog
    /// codec (vendor 0x10ec) which owns the speaker/headphone DACs. If none,
    /// fall back to the first codec that answers a valid VENDOR_ID.
    pub fn probe(c: &mut Controller) -> io::Result<Codec> {
        let mut fallback: Option<(u32, Codec)> = None;
        let mut realtek: Option<(u32, Codec)> = None;
        for cad in 0..4 {
            if c.codec_mask & (1 << cad) == 0 {
                continue;
            }
            match c.cmd(cad, NID_ROOT, VERB_PARAM, AC_PAR_VENDOR_ID) {
                Ok(vendor_id) if vendor_id != 0 && vendor_id != 0xffffffff => {
                    let subsystem_id = c.cmd(cad, NID_ROOT, VERB_PARAM, AC_PAR_SUBSYSTEM_ID).unwrap_or(0);
                    let revision_id = c.cmd(cad, NID_ROOT, VERB_PARAM, AC_PAR_REV_ID).unwrap_or(0);
                    log::info!(
                        "codec cad={} vendor=0x{:08x} subsys=0x{:08x} rev=0x{:08x}",
                        cad,
                        vendor_id,
                        subsystem_id,
                        revision_id
                    );
                    // Subsystem id on ALC269VC lives at the AFG node via
                    // AC_VERB_GET_SUBSYSTEM_ID when the root node returns 0.
                    let subsystem_id = if subsystem_id == 0 {
                        c.cmd(cad, NID_AFG, VERB_GET_SUBSYSTEM_ID, 0).unwrap_or(0)
                    } else {
                        subsystem_id
                    };
                    let codec = Codec { vendor_id, subsystem_id };
                    let hw = vendor_id >> 16;
                    if hw == 0x10ec && realtek.is_none() {
                        log::info!("selected cad={} (Realtek analog)", cad);
                        realtek = Some((cad, codec));
                    } else if fallback.is_none() {
                        fallback = Some((cad, codec));
                    }
                }
                Err(e) => log::debug!("codec cad={} vendor read: {e}", cad),
                _ => {}
            }
        }
        let (cad, codec) = realtek.or(fallback).ok_or_else(|| io::Error::other("no codec"))?;
        c.cad = cad;
        log::info!(
            "using codec cad={}: {:04x}/{:04x}",
            cad,
            codec.vendor_id >> 16,
            codec.vendor_id & 0xffff
        );
        Ok(codec)
    }

    /// Program the whole playback path: DAC format/amp + pin EAPD + amp.
    /// The DAC must already be bound to the controller stream tag.
    ///
    /// First *validates* that every node in the hardcoded map is the widget
    /// type we expect (DACs = output converters, muxes = mixer/selector, pins
    /// = pin complex). On a different codec where these NIDs mean something
    /// else we fail loudly instead of programming wrong widgets with wrong
    /// verbs.
    pub fn init_playback(c: &mut Controller, fmt: u16, stream_tag: u32) -> io::Result<()> {
        // Wake AFG + all involved nodes to D0.
        set_power(c, NID_AFG, AC_PWRST_D0)?;

        Self::check_widget(c, NID_DAC_SPK, &[AC_WID_AUD_OUT], "DAC_SPK")?;
        Self::check_widget(c, NID_DAC_HP, &[AC_WID_AUD_OUT], "DAC_HP")?;
        Self::check_widget(c, NID_MUX_SPK, &[AC_WID_AUD_MIX, AC_WID_AUD_SEL], "MUX_SPK")?;
        Self::check_widget(c, NID_MUX_HP, &[AC_WID_AUD_MIX, AC_WID_AUD_SEL], "MUX_HP")?;
        Self::check_widget(c, NID_PIN_SPK, &[AC_WID_PIN], "PIN_SPK")?;
        Self::check_widget(c, NID_PIN_HP, &[AC_WID_PIN], "PIN_HP")?;

        // Speaker DAC.
        set_power(c, NID_DAC_SPK, AC_PWRST_D0)?;
        c.cmd(c.cad, NID_DAC_SPK, VERB_SET_CHANNEL_STREAMID, stream_tag << 4)?;
        c.cmd(c.cad, NID_DAC_SPK, VERB_SET_STREAM_FORMAT, fmt as u32)?;
        set_output_amp(c, NID_DAC_SPK, DEFAULT_GAIN, false)?;

        // Mux to DAC.
        set_power(c, NID_MUX_SPK, AC_PWRST_D0)?;
        c.cmd(c.cad, NID_MUX_SPK, VERB_SET_CONNECT_SEL, 0x00)?; // conn 0 = DAC 0x02
        set_input_amp(c, NID_MUX_SPK, 0, 0, false)?;

        // Speaker pin 0x14: enable output + EAPD + clear output amp mute.
        set_power(c, NID_PIN_SPK, AC_PWRST_D0)?;
        c.cmd(c.cad, NID_PIN_SPK, VERB_SET_PIN_WIDGET_CONTROL, AC_PIN_OUT)?;
        c.cmd(c.cad, NID_PIN_SPK, VERB_SET_EAPD_BTLENABLE, AC_EAPD)?;
        set_output_amp(c, NID_PIN_SPK, 0, false)?;

        // Headphone DAC + pin.
        set_power(c, NID_DAC_HP, AC_PWRST_D0)?;
        c.cmd(c.cad, NID_DAC_HP, VERB_SET_CHANNEL_STREAMID, stream_tag << 4)?;
        c.cmd(c.cad, NID_DAC_HP, VERB_SET_STREAM_FORMAT, fmt as u32)?;
        set_output_amp(c, NID_DAC_HP, DEFAULT_GAIN, false)?;
        set_power(c, NID_MUX_HP, AC_PWRST_D0)?;
        c.cmd(c.cad, NID_MUX_HP, VERB_SET_CONNECT_SEL, 0x01)?; // conn 1 = DAC 0x03
        set_input_amp(c, NID_MUX_HP, 1, 0, false)?;
        set_power(c, NID_PIN_HP, AC_PWRST_D0)?;
        c.cmd(c.cad, NID_PIN_HP, VERB_SET_PIN_WIDGET_CONTROL, AC_PIN_OUT | AC_PIN_HP)?;
        c.cmd(c.cad, NID_PIN_HP, VERB_SET_EAPD_BTLENABLE, AC_EAPD)?;
        set_output_amp(c, NID_PIN_HP, 0, false)?;
        Ok(())
    }

    /// Verify nid is one of the expected widget types (`AC_PAR_AUDIO_WIDGET_CAP`
    /// type field, bits 23:20). Used to fence the hardcoded ALC269 node map.
    fn check_widget(
        c: &mut Controller,
        nid: u32,
        expected: &[u32],
        label: &str,
    ) -> io::Result<()> {
        let caps = c.cmd_quiet(c.cad, nid, VERB_PARAM, AC_PAR_AUDIO_WIDGET_CAP)?;
        let t = (caps >> 20) & 0xf;
        if !expected.contains(&t) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{label} nid=0x{nid:x} is widget type 0x{t:x}, expected {expected:?} — \
                     codec layout is not the expected ALC269-class topology"
                ),
            ));
        }
        Ok(())
    }

    /// Walk the codec widget graph and log the topology: widget type, stream
    /// capability, amps, connection list length/selection, and pin detail.
    /// Diagnostic aid for porting playback init to other codecs.
    pub fn dump_topology(c: &mut Controller) -> io::Result<()> {
        const MAX_NID: u32 = 0x20;
        let cad = c.cad;
        log::info!("codec topology dump (cad={cad}):");
        for nid in 1..=MAX_NID {
            let caps = match c.cmd_quiet(cad, nid, VERB_PARAM, AC_PAR_AUDIO_WIDGET_CAP) {
                Ok(v) if v != 0 && v != 0xffffffff => v,
                _ => continue,
            };
            let wtype = caps >> 20 & 0xf;
            let tname = match wtype {
                0 => "OUT", 1 => "IN", 2 => "MIX", 3 => "SEL", 4 => "PIN",
                5 => "PWR", 6 => "VOL", 7 => "BEEP", 0xe => "VENDOR", _ => "??",
            };
            let stereo = caps & 1 != 0;
            let in_amp = caps & (1 << 1) != 0;
            let out_amp = caps & (1 << 2) != 0;
            let has_conn = caps & (1 << 6) != 0;
            let st = c.cmd_quiet(cad, nid, VERB_PARAM, AC_PAR_STREAM_FORMATS).unwrap_or(0);
            log::info!(
                "  nid=0x{nid:02x} [{tname}] stereo={stereo} inamp={in_amp} outamp={out_amp} \
                 conn={has_conn} stream={} caps=0x{caps:08x}",
                st != 0
            );
            if wtype == AC_WID_PIN {
                let pincap = c.cmd_quiet(cad, nid, VERB_PARAM, AC_PAR_PIN_CAP).unwrap_or(0);
                let pin = c.cmd_quiet(cad, nid, VERB_GET_PIN_WIDGET_CONTROL, 0).unwrap_or(0);
                let sense = c.cmd_quiet(cad, nid, VERB_GET_PIN_SENSE, 0).unwrap_or(0);
                log::info!(
                    "      pincap=0x{pincap:08x} pinctl=0x{pin:x} sense=0x{sense:08x} \
                     presence={}",
                    sense & AC_PINSENSE_PRESENCE != 0
                );
            }
            if has_conn {
                let len = (c.cmd_quiet(cad, nid, VERB_PARAM, AC_PAR_CONNLIST_LEN).unwrap_or(0) & 0x7f) as usize;
                let sel = c.cmd_quiet(cad, nid, VERB_GET_CONNECT_SEL, 0).unwrap_or(0xffff);
                // The first 8 connections (4 bits each) come back in one read.
                let entries = c.cmd_quiet(cad, nid, VERB_GET_CONNECT_LIST, 0).unwrap_or(0);
                let first: Vec<u32> = (0..8.min(len))
                    .map(|i| (entries >> (i * 4)) & 0xf)
                    .collect();
                log::info!("      conn: len={len} sel=0x{sel:x} first={first:?}");
            }
        }
        Ok(())
    }

    /// Mute/unmute both output DACs by setting the output amp mute bit.
    pub fn set_mute(c: &mut Controller, mute: bool) -> io::Result<()> {
        set_output_amp(c, NID_DAC_SPK, DEFAULT_GAIN, mute)?;
        set_output_amp(c, NID_DAC_HP, DEFAULT_GAIN, mute)
    }

    /// Dump live codec register state for the playback path (readback verbs).
    /// Lets us compare our init against the kernel's working state after a
    /// link reset.
    pub fn dump_state(c: &mut Controller) -> io::Result<()> {
        for (name, nid) in [
            ("DAC_SPK", NID_DAC_SPK),
            ("DAC_HP", NID_DAC_HP),
            ("MUX_SPK", NID_MUX_SPK),
            ("MUX_HP", NID_MUX_HP),
            ("PIN_SPK", NID_PIN_SPK),
            ("PIN_HP", NID_PIN_HP),
        ] {
            let stream = c.cmd_quiet(c.cad, nid, VERB_GET_CHANNEL_STREAMID, 0)?;
            let fmt = c.cmd_quiet(c.cad, nid, VERB_GET_STREAM_FORMAT, 0)?;
            let sel = c.cmd_quiet(c.cad, nid, VERB_GET_CONNECT_SEL, 0)?;
            let pw = c.cmd_quiet(c.cad, nid, VERB_GET_POWER_STATE, 0)?;
            let pin = c.cmd_quiet(c.cad, nid, VERB_GET_PIN_WIDGET_CONTROL, 0)?;
            let eapd = c.cmd_quiet(c.cad, nid, VERB_GET_EAPD_BTLENABLE, 0)?;
            let amp_out = c.cmd_quiet(
                c.cad,
                nid,
                VERB_GET_AMP_GAIN_MUTE,
                // index 0, output amp, left channel
                AC_AMP_GET_OUTPUT | AC_AMP_GET_LEFT,
            )?;
            log::info!(
                "{name} nid=0x{nid:x} stream=0x{stream:x} fmt=0x{fmt:x} sel=0x{sel:x} \
                 power=0x{pw:x} pinctl=0x{pin:x} eapd=0x{eapd:x} amp_out=0x{amp_out:x}"
            );
        }
        Ok(())
    }

    /// Whether a device is currently plugged into the headphone pin 0x15.
    /// Reads pin sense (verb 0x709) and checks the presence-detect bit. A
    /// `None` means the read failed / pin unsupported — treat as speakers.
    pub fn headphone_present(c: &mut Controller) -> io::Result<bool> {
        let sense = c.cmd_quiet(c.cad, NID_PIN_HP, VERB_GET_PIN_SENSE, 0)?;
        Ok(sense & AC_PINSENSE_PRESENCE != 0)
    }

    /// Output path descriptor for status/debugging: which jack the sound is
    /// heading to, based on headphone presence (speakers lie on pin 0x14).
    pub fn output_path(c: &mut Controller) -> String {
        match Self::headphone_present(c) {
            Ok(true) => "headphone".to_string(),
            Ok(false) => "speakers".to_string(),
            Err(e) => format!("unknown ({e})"),
        }
    }
}

fn set_power(c: &mut Controller, nid: u32, state: u32) -> io::Result<()> {
    c.cmd(c.cad, nid, VERB_SET_POWER_STATE, state).map(|_| ())
}

fn set_output_amp(c: &mut Controller, nid: u32, gain: u32, mute: bool) -> io::Result<()> {
    let mut v = AC_AMP_OUTPUT | AC_AMP_LEFT | AC_AMP_RIGHT;
    v |= (gain & 0x7f) | if mute { AC_AMP_MUTE } else { 0 };
    c.cmd(c.cad, nid, VERB_SET_AMP_GAIN_MUTE, v).map(|_| ())
}

fn set_input_amp(c: &mut Controller, nid: u32, index: u32, gain: u32, mute: bool) -> io::Result<()> {
    let mut v = (index & 0xf) << AC_AMP_INDEX_SHIFT;
    v |= AC_AMP_LEFT | AC_AMP_RIGHT;
    v |= (gain & 0x7f) | if mute { AC_AMP_MUTE } else { 0 };
    c.cmd(c.cad, nid, VERB_SET_AMP_GAIN_MUTE, v).map(|_| ())
}