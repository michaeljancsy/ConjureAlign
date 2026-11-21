//! Runs ConjureAlign's own analysis and correction on two WAV files, offline.
//!
//!   cargo run --release --example analyze_pair -- <ref.wav> <mic.wav>
//!       [--out corrected.wav] [--max-shift-ms 50] [--fit-start S] [--fit-end S]
//!
//! Prints one JSON object on stdout. With `--out`, writes the mic corrected by
//! the plug-in's own delay line (fractional delay + polarity flip) as 32-bit
//! float WAV, the same length and alignment as the input, so it sums
//! sample-for-sample with the reference.
//!
//! What it reuses from the plug-in: `analysis::analyze_spliced`, the capture
//! gate (`dsp::gate::CaptureGate`, threshold -60 dBFS, 2 s auto-finish, 4 s
//! maximum, seams recorded as splices) and `dsp::delay::AlignDelay`. Inputs are
//! averaged to mono (sum divided by channel count) as `process()` does. The
//! search window is `--max-shift-ms` (plug-in default 50 ms).
//!
//! Sign convention: `offset_samples = t_ref - t_mic`. Positive means the mic
//! leads the reference and is delayed by that amount to align.

use conjure_align::analysis::{self, RejectReason};
use conjure_align::capture::MAX_SPLICES;
use conjure_align::dsp::delay::{AlignDelay, TapSpec};
use conjure_align::dsp::fractional::FIR_CENTER;
use conjure_align::dsp::gate::CaptureGate;
use conjure_align::params::CAPTURE_MAX_SECS;

// The `gate_threshold` parameter's default (params.rs) and `CAPTURE_AUTO_FINISH_SECONDS`
// (lib.rs, private). Keep these in step with the plug-in.
const GATE_THRESHOLD_DB: f32 = -60.0;
const AUTO_FINISH_SECONDS: f32 = 2.0;
const DEFAULT_MAX_SHIFT_MS: f64 = 50.0;

struct Wav {
    rate: u32,
    /// `channels[c][frame]`
    channels: Vec<Vec<f32>>,
}

fn read_wav(path: &str) -> Result<Wav, String> {
    let mut r = hound::WavReader::open(path).map_err(|e| format!("{path}: {e}"))?;
    let spec = r.spec();
    let n_ch = spec.channels as usize;
    let samples: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Float, 32) => r
            .samples::<f32>()
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?,
        (hound::SampleFormat::Int, bits @ (16 | 24)) => {
            let scale = 1.0 / (1u64 << (bits - 1)) as f32;
            r.samples::<i32>()
                .map(|s| s.map(|v| v as f32 * scale))
                .collect::<Result<_, _>>()
                .map_err(|e| e.to_string())?
        }
        (f, b) => return Err(format!("{path}: unsupported format {f:?} {b}-bit")),
    };
    let mut channels = vec![Vec::with_capacity(samples.len() / n_ch); n_ch];
    for frame in samples.chunks_exact(n_ch) {
        for (c, &s) in frame.iter().enumerate() {
            channels[c].push(s);
        }
    }
    Ok(Wav {
        rate: spec.sample_rate,
        channels,
    })
}

fn write_wav(path: &str, rate: u32, channels: &[Vec<f32>]) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: channels.len() as u16,
        sample_rate: rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).map_err(|e| e.to_string())?;
    for i in 0..channels[0].len() {
        for ch in channels {
            w.write_sample(ch[i]).map_err(|e| e.to_string())?;
        }
    }
    w.finalize().map_err(|e| e.to_string())
}

/// Mono as the plug-in's capture makes it: sum divided by channel count.
fn mono(channels: &[Vec<f32>]) -> Vec<f32> {
    let n = channels[0].len();
    (0..n)
        .map(|i| channels.iter().map(|c| c[i]).sum::<f32>() / channels.len() as f32)
        .collect()
}

struct Outcome {
    max_shift_samples: usize,
    captured_samples: usize,
    splices: usize,
    result: Result<analysis::AnalysisResult, RejectReason>,
}

