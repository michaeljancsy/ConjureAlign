//! Welch-averaged spectra of the two captures, for the editor's comb-filter
//! ("Spectrum") panel.
//!
//! Runs on the background analysis thread (estimation) and the GUI thread
//! (synthesis — plus one cached re-estimation per user-selected FFT size
//! when the panel's selector diverges from the snapshot); the audio thread
//! never calls into this module. Same sign
//! convention as `analysis.rs`: `detected offset = t_ref − t_main`, positive
//! ⇒ main leads and gets delayed, `ref[n] ≈ main[n − offset]`.
//!
//! Rather than storing rendered curves, [`estimate`] stores the averaged
//! auto-spectra and cross-spectrum. The power spectrum of the *sum* the user
//! hears — `p·main[n − δ] + ref[n]` for any shift δ (samples, fractional) and
//! polarity p — is then synthesized per bin as
//!
//! ```text
//! P(δ, p)[k] = Pmm[k] + Prr[k] + 2p·Re(Pmr[k]·e^{+jω_k(δ − prealign)}),
//! ω_k = 2πk/nfft
//! ```
//!
//! (pinned by `sign_convention_spectrum` below), which is what lets the
//! editor's corrected curve follow a trim drag live without re-running any
//! FFT. `Pmr` is estimated with main pre-shifted by the detected integer lag:
//! a Welch cross-spectrum loses coherence as the true offset grows toward the
//! segment length (gone entirely past it), while the rotation above is exact
//! at any δ — so estimate near zero lag, rotate to wherever the view needs.

use realfft::num_complex::Complex;
use realfft::RealFftPlanner;

use crate::analysis::{AnalysisReport, AnalysisResult, RejectReason};

/// Power floor before log10 (≈ −120 dB). Guards float rounding only:
/// Cauchy–Schwarz over the segment average gives |Pmr|² ≤ Pmm·Prr, so the
/// synthesized power is mathematically ≥ (√Pmm − √Prr)² ≥ 0.
pub const DB_FLOOR_POWER: f32 = 1e-12;

/// No FFT shorter than this — fewer bins than pixels helps nobody.
const MIN_NFFT: usize = 256;

/// Welch-averaged spectra of a capture pair. Bin `k` covers frequency
/// `k · sample_rate / nfft`; all three vectors have `nfft/2 + 1` bins.
pub struct SpectrumData {
    pub nfft: usize,
    /// Integer lag main was pre-shifted by during estimation (positive =
    /// main read earlier, i.e. the segment pairs were `main[n − prealign]`
    /// against `ref[n]`).
    pub prealign_samples: i32,
    /// Welch segments actually averaged (seam-crossing ones are skipped).
    pub segments: u32,
    /// Auto-power of (pre-shifted) main.
    pub pmm: Vec<f32>,
    /// Auto-power of the reference.
    pub prr: Vec<f32>,
    /// Cross-spectrum, `avg conj(MAIN)·REF` — the same conjugation
    /// `analysis.rs` uses for the correlation.
    pub pmr: Vec<Complex<f32>>,
}

/// Segment size targeting ≈6 Hz bins (8192 @ 44.1/48 kHz, 16384 @ 88.2/96,
/// 32768 @ 176.4/192 — constant bin spacing in Hz and segment *duration*
/// across rates), halved until a full segment fits in `usable_len`; `None`
/// below [`MIN_NFFT`].
pub fn pick_nfft(sample_rate: f32, usable_len: usize) -> Option<usize> {
    let mut nfft = ((sample_rate / 6.0).max(1.0) as usize).next_power_of_two();
    while nfft > usable_len {
        nfft /= 2;
        if nfft < MIN_NFFT {
            return None;
        }
    }
    Some(nfft)
}

/// The integer pre-alignment lag for [`estimate`], from the snapshot's OWN
/// analysis — never from the persisted detected-offset atomics, which still
/// hold a *previous* capture's offset after a rejection. `Ok` → the refined
/// offset rounded; rejected-with-curve (`LowConfidence`) → the curve's
/// max-|r| lag; no curve → 0.
pub fn prealign_lag(
    outcome: &Result<AnalysisResult, RejectReason>,
    corr: &[f32],
    max_shift_samples: usize,
) -> i32 {
    match outcome {
        Ok(r) => r.offset_samples.round() as i32,
        Err(_) => match corr
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
        {
            Some((idx, _)) => idx as i32 - max_shift_samples as i32,
            None => 0,
        },
    }
}

