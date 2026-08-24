//! End-to-end DSP test without a plugin host: synthesize a mic pair with a
//! known fractional offset, run the analysis, apply the correction through the
//! delay line exactly as `process()` would, and measure the residual
//! misalignment of the output against the reference.

use conjure_align::analysis;
use conjure_align::dsp::delay::{AlignDelay, TapSpec};
use conjure_align::dsp::fractional::FIR_CENTER;

/// Deterministic white noise (xorshift).
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

fn band_limited_noise(len: usize, seed: u64) -> Vec<f32> {
    let white = noise(len + 16, seed);
    (0..len)
        .map(|i| white[i..i + 16].iter().sum::<f32>() / 4.0)
        .collect()
}

/// `out[n] = signal(n − delay)` via windowed-sinc resampling (test-quality).
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
                let w = 0.5 + 0.5 * (std::f64::consts::PI * x / (width as f64 + 1.0)).cos();
                acc += signal[m as usize] as f64 * s * w;
            }
            acc as f32
        })
        .collect()
}

#[test]
fn capture_analyze_correct_leaves_residual_below_a_tenth_sample() {
    let sample_rate = 48_000.0f64;
    let n = 96_000; // 2 s capture
    let max_shift = 2400usize; // 50 ms at 48 kHz

    // The reference mic hears the source `true_offset` samples later than the
    // main mic (main leads), with polarity flipped for good measure.
    let true_offset = 12.34f64;
    let main = band_limited_noise(n, 4242);
    let reference: Vec<f32> = sinc_delayed_copy(&main, true_offset)
        .iter()
        .map(|&x| -x)
        .collect();

    // --- Analysis (what the background task does) ---
    let result = analysis::analyze(&main, &reference, max_shift).expect("analysis must succeed");
    assert!(result.inverted, "polarity flip must be detected");
    assert!(
        (result.offset_samples - true_offset).abs() < 0.05,
        "detected {} vs true {true_offset}",
        result.offset_samples
    );
    assert!(result.confidence > 0.9, "confidence {}", result.confidence);

    // --- Correction (what process() does with the result) ---
    let latency = max_shift + FIR_CENTER; // the PDC trick's reported latency
    let mut delay = AlignDelay::new(1, 2 * max_shift + FIR_CENTER + 64, 64);
    delay.retarget(TapSpec {
        delay_samples: latency as f64 + result.offset_samples,
        inverted: result.inverted,
    });
    delay.reset(); // snap to target like initialize() does

    let mut output = main.clone();
    let mut channels: Vec<&mut [f32]> = vec![output.as_mut_slice()];
    delay.process(&mut channels);

    // --- Measure the residual as the host would hear it ---
    // The host's PDC advances the plugin output by `latency` samples:
    // compensated[n] = output[n + latency] = −main(n − offset) ≈ reference[n]
    // — the plugin's own polarity flip already matches it to the reference.
    // Cross-correlate against the reference to measure any remaining error.
    let compensated: Vec<f32> = output[latency..].to_vec();
    let residual = analysis::analyze(&compensated, &reference[..compensated.len()], 100)
        .expect("residual analysis must succeed");
    assert!(
        residual.offset_samples.abs() < 0.1,
        "residual misalignment {} samples",
        residual.offset_samples
    );
    assert!(!residual.inverted, "polarity must now match");

    // Also check ms round-tripping, since the plugin persists ms not samples.
    let offset_ms = result.offset_samples / sample_rate * 1000.0;
    let back_to_samples = offset_ms as f32 as f64 / 1000.0 * sample_rate;
    assert!(
        (back_to_samples - result.offset_samples).abs() < 0.01,
        "f32 ms round-trip lost too much precision: {back_to_samples}"
    );
}