/// The capture-and-analyze half. `range` is in samples of the equal-length inputs.
fn analyze_pair(
    reference: &[f32],
    mic: &[f32],
    rate: u32,
    max_shift_ms: f64,
    range: (usize, usize),
) -> Outcome {
    let sr = rate as f32;
    let max_shift_samples = (max_shift_ms / 1000.0 * rate as f64).round() as usize;
    let cap_len = CAPTURE_MAX_SECS * rate as usize;
    let finish = (AUTO_FINISH_SECONDS * sr) as u32;
    let (lo, hi) = (range.0.min(mic.len()), range.1.min(mic.len()));

    // Same loop as `process()`: record only while the gate is open on both inputs.
    let mut gate = CaptureGate::new(sr, 10f32.powf(GATE_THRESHOLD_DB / 20.0));
    let mut m = Vec::new();
    let mut r = Vec::new();
    let mut splices = Vec::new();
    let mut prev_record = false;
    for i in lo..hi {
        if m.len() >= cap_len {
            break;
        }
        let record = gate.step(mic[i], reference[i]) || splices.len() == MAX_SPLICES;
        if record {
            if !prev_record && !m.is_empty() && splices.len() < MAX_SPLICES {
                splices.push(m.len());
            }
            m.push(mic[i]);
            r.push(reference[i]);
        }
        prev_record = record;
        if !m.is_empty() && gate.closed_streak() >= finish {
            break;
        }
    }
    let report = analysis::analyze_spliced(&m, &r, max_shift_samples, &splices);
    Outcome {
        max_shift_samples,
        captured_samples: m.len(),
        splices: splices.len(),
        result: report.outcome,
    }
}

/// The correction half: the mic through the plug-in's delay line at its
/// steady-state target, with the host's latency compensation applied (the first
/// `latency` output samples are dropped and `latency` zeros are fed at the end),
/// so the result is sample-aligned with the input.
fn correct(
    channels: &[Vec<f32>],
    offset_samples: f64,
    inverted: bool,
    max_shift: usize,
) -> Vec<Vec<f32>> {
    let latency = max_shift + FIR_CENTER;
    let offset = offset_samples.clamp(-(max_shift as f64), max_shift as f64);
    let n = channels[0].len();
    let mut delay = AlignDelay::new(channels.len(), 2 * max_shift + FIR_CENTER + 1, 64);
    delay.retarget(TapSpec {
        delay_samples: latency as f64 + offset,
        inverted,
    });
    delay.reset(); // settles on the target at once, no crossfade
    let mut padded: Vec<Vec<f32>> = channels
        .iter()
        .map(|c| {
            let mut v = c.clone();
            v.resize(n + latency, 0.0);
            v
        })
        .collect();
    let mut refs: Vec<&mut [f32]> = padded.iter_mut().map(|v| v.as_mut_slice()).collect();
    delay.process(&mut refs);
    padded.into_iter().map(|v| v[latency..].to_vec()).collect()
}

