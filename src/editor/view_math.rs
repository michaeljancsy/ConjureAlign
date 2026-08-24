//! Pan/zoom arithmetic shared by the three graph panels.
//!
//! Pure functions, no egui types — everything here is unit-tested. All
//! values are in the caller's axis space (seconds, milliseconds, or ln-Hz
//! for the spectrum's log axis); callers transform at the call site.

/// Zooms the view `(start, span)` about `anchor_frac` (0..1 across the view)
/// by `factor` (> 1 zooms in). The span is clamped to
/// `[min_span, full_hi − full_lo]` and the start so the view stays inside
/// `[full_lo, full_hi]`; the point under the anchor stays put whenever no
/// clamp engages. Degenerate input (non-finite/non-positive factor or span,
/// empty full range) returns the view unchanged.
pub fn zoom_about(
    start: f64,
    span: f64,
    anchor_frac: f64,
    factor: f64,
    min_span: f64,
    full_lo: f64,
    full_hi: f64,
) -> (f64, f64) {
    let full = full_hi - full_lo;
    if !factor.is_finite() || factor <= 0.0 || !span.is_finite() || span <= 0.0 || full <= 0.0 {
        return (start, span);
    }
    let anchor = start + anchor_frac * span;
    let new_span = (span / factor).clamp(min_span.min(full), full);
    let new_start = (anchor - anchor_frac * new_span).clamp(full_lo, full_hi - new_span);
    (new_start, new_span)
}

/// Pans the view start by `delta` axis units, keeping `[start, start+span]`
/// inside `[full_lo, full_hi]`.
pub fn pan(start: f64, span: f64, delta: f64, full_lo: f64, full_hi: f64) -> f64 {
    if !delta.is_finite() {
        return start;
    }
    (start + delta).clamp(full_lo, (full_hi - span).max(full_lo))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_point_stays_put() {
        // View [2, 6) of full [0, 10); anchor 25% in → axis value 3.
        let (s, sp) = zoom_about(2.0, 4.0, 0.25, 2.0, 0.1, 0.0, 10.0);
        assert!((sp - 2.0).abs() < 1e-12);
        assert!((s + 0.25 * sp - 3.0).abs() < 1e-12, "anchor moved: {s}");
        // Zoom back out: anchor still fixed.
        let (s2, sp2) = zoom_about(s, sp, 0.25, 0.5, 0.1, 0.0, 10.0);
        assert!((sp2 - 4.0).abs() < 1e-12);
        assert!((s2 + 0.25 * sp2 - 3.0).abs() < 1e-12);
    }

    #[test]
    fn zoom_out_clamps_at_edges() {
        // At the left edge, zooming out must extend right only.
        let (s, sp) = zoom_about(0.0, 2.0, 0.1, 0.5, 0.1, 0.0, 10.0);
        assert_eq!(s, 0.0);
        assert!((sp - 4.0).abs() < 1e-12);
        // At the right edge, start moves left so the view stays inside.
        let (s, sp) = zoom_about(8.0, 2.0, 0.9, 0.5, 0.1, 0.0, 10.0);
        assert!((sp - 4.0).abs() < 1e-12);
        assert!((s + sp - 10.0).abs() < 1e-12, "view exceeds range: {s}+{sp}");
    }

    #[test]
    fn min_span_floor_and_full_collapse() {
        let (_, sp) = zoom_about(4.0, 1.0, 0.5, 100.0, 0.5, 0.0, 10.0);
        assert_eq!(sp, 0.5);
        // Zooming out far collapses to the full range exactly.
        let (s, sp) = zoom_about(4.0, 1.0, 0.5, 1e-6, 0.5, 0.0, 10.0);
        assert_eq!((s, sp), (0.0, 10.0));
        // min_span larger than the full range must not exceed it.
        let (_, sp) = zoom_about(0.0, 5.0, 0.5, 10.0, 100.0, 0.0, 10.0);
        assert_eq!(sp, 10.0);
    }

    #[test]
    fn pan_clamps_both_ends() {
        assert_eq!(pan(2.0, 4.0, -100.0, 0.0, 10.0), 0.0);
        assert_eq!(pan(2.0, 4.0, 100.0, 0.0, 10.0), 6.0);
        assert_eq!(pan(2.0, 4.0, 1.5, 0.0, 10.0), 3.5);
        // Span == full range: pinned.
        assert_eq!(pan(0.0, 10.0, 3.0, 0.0, 10.0), 0.0);
    }

    #[test]
    fn log_space_round_trip() {
        // Zoom about 1 kHz in ln-space over [20, 24000] Hz.
        let (lo, hi) = (20.0f64.ln(), 24_000.0f64.ln());
        let anchor_hz = 1000.0f64;
        let frac = (anchor_hz.ln() - lo) / (hi - lo);
        let (s, sp) = zoom_about(lo, hi - lo, frac, 3.0, 0.1, lo, hi);
        let (f_lo, f_hi) = (s.exp(), (s + sp).exp());
        assert!(f_lo >= 20.0 - 1e-9 && f_hi <= 24_000.0 + 1e-9);
        // The anchor frequency sits at the same fraction of the new view.
        let new_frac = (anchor_hz.ln() - s) / sp;
        assert!((new_frac - frac).abs() < 1e-9);
    }

    #[test]
    fn degenerate_inputs_are_no_ops() {
        assert_eq!(zoom_about(1.0, 2.0, 0.5, f64::NAN, 0.1, 0.0, 10.0), (1.0, 2.0));
        assert_eq!(zoom_about(1.0, 2.0, 0.5, 0.0, 0.1, 0.0, 10.0), (1.0, 2.0));
        assert_eq!(zoom_about(1.0, 2.0, 0.5, -1.0, 0.1, 0.0, 10.0), (1.0, 2.0));
        assert_eq!(zoom_about(1.0, 2.0, 0.5, 2.0, 0.1, 5.0, 5.0), (1.0, 2.0));
        assert_eq!(pan(1.0, 2.0, f64::INFINITY, 0.0, 10.0), 1.0);
    }
}
