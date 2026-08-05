//! Fractional-delay FIR design: a Kaiser-windowed sinc interpolator.
//!
//! The kernel realizes a delay of `FIR_CENTER + frac` samples with linear phase
//! and flat magnitude across the audible band. Alignment is about phase, so a
//! linear-phase interpolator (constant group delay at every frequency) is used
//! instead of Lagrange or Thiran designs.

pub const FIR_LEN: usize = 64;
/// Integer group delay contributed by the FIR at `frac == 0`. Folded into the
/// plugin's reported latency.
pub const FIR_CENTER: usize = 31;

const KAISER_BETA: f64 = 9.0;
const HALF_LEN: f64 = 32.0;

/// Zeroth-order modified Bessel function of the first kind (power series).
fn bessel_i0(x: f64) -> f64 {
    let x_half_sq = (x / 2.0) * (x / 2.0);
    let mut sum = 1.0;
    let mut term = 1.0;
    for k in 1..=40u64 {
        term *= x_half_sq / ((k * k) as f64);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

/// Designs the 64-tap kernel for a delay of `FIR_CENTER + frac` samples,
/// `frac` in [0, 1]. Allocation-free: safe to call on the audio thread (it
/// only runs when the target delay actually changes). The window is centered
/// on the shifted sinc peak so the response stays symmetric for every `frac`,
/// and the kernel is normalized to exactly unity DC gain.
pub fn design_kernel(frac: f64, kernel: &mut [f32; FIR_LEN]) {
    debug_assert!((0.0..=1.0).contains(&frac));
    let i0_beta = bessel_i0(KAISER_BETA);
    let mut sum = 0.0f64;
    let mut taps = [0.0f64; FIR_LEN];
    for (k, tap) in taps.iter_mut().enumerate() {
        let x = k as f64 - FIR_CENTER as f64 - frac;
        let t = 1.0 - (x / HALF_LEN) * (x / HALF_LEN);
        let w = if t > 0.0 {
            bessel_i0(KAISER_BETA * t.sqrt()) / i0_beta
        } else {
            0.0
        };
        *tap = sinc(x) * w;
        sum += *tap;
    }
    for (out, tap) in kernel.iter_mut().zip(&taps) {
        *out = (tap / sum) as f32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Magnitude response of the kernel at normalized frequency `f` (cycles/sample).
    fn magnitude_at(kernel: &[f32; FIR_LEN], f: f64) -> f64 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (k, &h) in kernel.iter().enumerate() {
            let phase = -2.0 * std::f64::consts::PI * f * k as f64;
            re += h as f64 * phase.cos();
            im += h as f64 * phase.sin();
        }
        (re * re + im * im).sqrt()
    }

    #[test]
    fn zero_frac_is_identity() {
        let mut kernel = [0.0f32; FIR_LEN];
        design_kernel(0.0, &mut kernel);
        // With frac = 0 the sinc reduces to a unit impulse at FIR_CENTER.
        for (k, &h) in kernel.iter().enumerate() {
            if k == FIR_CENTER {
                assert!((h - 1.0).abs() < 1e-6, "center tap {h}");
            } else {
                assert!(h.abs() < 1e-6, "tap {k} = {h}");
            }
        }
    }

    #[test]
    fn magnitude_flat_to_0_45_fs() {
        // Worst case is frac = 0.5; also check other fractions.
        for frac in [0.1, 0.25, 0.5, 0.75, 0.9] {
            let mut kernel = [0.0f32; FIR_LEN];
            design_kernel(frac, &mut kernel);
            let mut f = 0.0;
            while f <= 0.45 {
                let mag_db = 20.0 * magnitude_at(&kernel, f).log10();
                assert!(
                    mag_db.abs() < 0.1,
                    "frac {frac}: |H({f})| = {mag_db:.4} dB exceeds 0.1 dB"
                );
                f += 0.005;
            }
        }
    }

    #[test]
    fn group_delay_matches_frac() {
        // Measure the delay of a sine passed through the kernel by comparing
        // phase at a mid-band frequency.
        for frac in [0.0, 0.25, 0.5, 0.75] {
            let mut kernel = [0.0f32; FIR_LEN];
            design_kernel(frac, &mut kernel);
            let f = 0.1; // cycles/sample
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for (k, &h) in kernel.iter().enumerate() {
                let phase = -2.0 * std::f64::consts::PI * f * k as f64;
                re += h as f64 * phase.cos();
                im += h as f64 * phase.sin();
            }
            let measured_delay = -im.atan2(re) / (2.0 * std::f64::consts::PI * f);
            let expected = FIR_CENTER as f64 + frac;
            // atan2 wraps; delay of ~31 samples at f=0.1 wraps many times, so
            // compare modulo the period 1/f = 10 samples.
            let err = (measured_delay - expected).rem_euclid(1.0 / f);
            let err = err.min(1.0 / f - err);
            assert!(err < 0.01, "frac {frac}: phase-delay error {err}");
        }
    }
}