fn reason_str(r: RejectReason) -> &'static str {
    match r {
        RejectReason::TooShort => "too_short",
        RejectReason::Silence => "silence",
        RejectReason::NonFinite => "non_finite",
        RejectReason::LowConfidence => "low_confidence",
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut pos = Vec::new();
    let mut out = None;
    let mut max_ms = DEFAULT_MAX_SHIFT_MS;
    let (mut fit_start, mut fit_end) = (None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--out" => out = Some(val("--out")?),
            "--max-shift-ms" => max_ms = val(a)?.parse().map_err(|_| "bad --max-shift-ms")?,
            "--fit-start" => fit_start = Some(val(a)?.parse::<f64>().map_err(|_| "bad --fit-start")?),
            "--fit-end" => fit_end = Some(val(a)?.parse::<f64>().map_err(|_| "bad --fit-end")?),
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            _ => pos.push(a.clone()),
        }
    }
    if pos.len() != 2 {
        return Err("usage: analyze_pair <ref.wav> <mic.wav> [--out corrected.wav] [--max-shift-ms 50] [--fit-start S --fit-end S]".into());
    }
    let (rw, mw) = (read_wav(&pos[0])?, read_wav(&pos[1])?);
    if rw.rate != mw.rate {
        return Err(format!("sample rates differ: {} vs {}", rw.rate, mw.rate));
    }
    if !(1.0..=200.0).contains(&max_ms) {
        return Err("--max-shift-ms must be within 1..200".into());
    }
    // Equal length, as in a capture: pad the shorter with silence.
    let n = rw.channels[0].len().max(mw.channels[0].len());
    let pad = |mut chs: Vec<Vec<f32>>| {
        chs.iter_mut().for_each(|c| c.resize(n, 0.0));
        chs
    };
    let (rch, mch) = (pad(rw.channels), pad(mw.channels));
    let rate = rw.rate;
    let to_s = |s: f64| (s.max(0.0) * rate as f64).round() as usize;
    let range = (fit_start.map_or(0, to_s), fit_end.map_or(n, to_s));

    let o = analyze_pair(&mono(&rch), &mono(&mch), rate, max_ms, range);
    let (accepted, offset_samples, inverted, confidence, reason) = match o.result {
        Ok(r) => (true, r.offset_samples, r.inverted, r.confidence as f64, None),
        Err(e) => (false, 0.0, false, 0.0, Some(reason_str(e))),
    };
    let wrote = if let Some(path) = &out {
        let corrected = if accepted {
            correct(&mch, offset_samples, inverted, o.max_shift_samples)
        } else {
            mch.clone()
        };
        write_wav(path, rate, &corrected)?;
        Some(path.clone())
    } else {
        None
    };
    let out_note = match (&wrote, accepted) {
        (Some(_), true) => "mic corrected by the plug-in's delay (net shift = offset), same length and alignment as the input",
        (Some(_), false) => "analysis rejected: mic written unchanged",
        _ => "no --out given",
    };
    let j = serde_json::json!({
        "accepted": accepted,
        "reject_reason": reason,
        "offset_samples": offset_samples,
        "offset_ms": offset_samples / rate as f64 * 1000.0,
        "offset_note": "offset = t_ref - t_mic; positive means the mic leads the reference and is delayed by this much to align",
        "polarity_inverted": inverted,
        "confidence": confidence,
        "sample_rate": rate,
        "max_shift_ms": max_ms,
        "max_shift_samples": o.max_shift_samples,
        "captured_samples": o.captured_samples,
        "splices": o.splices,
        "out_file": wrote,
        "out_note": out_note,
    });
    println!("{j}");
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(len: usize, seed: u64) -> Vec<f32> {
        let mut s = seed.max(1);
        (0..len)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                ((s >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) as f32 * 0.5
            })
            .collect()
    }

    fn rms(s: &[f32]) -> f64 {
        (s.iter().map(|&x| x as f64 * x as f64).sum::<f64>() / s.len() as f64).sqrt()
    }

    /// The reference is the mic delayed by `lead` samples (so the mic leads),
    /// and the mic optionally has its polarity flipped.
    fn pair(n: usize, lead: usize, invert: bool) -> (Vec<f32>, Vec<f32>) {
        let base = noise(n, 7);
        let mut reference = vec![0.0f32; n];
        reference[lead..].copy_from_slice(&base[..n - lead]);
        let mic = if invert {
            base.iter().map(|x| -x).collect()
        } else {
            base
        };
        (reference, mic)
    }

    #[test]
    fn delay_and_polarity_come_back_and_output_nulls() {
        for &(lead, invert) in &[(120usize, false), (95, true)] {
            let rate = 48_000;
            let (reference, mic) = pair(5 * rate as usize, lead, invert);
            let o = analyze_pair(&reference, &mic, rate, 50.0, (0, reference.len()));
            let r = o.result.expect("accepted");
            assert!(
                (r.offset_samples - lead as f64).abs() < 0.01,
                "{}",
                r.offset_samples
            );
            assert_eq!(r.inverted, invert);
            assert!(o.captured_samples <= 4 * rate as usize);

            let out = correct(std::slice::from_ref(&mic), r.offset_samples, r.inverted, o.max_shift_samples);
            assert_eq!(out[0].len(), mic.len());
            let (a, b) = (4096, mic.len() - 4096);
            let resid: Vec<f32> = (a..b).map(|i| out[0][i] - reference[i]).collect();
            let depth = 20.0 * (rms(&resid) / rms(&reference[a..b])).log10();
            assert!(depth < -80.0, "null depth {depth} dB");
        }
    }

    #[test]
    fn silent_mic_is_rejected() {
        let reference = noise(96_000, 3);
        let o = analyze_pair(&reference, &vec![0.0; 96_000], 48_000, 50.0, (0, 96_000));
        assert!(o.result.is_err());
    }
}
