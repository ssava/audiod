//! Per-client DSP pipeline: client PCM bytes → interleaved f32 stereo →
//! resample to the mix rate → S16LE stereo bytes for the mixer rings.
//!
//! The default resampler is a windowed-sinc polyphase-style table
//! (`PHASES` rows × `TAPS` coefficients, interpolated between adjacent rows),
//! which anti-aliases on downsample and suppresses imaging on upsample.
//! `"linear"` remains available as a debug/A-B fallback via config.

use audcommon::{pcm_format_bytes, FORMAT_FLOAT_LE};
use std::f64::consts::PI;

/// Taps per kernel row. 48 gives ≥60 dB stopband with margin for the
/// between-phase interpolation, at negligible CPU cost.
pub const TAPS: usize = 48;
/// Precomputed fractional delays; adjacent rows are linearly interpolated.
pub const PHASES: usize = 256;

/// Windowed-sinc kernel table for one src→dst rate pair.
///
/// Row `p` is the kernel delayed by `p/PHASES` of an input sample. Each row is
/// normalized to sum 1 so every phase has unity DC gain. Cutoff scales with
/// `min(1, dst/src)` so downsampling lowpasses at the output Nyquist.
pub struct KernelTable {
    coeffs: Vec<f32>, // PHASES × TAPS
}

impl KernelTable {
    pub fn new(src: u32, dst: u32) -> Self {
        let ratio = dst as f64 / src as f64;
        // Cutoff in cycles per input sample; 0.95 headroom keeps the
        // transition band below the output Nyquist when downsampling.
        let fc = 0.475 * ratio.min(1.0);
        let center = (TAPS as f64 - 1.0) / 2.0;
        let mut coeffs = vec![0f32; PHASES * TAPS];
        for p in 0..PHASES {
            let off = p as f64 / PHASES as f64;
            let row = &mut coeffs[p * TAPS..(p + 1) * TAPS];
            let mut sum = 0f64;
            for (i, c) in row.iter_mut().enumerate() {
                // Kernel for reconstructing at fractional offset `off`: by
                // evenness of the windowed sinc, tap k needs h(k − c − off).
                // (Sign matters: +off scrambles high frequencies.)
                let t = i as f64 - center - off;
                // sinc(2·fc·t) scaled by 2·fc → unity area before windowing.
                let x = 2.0 * PI * fc * t;
                let s = if x.abs() < 1e-12 { 2.0 * fc } else { x.sin() / x };
                // Blackman window over the tap span, shifted with the sinc.
                let n = i as f64 - off;
                let ph = 2.0 * PI * n / (TAPS as f64 - 1.0);
                let w = 0.42 - 0.5 * ph.cos() + 0.08 * (2.0 * ph).cos();
                let v = s * w;
                sum += v;
                *c = v as f32;
            }
            if sum.abs() > 1e-12 {
                for c in row.iter_mut() {
                    *c = (*c as f64 / sum) as f32;
                }
            }
        }
        KernelTable { coeffs }
    }

    #[inline]
    fn row(&self, p: usize) -> &[f32] {
        &self.coeffs[p * TAPS..(p + 1) * TAPS]
    }
}

enum Kind {
    Sinc(KernelTable),
    Linear,
}

/// Stateful streaming resampler for interleaved stereo f32.
///
/// Position is tracked as an exact integer in `1/step_den`-ths of an input
/// frame (`step = src/gcd : dst/gcd`), so feeding the same total input in
/// different chunk sizes produces bit-identical output (see
/// `chunk_split_invariance` test) and long streams never lose float precision.
pub struct Resampler {
    kind: Kind,
    /// Position advance per output frame: `step_num / step_den` input frames.
    step_num: u64,
    step_den: u64,
    /// Position of the next output frame relative to `hist[0]`, in
    /// `step_den`-ths of an input frame.
    pos: u64,
    hist: Vec<f32>,
}

impl Resampler {
    /// `None` when `src == dst` (caller sends the stream through unresampled).
    pub fn new(src: u32, dst: u32, kind: &str) -> Option<Self> {
        if src == dst || src == 0 || dst == 0 {
            return None;
        }
        let kind = match kind {
            "linear" => Kind::Linear,
            _ => Kind::Sinc(KernelTable::new(src, dst)),
        };
        let g = gcd(src as u64, dst as u64);
        Some(Resampler {
            kind,
            step_num: src as u64 / g,
            step_den: dst as u64 / g,
            pos: 0,
            hist: Vec::new(),
        })
    }

