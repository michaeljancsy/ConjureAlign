//! The cross-correlation panel.
//!
//! Draws the normalized correlation-vs-lag curve from the last analysis, a
//! solid marker at the detected offset, and a dashed *live* marker at the
//! currently applied shift (detected + trim, clamped) that tracks trim
//! changes — with the curve's value at that lag read out next to it.

use std::sync::Arc;

use nih_plug_egui::egui::{
    self, Align2, Color32, FontId, Pos2, Sense, Shape, Stroke, StrokeKind, Ui, Vec2,
};

use super::decimate::{min_max_decimate, sample_linear, MinMax};
use super::waveform_view::nice_step;
use super::{
    PanelOutput, ACCENT_DETECTED, ACCENT_LIVE, ACCENT_WARN, CURVE_COLOR, GRID_COLOR, PANEL_BG,
    TEXT_DIM,
};
use crate::analysis::CONFIDENCE_THRESHOLD;
use crate::shared::AnalysisSnapshot;

#[derive(PartialEq, Clone, Copy)]
struct CorrKey {
    snap: usize,
    x0_bits: u64,
    span_bits: u64,
    cols: usize,
}

pub struct CorrCache {
    key: CorrKey,
    env: Vec<MinMax>,
    /// Lag-indices per display column; < 1 means the view is over-zoomed and
    /// the envelope is an interpolated line.
    per_bin: f64,
    /// Largest |value| of the whole curve, for y auto-scaling.
    max_abs: f32,
}

pub struct CorrArgs<'a> {
    pub snapshot: Option<&'a Arc<AnalysisSnapshot>>,
    /// The currently stored detection, if any analysis was ever accepted.
    /// May predate the snapshot (see `held`).
    pub detected_ms: Option<f32>,
    /// The stored detection came from an *earlier* capture — the snapshot's
    /// own analysis was rejected.
    pub held: bool,
    /// Currently applied shift in ms (detected + trim, clamped).
    pub net_ms: f32,
    pub clamped: bool,
    pub align_on: bool,
    /// Active clamp window (± ms) — can differ from the snapshot's search
    /// window after a Max Shift change plus reactivation without recapture.
    pub active_window_ms: Option<f32>,
}