/// Hann-windowed, 50 %-overlap Welch estimation. Segment `s` pairs
/// `ref[s..s+nfft]` with `main[s−D..s−D+nfft]` (D = `prealign_samples`),
/// with `s` ranged so both stay in bounds; `None` when no full segment fits
/// (or every segment crosses a splice seam). Absolute scale is arbitrary but
/// self-consistent (the panel's dB axis is uncalibrated; only shape and
/// differences matter).
///
/// `splices` are chunk-start positions of a gated capture. A segment is
/// invalid iff any seam falls strictly inside the union hull of its two
/// windows: a seam inside a window is a discontinuity, and a seam between
/// the windows means they pair content from different chunks.
pub fn estimate(
    main: &[f32],
    reference: &[f32],
    prealign_samples: i32,
    nfft: usize,
    splices: &[usize],
) -> Option<SpectrumData> {
    let n = main.len().min(reference.len()) as i64;
    let d = prealign_samples as i64;
    let s_lo = d.max(0);
    let s_hi = n - nfft as i64 + d.min(0);
    if nfft < 2 || s_hi < s_lo {
        return None;
    }

    let mut planner = RealFftPlanner::<f32>::new();
    let r2c = planner.plan_fft_forward(nfft);
    let bins = nfft / 2 + 1;

    let window: Vec<f32> = (0..nfft)
        .map(|i| {
            0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / nfft as f64).cos() as f32
        })
        .collect();

    let mut buf = r2c.make_input_vec();
    let mut spec_main = r2c.make_output_vec();
    let mut spec_ref = r2c.make_output_vec();
    // f32 FFTs (ample once averaged), f64 accumulation, f32 storage.
    let mut pmm = vec![0.0f64; bins];
    let mut prr = vec![0.0f64; bins];
    let mut pmr = vec![Complex::<f64>::new(0.0, 0.0); bins];

    let hop = (nfft / 2).max(1) as i64;
    let mut segments = 0u32;
    let mut s = s_lo;
    while s <= s_hi {
        let m0 = (s - d) as usize;
        let r0 = s as usize;
        // Hull rule (see doc above). No seam in the hull means both windows
        // sample one contiguous inter-seam chunk, exactly as an unspliced
        // capture of that chunk would.
        let hull_lo = m0.min(r0);
        let hull_hi = m0.max(r0) + nfft;
        if splices.iter().any(|&t| t > hull_lo && t < hull_hi) {
            s += hop;
            continue;
        }
        for ((dst, &src), &w) in buf.iter_mut().zip(&main[m0..m0 + nfft]).zip(&window) {
            *dst = src * w;
        }
        // The planner sized every buffer, so process() cannot fail; bail
        // defensively rather than panicking on a background thread.
        r2c.process(&mut buf, &mut spec_main).ok()?;
        for ((dst, &src), &w) in buf.iter_mut().zip(&reference[r0..r0 + nfft]).zip(&window) {
            *dst = src * w;
        }
        r2c.process(&mut buf, &mut spec_ref).ok()?;

        for (k, (m, r)) in spec_main.iter().zip(&spec_ref).enumerate() {
            pmm[k] += (m.re as f64).powi(2) + (m.im as f64).powi(2);
            prr[k] += (r.re as f64).powi(2) + (r.im as f64).powi(2);
            // conj(m)·r
            pmr[k].re += m.re as f64 * r.re as f64 + m.im as f64 * r.im as f64;
            pmr[k].im += m.re as f64 * r.im as f64 - m.im as f64 * r.re as f64;
        }
        segments += 1;
        s += hop;
    }

    // Every segment crossed a seam; the caller decides on a fallback.
    if segments == 0 {
        return None;
    }

    // Periodogram normalization: per-segment window power, averaged.
    let u: f64 = window.iter().map(|&w| w as f64 * w as f64).sum();
    let norm = 1.0 / (segments as f64 * u);
    Some(SpectrumData {
        nfft,
        prealign_samples,
        segments,
        pmm: pmm.iter().map(|&v| (v * norm) as f32).collect(),
        prr: prr.iter().map(|&v| (v * norm) as f32).collect(),
        pmr: pmr
            .iter()
            .map(|c| Complex::new((c.re * norm) as f32, (c.im * norm) as f32))
            .collect(),
    })
}

