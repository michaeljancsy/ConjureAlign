//! End-to-end gated capture without a plugin host: drive the capture gate
//! sample-by-sample over bursty material exactly as `process()` would
//! (gating, splice bookkeeping, the force-open-at-capacity rule, the
//! buffer-full stop), then run the splice-aware analysis and apply the
//! correction through the delay line, measuring the residual.

use audio_align::analysis;
use audio_align::dsp::delay::{AlignDelay, TapSpec};
use audio_align::dsp::fractional::FIR_CENTER;
use audio_align::dsp::gate::CaptureGate;

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

/// `out[n] = signal[n − k]` for integer k (positive k ⇒ main leads).
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

/// A clip shorter than the buffer must not leave the capture paused forever:
/// once the gate has been closed for the auto-finish window (2 s, matching
/// `CAPTURE_AUTO_FINISH_SECONDS` in lib.rs), process() stops and analyzes.
/// This drives the same loop process() runs, including the auto-finish check.
#[test]
fn short_clip_auto_finishes_and_detects() {
    let sr = 48_000.0f32;
    let n = 240_000; // 5 s timeline: one 1.2 s burst, then silence forever
    let true_offset = 240i64;
    let auto_finish = (2.0 * sr) as u32;

    let src = band_limited_noise(n, 7);
    let main: Vec<f32> = src
        .iter()
        .enumerate()
        .map(|(i, &x)| {
            let t = i as f32 / sr;
            if (0.3..1.5).contains(&t) {
                x
            } else {
                0.0
            }
        })
        .collect();
    let reference = delayed_copy(&main, true_offset);

    let mut gate = CaptureGate::new(sr, 1e-3);
    let cap_len = 192_000usize;
    let mut cm = vec![0.0f32; cap_len];
    let mut cr = vec![0.0f32; cap_len];
    let mut filled = 0usize;
    let mut finished = false;
    for i in 0..n {
        if filled >= cap_len {
            break;
        }
        if gate.step(main[i], reference[i]) {
            cm[filled] = main[i];
            cr[filled] = reference[i];
            filled += 1;
        }
        if filled > 0 && gate.closed_streak() >= auto_finish {
            finished = true;
            break;
        }
    }
    assert!(finished, "auto-finish must fire on sustained silence");
    // Recorded ≈ the burst plus the gate's release+hold hang, nowhere near
    // the 4 s buffer.
    assert!(
        (57_600..120_000).contains(&filled),
        "expected ≈1.2–2 s recorded, got {filled}"
    );

    let report = analysis::analyze_spliced(&cm[..filled], &cr[..filled], 2400, &[]);
    let result = report.outcome.expect("short clip must still analyze");
    assert!(
        (result.offset_samples - true_offset as f64).abs() < 0.1,
        "detected {}",
        result.offset_samples
    );
}

#[test]
fn gated_capture_analyze_correct_leaves_residual_below_a_tenth_sample() {
    let sr = 48_000.0f32;
    let n = 384_000; // 8 s of source material
    let true_offset = 300i64;
    let max_shift = 2400usize; // 50 ms at 48 kHz
    let cap_len = 192_000usize; // the 4 s capture buffer

    // Bursty source: 0.6 s bursts separated by 1.0 s of true silence — long
    // enough for the gate's release (+ hold) to actually close between them.
    let src = band_limited_noise(n, 4242);
    let main: Vec<f32> = src
        .iter()
        .enumerate()
        .map(|(i, &x)| {
            let t = i as f32 / sr;
            if (t / 1.6).fract() < 0.375 {
                x
            } else {
                0.0
            }
        })
        .collect();
    let reference = delayed_copy(&main, true_offset);

    // --- The capture loop, exactly as process() runs it ---
    let mut gate = CaptureGate::new(sr, 1e-3); // −60 dBFS default threshold
    let mut cm = vec![0.0f32; cap_len];
    let mut cr = vec![0.0f32; cap_len];
    let mut splices: Vec<usize> = Vec::with_capacity(64);
    let cap = splices.capacity();
    let mut filled = 0usize;
    let mut prev_record = false;
    for i in 0..n {
        if filled >= cap_len {
            break; // the buffer-full auto-stop
        }
        let record = gate.step(main[i], reference[i]) || splices.len() == cap;
        if record {
            if !prev_record && filled > 0 && splices.len() < cap {
                splices.push(filled);
            }
            cm[filled] = main[i];
            cr[filled] = reference[i];
            filled += 1;
        }
        prev_record = record;
    }
    assert_eq!(filled, cap_len, "8 s of bursts must fill the 4 s gated buffer");
    assert!(
        !splices.is_empty() && splices.len() <= 4,
        "expected a few seams, got {:?}",
        splices
    );

    // --- Splice-aware analysis (what the background task does) ---
    let report = analysis::analyze_spliced(&cm[..filled], &cr[..filled], max_shift, &splices);
    let result = report.outcome.expect("analysis must succeed");
    assert!(
        (result.offset_samples - true_offset as f64).abs() < 0.1,
        "detected {} vs true {true_offset}",
        result.offset_samples
    );
    assert!(!result.inverted);
    assert!(result.confidence > 0.8, "confidence {}", result.confidence);

    // --- Correction (what process() does with the result) ---
    let latency = max_shift + FIR_CENTER;
    let mut delay = AlignDelay::new(1, 2 * max_shift + FIR_CENTER + 64, 64);
    delay.retarget(TapSpec {
        delay_samples: latency as f64 + result.offset_samples,
        inverted: result.inverted,
    });
    delay.reset();
    let mut output = cm[..filled].to_vec();
    let mut channels: Vec<&mut [f32]> = vec![output.as_mut_slice()];
    delay.process(&mut channels);

    // --- Residual, as the host would hear it after PDC ---
    // The delay line runs straight across the seams, so around each one the
    // shifted main briefly pairs the previous chunk's tail against the new
    // chunk's reference; a residual search window of 600 samples makes the
    // splice guards (width = the window) cover that mismatch region
    // (true_offset + FIR tail ≈ 364 samples).
    let compensated: Vec<f32> = output[latency..].to_vec();
    let residual = analysis::analyze_spliced(
        &compensated,
        &cr[..compensated.len()],
        600,
        &splices,
    )
    .outcome
    .expect("residual analysis must succeed");
    assert!(
        residual.offset_samples.abs() < 0.1,
        "residual misalignment {} samples",
        residual.offset_samples
    );
    assert!(!residual.inverted);
}
