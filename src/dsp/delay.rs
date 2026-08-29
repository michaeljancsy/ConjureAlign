//! Multi-channel delay line with dual read taps and click-free retargeting.
//!
//! The line always outputs through one active tap. When the target delay or
//! polarity changes, a second tap is created at the new position and the output
//! crossfades equal-power from old to new over the fade length. Retargets that
//! arrive mid-fade are coalesced: only the latest pending target is kept and it
//! starts fading once the current fade completes.

use crate::dsp::fractional::{design_kernel, FIR_CENTER, FIR_LEN};

/// A complete description of where the line should read from.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TapSpec {
    /// Total delay in samples, *including* the interpolator's `FIR_CENTER`
    /// group delay. Must be in `[FIR_CENTER, max_delay]`.
    pub delay_samples: f64,
    /// Whether the tap output has its polarity inverted.
    pub inverted: bool,
}

struct Tap {
    spec: TapSpec,
    int_delay: usize,
    kernel: [f32; FIR_LEN],
    gain: f32,
}

impl Tap {
    /// Allocation-free; safe on the audio thread.
    fn new(spec: TapSpec) -> Self {
        let d = spec.delay_samples - FIR_CENTER as f64;
        debug_assert!(d >= 0.0, "delay {} below FIR_CENTER", spec.delay_samples);
        let d = d.max(0.0);
        let int_delay = d.floor() as usize;
        let frac = d - int_delay as f64;
        let mut kernel = [0.0f32; FIR_LEN];
        design_kernel(frac, &mut kernel);
        Self {
            spec,
            int_delay,
            kernel,
            gain: if spec.inverted { -1.0 } else { 1.0 },
        }
    }
}

pub struct AlignDelay {
    /// One circular buffer per channel; length is a power of two.
    bufs: Vec<Vec<f32>>,
    mask: usize,
    write_pos: usize,
    active: Tap,
    fading_in: Option<Tap>,
    fade_pos: usize,
    fade_len: usize,
    pending: Option<TapSpec>,
}

impl AlignDelay {
    /// `max_delay_samples` is the largest `TapSpec::delay_samples` that will
    /// ever be requested; the buffer is sized once here so retargeting never
    /// allocates.
    pub fn new(num_channels: usize, max_delay_samples: usize, fade_len: usize) -> Self {
        let capacity = (max_delay_samples + FIR_LEN + 1).next_power_of_two();
        Self {
            bufs: vec![vec![0.0; capacity]; num_channels],
            mask: capacity - 1,
            write_pos: 0,
            active: Tap::new(TapSpec {
                delay_samples: FIR_CENTER as f64,
                inverted: false,
            }),
            fading_in: None,
            fade_pos: 0,
            fade_len: fade_len.max(1),
            pending: None,
        }
    }

    /// Clears audio history and finishes any fade instantly (used on transport
    /// resets, where a click is impossible anyway because history is zeroed).
    pub fn reset(&mut self) {
        for buf in &mut self.bufs {
            buf.fill(0.0);
        }
        self.write_pos = 0;
        let final_spec = self.ultimate_target();
        if final_spec != self.active.spec {
            self.active = Tap::new(final_spec);
        }
        self.fading_in = None;
        self.pending = None;
        self.fade_pos = 0;
    }

    /// The spec the line will have settled on once all fades complete.
    pub fn ultimate_target(&self) -> TapSpec {
        self.pending
            .or(self.fading_in.as_ref().map(|t| t.spec))
            .unwrap_or(self.active.spec)
    }

    /// Requests a new delay/polarity. No-op if it already matches the final
    /// target; otherwise starts a crossfade (or queues it behind the current
    /// one, replacing any previously queued target).
    pub fn retarget(&mut self, spec: TapSpec) {
        if spec == self.ultimate_target() {
            return;
        }
        if self.fading_in.is_some() {
            self.pending = Some(spec);
        } else {
            self.fading_in = Some(Tap::new(spec));
            self.fade_pos = 0;
        }
    }

    fn read_tap(&self, tap: &Tap, ch: usize) -> f32 {
        let buf = &self.bufs[ch];
        let base = self.write_pos.wrapping_sub(tap.int_delay);
        let mut acc = 0.0f32;
        for (k, &h) in tap.kernel.iter().enumerate() {
            acc += h * buf[base.wrapping_sub(k) & self.mask];
        }
        acc * tap.gain
    }