/// [`estimate`], falling back to ignoring the seams when every segment
/// crosses one (short chunks vs. the segment length): a slightly smeared
/// spectrum beats an empty panel. Shared by [`welch_for_capture`] and the
/// panel's GUI-side re-estimation at a user-selected segment size, so both
/// apply the same policy.
pub fn estimate_with_seam_fallback(
    main: &[f32],
    reference: &[f32],
    prealign_samples: i32,
    nfft: usize,
    splices: &[usize],
) -> Option<SpectrumData> {
    estimate(main, reference, prealign_samples, nfft, splices).or_else(|| {
        if splices.is_empty() {
            None
        } else {
            estimate(main, reference, prealign_samples, nfft, &[])
        }
    })
}

/// The one-call wrapper both snapshot construction sites use: gate on the
/// report having a correlation curve (mirroring the correlation panel's
/// rejected-before-analysis treatment), then pre-align, pick a segment size,
/// estimate. `nfft_override` is the user's fixed segment size (the Spectrum
/// panel's FFT selector); it is honored when a full segment fits and
/// otherwise falls back to the automatic [`pick_nfft`] choice — never to
/// `None`, so an over-large selection degrades to Auto instead of an empty
/// panel.
pub fn welch_for_capture(
    main: &[f32],
    reference: &[f32],
    sample_rate: f32,
    report: &AnalysisReport,
    splices: &[usize],
    nfft_override: Option<usize>,
) -> Option<SpectrumData> {
    if report.corr_curve.is_empty() {
        return None;
    }
    let d = prealign_lag(&report.outcome, &report.corr_curve, report.max_shift_samples);
    let usable = main
        .len()
        .min(reference.len())
        .saturating_sub(d.unsigned_abs() as usize);
    let nfft = match nfft_override {
        // `nfft <= usable` is exactly `estimate`'s no-full-segment-fits
        // bound (`s_hi >= s_lo`), so a fitting override cannot come back
        // empty (modulo the seam fallback, which the helper handles).
        Some(n) if n >= MIN_NFFT && n <= usable => n,
        _ => pick_nfft(sample_rate, usable)?,
    };
    estimate_with_seam_fallback(main, reference, d, nfft, splices)
}

