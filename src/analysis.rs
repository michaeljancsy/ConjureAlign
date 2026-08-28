//! Offset estimation between the captured main and reference signals.
//!
//! Runs on a background thread (allocation is fine here, and analysis of a
//! multi-second capture takes tens of milliseconds). The audio thread never
//! calls into this module.
//!
//! Sign convention — pinned by `sign_convention` below, do not change without
//! changing the delay math in `lib.rs`:
//! `detected offset = t_ref − t_main`. Positive means the main signal LEADS
//! the reference (the same wavefront appears earlier on main), so main must be
//! delayed by the offset to align. Equivalently: `ref[n] ≈ main[n − offset]`.

use realfft::RealFftPlanner;

/// Captures whose RMS falls below this (≈ −80 dBFS) are rejected outright.
pub const SILENCE_RMS_THRESHOLD: f64 = 1e-4;
/// Captures shorter than this are rejected as [`RejectReason::TooShort`]: a
/// normalized correlation over a handful of samples is statistically
/// meaningless (two *unrelated* loud samples score confidence ≈ 1). ~43 ms at
/// 48 kHz — far below anything the gated capture path produces (the gate's
/// hold alone is 250 ms); only a near-instant manual Stop ever gets here.
pub const MIN_ANALYSIS_SAMPLES: usize = 2048;
/// Normalized correlation below this means the signals are effectively
/// unrelated (two independent noise sources correlate near 0; two mics on one
/// source at 0 dB SNR still reach ≈ 0.5). Reject and keep the previous offset.
pub const CONFIDENCE_THRESHOLD: f32 = 0.2;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnalysisResult {
    /// `t_ref − t_main` in samples, sub-sample precise.
    pub offset_samples: f64,
    /// True if the best alignment has the signals polarity-inverted.
    pub inverted: bool,
    /// Normalized cross-correlation coefficient at the peak, in [0, 1].
    pub confidence: f32,
}

/// Why an analysis produced no usable offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// Shorter than [`MIN_ANALYSIS_SAMPLES`] or an empty search window.
    TooShort,
    /// Either signal below `SILENCE_RMS_THRESHOLD` (e.g. no sidechain
    /// connected).
    Silence,
    /// A non-finite sample (NaN/±Inf from a misbehaving upstream plugin)
    /// reached the capture; the correlation math cannot run on it. Without
    /// this check the FFT's own input validation rejects the capture with a
    /// misleading `TooShort`.
    NonFinite,
    /// Peak normalized correlation below `CONFIDENCE_THRESHOLD`.
    LowConfidence,
}

/// Everything an analysis run produced, including what a GUI needs to draw the
/// correlation curve — present even for `LowConfidence` rejections, so the
/// display can show *why* a capture failed.
#[derive(Debug, Clone)]
pub struct AnalysisReport {
    pub outcome: Result<AnalysisResult, RejectReason>,
    /// Normalized cross-correlation at every integer lag in
    /// `[-max_shift_samples, +max_shift_samples]`; index `i` holds lag
    /// `i - max_shift_samples`. Sign is preserved (an inverted-polarity match
    /// peaks negative), so the value at the peak equals ±`confidence`. Empty
    /// when the analysis was rejected before the FFT ran (`TooShort`,
    /// `Silence`).
    pub corr_curve: Vec<f32>,
    /// The half-window actually searched (the requested window clamped to what
    /// the FFT size allows).
    pub max_shift_samples: usize,
}

/// Estimates the time offset between `main` and `reference` (equal-length mono
/// captures recorded simultaneously), searching lags within
/// `±max_shift_samples`. Returns `None` when either signal is near-silent or
/// the correlation peak is too weak to trust.
pub fn analyze(main: &[f32], reference: &[f32], max_shift_samples: usize) -> Option<AnalysisResult> {
    analyze_detailed(main, reference, max_shift_samples)
        .outcome
        .ok()
}