    /// Processes one block in place. All channel slices must share one length.
    pub fn process(&mut self, channels: &mut [&mut [f32]]) {
        if channels.is_empty() {
            return;
        }
        let num_samples = channels[0].len();
        debug_assert!(channels.iter().all(|ch| ch.len() == num_samples));
        debug_assert!(channels.len() <= self.bufs.len());

        for i in 0..num_samples {
            for (ch, samples) in channels.iter_mut().enumerate() {
                self.bufs[ch][self.write_pos & self.mask] = samples[i];
            }
            match &self.fading_in {
                Some(tap_in) => {
                    // Equal-power crossfade, evaluated at the midpoint of each
                    // fade step.
                    let t = (self.fade_pos as f32 + 0.5) / self.fade_len as f32;
                    let theta = t * std::f32::consts::FRAC_PI_2;
                    let (g_out, g_in) = (theta.cos(), theta.sin());
                    for (ch, samples) in channels.iter_mut().enumerate() {
                        samples[i] = g_out * self.read_tap(&self.active, ch)
                            + g_in * self.read_tap(tap_in, ch);
                    }
                }
                None => {
                    for (ch, samples) in channels.iter_mut().enumerate() {
                        samples[i] = self.read_tap(&self.active, ch);
                    }
                }
            }
            self.write_pos = self.write_pos.wrapping_add(1);
            if self.fading_in.is_some() {
                self.fade_pos += 1;
                if self.fade_pos >= self.fade_len {
                    self.active = self.fading_in.take().unwrap();
                    self.fade_pos = 0;
                    if let Some(next) = self.pending.take() {
                        // Coalescing can queue the very spec that just
                        // landed (A→B, then C then B again within one
                        // fade). That target is already satisfied, and
                        // "fading" a tap into itself is not a no-op: the
                        // equal-power gains sum to √2 mid-fade, a +3 dB
                        // bump. Start a fade only toward a different spec.
                        if next != self.active.spec {
                            self.fading_in = Some(Tap::new(next));
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
// Index loops compare output[i] against analytic functions of i; iterator
// rewrites would obscure that.
#[allow(clippy::needless_range_loop)]
mod tests {
    use super::*;

    fn process_all(delay: &mut AlignDelay, input: &[Vec<f32>]) -> Vec<Vec<f32>> {
        let mut out: Vec<Vec<f32>> = input.to_vec();
        let mut refs: Vec<&mut [f32]> = out.iter_mut().map(|v| v.as_mut_slice()).collect();
        delay.process(&mut refs);
        out
    }

    #[test]
    fn impulse_lands_at_exact_integer_delay() {
        for d_total in [FIR_CENTER, 100, 500, 4095] {
            let mut delay = AlignDelay::new(1, 8192, 64);
            delay.reset();
            delay.retarget(TapSpec {
                delay_samples: d_total as f64,
                inverted: false,
            });
            // Let the initial fade settle on silence first.
            process_all(&mut delay, &[vec![0.0; 256]]);
            let mut input = vec![0.0f32; d_total + 256];
            input[0] = 1.0;
            let out = process_all(&mut delay, &[input]);
            let peak = out[0]
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
                .unwrap();
            assert_eq!(
                peak.0, d_total,
                "impulse arrived at {} not {d_total}",
                peak.0
            );
            assert!((peak.1 - 1.0).abs() < 1e-4, "peak amplitude {}", peak.1);
        }
    }

    #[test]
    fn fractional_delay_shifts_sine_phase() {
        let d_total = 100.25f64;
        let freq = 0.05; // cycles per sample
        let mut delay = AlignDelay::new(1, 8192, 64);
        delay.retarget(TapSpec {
            delay_samples: d_total,
            inverted: false,
        });
        process_all(&mut delay, &[vec![0.0; 512]]); // settle fade on silence
        let n = 4096;
        let input: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * freq * i as f64).sin() as f32)
            .collect();
        let out = process_all(&mut delay, &[input]);
        // Compare against the analytically delayed sine, well after startup.
        for i in 1000..n {
            let expected = (2.0 * std::f64::consts::PI * freq * (i as f64 - d_total)).sin() as f32;
            assert!(
                (out[0][i] - expected).abs() < 1e-3,
                "sample {i}: {} vs {expected}",
                out[0][i]
            );
        }
    }

    #[test]
    fn polarity_inversion_flips_sign() {
        let mut delay = AlignDelay::new(1, 1024, 8);
        delay.retarget(TapSpec {
            delay_samples: FIR_CENTER as f64,
            inverted: true,
        });
        process_all(&mut delay, &[vec![0.0; 64]]);
        let input = vec![0.5f32; 256];
        let out = process_all(&mut delay, &[input]);
        assert!((out[0][200] + 0.5).abs() < 1e-4, "got {}", out[0][200]);
    }

    #[test]
    fn crossfade_is_click_free_and_settles() {
        let freq = 0.01;
        let n = 48_000;
        let mut delay = AlignDelay::new(1, 8192, 2400);
        delay.retarget(TapSpec {
            delay_samples: 200.0,
            inverted: false,
        });
        // Settle the initial fade with real signal so the retarget below is the
        // only transition under test.
        let settle: Vec<f32> = (0..8192)
            .map(|i| (2.0 * std::f64::consts::PI * freq * i as f64).sin() as f32)
            .collect();
        process_all(&mut delay, &[settle]);
        // Retarget mid-stream.
        delay.retarget(TapSpec {
            delay_samples: 700.5,
            inverted: false,
        });
        let input: Vec<f32> = (8192..8192 + n)
            .map(|i| (2.0 * std::f64::consts::PI * freq * i as f64).sin() as f32)
            .collect();
        let out = process_all(&mut delay, &[input]);
        // No sample-to-sample discontinuity beyond what the crossfade of two
        // out-of-phase sines can legitimately produce: each tap's step is at
        // most 2πf, and with equal-power gains the combined step is bounded by
        // √2·2πf (plus a tiny gain-slope term). A genuine click — an unfaded
        // tap swap — would step by O(amplitude difference), up to 2.0 here.
        let max_step = std::f64::consts::SQRT_2 * 2.0 * std::f64::consts::PI * freq + 0.02;
        for i in 1..n {
            let step = (out[0][i] - out[0][i - 1]).abs() as f64;
            assert!(step < max_step, "click at {i}: step {step}");
        }
        // After the fade the output equals the pure new-tap signal.
        for i in n - 1000..n {
            let expected =
                (2.0 * std::f64::consts::PI * freq * ((8192 + i) as f64 - 700.5)).sin() as f32;
            assert!(
                (out[0][i] - expected).abs() < 1e-3,
                "sample {i}: {} vs {expected}",
                out[0][i]
            );
        }
    }

    #[test]
    fn rapid_retargets_coalesce_to_latest() {
        let mut delay = AlignDelay::new(1, 4096, 32);
        for d in [100.0, 200.0, 300.0, 400.0] {
            delay.retarget(TapSpec {
                delay_samples: d,
                inverted: false,
            });
        }
        assert_eq!(delay.ultimate_target().delay_samples, 400.0);
        // Run long enough for all queued fades to finish.
        process_all(&mut delay, &[vec![0.0; 4096]]);
        assert_eq!(delay.active.spec.delay_samples, 400.0);
        assert!(delay.fading_in.is_none() && delay.pending.is_none());
    }

    #[test]
    fn reland_of_identical_target_starts_no_second_fade() {
        // A→B, then C then B again within one fade: the pending B equals the
        // tap that lands, so no second fade may start — a B→B equal-power
        // crossfade would bump the output by +3 dB (gains sum to √2) at
        // mid-fade.
        let fade = 400;
        let b = TapSpec {
            delay_samples: 200.0,
            inverted: false,
        };
        let c = TapSpec {
            delay_samples: 300.0,
            inverted: false,
        };
        let mut delay = AlignDelay::new(1, 4096, fade);
        // Prime the whole read region with DC so every tap reads exactly 1.0
        // (the kernel has unity DC gain).
        process_all(&mut delay, &[vec![1.0; 2048]]);
        delay.retarget(b);
        process_all(&mut delay, &[vec![1.0; 100]]); // mid-fade...
        delay.retarget(c); // ...request C...
        delay.retarget(b); // ...then coalesce back to B before it lands.
        assert_eq!(delay.ultimate_target(), b);
        // 300 samples finish the A→B fade; just past it, nothing may be
        // fading any more.
        process_all(&mut delay, &[vec![1.0; 310]]);
        assert_eq!(delay.active.spec, b);
        assert!(delay.fading_in.is_none() && delay.pending.is_none());
        // And the output stays flat: a started B→B fade would peak at ~1.414
        // in this window.
        let out = process_all(&mut delay, &[vec![1.0; 700]]);
        for i in 0..700 {
            assert!(
                (out[0][i] - 1.0).abs() < 1e-3,
                "sample {i}: {} — a B→B fade ran after landing",
                out[0][i]
            );
        }
    }

    #[test]
    fn pending_target_unlike_landed_still_fades() {
        // Control for the test above: a pending target that differs from the
        // landing tap must still get its fade.
        let fade = 64;
        let b = TapSpec {
            delay_samples: 200.0,
            inverted: false,
        };
        let c = TapSpec {
            delay_samples: 300.0,
            inverted: false,
        };
        let mut delay = AlignDelay::new(1, 4096, fade);
        delay.retarget(b);
        process_all(&mut delay, &[vec![0.0; 16]]); // mid-fade
        delay.retarget(c);
        // Just past the first landing: B is active and the C fade is live.
        process_all(&mut delay, &[vec![0.0; fade - 16 + 8]]);
        assert_eq!(delay.active.spec, b);
        assert_eq!(delay.fading_in.as_ref().map(|t| t.spec), Some(c));
        // And it completes.
        process_all(&mut delay, &[vec![0.0; 4096]]);
        assert_eq!(delay.active.spec, c);
        assert!(delay.fading_in.is_none() && delay.pending.is_none());
    }
}
