//! Peak-preserving min/max decimation shared by both display panels.
//!
//! Pure functions, no egui types — everything here is unit-tested.

/// One display column: the extremes of every sample in its bucket.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MinMax {
    pub min: f32,
    pub max: f32,
}

/// Min/max-decimates `src` over the half-open fractional window
/// `[start, start + span)` (positions in samples), mapped onto `bins` equal
/// buckets. Positions outside `src` contribute silence (0.0), so a view
/// scrolled past either end renders flat instead of garbage. When a bucket
/// covers less than one sample, the signal is linearly interpolated at the
/// bucket center instead (`min == max`), so over-zoomed views degrade to a
/// smooth line rather than gaps.
pub fn min_max_decimate(src: &[f32], start: f64, span: f64, bins: usize) -> Vec<MinMax> {
    if bins == 0 || span <= 0.0 {
        return Vec::new();
    }
    let per_bin = span / bins as f64;
    let mut out = Vec::with_capacity(bins);
    if per_bin < 1.0 {
        for b in 0..bins {
            let v = sample_linear(src, start + (b as f64 + 0.5) * per_bin);
            out.push(MinMax { min: v, max: v });
        }
    } else {
        for b in 0..bins {
            let lo = start + b as f64 * per_bin;
            let hi = lo + per_bin;
            // Integer positions n with lo <= n < hi; a half-open interval of
            // length >= 1 always contains at least one.
            let first = lo.ceil() as i64;
            let last = (hi.ceil() as i64) - 1;
            let mut mm = MinMax {
                min: f32::INFINITY,
                max: f32::NEG_INFINITY,
            };
            for n in first..=last {
                let v = sample_at(src, n);
                mm.min = mm.min.min(v);
                mm.max = mm.max.max(v);
            }
            if mm.min > mm.max {
                mm = MinMax { min: 0.0, max: 0.0 };
            }
            out.push(mm);
        }
    }
    out
}

/// Linear interpolation of `src` at a fractional position; 0.0 outside.
pub fn sample_linear(src: &[f32], pos: f64) -> f32 {
    let base = pos.floor();
    let frac = (pos - base) as f32;
    let i = base as i64;
    let a = sample_at(src, i);
    let b = sample_at(src, i + 1);
    a + (b - a) * frac
}

fn sample_at(src: &[f32], i: i64) -> f32 {
    if i >= 0 && (i as usize) < src.len() {
        src[i as usize]
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spike_is_preserved_at_any_bin_count() {
        let mut src = vec![0.0f32; 10_000];
        src[7_777] = 1.0;
        src[3_333] = -1.0;
        for bins in [7, 100, 999] {
            let env = min_max_decimate(&src, 0.0, src.len() as f64, bins);
            assert_eq!(env.len(), bins);
            let peak = env.iter().fold(0.0f32, |m, mm| m.max(mm.max));
            let trough = env.iter().fold(0.0f32, |m, mm| m.min(mm.min));
            assert_eq!(peak, 1.0, "bins={bins}");
            assert_eq!(trough, -1.0, "bins={bins}");
        }
    }

    #[test]
    fn constant_input_gives_flat_envelope() {
        let src = vec![0.5f32; 1000];
        for mm in min_max_decimate(&src, 0.0, 1000.0, 50) {
            assert_eq!(mm, MinMax { min: 0.5, max: 0.5 });
        }
    }

    #[test]
    fn out_of_range_is_silence() {
        let src = vec![1.0f32; 100];
        // Entirely before the signal.
        for mm in min_max_decimate(&src, -1000.0, 500.0, 10) {
            assert_eq!(mm, MinMax { min: 0.0, max: 0.0 });
        }
        // Window extending past the end of the signal: buckets fully inside
        // stay at the signal value, buckets fully past the end are silent.
        let env = min_max_decimate(&src, 50.0, 100.0, 10);
        assert_eq!(env[0], MinMax { min: 1.0, max: 1.0 });
        assert_eq!(env[4], MinMax { min: 1.0, max: 1.0 }); // positions 90..100
        assert_eq!(env[5], MinMax { min: 0.0, max: 0.0 }); // positions 100..110
        assert_eq!(env[9], MinMax { min: 0.0, max: 0.0 });
    }

    #[test]
    fn fractional_start_shifts_buckets() {
        // Ramp 0,1,2,...: bucket max = last contained sample index.
        let src: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let a = min_max_decimate(&src, 0.0, 10.0, 5); // buckets of 2 samples
        assert_eq!(a[0], MinMax { min: 0.0, max: 1.0 });
        let b = min_max_decimate(&src, 0.5, 10.0, 5); // covers [0.5, 2.5) -> {1, 2}
        assert_eq!(b[0], MinMax { min: 1.0, max: 2.0 });
    }

    #[test]
    fn over_zoom_interpolates() {
        let src = vec![0.0f32, 1.0];
        // Window [0, 1) split into 4 bins: centers at 0.125, 0.375, ...
        let env = min_max_decimate(&src, 0.0, 1.0, 4);
        let expect = [0.125f32, 0.375, 0.625, 0.875];
        for (mm, e) in env.iter().zip(expect) {
            assert!((mm.min - e).abs() < 1e-6);
            assert_eq!(mm.min, mm.max);
        }
    }

    #[test]
    fn empty_and_degenerate_inputs() {
        assert!(min_max_decimate(&[], 0.0, 0.0, 10).is_empty());
        assert!(min_max_decimate(&[1.0], 0.0, 1.0, 0).is_empty());
        assert!(min_max_decimate(&[1.0], 0.0, -5.0, 10).is_empty());
        // Empty source with a valid window: silence.
        for mm in min_max_decimate(&[], 0.0, 100.0, 4) {
            assert_eq!(mm, MinMax { min: 0.0, max: 0.0 });
        }
    }

    #[test]
    fn sample_linear_basics() {
        let src = [0.0f32, 2.0];
        assert_eq!(sample_linear(&src, 0.0), 0.0);
        assert_eq!(sample_linear(&src, 0.5), 1.0);
        assert_eq!(sample_linear(&src, 1.0), 2.0);
        // Fades to the 0.0 outside value past the last sample.
        assert_eq!(sample_linear(&src, 1.5), 1.0);
        assert_eq!(sample_linear(&src, -10.0), 0.0);
    }
}