/// Like [`analyze_detailed`], but for gated captures whose silent stretches
/// were spliced out. Each entry of `splices` is a position in the spliced
/// buffers where a new contiguous chunk begins; a cross-product straddling
/// such a seam pairs wall-clock-mismatched content and would smear the
/// correlation. Every straddling product at every searched lag has its main
/// index within `max_shift_samples` of a seam, so zeroing `[s − W, s + W)`
/// removes exactly the corrupted products. Both signals are zeroed
/// symmetrically so the energy normalization shrinks in step and the
/// confidence stays calibrated at the true peak.
///
/// Background thread only (allocates working copies).
pub fn analyze_spliced(
    main: &[f32],
    reference: &[f32],
    max_shift_samples: usize,
    splices: &[usize],
) -> AnalysisReport {
    if splices.is_empty() {
        return analyze_detailed(main, reference, max_shift_samples);
    }
    let n = main.len().min(reference.len());
    let w = max_shift_samples;
    let mut main_z = main[..n].to_vec();
    let mut ref_z = reference[..n].to_vec();
    for &s in splices {
        let lo = s.saturating_sub(w);
        let hi = s.saturating_add(w).min(n);
        if lo < hi {
            main_z[lo..hi].fill(0.0);
            ref_z[lo..hi].fill(0.0);
        }
    }
    let report = analyze_detailed(&main_z, &ref_z, max_shift_samples);

    // Dense seams × a large search window can erase most of a capture that
    // genuinely had signal; reporting that as Silence ("is the sidechain
    // connected?") would misdiagnose it. Downgrade to TooShort — too little
    // contiguous signal for this Max Shift.
    if report.outcome == Err(RejectReason::Silence) {
        let energy = |s: &[f32]| s.iter().map(|&x| x as f64 * x as f64).sum::<f64>();
        let silence_energy = SILENCE_RMS_THRESHOLD * SILENCE_RMS_THRESHOLD * n as f64;
        if energy(&main[..n]) >= silence_energy && energy(&reference[..n]) >= silence_energy {
            return AnalysisReport {
                outcome: Err(RejectReason::TooShort),
                corr_curve: Vec::new(),
                max_shift_samples: report.max_shift_samples,
            };
        }
    }
    report
}