pub fn show(
    ui: &mut Ui,
    height: f32,
    args: &CorrArgs,
    zoom_peak: &mut bool,
    cache: &mut Option<CorrCache>,
) -> PanelOutput {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("Cross-correlation vs lag")
                .small()
                .color(TEXT_DIM),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.selectable_label(*zoom_peak, "Zoom to peak").clicked() {
                *zoom_peak = !*zoom_peak;
            }
        });
    });

    let (response, painter) =
        ui.allocate_painter(Vec2::new(ui.available_width(), height), Sense::click_and_drag());
    let rect = response.rect.shrink(1.0);
    painter.rect_filled(rect, 4.0, PANEL_BG);
    painter.rect_stroke(rect, 4.0, Stroke::new(1.0, GRID_COLOR), StrokeKind::Inside);

    let snap = match args.snapshot {
        Some(s) if !s.corr.is_empty() => s,
        Some(_) => {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "No correlation data — the capture was rejected before analysis",
                FontId::proportional(13.0),
                TEXT_DIM,
            );
            return PanelOutput {
                response,
                ms_per_px: None,
            };
        }
        None => {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "The correlation curve appears after a capture",
                FontId::proportional(13.0),
                TEXT_DIM,
            );
            return PanelOutput {
                response,
                ms_per_px: None,
            };
        }
    };

    let sr = snap.sample_rate.max(1.0) as f64;
    let full_ms = snap.max_shift_samples as f64 / sr * 1000.0;
    let (x0, x1) = if *zoom_peak {
        let c = (args.detected_ms.unwrap_or(0.0) as f64).clamp(-full_ms, full_ms);
        let half = 15.0f64.min(full_ms);
        ((c - half).max(-full_ms), (c + half).min(full_ms))
    } else {
        (-full_ms, full_ms)
    };
    let span_ms = (x1 - x0).max(1e-6);
    let x_of = |lag_ms: f64| rect.left() + ((lag_ms - x0) / span_ms) as f32 * rect.width();
    let idx_of = |lag_ms: f64| lag_ms / 1000.0 * sr + snap.max_shift_samples as f64;

    // --- Envelope of the curve (cached) ---
    let cols = (rect.width().floor() as usize).max(8);
    let key = CorrKey {
        snap: Arc::as_ptr(snap) as usize,
        x0_bits: x0.to_bits(),
        span_bits: span_ms.to_bits(),
        cols,
    };
    if !cache.as_ref().is_some_and(|c| c.key == key) {
        let max_abs = match cache.as_ref() {
            Some(c) if c.key.snap == key.snap => c.max_abs,
            _ => snap.corr.iter().fold(0.0f32, |m, &v| m.max(v.abs())),
        };
        let span_idx = span_ms / 1000.0 * sr;
        *cache = Some(CorrCache {
            key,
            env: min_max_decimate(&snap.corr, idx_of(x0), span_idx, cols),
            per_bin: span_idx / cols as f64,
            max_abs,
        });
    }
    let cache = cache.as_ref().unwrap();

    let y_max = (cache.max_abs * 1.15).max(0.05);
    let half = rect.height() / 2.0 - 4.0;
    let y_of = |v: f32| rect.center().y - (v / y_max).clamp(-1.0, 1.0) * half;

    // --- Grid ---
    let stroke = Stroke::new(1.0, GRID_COLOR);
    let step = nice_step(span_ms, 8.0);
    let mut t = (x0 / step).ceil() * step;
    while t <= x1 {
        let x = x_of(t);
        painter.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())], stroke);
        let label = if step < 1.0 {
            format!("{t:+.2}")
        } else {
            format!("{t:+.0}")
        };
        painter.text(
            Pos2::new(x + 3.0, rect.bottom() - 3.0),
            Align2::LEFT_BOTTOM,
            label,
            FontId::proportional(10.0),
            TEXT_DIM,
        );
        t += step;
    }
    painter.line_segment(
        [
            Pos2::new(rect.left(), rect.center().y),
            Pos2::new(rect.right(), rect.center().y),
        ],
        stroke,
    );
    painter.text(
        Pos2::new(rect.left() + 4.0, rect.top() + 3.0),
        Align2::LEFT_TOP,
        format!("±{y_max:.2}"),
        FontId::proportional(10.0),
        TEXT_DIM,
    );
    painter.text(
        Pos2::new(rect.right() - 4.0, rect.bottom() - 3.0),
        Align2::RIGHT_BOTTOM,
        "lag (ms)",
        FontId::proportional(10.0),
        TEXT_DIM,
    );
    // Confidence-threshold guides.
    if CONFIDENCE_THRESHOLD < y_max {
        for sign in [1.0f32, -1.0] {
            let y = y_of(sign * CONFIDENCE_THRESHOLD);
            painter.extend(Shape::dashed_line(
                &[Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
                Stroke::new(1.0, GRID_COLOR),
                3.0,
                5.0,
            ));
        }
    }

    // --- Shade lags beyond the *active* clamp window ---
    if let Some(w) = args.active_window_ms {
        let w = w as f64;
        for (a, b) in [(x0, -w), (w, x1)] {
            if b > a {
                let shade = egui::Rect::from_min_max(
                    Pos2::new(x_of(a), rect.top()),
                    Pos2::new(x_of(b), rect.bottom()),
                );
                painter.rect_filled(
                    shade.intersect(rect),
                    0.0,
                    Color32::from_rgba_unmultiplied(229, 115, 115, 14),
                );
            }
        }
    }

    // --- The curve itself ---
    if cache.per_bin < 1.0 {
        let points: Vec<Pos2> = cache
            .env
            .iter()
            .enumerate()
            .map(|(i, mm)| Pos2::new(rect.left() + i as f32 + 0.5, y_of(mm.max)))
            .collect();
        painter.add(Shape::line(points, Stroke::new(1.5, CURVE_COLOR)));
    } else {
        for (i, mm) in cache.env.iter().enumerate() {
            let x = rect.left() + i as f32 + 0.5;
            let y_top = y_of(mm.max);
            let y_bot = y_of(mm.min).max(y_top + 0.75);
            painter.line_segment(
                [Pos2::new(x, y_top), Pos2::new(x, y_bot)],
                Stroke::new(1.0, CURVE_COLOR),
            );
        }
    }

    // --- Detected (solid) marker ---
    if let Some(d) = args.detected_ms {
        let d = d as f64;
        if (x0..=x1).contains(&d) {
            let x = x_of(d);
            painter.line_segment(
                [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
                Stroke::new(1.5, ACCENT_DETECTED),
            );
            painter.text(
                Pos2::new(x + 4.0, rect.top() + 3.0),
                Align2::LEFT_TOP,
                if args.held { "held" } else { "detected" },
                FontId::proportional(10.0),
                ACCENT_DETECTED,
            );
        }
    }

    // --- Live (dashed) marker at the applied shift ---
    let live_color = if args.clamped { ACCENT_WARN } else { ACCENT_LIVE };
    // Marker and readout both use the clamped lag so the dot always sits on
    // the drawn curve; a shift beyond the snapshot's search window (possible
    // after a Max Shift change without a recapture) is reported as off-curve
    // instead of a bogus r of 0.
    let on_curve = (args.net_ms as f64).abs() <= full_ms;
    let lag = (args.net_ms as f64).clamp(x0, x1);
    let x = x_of(lag);
    painter.extend(Shape::dashed_line(
        &[Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
        Stroke::new(1.5, live_color),
        5.0,
        4.0,
    ));
    let r = sample_linear(&snap.corr, idx_of(lag));
    painter.circle_filled(Pos2::new(x, y_of(r)), 3.5, live_color);
    let readout = if !args.align_on {
        "alignment off".to_string()
    } else if !on_curve {
        format!("off curve @ {:+.2} ms", args.net_ms)
    } else if args.clamped {
        format!("r {r:+.2} @ {:+.2} ms (clamped)", args.net_ms)
    } else {
        format!("r {r:+.2} @ {:+.2} ms", args.net_ms)
    };
    // Keep the readout inside the panel: flip sides near the right edge.
    let (anchor, tx) = if x > rect.right() - 130.0 {
        (Align2::RIGHT_TOP, x - 6.0)
    } else {
        (Align2::LEFT_TOP, x + 6.0)
    };
    painter.text(
        Pos2::new(tx, rect.top() + 16.0),
        anchor,
        readout,
        FontId::proportional(11.0),
        live_color,
    );

    PanelOutput {
        response,
        // A degenerate panel width would give an absurd (or negative, or
        // non-finite) drag axis; disable the gesture instead.
        ms_per_px: (rect.width() > 1.0).then(|| span_ms / rect.width() as f64),
    }
}