/// dB magnitude spectrum of the summed signal `p·main[n − δ] + ref[n]`:
/// `10·log10(max(Pmm + Prr + 2p·Re(Pmr·e^{+jω_k(δ − prealign)}), FLOOR))`.
/// `δ` is in samples and may be fractional. Reuses `out`'s allocation.
///
/// Phase math stays in f64: ω·(δ − prealign) reaches ~1.2e5 rad at the
/// 200 ms / 192 kHz extreme, far past f32 sincos accuracy.
pub fn synth_sum_db(s: &SpectrumData, delta_samples: f64, inverted: bool, out: &mut Vec<f32>) {
    out.clear();
    out.reserve(s.pmm.len());
    let rot = delta_samples - s.prealign_samples as f64;
    let w = 2.0 * std::f64::consts::PI * rot / s.nfft as f64;
    let p = if inverted { -1.0f64 } else { 1.0 };
    for (k, ((&pmm, &prr), c)) in s.pmm.iter().zip(&s.prr).zip(&s.pmr).enumerate() {
        let (sin, cos) = (w * k as f64).sin_cos();
        let cross = c.re as f64 * cos - c.im as f64 * sin;
        let power = pmm as f64 + prr as f64 + 2.0 * p * cross;
        out.push((10.0 * power.max(DB_FLOOR_POWER as f64).log10()) as f32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::analyze_detailed;

    /// Deterministic white noise (xorshift; same generator as analysis.rs).
    fn noise(len: usize, seed: u64) -> Vec<f32> {
        let mut state = seed.max(1);
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                ((state >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) as f32
            })
            .collect()
    }

    /// Band-limited noise via a simple moving-average smoothing of white noise.
    fn band_limited_noise(len: usize, seed: u64) -> Vec<f32> {
        let white = noise(len + 16, seed);
        (0..len)
            .map(|i| white[i..i + 16].iter().sum::<f32>() / 4.0)
            .collect()
    }

    /// `out[n] = signal[n − k]` for integer k (positive k ⇒ main leads when
    /// used as the reference).
    fn delayed_copy(signal: &[f32], k: i64) -> Vec<f32> {
        (0..signal.len() as i64)
            .map(|n| {
                let idx = n - k;
                if idx >= 0 && (idx as usize) < signal.len() {
                    signal[idx as usize]
                } else {
                    0.0
                }
            })
            .collect()
    }

    /// Fractionally delayed copy via direct windowed-sinc resampling.
    fn sinc_delayed_copy(signal: &[f32], delay: f64) -> Vec<f32> {
        let width = 64i64;
        (0..signal.len() as i64)
            .map(|n| {
                let center = n as f64 - delay;
                let mut acc = 0.0f64;
                let lo = (center.floor() as i64 - width).max(0);
                let hi = (center.floor() as i64 + width).min(signal.len() as i64 - 1);
                for m in lo..=hi {
                    let x = m as f64 - center;
                    let s = if x.abs() < 1e-12 {
                        1.0
                    } else {
                        (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
                    };
                    let w = 0.5
                        + 0.5 * (std::f64::consts::PI * x / (width as f64 + 1.0)).cos();
                    acc += signal[m as usize] as f64 * s * w;
                }
                acc as f32
            })
            .collect()
    }

    fn db_of_power(p: f64) -> f64 {
        10.0 * p.max(DB_FLOOR_POWER as f64).log10()
    }

    /// 10·log10(Pmm + Prr): the incoherent power sum, the natural baseline.
    fn incoherent_db(s: &SpectrumData, k: usize) -> f64 {
        db_of_power(s.pmm[k] as f64 + s.prr[k] as f64)
    }

    /// The strongest end-to-end check of the synthesis formula: for two
    /// unrelated signals with no pre-alignment, the synthesized sum spectrum
    /// at δ = 0 must equal the Welch auto-spectrum of the literal sum —
    /// |M + R|² = |M|² + |R|² + 2Re(conj(M)·R) per segment, algebraically.
    #[test]
    fn sum_identity_no_prealign() {
        let n = 1 << 16;
        let main = noise(n, 11);
        let reference = band_limited_noise(n, 22);
        let nfft = 1024;
        let spec = estimate(&main, &reference, 0, nfft, &[]).unwrap();

        let summed: Vec<f32> = main.iter().zip(&reference).map(|(&a, &b)| a + b).collect();
        let direct = estimate(&summed, &summed, 0, nfft, &[]).unwrap();
        let mut db = Vec::new();
        synth_sum_db(&spec, 0.0, false, &mut db);
        for (k, &v) in db.iter().enumerate() {
            let expect = db_of_power(direct.pmm[k] as f64);
            assert!(
                (v as f64 - expect).abs() < 1e-2,
                "bin {k}: synth {v} vs direct {expect}"
            );
        }

        // And the polarity-flipped synthesis must match the literal difference.
        let diffed: Vec<f32> = main.iter().zip(&reference).map(|(&a, &b)| a - b).collect();
        let direct = estimate(&diffed, &diffed, 0, nfft, &[]).unwrap();
        synth_sum_db(&spec, 0.0, true, &mut db);
        for (k, &v) in db.iter().enumerate() {
            let expect = db_of_power(direct.pmm[k] as f64);
            assert!(
                (v as f64 - expect).abs() < 1e-2,
                "bin {k} (inverted): synth {v} vs direct {expect}"
            );
        }
    }

    /// A 256-sample delay comb-filters the raw sum with notches exactly at
    /// bins k = 16·(2m+1) of an 8192 FFT (256 divides 8192 — no off-bin
    /// tolerance games) and +6 dB peaks at k = 32m.
    #[test]
    fn comb_notch_positions() {
        let main = noise(1 << 18, 42);
        let reference = delayed_copy(&main, 256);
        let spec = estimate(&main, &reference, 256, 8192, &[]).unwrap();
        let mut db = Vec::new();
        synth_sum_db(&spec, 0.0, false, &mut db);
        for m in 0..30usize {
            let notch = 16 * (2 * m + 1);
            let peak = 32 * (m + 1);
            assert!(
                (db[notch] as f64) < db[peak] as f64 - 25.0,
                "m={m}: notch bin {notch} at {} not ≥25 dB below peak bin {peak} at {}",
                db[notch],
                db[peak]
            );
            // Peak = fully coherent sum = 4·Pmm (+6 dB over one signal).
            let expect = db_of_power(4.0 * spec.pmm[peak] as f64);
            assert!(
                (db[peak] as f64 - expect).abs() < 0.5,
                "m={m}: peak bin {peak} at {} vs expected {expect}",
                db[peak]
            );
        }
    }

    /// Rotating by the true delay flattens the comb to a constant +3 dB over
    /// the incoherent sum — estimated WITHOUT pre-alignment, so this
    /// exercises a genuinely large rotation, not rot = 0.
    #[test]
    fn rotation_flattens_at_true_delay() {
        let main = noise(1 << 18, 9);
        let reference = delayed_copy(&main, 256);
        let spec = estimate(&main, &reference, 0, 8192, &[]).unwrap();
        let mut db = Vec::new();
        synth_sum_db(&spec, 256.0, false, &mut db);
        let interior = db.len() - 2;
        for (k, &v) in db.iter().enumerate().skip(1).take(interior) {
            let rel = v as f64 - incoherent_db(&spec, k);
            assert!(
                (rel - 3.01).abs() < 0.6,
                "bin {k}: {rel:.2} dB over incoherent, expected ≈ +3.01"
            );
        }
    }

    /// Fractional true delay, fractional rotation: flat within 2 dB on
    /// energetic bins.
    #[test]
    fn fractional_delay_flattens() {
        let main = band_limited_noise(1 << 17, 7);
        let reference = sinc_delayed_copy(&main, 240.5);
        let spec = estimate(&main, &reference, 240, 8192, &[]).unwrap();
        let mut db = Vec::new();
        synth_sum_db(&spec, 240.5, false, &mut db);
        let max_pmm = spec.pmm.iter().fold(0.0f32, |m, &v| m.max(v));
        let mut checked = 0;
        let interior = db.len() - 2;
        for (k, &v) in db.iter().enumerate().skip(1).take(interior) {
            // Skip the moving-average filter's spectral nulls (and the sinc
            // interpolator's accuracy limit near them): −40 dB of max.
            if spec.pmm[k] < max_pmm * 1e-4 {
                continue;
            }
            // Fully coherent sum per bin.
            let expect = db_of_power(
                ((spec.pmm[k] as f64).sqrt() + (spec.prr[k] as f64).sqrt()).powi(2),
            );
            assert!(
                (v as f64 - expect).abs() < 2.0,
                "bin {k}: {v} vs coherent {expect:.2}"
            );
            checked += 1;
        }
        assert!(checked > 1000, "energetic-bin filter too aggressive: {checked}");
    }

    /// Inverted polarity turns the comb inside out: peaks where the normal
    /// sum had notches and vice versa.
    #[test]
    fn polarity_flip_swaps_notches() {
        let main = noise(1 << 18, 42);
        let reference = delayed_copy(&main, 256);
        let spec = estimate(&main, &reference, 256, 8192, &[]).unwrap();
        let mut db = Vec::new();
        synth_sum_db(&spec, 0.0, true, &mut db);
        for m in 0..30usize {
            let peak = 16 * (2 * m + 1); // the normal sum's notches
            let notch = 32 * (m + 1); // the normal sum's peaks
            assert!(
                (db[notch] as f64) < db[peak] as f64 - 25.0,
                "m={m}: inverted notch {notch} at {} not below peak {peak} at {}",
                db[notch],
                db[peak]
            );
            let expect = db_of_power(4.0 * spec.pmm[peak] as f64);
            assert!((db[peak] as f64 - expect).abs() < 0.5);
        }
    }

    /// Estimating with and without pre-alignment must synthesize the same
    /// curve at equal δ — pins the e^{−jωD} bookkeeping between
    /// `prealign_samples` and the rotation.
    #[test]
    fn prealign_roundtrip() {
        let main = noise(1 << 18, 5);
        let reference = delayed_copy(&main, 64);
        let a = estimate(&main, &reference, 64, 8192, &[]).unwrap();
        let b = estimate(&main, &reference, 0, 8192, &[]).unwrap();
        let (mut da, mut db) = (Vec::new(), Vec::new());
        for delta in [0.0f64, 64.0, 100.25] {
            synth_sum_db(&a, delta, false, &mut da);
            synth_sum_db(&b, delta, false, &mut db);
            // The two estimates necessarily use shifted segment grids, so
            // per-bin estimation noise differs; on comb slopes that noise is
            // amplified. A rotation-bookkeeping error would displace the
            // whole comb (mean error of many dB), so the meaningful check is
            // a tight MEAN over energetic bins plus a loose per-bin bound.
            let mut checked = 0u32;
            let mut sum_abs = 0.0f64;
            for k in 1..da.len() - 1 {
                // Near a notch the power → 0 and dB differences blow up on
                // estimation noise alone; compare away from the notch floor.
                if (da[k] as f64) < incoherent_db(&a, k) - 10.0 {
                    continue;
                }
                let diff = (da[k] - db[k]).abs();
                assert!(
                    diff < 1.5,
                    "δ={delta}, bin {k}: prealigned {} vs raw {}",
                    da[k],
                    db[k]
                );
                sum_abs += diff as f64;
                checked += 1;
            }
            assert!(checked > 1000, "δ={delta}: only {checked} bins compared");
            let mean = sum_abs / checked as f64;
            assert!(mean < 0.15, "δ={delta}: mean |diff| {mean:.3} dB");
        }
    }

    /// THE test that pins the rotation sign against the plugin's delay
    /// direction. `ref[n] = main[n − D]` with D > 0 means main leads and the
    /// plugin corrects by DELAYING main by +D — so the synthesized sum must
    /// be flat at δ = +D and comb-filtered at δ = −D. Estimated without
    /// pre-alignment: with prealign = D the corrected curve would sit at
    /// rot = 0 and both signs of the exponent would pass.
    #[test]
    fn sign_convention_spectrum() {
        let main = band_limited_noise(1 << 17, 42);
        let reference = delayed_copy(&main, 256);
        let spec = estimate(&main, &reference, 0, 8192, &[]).unwrap();
        let (mut pos, mut neg) = (Vec::new(), Vec::new());
        synth_sum_db(&spec, 256.0, false, &mut pos);
        synth_sum_db(&spec, -256.0, false, &mut neg);
        let max_pmm = spec.pmm.iter().fold(0.0f32, |m, &v| m.max(v));
        let mut neg_min_rel = f64::INFINITY;
        for k in 1..pos.len() - 1 {
            if spec.pmm[k] < max_pmm * 1e-3 {
                continue;
            }
            let rel = pos[k] as f64 - incoherent_db(&spec, k);
            assert!(
                (rel - 3.01).abs() < 1.0,
                "bin {k}: correcting by +D must flatten; got {rel:.2} dB over incoherent"
            );
            neg_min_rel = neg_min_rel.min(neg[k] as f64 - incoherent_db(&spec, k));
        }
        assert!(
            neg_min_rel < -8.0,
            "correcting by −D must comb-filter; deepest notch only {neg_min_rel:.2} dB"
        );
    }

    /// The full data path the plugin uses: analyze, then welch_for_capture,
    /// then synthesize at the detected offset — flat.
    #[test]
    fn welch_for_capture_end_to_end() {
        let main = band_limited_noise(96_000, 21);
        let reference = delayed_copy(&main, 240);
        let report = analyze_detailed(&main, &reference, 960);
        let detected = report.outcome.expect("must detect").offset_samples;
        assert!((detected - 240.0).abs() < 0.1);
        let spec = welch_for_capture(&main, &reference, 48_000.0, &report, &[], None).unwrap();
        assert_eq!(spec.nfft, 8192);
        assert_eq!(spec.prealign_samples, 240);

        // A fitting override is honored; an over-large or under-minimum one
        // degrades to the automatic pick, never to an empty panel.
        for (over, want) in [(2048, 2048), (1 << 20, 8192), (64, 8192)] {
            let s = welch_for_capture(&main, &reference, 48_000.0, &report, &[], Some(over));
            assert_eq!(s.unwrap().nfft, want, "override {over}");
        }
        let mut db = Vec::new();
        synth_sum_db(&spec, detected, false, &mut db);
        let max_pmm = spec.pmm.iter().fold(0.0f32, |m, &v| m.max(v));
        let interior = db.len() - 2;
        for (k, &v) in db.iter().enumerate().skip(1).take(interior) {
            if spec.pmm[k] < max_pmm * 1e-3 {
                continue;
            }
            let rel = v as f64 - incoherent_db(&spec, k);
            assert!((rel - 3.01).abs() < 1.0, "bin {k}: {rel:.2} dB over incoherent");
        }
    }

    /// Silence hits the floor exactly, with no NaN/inf at any δ or polarity.
    #[test]
    fn silence_gives_floor() {
        let zeros = vec![0.0f32; 32_768];
        let spec = estimate(&zeros, &zeros, 0, 1024, &[]).unwrap();
        let floor = 10.0 * (DB_FLOOR_POWER as f64).log10();
        let mut db = Vec::new();
        for (delta, inverted) in [(0.0, false), (123.4, true), (-9600.0, false)] {
            synth_sum_db(&spec, delta, inverted, &mut db);
            assert_eq!(db.len(), 513);
            for &v in &db {
                assert!(v.is_finite());
                assert!((v as f64 - floor).abs() < 1e-6, "expected floor, got {v}");
            }
        }
    }

    #[test]
    fn seam_crossing_segments_are_skipped() {
        let main = noise(1 << 16, 33);
        let reference = delayed_copy(&main, 64);
        let nfft = 4096;
        let full = estimate(&main, &reference, 64, nfft, &[]).unwrap();
        let spliced = estimate(&main, &reference, 64, nfft, &[32_000]).unwrap();
        assert!(spliced.segments > 0);
        assert!(
            spliced.segments < full.segments,
            "seam must drop segments: {} vs {}",
            spliced.segments,
            full.segments
        );
        // hop = nfft/2, hull width ≈ nfft + |d|: the seam invalidates only
        // the two-or-so segments whose hull contains it.
        assert!(full.segments - spliced.segments <= 4);
    }

    #[test]
    fn all_segments_skipped_falls_back() {
        let main = noise(20_000, 44);
        let reference = delayed_copy(&main, 16);
        // A seam every 1000 samples: every possible 8192-wide hull crosses
        // one, so the seam-aware estimate is impossible...
        let seams: Vec<usize> = (1..20).map(|i| i * 1000).collect();
        assert!(estimate(&main, &reference, 16, 8192, &seams).is_none());
        // ...and the fallback helper ignores the seams instead.
        let spec = estimate_with_seam_fallback(&main, &reference, 16, 8192, &seams).unwrap();
        assert!(spec.segments > 0);
        // welch_for_capture routes through the same helper.
        let report = analyze_detailed(&main, &reference, 960);
        let spec = welch_for_capture(&main, &reference, 48_000.0, &report, &seams, None).unwrap();
        assert!(spec.segments > 0);
    }

    #[test]
    fn degenerate_lengths() {
        // Rate scaling at ample length.
        assert_eq!(pick_nfft(44_100.0, 1 << 20), Some(8192));
        assert_eq!(pick_nfft(48_000.0, 1 << 20), Some(8192));
        assert_eq!(pick_nfft(96_000.0, 1 << 20), Some(16_384));
        assert_eq!(pick_nfft(192_000.0, 1 << 20), Some(32_768));
        // Halving on short captures; None below the minimum.
        assert_eq!(pick_nfft(48_000.0, 5000), Some(4096));
        assert_eq!(pick_nfft(48_000.0, 300), Some(256));
        assert_eq!(pick_nfft(48_000.0, 200), None);

        // estimate: no full segment fits.
        let x = noise(1000, 1);
        assert!(estimate(&x, &x, 0, 1024, &[]).is_none());
        let y = noise(1124, 2);
        assert!(estimate(&y, &y, 200, 1024, &[]).is_none());
        assert!(estimate(&y, &y, -200, 1024, &[]).is_none());
        // Exactly one segment fits.
        assert!(estimate(&y, &y, 100, 1024, &[]).is_some());
        assert!(estimate(&y, &y, -100, 1024, &[]).is_some());
    }

    #[test]
    fn prealign_lag_selection() {
        let ok: Result<AnalysisResult, RejectReason> = Ok(AnalysisResult {
            offset_samples: 240.6,
            inverted: false,
            confidence: 0.9,
        });
        assert_eq!(prealign_lag(&ok, &[], 960), 241);
        let ok_neg: Result<AnalysisResult, RejectReason> = Ok(AnalysisResult {
            offset_samples: -3.4,
            inverted: false,
            confidence: 0.9,
        });
        assert_eq!(prealign_lag(&ok_neg, &[], 960), -3);

        // LowConfidence: the curve's max-|r| lag, sign preserved through the
        // magnitude (an inverted match peaks negative).
        let corr = [0.05f32, -0.15, 0.02, 0.08, 0.01];
        let err: Result<AnalysisResult, RejectReason> = Err(RejectReason::LowConfidence);
        assert_eq!(prealign_lag(&err, &corr, 2), -1);

        // No curve at all → 0.
        assert_eq!(prealign_lag(&err, &[], 2), 0);
    }
}