/// Like [`analyze`], but also reports the correlation curve and the rejection
/// reason. Same cost; [`analyze`] is a thin wrapper around this.
pub fn analyze_detailed(
    main: &[f32],
    reference: &[f32],
    max_shift_samples: usize,
) -> AnalysisReport {
    let reject = |reason, max_shift_samples| AnalysisReport {
        outcome: Err(reason),
        corr_curve: Vec::new(),
        max_shift_samples,
    };

    let n = main.len().min(reference.len());
    if n < MIN_ANALYSIS_SAMPLES || max_shift_samples == 0 {
        return reject(RejectReason::TooShort, max_shift_samples);
    }

    let e_main: f64 = main[..n].iter().map(|&x| x as f64 * x as f64).sum();
    let e_ref: f64 = reference[..n].iter().map(|&x| x as f64 * x as f64).sum();
    // Before the silence check: NaN energies fail `<` and would sail past it.
    if !e_main.is_finite() || !e_ref.is_finite() {
        return reject(RejectReason::NonFinite, max_shift_samples);
    }
    let silence_energy = SILENCE_RMS_THRESHOLD * SILENCE_RMS_THRESHOLD * n as f64;
    if e_main < silence_energy || e_ref < silence_energy {
        return reject(RejectReason::Silence, max_shift_samples);
    }

    // Zero-pad past n + max_shift so every lag in the search window is a true
    // linear (not circular) correlation.
    let nfft = (n + max_shift_samples + 1).next_power_of_two();
    let max_shift = max_shift_samples.min(nfft / 2 - 2) as i64;

    let mut planner = RealFftPlanner::<f64>::new();
    let r2c = planner.plan_fft_forward(nfft);
    let c2r = planner.plan_fft_inverse(nfft);

    let mut buf_main = vec![0.0f64; nfft];
    let mut buf_ref = vec![0.0f64; nfft];
    for (dst, &src) in buf_main.iter_mut().zip(&main[..n]) {
        *dst = src as f64;
    }
    for (dst, &src) in buf_ref.iter_mut().zip(&reference[..n]) {
        *dst = src as f64;
    }

    let mut spec_main = r2c.make_output_vec();
    let mut spec_ref = r2c.make_output_vec();
    // The planner sized every buffer, so process() cannot fail; reject
    // defensively rather than panicking on a background thread.
    if r2c.process(&mut buf_main, &mut spec_main).is_err()
        || r2c.process(&mut buf_ref, &mut spec_ref).is_err()
    {
        return reject(RejectReason::TooShort, max_shift as usize);
    }

    // Cross-correlation theorem: DFT of Σ_n main[n]·ref[n+τ] is conj(MAIN)·REF.
    // The peak of that sequence sits at τ = offset under our sign convention
    // (ref[n] = main[n − d] ⇒ peak at τ = d).
    for (m, r) in spec_main.iter_mut().zip(&spec_ref) {
        *m = m.conj() * r;
    }
    // The inverse transform scrambles its input, so keep the cross-spectrum
    // for the sub-sample refinement below.
    let cross_spectrum = spec_main.clone();
    let mut corr = c2r.make_output_vec();
    if c2r.process(&mut spec_main, &mut corr).is_err() {
        return reject(RejectReason::TooShort, max_shift as usize);
    }

    // Negative lags live at the top of the buffer: corr[nfft + τ].
    let at = |lag: i64| -> f64 { corr[lag.rem_euclid(nfft as i64) as usize] };

    // corr is the unnormalized IFFT (scaled by nfft); dividing by the
    // geometric mean of the signal energies turns it into a proper correlation
    // coefficient — the same scale as `confidence`, so the curve's value at
    // the peak lag equals ±confidence.
    let norm = nfft as f64 * (e_main * e_ref).sqrt();
    let corr_curve: Vec<f32> = (-max_shift..=max_shift)
        .map(|lag| (at(lag) / norm) as f32)
        .collect();

    let mut peak_lag = 0i64;
    let mut peak_val = 0.0f64;
    for lag in -max_shift..=max_shift {
        let v = at(lag);
        if v.abs() > peak_val.abs() {
            peak_val = v;
            peak_lag = lag;
        }
    }

    let confidence = ((peak_val.abs() / norm).min(1.0)) as f32;
    if confidence < CONFIDENCE_THRESHOLD {
        return AnalysisReport {
            outcome: Err(RejectReason::LowConfidence),
            corr_curve,
            max_shift_samples: max_shift as usize,
        };
    }

    // Sub-sample refinement. A 3-point parabolic fit is biased on broadband
    // correlation peaks (the peak is sinc-shaped, not parabolic), so instead
    // evaluate the *continuous* cross-correlation — the exact DTFT of the
    // cross-spectrum — at fractional lags and maximize it directly. This has
    // no interpolation model to be wrong about; accuracy is noise-limited.
    let sign = peak_val.signum();
    let goal = |tau: f64| sign * continuous_corr(&cross_spectrum, nfft, tau);
    let refined = golden_section_max(goal, peak_lag as f64 - 0.6, peak_lag as f64 + 0.6);

    AnalysisReport {
        outcome: Ok(AnalysisResult {
            offset_samples: refined,
            inverted: peak_val < 0.0,
            confidence,
        }),
        corr_curve,
        max_shift_samples: max_shift as usize,
    }
}

/// The circular cross-correlation evaluated at a *fractional* lag `tau`:
/// the unnormalized inverse DFT of the cross-spectrum at a non-integer index.
/// Matches `corr[m]` exactly at integer `tau = m`.
fn continuous_corr(cross: &[realfft::num_complex::Complex<f64>], nfft: usize, tau: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * tau / nfft as f64;
    let nyquist = nfft / 2;
    let mut acc = cross[0].re;
    // The phasor e^{iwk} advances by one complex multiply per bin instead of
    // a sin/cos pair. This loop runs ~32 times per refinement over up to
    // nfft/2 bins, and the transcendental version dominated the whole
    // analysis at high sample rates — eroding the bounded wait budget in
    // `initialize()`. Reseeded from sin_cos periodically so rounding drift
    // stays ~1e-12, far below the estimate's noise floor.
    let (sin_w, cos_w) = w.sin_cos();
    let (mut ph_re, mut ph_im) = (1.0f64, 0.0f64); // e^{iw·0}
    const RESEED_EVERY: usize = 4096;
    for (k, c) in cross.iter().enumerate().skip(1) {
        if k % RESEED_EVERY == 0 {
            let (s, cs) = (w * k as f64).sin_cos();
            ph_re = cs;
            ph_im = s;
        } else {
            let re = ph_re * cos_w - ph_im * sin_w;
            let im = ph_re * sin_w + ph_im * cos_w;
            ph_re = re;
            ph_im = im;
        }
        // Re(c · e^{iwk}) — identical to the direct cos/sin form. Real-input
        // FFT stores only positive frequencies; interior bins represent
        // themselves plus their conjugate mirror.
        let re = c.re * ph_re - c.im * ph_im;
        acc += if k == nyquist { re } else { 2.0 * re };
    }
    acc
}

