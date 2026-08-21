//! Frequency-axis bucketing and ticks for the spectrum panel.
//!
//! Pure functions, no egui types — everything here is unit-tested.
//! `decimate::min_max_decimate` can't serve a log axis (its buckets are equal
//! in index space), so the panel bucketes through [`bucket_edges`] +
//! [`bucket_curve`] instead, for the linear axis too.

use super::decimate::{sample_linear, MinMax};

/// Bucket edges in *bin index* units for `cols` display columns spanning
/// `[f_lo, f_hi]` Hz, log- or linear-spaced. `bin_hz` is the spectrum's bin
/// spacing (`sample_rate / nfft`). Returns `cols + 1` strictly increasing
/// edges with exact endpoints, or an empty vec on degenerate input.
pub fn bucket_edges(f_lo: f64, f_hi: f64, bin_hz: f64, cols: usize, log: bool) -> Vec<f64> {
    if cols == 0 || f_hi <= f_lo || bin_hz <= 0.0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(cols + 1);
    if log {
        let lo = f_lo.max(f_hi * 1e-9);
        let ratio = f_hi / lo;
        for i in 0..=cols {
            out.push(lo * ratio.powf(i as f64 / cols as f64) / bin_hz);
        }
    } else {
        for i in 0..=cols {
            out.push((f_lo + (f_hi - f_lo) * i as f64 / cols as f64) / bin_hz);
        }
    }
    // Pin the endpoints exactly (powf rounding would leave them fuzzy).
    out[0] = f_lo / bin_hz;
    out[cols] = f_hi / bin_hz;
    out
}

/// Min/max of `db` over each `[edges[i], edges[i+1]]` bucket. The linearly
/// interpolated values at BOTH bucket boundaries are included in each
/// bucket's min/max, so adjacent columns always share a value — the drawn
/// stubs stay connected across a log axis's mixed bucket densities
/// (sub-bin buckets in the low decades, many bins per bucket at the top)
/// without a separate over-zoom drawing mode. Positions outside `db` read
/// as 0.0, matching `decimate`.
pub fn bucket_curve(db: &[f32], edges: &[f64]) -> Vec<MinMax> {
    if db.is_empty() || edges.len() < 2 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(edges.len() - 1);
    for pair in edges.windows(2) {
        let (lo, hi) = (pair[0], pair[1]);
        let a = sample_linear(db, lo);
        let b = sample_linear(db, hi);
        let mut mm = MinMax {
            min: a.min(b),
            max: a.max(b),
        };
        // Whole bins strictly inside the bucket (the boundaries are already
        // covered by the interpolated values above).
        let first = lo.ceil() as i64;
        let last = hi.floor() as i64;
        for n in first..=last {
            let v = if n >= 0 && (n as usize) < db.len() {
                db[n as usize]
            } else {
                0.0
            };
            mm.min = mm.min.min(v);
            mm.max = mm.max.max(v);
        }
        out.push(mm);
    }
    out
}

/// 1-2-5-per-decade tick positions within `[f_lo, f_hi]`, ascending
/// (e.g. 20, 50, 100, 200, 500, 1k, 2k, 5k, 10k, 20k).
pub fn log_ticks(f_lo: f64, f_hi: f64) -> Vec<f64> {
    let mut out = Vec::new();
    if f_hi <= f_lo || f_hi <= 0.0 {
        return out;
    }
    let lo = f_lo.max(f_hi * 1e-9);
    let mut decade = 10f64.powi(lo.log10().floor() as i32);
    while decade <= f_hi {
        for m in [1.0, 2.0, 5.0] {
            let f = decade * m;
            if f >= lo && f <= f_hi {
                out.push(f);
            }
        }
        decade *= 10.0;
    }
    out
}

