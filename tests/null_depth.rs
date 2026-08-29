//! Measures the achievable null depth of the full correction path — how many
//! dB below the reference the residual sits after capture → analyze → correct.
//!
//! This is a stricter metric than residual *offset*: null depth is limited by
//! the fractional interpolator's response near Nyquist, not just by detection
//! accuracy. A fractional delay is mathematically impossible to realize AT
//! Nyquist for a real filter, so content in the top ~5% of the band cannot
//! null. Integer offsets have no such limit. Run with `--nocapture` to see the
//! measured table.

use conjure_align::analysis;
use conjure_align::dsp::delay::{AlignDelay, TapSpec};
use conjure_align::dsp::fractional::FIR_CENTER;

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

/// One-pole lowpass — crude "musical spectrum" (guitar-ish energy rolloff).
fn lowpassed_noise(len: usize, seed: u64, coeff: f32) -> Vec<f32> {
    let white = noise(len, seed);
    let mut y = 0.0f32;
    white
        .iter()
        .map(|&x| {
            y += coeff * (x - y);
            y
        })
        .collect()
}

/// Exact fractional delay via the DFT: multiplies the full-buffer spectrum by
/// e^{−jωd}. Circular, so only the interior (away from the wrap region) is
/// valid — the measurement below skips generous margins. Unlike a windowed-
/// sinc resampler this has NO in-band response error of its own, so the
/// measured null reflects the plugin path, not the test harness.
fn exact_delayed_copy(signal: &[f32], delay: f64) -> Vec<f32> {
    use realfft::RealFftPlanner;
    let n = signal.len();
    let mut planner = RealFftPlanner::<f64>::new();
    let r2c = planner.plan_fft_forward(n);
    let c2r = planner.plan_fft_inverse(n);
    let mut buf: Vec<f64> = signal.iter().map(|&x| x as f64).collect();
    let mut spec = r2c.make_output_vec();
    r2c.process(&mut buf, &mut spec).unwrap();
    let nyquist = n / 2;
    for (k, c) in spec.iter_mut().enumerate() {
        let omega = 2.0 * std::f64::consts::PI * k as f64 / n as f64;
        let phase = -omega * delay;
        *c *= realfft::num_complex::Complex::new(phase.cos(), phase.sin());
        // The inverse real FFT requires purely real DC/Nyquist bins; zeroing
        // the Nyquist imaginary part discards un-delayable content there.
        if k == nyquist && n.is_multiple_of(2) {
            c.im = 0.0;
        }
    }
    let mut out = c2r.make_output_vec();
    c2r.process(&mut spec, &mut out).unwrap();
    out.iter().map(|&x| (x / n as f64) as f32).collect()
}

fn rms(s: &[f32]) -> f64 {
    (s.iter().map(|&x| x as f64 * x as f64).sum::<f64>() / s.len() as f64).sqrt()
}

/// RMS of only the content below `cutoff` (fraction of the sample rate),
/// via Parseval over the FFT bins. Used to measure the *audible-band* null:
/// fractional delay is unrealizable near Nyquist, so broadband null tests are
/// floored by ultrasonic residual that this measurement excludes.
fn rms_below(signal: &[f32], cutoff: f64) -> f64 {
    use realfft::RealFftPlanner;
    let n = signal.len();
    let mut planner = RealFftPlanner::<f64>::new();
    let r2c = planner.plan_fft_forward(n);
    let mut buf: Vec<f64> = signal.iter().map(|&x| x as f64).collect();
    let mut spec = r2c.make_output_vec();
    r2c.process(&mut buf, &mut spec).unwrap();
    let max_bin = (cutoff * n as f64) as usize;
    let mut power = 0.0f64;
    for (k, c) in spec.iter().enumerate().take(max_bin.min(spec.len())) {
        let weight = if k == 0 { 1.0 } else { 2.0 };
        power += weight * c.norm_sqr();
    }
    (power / (n as f64 * n as f64)).sqrt()
}