    /// Append interleaved stereo input; push produced frames onto `out`.
    /// Returns the number of output frames appended.
    pub fn resample(&mut self, input: &[f32], out: &mut Vec<f32>) -> usize {
        debug_assert_eq!(input.len() % 2, 0);
        self.hist.extend_from_slice(input);
        let frames = self.hist.len() / 2;
        let mut produced = 0usize;
        loop {
            let base = (self.pos / self.step_den) as usize;
            if base + TAPS > frames {
                break;
            }
            let rem = self.pos % self.step_den;
            let (l, r) = match &self.kind {
                Kind::Sinc(tab) => {
                    let pf = (rem as f32 / self.step_den as f32) * PHASES as f32;
                    let p0 = (pf as usize).min(PHASES - 1);
                    let p1 = (p0 + 1) % PHASES;
                    let t = pf - p0 as f32;
                    let r0 = tab.row(p0);
                    let r1 = tab.row(p1);
                    let mut l = 0f32;
                    let mut r = 0f32;
                    for k in 0..TAPS {
                        let c = r0[k] + (r1[k] - r0[k]) * t;
                        let s = (base + k) * 2;
                        l += c * self.hist[s];
                        r += c * self.hist[s + 1];
                    }
                    (l, r)
                }
                Kind::Linear => {
                    let frac = rem as f32 / self.step_den as f32;
                    let s = base * 2;
                    (
                        self.hist[s] + (self.hist[s + 2] - self.hist[s]) * frac,
                        self.hist[s + 1] + (self.hist[s + 3] - self.hist[s + 1]) * frac,
                    )
                }
            };
            out.push(l);
            out.push(r);
            produced += 1;
            self.pos += self.step_num;
        }
        // Retire consumed frames, keeping enough context for the next output.
        let keep = match self.kind {
            Kind::Sinc(_) => TAPS - 1,
            Kind::Linear => 1,
        };
        let consumed = (self.pos / self.step_den).min(frames.saturating_sub(keep) as u64);
        if consumed > 0 {
            self.hist.drain(..consumed as usize * 2);
            self.pos -= consumed * self.step_den;
        }
        produced
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Convert client PCM bytes (S16LE or FLOATLE, 1..=8 channels) into
/// interleaved f32 stereo. Mono duplicates; >2 channels average into L/R
/// (even-index channels → L, odd-index → R).
pub fn fold_to_stereo(src: &[u8], fmt: u32, ch: u32, out: &mut Vec<f32>) {
    let bps = pcm_format_bytes(fmt as u16);
    let fsz = bps * ch as usize;
    let frames = src.len() / fsz;
    out.reserve(frames * 2);
    let mut chans = [0f32; 8];
    for f in 0..frames {
        let s = &src[f * fsz..f * fsz + fsz];
        for (c, ch_v) in chans.iter_mut().enumerate().take(ch as usize) {
            *ch_v = match fmt {
                FORMAT_FLOAT_LE => f32::from_le_bytes([
                    s[c * 4],
                    s[c * 4 + 1],
                    s[c * 4 + 2],
                    s[c * 4 + 3],
                ]),
                _ => i16::from_le_bytes([s[c * 2], s[c * 2 + 1]]) as f32 / 32768.0,
            };
        }
        let (l, r) = match ch {
            1 => (chans[0], chans[0]),
            2 => (chans[0], chans[1]),
            n => {
                let (mut l, mut r) = (0f32, 0f32);
                let (mut nl, mut nr) = (0u32, 0u32);
                for (c, &v) in chans.iter().enumerate().take(n as usize) {
                    if c % 2 == 0 {
                        l += v;
                        nl += 1;
                    } else {
                        r += v;
                        nr += 1;
                    }
                }
                (l / nl as f32, r / nr as f32)
            }
        };
        out.push(l);
        out.push(r);
    }
}

/// Pack interleaved f32 stereo into S16LE bytes (clamped).
pub fn f32_stereo_to_s16(inp: &[f32], out: &mut Vec<u8>) {
    out.reserve(inp.len() * 2);
    for &v in inp {
        let c = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
        out.extend_from_slice(&c.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Interleaved stereo sine (L=R) — matches `Resampler::resample` input.
    fn sine(rate: f64, freq: f64, amp: f64, secs: f64) -> Vec<f32> {
        let n = (rate * secs) as usize;
        (0..n)
            .flat_map(|i| {
                let v = (amp * (2.0 * PI * freq * i as f64 / rate).sin()) as f32;
                [v, v]
            })
            .collect()
    }

    /// Goertzel amplitude estimate of `freq` in x (single channel).
    fn goertzel(x: &[f32], rate: f64, freq: f64) -> f64 {
        let k = 2.0 * PI * freq / rate;
        let coeff = 2.0 * k.cos();
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &v in x {
            let s0 = v as f64 + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
        2.0 * power.sqrt() / x.len() as f64
    }

    fn trim(x: &[f32]) -> &[f32] {
        let m = x.len() / 10;
        &x[m..x.len() - m]
    }

    fn rs(src: u32, dst: u32) -> Resampler {
        Resampler::new(src, dst, "sinc").expect("resampler")
    }

    /// Left channel of interleaved stereo, for single-channel analysis.
    fn left(x: &[f32]) -> Vec<f32> {
        x.chunks(2).map(|c| c[0]).collect()
    }

    #[test]
    fn passband_flat_to_18khz() {
        for &freq in &[500.0, 4000.0, 8000.0, 12000.0, 15000.0, 18000.0] {
            let mut r = rs(44100, 48000);
            let mut out = Vec::new();
            r.resample(&sine(44100.0, freq, 0.8, 0.25), &mut out);
            let a = goertzel(trim(&left(&out)), 48000.0, freq);
            assert!(
                (0.755..=0.845).contains(&a),
                "{freq} Hz amplitude {a:.4} (expected 0.8 ±0.5 dB)"
            );
        }
    }

    #[test]
    fn nyquist_edge_rolloff() {
        // 48-tap kernel: transition spans roughly the last 3 kHz below the
        // input Nyquist; content there images only above 22 kHz after
        // upsampling, so moderate attenuation at the edge is by design.
        for &(freq, max_db) in &[(21000.0, -5.0), (22000.0, -17.0)] {
            let mut r = rs(44100, 48000);
            let mut out = Vec::new();
            r.resample(&sine(44100.0, freq, 0.8, 0.25), &mut out);
            let a = goertzel(trim(&left(&out)), 48000.0, freq);
            let db = 20.0 * (a / 0.8).log10();
            assert!(db < max_db, "{freq} Hz too strong: {db:.1} dB");
        }
    }

    #[test]
    fn impulse_response_sane() {
        // Unit impulse (both channels) at frame 24 through 44100->48000:
        // peak ≈ kernel center value (0.63..=0.96), nothing clips above 1.
        let mut r = rs(44100, 48000);
        let mut inp = vec![0f32; 400];
        inp[48] = 1.0;
        inp[49] = 1.0;
        let mut out = Vec::new();
        r.resample(&inp, &mut out);
        let l = left(&out);
        let peak = l
            .iter()
            .enumerate()
            .fold((0usize, 0f32), |(bi, bv), (i, &v)| {
                if v.abs() > bv.abs() { (i, v) } else { (bi, bv) }
            });
        assert!(
            (0.6..=0.96).contains(&peak.1.abs()),
            "impulse response peak {} at {}",
            peak.1,
            peak.0
        );
        assert!(
            l.iter().all(|&v| v.abs() <= 1.0),
            "impulse response overshoot"
        );
    }

    #[test]
    fn dc_gain_is_unity() {
        let mut r = rs(44100, 48000);
        let mut out = Vec::new();
        let inp = vec![0.5f32, 0.5];
        for _ in 0..2000 {
            r.resample(&inp, &mut out);
        }
        let mean: f32 = trim(&out).iter().sum::<f32>() / trim(&out).len() as f32;
        assert!((mean - 0.5).abs() < 1e-3, "dc gain {mean}");
    }

    #[test]
    fn passband_tone_preserved() {
        let mut r = rs(44100, 48000);
        let mut out = Vec::new();
        r.resample(&sine(44100.0, 1000.0, 0.8, 0.25), &mut out);
        let a = goertzel(trim(&left(&out)), 48000.0, 1000.0);
        assert!(
            (0.755..=0.845).contains(&a),
            "1 kHz amplitude {a:.4} (expected 0.8 ±0.5 dB)"
        );
    }

    #[test]
    fn downsample_alias_rejected() {
        // 30 kHz tone at 96k folds to 18 kHz at 48k unless the anti-alias
        // filter removes it. Linear resampling would leave it near full scale.
        let mut r = rs(96000, 48000);
        let mut out = Vec::new();
        r.resample(&sine(96000.0, 30000.0, 0.8, 0.25), &mut out);
        let alias = goertzel(trim(&left(&out)), 48000.0, 18000.0);
        let db = 20.0 * (alias / 0.8).log10();
        assert!(db < -50.0, "alias at 18 kHz: {alias:.5} ({db:.1} dB)");
    }

    #[test]
    fn upsample_image_rejected() {
        // 11 kHz tone at 44.1k images at 33.1 kHz after 48k upsampling.
        let mut r = rs(44100, 48000);
        let mut out = Vec::new();
        r.resample(&sine(44100.0, 11000.0, 0.8, 0.25), &mut out);
        let lch = left(&out);
        let o = trim(&lch);
        let img = goertzel(&o, 48000.0, 33100.0);
        let db = 20.0 * (img / 0.8).log10();
        assert!(db < -50.0, "image at 33.1 kHz: {img:.5} ({db:.1} dB)");
        let tone = goertzel(&o, 48000.0, 11000.0);
        assert!((0.755..=0.845).contains(&tone), "tone amplitude {tone:.4}");
    }

    #[test]
    fn chunk_split_invariance() {
        // Sum of tones — arbitrary chunking must give bit-identical output.
        let rate = 44100.0f64;
        let n = 10000; // frames
        let inp: Vec<f32> = (0..n)
            .flat_map(|i| {
                let t = i as f64 / rate;
                let v = (0.6 * (2.0 * PI * 137.0 * t).sin()
                    + 0.4 * (2.0 * PI * 3333.0 * t).sin()) as f32;
                [v, v]
            })
            .collect();
        let mut whole = rs(44100, 48000);
        let mut a = Vec::new();
        whole.resample(&inp, &mut a);

        let mut chunked = rs(44100, 48000);
        let mut b = Vec::new();
        for c in inp.chunks(26) {
            chunked.resample(c, &mut b);
        }
        assert_eq!(a, b, "chunked output differs");
        // Upsampling must yield more frames than input (modulo the ~TAPS
        // frames of tail the streaming window legitimately holds back).
        assert!(a.len() / 2 > n, "produced too little: {} frames", a.len() / 2);
    }

    #[test]
    fn linear_kind_works() {
        let mut r = Resampler::new(8000, 48000, "linear").unwrap();
        let mut out = Vec::new();
        r.resample(&sine(8000.0, 500.0, 0.8, 0.25), &mut out);
        let a = goertzel(trim(&left(&out)), 48000.0, 500.0);
        assert!((0.7..=0.85).contains(&a), "linear 500 Hz amplitude {a:.4}");
    }

    #[test]
    fn same_rate_bypasses() {
        assert!(Resampler::new(48000, 48000, "sinc").is_none());
    }

    #[test]
    fn fold_mono_duplicates() {
        let src: Vec<u8> = [100i16, -200i16].iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut out = Vec::new();
        fold_to_stereo(&src, audcommon::FORMAT_S16_LE, 1, &mut out);
        let s = 32768.0f32;
        assert_eq!(out, vec![100.0 / s, 100.0 / s, -200.0 / s, -200.0 / s]);
    }

    #[test]
    fn fold_five_channels_averages() {
        // Frame [a,b,c,d,e]: L = (a+c+e)/3, R = (b+d)/2.
        let frame = [1000i16, -2000i16, 3000i16, -4000i16, 5000i16];
        let src: Vec<u8> = frame.iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut out = Vec::new();
        fold_to_stereo(&src, audcommon::FORMAT_S16_LE, 5, &mut out);
        let s = 32768.0f32;
        assert_eq!(out, vec![(1000.0 + 3000.0 + 5000.0) / 3.0 / s, (-2000.0 - 4000.0) / 2.0 / s]);
    }

    #[test]
    fn float_pack_clamps() {
        let mut out = Vec::new();
        f32_stereo_to_s16(&[2.0, -2.0, 0.5], &mut out);
        let v: Vec<i16> = out.chunks(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(v, vec![32767, -32767, 16383]);
    }
}