/// Golden-section search for the maximum of a unimodal function on [lo, hi].
fn golden_section_max(f: impl Fn(f64) -> f64, mut lo: f64, mut hi: f64) -> f64 {
    const INV_PHI: f64 = 0.618_033_988_749_894_9;
    let mut x1 = hi - INV_PHI * (hi - lo);
    let mut x2 = lo + INV_PHI * (hi - lo);
    let mut f1 = f(x1);
    let mut f2 = f(x2);
    // 30 iterations shrink the 1.2-sample bracket by phi^30 to ≈ 4e-7
    // samples — orders of magnitude below the noise floor of the estimate;
    // each evaluation is O(nfft) on a background thread, so iterations are
    // the analysis's dominant cost at high sample rates.
    for _ in 0..30 {
        if f1 > f2 {
            hi = x2;
            x2 = x1;
            f2 = f1;
            x1 = hi - INV_PHI * (hi - lo);
            f1 = f(x1);
        } else {
            lo = x1;
            x1 = x2;
            f1 = f2;
            x2 = lo + INV_PHI * (hi - lo);
            f2 = f(x2);
        }
    }
    (lo + hi) / 2.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic white noise (xorshift; no external RNG dependency).
    pub fn noise(len: usize, seed: u64) -> Vec<f32> {
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

    /// `ref[n] = main[n − k]` for integer k (positive k ⇒ main leads).
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

    /// Fractionally delayed copy via direct sinc resampling (test-quality,
    /// O(n·width) but exact for band-limited content).
    pub fn sinc_delayed_copy(signal: &[f32], delay: f64) -> Vec<f32> {
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
                    // Hann window over the sinc to bound truncation error.
                    let w = 0.5
                        + 0.5
                            * (std::f64::consts::PI * x / (width as f64 + 1.0))
                                .cos();
                    acc += signal[m as usize] as f64 * s * w;
                }
                acc as f32
            })
            .collect()
    }

    /// THE test that pins the sign convention. `ref[n] = main[n − k]` with
    /// k > 0 means main leads and the detected offset must equal +k.
    #[test]
    fn sign_convention() {
        let main = band_limited_noise(48_000, 42);
        let k = 237i64;
        let reference = delayed_copy(&main, k);
        let r = analyze(&main, &reference, 1000).expect("must detect");
        assert!(
            (r.offset_samples - k as f64).abs() < 0.1,
            "expected +{k}, got {}",
            r.offset_samples
        );
        assert!(!r.inverted);
    }

    #[test]
    fn integer_offsets_across_window() {
        let main = band_limited_noise(48_000, 7);
        for k in [-1000i64, -500, -1, 0, 1, 499, 1000] {
            let reference = delayed_copy(&main, k);
            let r = analyze(&main, &reference, 1000).expect("must detect");
            assert!(
                (r.offset_samples - k as f64).abs() < 0.1,
                "k={k}: got {}",
                r.offset_samples
            );
        }
    }

    #[test]
    fn fractional_offsets() {
        let main = band_limited_noise(48_000, 99);
        for d in [-100.5, -3.25, 0.5, 10.75, 500.33] {
            let reference = sinc_delayed_copy(&main, d); // ref[n] = main[n − d]
            let r = analyze(&main, &reference, 1000).expect("must detect");
            assert!(
                (r.offset_samples - d).abs() < 0.05,
                "d={d}: got {} (err {})",
                r.offset_samples,
                (r.offset_samples - d).abs()
            );
        }
    }

    #[test]
    fn inverted_polarity_detected() {
        let main = band_limited_noise(48_000, 5);
        for k in [-250i64, 0, 250] {
            let mut reference = delayed_copy(&main, k);
            for x in &mut reference {
                *x = -*x;
            }
            let r = analyze(&main, &reference, 1000).expect("must detect");
            assert!(r.inverted, "k={k}: polarity not detected");
            assert!(
                (r.offset_samples - k as f64).abs() < 0.1,
                "k={k}: got {}",
                r.offset_samples
            );
        }
    }

    #[test]
    fn robust_to_noise_at_0db_snr() {
        let main_clean = band_limited_noise(96_000, 11);
        let k = 333i64;
        let ref_clean = delayed_copy(&main_clean, k);
        let rms = |s: &[f32]| {
            (s.iter().map(|&x| x as f64 * x as f64).sum::<f64>() / s.len() as f64).sqrt()
        };
        let signal_rms = rms(&main_clean);
        let make_noisy = |clean: &[f32], seed: u64| -> Vec<f32> {
            let n = noise(clean.len(), seed);
            let noise_rms = rms(&n);
            clean
                .iter()
                .zip(&n)
                .map(|(&c, &x)| c + x * (signal_rms / noise_rms) as f32)
                .collect()
        };
        let main = make_noisy(&main_clean, 1001);
        let reference = make_noisy(&ref_clean, 2002);
        let r = analyze(&main, &reference, 1000).expect("must detect at 0 dB SNR");
        assert!(
            (r.offset_samples - k as f64).abs() < 0.5,
            "got {}",
            r.offset_samples
        );
    }

    #[test]
    fn robust_to_synthetic_reverb() {
        // Reference = delayed direct sound + exponentially decaying diffuse
        // tail (decaying noise convolved in, 100 ms at 48 kHz).
        let main = band_limited_noise(96_000, 21);
        let k = 150usize;
        let direct = delayed_copy(&main, k as i64);
        let ir: Vec<f32> = noise(4800, 77)
            .iter()
            .enumerate()
            .map(|(i, &x)| x * 0.3 * (-(i as f32) / 800.0).exp())
            .collect();
        let mut reference = direct.clone();
        for (i, out) in reference.iter_mut().enumerate() {
            let mut acc = 0.0f32;
            // Sparse convolution tail (every 4th tap) keeps the test fast while
            // still smearing energy over 100 ms.
            for (j, &h) in ir.iter().enumerate().step_by(4) {
                if i >= k + j {
                    acc += main[i - k - j] * h;
                }
            }
            *out += acc;
        }
        let r = analyze(&main, &reference, 1000).expect("must detect in reverb");
        assert!(
            (r.offset_samples - k as f64).abs() < 0.5,
            "got {}",
            r.offset_samples
        );
    }

    #[test]
    fn silence_rejected() {
        let main = vec![0.0f32; 48_000];
        let reference = band_limited_noise(48_000, 3);
        assert!(analyze(&main, &reference, 1000).is_none());
        assert!(analyze(&reference, &main, 1000).is_none());
        let tiny: Vec<f32> = reference.iter().map(|&x| x * 1e-6).collect();
        assert!(analyze(&tiny, &reference, 1000).is_none());
    }

    #[test]
    fn unrelated_noise_rejected() {
        let main = noise(48_000, 123);
        let reference = noise(48_000, 456);
        assert!(
            analyze(&main, &reference, 1000).is_none(),
            "independent noise must not produce a confident result"
        );
    }

    #[test]
    fn curve_peak_matches_confidence() {
        let main = band_limited_noise(48_000, 42);
        let k = 237i64;
        let reference = delayed_copy(&main, k);
        let report = analyze_detailed(&main, &reference, 1000);
        let r = report.outcome.expect("must detect");
        assert_eq!(report.max_shift_samples, 1000);
        assert_eq!(report.corr_curve.len(), 2 * 1000 + 1);
        let peak_idx = (report.max_shift_samples as i64 + k) as usize;
        let v = report.corr_curve[peak_idx];
        assert!(
            (v - r.confidence).abs() < 1e-3,
            "curve value {v} vs confidence {}",
            r.confidence
        );
        // The detected lag must be the curve's max-magnitude point.
        let max_idx = report
            .corr_curve
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
            .unwrap()
            .0;
        assert_eq!(max_idx, peak_idx);
    }

    #[test]
    fn inverted_curve_peak_is_negative() {
        let main = band_limited_noise(48_000, 5);
        let k = 250i64;
        let mut reference = delayed_copy(&main, k);
        for x in &mut reference {
            *x = -*x;
        }
        let report = analyze_detailed(&main, &reference, 1000);
        let r = report.outcome.expect("must detect");
        assert!(r.inverted);
        let v = report.corr_curve[(1000 + k) as usize];
        assert!(v < 0.0, "inverted peak must be negative, got {v}");
        assert!((v + r.confidence).abs() < 1e-3);
    }

    #[test]
    fn low_confidence_still_returns_curve() {
        let main = noise(48_000, 123);
        let reference = noise(48_000, 456);
        let report = analyze_detailed(&main, &reference, 1000);
        assert_eq!(report.outcome, Err(RejectReason::LowConfidence));
        assert_eq!(report.corr_curve.len(), 2 * 1000 + 1);
        let max_abs = report.corr_curve.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(max_abs < CONFIDENCE_THRESHOLD);
    }

    /// Build a gated-capture pair: cut the same signal-time windows out of a
    /// (main, ref) pair and concatenate them, recording each seam — exactly
    /// what the capture gate produces.
    fn splice_capture(
        main: &[f32],
        reference: &[f32],
        keep: &[(usize, usize)], // (start, len) windows in original time
    ) -> (Vec<f32>, Vec<f32>, Vec<usize>) {
        let mut m = Vec::new();
        let mut r = Vec::new();
        let mut seams = Vec::new();
        for &(start, len) in keep {
            if !m.is_empty() {
                seams.push(m.len());
            }
            m.extend_from_slice(&main[start..start + len]);
            r.extend_from_slice(&reference[start..start + len]);
        }
        (m, r, seams)
    }

    #[test]
    fn spliced_with_no_seams_is_identical() {
        let main = band_limited_noise(48_000, 42);
        let reference = delayed_copy(&main, 237);
        let a = analyze_detailed(&main, &reference, 1000);
        let b = analyze_spliced(&main, &reference, 1000, &[]);
        assert_eq!(a.corr_curve, b.corr_curve);
        assert_eq!(a.outcome, b.outcome);
    }

    #[test]
    fn spliced_capture_detects_offset() {
        let main = band_limited_noise(96_000, 21);
        let k = 300i64;
        let reference = delayed_copy(&main, k);
        let keep = [
            (1_000, 12_000),
            (25_000, 10_000),
            (48_000, 14_000),
            (75_000, 9_000),
        ];
        let (m, r, seams) = splice_capture(&main, &reference, &keep);
        assert_eq!(seams.len(), 3);
        let report = analyze_spliced(&m, &r, 1000, &seams);
        let res = report.outcome.expect("must detect");
        assert!(
            (res.offset_samples - k as f64).abs() < 0.1,
            "got {}",
            res.offset_samples
        );
        assert!(!res.inverted);
        // Symmetric guard zeroing keeps the confidence calibrated even with
        // ~13% of the buffers zeroed.
        assert!(res.confidence > 0.8, "confidence {}", res.confidence);
    }

    #[test]
    fn seams_near_buffer_edges_are_clamped() {
        let main = band_limited_noise(20_000, 3);
        let reference = delayed_copy(&main, 100);
        let report = analyze_spliced(&main, &reference, 1000, &[5, 19_999]);
        let res = report.outcome.expect("the un-zeroed middle must carry it");
        assert!((res.offset_samples - 100.0).abs() < 0.1);
    }

    #[test]
    fn guard_erased_capture_reports_too_short_not_silence() {
        // Chunks far shorter than the guard width: the zeroing erases the
        // whole capture. The raw signal is loud, so Silence ("is the
        // sidechain connected?") would misdiagnose it.
        let main = band_limited_noise(4_000, 9);
        let reference = delayed_copy(&main, 10);
        let seams: Vec<usize> = (1..8).map(|i| i * 500).collect();
        let report = analyze_spliced(&main, &reference, 1000, &seams);
        assert_eq!(report.outcome, Err(RejectReason::TooShort));
        assert!(report.corr_curve.is_empty());
    }

    #[test]
    fn early_rejects_have_empty_curves() {
        let quiet = vec![0.0f32; 48_000];
        let loud = band_limited_noise(48_000, 3);
        let report = analyze_detailed(&quiet, &loud, 1000);
        assert_eq!(report.outcome, Err(RejectReason::Silence));
        assert!(report.corr_curve.is_empty());
        let report = analyze_detailed(&loud[..1], &loud[..1], 1000);
        assert_eq!(report.outcome, Err(RejectReason::TooShort));
        assert!(report.corr_curve.is_empty());
    }
}