/// Full pipeline: detect the offset, correct main, subtract the reference the
/// way an invert-and-sum null test does. Returns (broadband, audible-band)
/// depths in dB (more negative = deeper null); the audible-band figure
/// excludes content above 0.44·fs (≈19.4 kHz at 44.1 kHz).
fn measure_null_depth(main: &[f32], reference: &[f32], max_shift: usize) -> (f64, f64) {
    let result = analysis::analyze(main, reference, max_shift).expect("detection failed");

    let latency = max_shift + FIR_CENTER;
    let mut delay = AlignDelay::new(1, 2 * max_shift + FIR_CENTER + 64, 64);
    delay.retarget(TapSpec {
        delay_samples: latency as f64 + result.offset_samples,
        inverted: result.inverted,
    });
    delay.reset();

    let mut output = main.to_vec();
    let mut channels: Vec<&mut [f32]> = vec![output.as_mut_slice()];
    delay.process(&mut channels);

    // Post-PDC output, trimmed to steady state (skip edges).
    let n = main.len();
    let lo = latency + 4096;
    let hi = n - 4096;
    let residual: Vec<f32> = (lo..hi)
        .map(|i| output[i] - reference[i - latency])
        .collect();
    let reference = &reference[lo - latency..hi - latency];
    let broadband = 20.0 * (rms(&residual) / rms(reference)).log10();
    let audible = 20.0 * (rms_below(&residual, 0.44) / rms_below(reference, 0.44)).log10();
    (broadband, audible)
}

#[test]
fn null_depth_table() {
    let n = 96_000;
    let max_shift = 2400;

    // Integer offset: nothing fundamental limits the null.
    let main = noise(n, 1);
    let reference = {
        let mut r = vec![0.0f32; n];
        r[240..].copy_from_slice(&main[..n - 240]);
        r
    };
    let int_white = measure_null_depth(&main, &reference, max_shift);

    // Fractional offset, full-band white noise: worst case — ~10% of the
    // energy lives above 0.45·fs where a real fractional delay cannot be flat.
    let reference = exact_delayed_copy(&main, 240.5);
    let frac_white = measure_null_depth(&main, &reference, max_shift);

    // Fractional offset, band-limited material (survives like real music).
    let main_lp = lowpassed_noise(n, 2, 0.5); // ≈ −3 dB around 0.09·fs
    let reference = exact_delayed_copy(&main_lp, 240.5);
    let frac_musical = measure_null_depth(&main_lp, &reference, max_shift);

    let main_dark = lowpassed_noise(n, 3, 0.2); // darker still
    let reference = exact_delayed_copy(&main_dark, 240.5);
    let frac_dark = measure_null_depth(&main_dark, &reference, max_shift);

    println!("null depth (broadband / audible band <0.44·fs):");
    println!(
        "  integer offset, white noise:      {:8.1} dB / {:8.1} dB",
        int_white.0, int_white.1
    );
    println!(
        "  fractional offset, white noise:   {:8.1} dB / {:8.1} dB",
        frac_white.0, frac_white.1
    );
    println!(
        "  fractional offset, musical-ish:   {:8.1} dB / {:8.1} dB",
        frac_musical.0, frac_musical.1
    );
    println!(
        "  fractional offset, dark material: {:8.1} dB / {:8.1} dB",
        frac_dark.0, frac_dark.1
    );

    // Documented expectations: integer offsets null essentially perfectly;
    // fractional offsets null deeply in the audible band but are floored
    // broadband by near-Nyquist content no real filter can fractionally delay.
    assert!(
        int_white.0 < -80.0,
        "integer null too shallow: {} dB",
        int_white.0
    );
    assert!(
        frac_white.1 < -60.0,
        "audible-band fractional null too shallow: {} dB",
        frac_white.1
    );
    assert!(
        frac_musical.0 < -20.0,
        "broadband fractional null shallower than documented: {} dB",
        frac_musical.0
    );
}