/// Tick labels: "20", "500", "1k", "12.5k".
pub fn fmt_hz(f: f64) -> String {
    if f < 999.5 {
        format!("{f:.0}")
    } else {
        let k = f / 1000.0;
        if (k - k.round()).abs() < 0.1 {
            format!("{:.0}k", k.round())
        } else {
            format!("{k:.1}k")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_shape_and_spacing() {
        for log in [false, true] {
            let e = bucket_edges(20.0, 20_000.0, 48_000.0 / 8192.0, 800, log);
            assert_eq!(e.len(), 801);
            for w in e.windows(2) {
                assert!(w[1] > w[0], "edges must be strictly increasing ({log})");
            }
            assert_eq!(e[0], 20.0 / (48_000.0 / 8192.0));
            assert_eq!(e[800], 20_000.0 / (48_000.0 / 8192.0));
        }
        // Linear spacing is uniform; log spacing has a constant ratio.
        let lin = bucket_edges(0.0, 1000.0, 1.0, 10, false);
        for w in lin.windows(2) {
            assert!((w[1] - w[0] - 100.0).abs() < 1e-9);
        }
        let log = bucket_edges(10.0, 10_000.0, 1.0, 3, true);
        for w in log.windows(2) {
            assert!((w[1] / w[0] - 10.0).abs() < 1e-9);
        }
    }

    #[test]
    fn buckets_share_boundary_values() {
        // On a monotone ramp each bucket's range is exactly its two boundary
        // interpolations, so adjacent columns must touch.
        let db: Vec<f32> = (0..1000).map(|i| i as f32 * 0.1).collect();
        let edges = bucket_edges(20.0, 900.0, 1.0, 50, true);
        let env = bucket_curve(&db, &edges);
        assert_eq!(env.len(), 50);
        for pair in env.windows(2) {
            assert!(
                (pair[0].max - pair[1].min).abs() < 1e-5,
                "columns must share the boundary value: {} vs {}",
                pair[0].max,
                pair[1].min
            );
        }
    }

    #[test]
    fn notch_survives_wide_buckets() {
        let mut db = vec![0.0f32; 4097];
        db[3000] = -40.0;
        let edges = bucket_edges(20.0, 4000.0, 1.0, 30, true);
        let env = bucket_curve(&db, &edges);
        let deepest = env.iter().fold(0.0f32, |m, mm| m.min(mm.min));
        assert!(deepest <= -39.0, "notch lost: {deepest}");
        // ...and it survives in exactly one column's min.
        assert_eq!(env.iter().filter(|mm| mm.min <= -39.0).count(), 1);
    }

    #[test]
    fn degenerate_inputs() {
        assert!(bucket_edges(20.0, 20_000.0, 1.0, 0, true).is_empty());
        assert!(bucket_edges(20_000.0, 20.0, 1.0, 10, true).is_empty());
        assert!(bucket_edges(100.0, 100.0, 1.0, 10, false).is_empty());
        assert!(bucket_edges(20.0, 20_000.0, 0.0, 10, true).is_empty());
        assert!(bucket_curve(&[], &[0.0, 1.0]).is_empty());
        assert!(bucket_curve(&[1.0], &[0.5]).is_empty());

        // Sub-bin buckets: interpolated boundaries, min ≤ max, values inside
        // the neighboring bins' range.
        let db = [0.0f32, 10.0];
        let edges = bucket_edges(0.1, 0.9, 1.0, 8, false);
        let env = bucket_curve(&db, &edges);
        for mm in &env {
            assert!(mm.min <= mm.max);
            assert!(mm.min >= 0.0 && mm.max <= 10.0);
        }
        // Continuity holds in the sub-bin regime too.
        for pair in env.windows(2) {
            assert!((pair[0].max - pair[1].min).abs() < 1e-5);
        }
    }

    #[test]
    fn ticks_and_labels() {
        assert_eq!(
            log_ticks(20.0, 20_000.0),
            vec![20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10_000.0, 20_000.0]
        );
        assert_eq!(log_ticks(30.0, 90.0), vec![50.0]);
        assert!(log_ticks(90.0, 30.0).is_empty());
        assert_eq!(fmt_hz(20.0), "20");
        assert_eq!(fmt_hz(500.0), "500");
        assert_eq!(fmt_hz(1000.0), "1k");
        assert_eq!(fmt_hz(2000.0), "2k");
        assert_eq!(fmt_hz(12_500.0), "12.5k");
        assert_eq!(fmt_hz(22_050.0), "22k");
    }
}
