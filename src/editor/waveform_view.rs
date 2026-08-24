//! The overlaid waveform panel.
//!
//! Both captures are drawn on the reference's timeline; the main waveform is
//! drawn shifted right by the currently applied alignment, so trim changes
//! slide it live. Envelopes are recomputed from the raw snapshot whenever the
//! view, width, or shift changes (cache below), which keeps the slide
//! pixel-exact at any zoom level.

use std::sync::Arc;

use nih_plug_egui::egui::{
    self, Align2, Color32, FontId, Pos2, Sense, Stroke, StrokeKind, Ui, Vec2,
};

use super::decimate::{min_max_decimate, MinMax};
use super::{
    view_math, PanelOutput, ACCENT_LIVE, ACCENT_MAIN, ACCENT_REF, GRID_COLOR, PANEL_BG, TEXT_DIM,
};
use crate::shared::AnalysisSnapshot;

/// Live capture status drawn over the waveform panel (built by `mod.rs` from
/// the capture phase + gate bits).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CaptureOverlay {
    Idle,
    Armed {
        main_quiet: bool,
        ref_quiet: bool,
    },
    Capturing {
        /// Fill fraction of the capture buffer (accumulated / capacity).
        frac: f32,
        /// Accumulated gated signal, seconds.
        secs: f32,
        /// `Some(reason)` while the gate is holding recording paused.
        paused_reason: Option<&'static str>,
    },
}

/// Visible window on the reference timeline, in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimeView {
    pub start_s: f64,
    pub span_s: f64,
}

#[derive(Default)]
pub struct WaveViewState {
    /// `None` = fit the whole capture.
    pub view: Option<TimeView>,
    pub cache: Option<WaveCache>,
}

/// Key for the shift-independent envelopes (reference + unshifted ghost).
#[derive(PartialEq, Clone, Copy)]
struct StaticKey {
    snap: usize,
    start_bits: u64,
    span_bits: u64,
    cols: usize,
    show_raw: bool,
}

/// Key for the main envelope, which additionally depends on the applied
/// shift and polarity flip. Split from [`StaticKey`] so a trim drag only
/// re-decimates the one envelope that actually changed.
#[derive(PartialEq, Clone, Copy)]
struct MainKey {
    stat: StaticKey,
    net_bits: u32,
    flip: bool,
}

pub struct WaveCache {
    static_key: StaticKey,
    main_key: MainKey,
    reference: Vec<MinMax>,
    /// Unshifted main ghost; empty unless `show_raw`.
    raw: Vec<MinMax>,
    main: Vec<MinMax>,
    /// Display gain: normalizes the tallest sample of either capture. Only
    /// recomputed when the snapshot changes (`gain_snap` key).
    gain: f32,
    gain_snap: usize,
}

pub struct WaveArgs<'a> {
    pub snapshot: Option<&'a Arc<AnalysisSnapshot>>,
    /// Currently applied shift (detected + trim, clamped), in ms.
    pub net_ms: f32,
    /// Draw the main waveform polarity-flipped.
    pub flip_main: bool,
    /// Live capture status overlay.
    pub overlay: CaptureOverlay,
}

/// Header row + canvas. Returns the canvas response for trim-drag handling.
/// `height` is the panel's TOTAL height — the canvas gets what the header
/// row leaves, so the caller can budget without guessing a header height.
pub fn show(
    ui: &mut Ui,
    height: f32,
    args: &WaveArgs,
    show_raw: &mut bool,
    vs: &mut WaveViewState,
) -> PanelOutput {
    let len_s = args
        .snapshot
        .map(|s| s.reference.len() as f64 / s.sample_rate.max(1.0) as f64)
        .unwrap_or(0.0);

    // --- Header: legend + raw ghost toggle + zoom presets ---
    let header = ui.horizontal(|ui| {
        legend_chip(ui, ACCENT_MAIN, "Main");
        legend_chip(ui, ACCENT_REF, "Reference");
        ui.add_space(8.0);
        ui.checkbox(show_raw, egui::RichText::new("unaligned ghost").small())
            .on_hover_text("Also draw the main waveform where it sits without alignment");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            for (label, span) in [("20 ms", 0.02), ("100 ms", 0.1), ("1 s", 1.0)] {
                if ui.small_button(label).clicked() && len_s > 0.0 {
                    set_span(vs, len_s, span);
                }
            }
            if ui.small_button("Fit").clicked() {
                vs.view = None;
            }
            ui.label(egui::RichText::new("zoom:").color(TEXT_DIM).small());
        });
    });

    let width = ui.available_width();
    let (response, painter) = ui.allocate_painter(
        Vec2::new(width, super::canvas_height(ui, height, &header.response)),
        Sense::click_and_drag(),
    );
    let rect = response.rect.shrink(1.0);
    painter.rect_filled(rect, 4.0, PANEL_BG);
    painter.rect_stroke(rect, 4.0, Stroke::new(1.0, GRID_COLOR), StrokeKind::Inside);

    let Some(snap) = args.snapshot else {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "No capture yet — play both tracks and press Capture",
            FontId::proportional(14.0),
            TEXT_DIM,
        );
        draw_capture_overlay(&painter, rect, &args.overlay);
        return PanelOutput { response };
    };

    let sr = snap.sample_rate.max(1.0) as f64;
    // Fit view, and clamp a panned/zoomed view to the capture.
    let mut view = vs.view.unwrap_or(TimeView {
        start_s: 0.0,
        span_s: len_s.max(1e-3),
    });
    let min_span = (16.0 / sr).min(len_s.max(1e-3));
    view.span_s = view.span_s.clamp(min_span, len_s.max(1e-3));

    // --- Interactions: drag/scroll to pan, pinch (or ⌘/⌃-scroll — egui
    // folds both into zoom_delta) to zoom the time axis about the cursor,
    // double-click to fit. The vertical axis is plugin-scaled, always.
    let full = len_s.max(1e-3);
    if response.hovered() {
        let (zoom, scroll) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta));
        if zoom != 1.0 {
            let frac = response
                .hover_pos()
                .map(|p| ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0))
                .unwrap_or(0.5) as f64;
            let (s, sp) = view_math::zoom_about(
                view.start_s,
                view.span_s,
                frac,
                zoom as f64,
                min_span,
                0.0,
                full,
            );
            view.start_s = s;
            view.span_s = sp;
        }
        let pan_px = if scroll.x.abs() > scroll.y.abs() {
            scroll.x
        } else {
            scroll.y
        };
        if pan_px != 0.0 && rect.width() > 1.0 {
            let delta = -pan_px as f64 * view.span_s / rect.width() as f64;
            view.start_s = view_math::pan(view.start_s, view.span_s, delta, 0.0, full);
        }
    }
    if response.dragged() {
        let dx = response.drag_delta().x;
        if dx != 0.0 && rect.width() > 1.0 {
            let delta = -dx as f64 * view.span_s / rect.width() as f64;
            view.start_s = view_math::pan(view.start_s, view.span_s, delta, 0.0, full);
        }
    }
    if response.double_clicked() {
        vs.view = None;
        view = TimeView {
            start_s: 0.0,
            span_s: full,
        };
    }
    view.start_s = view.start_s.clamp(0.0, (len_s - view.span_s).max(0.0));
    if vs.view.is_some() || view.span_s < len_s {
        vs.view = Some(view);
    }

    // --- Envelopes (cached; the shift-dependent main envelope is keyed
    // separately so a trim drag doesn't re-decimate reference and ghost) ---
    let cols = (rect.width().floor() as usize).max(8);
    let snap_addr = Arc::as_ptr(snap) as usize;
    let static_key = StaticKey {
        snap: snap_addr,
        start_bits: view.start_s.to_bits(),
        span_bits: view.span_s.to_bits(),
        cols,
        show_raw: *show_raw,
    };
    let main_key = MainKey {
        stat: static_key,
        net_bits: args.net_ms.to_bits(),
        flip: args.flip_main,
    };
    let start = view.start_s * sr;
    let span = view.span_s * sr;
    let (old_static, old_main, old_gain) = match vs.cache.take() {
        Some(c) => (
            (c.static_key == static_key).then_some((c.reference, c.raw)),
            (c.main_key == main_key).then_some(c.main),
            (c.gain_snap == snap_addr).then_some(c.gain),
        ),
        None => (None, None, None),
    };
    let gain = old_gain.unwrap_or_else(|| {
        let peak = snap
            .main
            .iter()
            .chain(&snap.reference)
            .fold(0.0f32, |m, &v| m.max(v.abs()));
        // Cap the normalization so a near-silent (rejected) capture still
        // looks silent instead of being blown up to full scale.
        (1.0 / peak.max(1e-6)).min(64.0)
    });
    let (reference, raw) = old_static.unwrap_or_else(|| {
        (
            min_max_decimate(&snap.reference, start, span, cols),
            if *show_raw {
                min_max_decimate(&snap.main, start, span, cols)
            } else {
                Vec::new()
            },
        )
    });
    let main = old_main.unwrap_or_else(|| {
        let main_start = (view.start_s - args.net_ms as f64 / 1000.0) * sr;
        let mut main = min_max_decimate(&snap.main, main_start, span, cols);
        if args.flip_main {
            for mm in &mut main {
                *mm = MinMax {
                    min: -mm.max,
                    max: -mm.min,
                };
            }
        }
        main
    });
    vs.cache = Some(WaveCache {
        static_key,
        main_key,
        reference,
        raw,
        main,
        gain,
        gain_snap: snap_addr,
    });
    let cache = vs.cache.as_ref().unwrap();

    // --- Grid ---
    draw_time_grid(&painter, rect, &view);

    // Splice seams: the timeline is gated *signal*-time, and each seam marks
    // where a silent stretch was cut out. One dashed marker per seam, on the
    // reference timeline (the main waveform is drawn shifted, but both
    // channels are spliced at identical positions).
    for &s in &snap.splices {
        let t = s as f64 / sr;
        if t <= view.start_s || t >= view.start_s + view.span_s {
            continue;
        }
        let x = rect.left() + ((t - view.start_s) / view.span_s) as f32 * rect.width();
        painter.add(egui::Shape::dashed_line(
            &[Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            Stroke::new(1.0, TEXT_DIM.gamma_multiply(0.5)),
            4.0,
            6.0,
        ));
    }

    // --- Waveforms: reference first, ghost, then main on top. Translucent so
    // the overlap region shows both signals. Over-zoomed views (interpolated
    // buckets) draw as connected lines instead of per-column stubs.
    let as_line = span / (cols as f64) < 1.0;
    let half = rect.height() / 2.0 - 4.0;
    let y_of = |v: f32| rect.center().y - (v * cache.gain).clamp(-1.0, 1.0) * half;
    draw_envelope(&painter, rect, &cache.raw, y_of, TEXT_DIM.gamma_multiply(0.4), as_line);
    draw_envelope(
        &painter,
        rect,
        &cache.reference,
        y_of,
        ACCENT_REF.gamma_multiply(0.75),
        as_line,
    );
    draw_envelope(
        &painter,
        rect,
        &cache.main,
        y_of,
        ACCENT_MAIN.gamma_multiply(0.62),
        as_line,
    );

    draw_capture_overlay(&painter, rect, &args.overlay);

    PanelOutput { response }
}

fn set_span(vs: &mut WaveViewState, len_s: f64, span: f64) {
    let span = span.min(len_s);
    let center = match vs.view {
        Some(v) => v.start_s + v.span_s / 2.0,
        None => len_s / 2.0,
    };
    vs.view = Some(TimeView {
        start_s: (center - span / 2.0).clamp(0.0, (len_s - span).max(0.0)),
        span_s: span,
    });
}

pub(crate) fn legend_chip(ui: &mut Ui, color: Color32, label: &str) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
    ui.label(egui::RichText::new(label).small());
}

fn draw_envelope(
    painter: &egui::Painter,
    rect: egui::Rect,
    env: &[MinMax],
    y_of: impl Fn(f32) -> f32,
    color: Color32,
    as_line: bool,
) {
    if env.is_empty() {
        return;
    }
    if as_line {
        // Over-zoomed: the buckets are single interpolated values (min ==
        // max); connect them so the waveform reads as a line, not dots.
        let points: Vec<Pos2> = env
            .iter()
            .enumerate()
            .map(|(i, mm)| Pos2::new(rect.left() + i as f32 + 0.5, y_of(mm.max)))
            .collect();
        painter.add(egui::Shape::line(points, Stroke::new(1.5, color)));
        return;
    }
    let stroke = Stroke::new(1.0, color);
    for (i, mm) in env.iter().enumerate() {
        let x = rect.left() + i as f32 + 0.5;
        if x > rect.right() {
            break;
        }
        let y_top = y_of(mm.max);
        // At least one pixel tall so silence still draws a center line.
        let y_bot = y_of(mm.min).max(y_top + 0.75);
        painter.line_segment([Pos2::new(x, y_top), Pos2::new(x, y_bot)], stroke);
    }
}

fn draw_time_grid(painter: &egui::Painter, rect: egui::Rect, view: &TimeView) {
    let step = nice_step(view.span_s, 6.0);
    let mut t = (view.start_s / step).ceil() * step;
    let stroke = Stroke::new(1.0, GRID_COLOR);
    while t < view.start_s + view.span_s {
        let x = rect.left() + ((t - view.start_s) / view.span_s) as f32 * rect.width();
        painter.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())], stroke);
        painter.text(
            Pos2::new(x + 3.0, rect.bottom() - 3.0),
            Align2::LEFT_BOTTOM,
            fmt_time(t),
            FontId::proportional(10.0),
            TEXT_DIM,
        );
        t += step;
    }
    // Zero line.
    painter.line_segment(
        [
            Pos2::new(rect.left(), rect.center().y),
            Pos2::new(rect.right(), rect.center().y),
        ],
        stroke,
    );
}

fn draw_capture_overlay(painter: &egui::Painter, rect: egui::Rect, overlay: &CaptureOverlay) {
    match *overlay {
        CaptureOverlay::Idle => {}
        CaptureOverlay::Armed {
            main_quiet,
            ref_quiet,
        } => {
            painter.text(
                rect.center_top() + Vec2::new(0.0, 10.0),
                Align2::CENTER_CENTER,
                format!("Armed — waiting for signal ({})", quiet_label(main_quiet, ref_quiet)),
                FontId::proportional(12.0),
                ACCENT_LIVE,
            );
        }
        CaptureOverlay::Capturing {
            frac,
            secs,
            paused_reason,
        } => {
            let bar = egui::Rect::from_min_size(
                rect.left_top(),
                Vec2::new(rect.width() * frac.clamp(0.0, 1.0), 3.0),
            );
            painter.rect_filled(bar, 0.0, ACCENT_MAIN);
            let label = match paused_reason {
                Some(reason) => format!("Capturing… {secs:.1} s (paused — {reason})"),
                None => format!("Capturing… {secs:.1} s"),
            };
            painter.text(
                rect.center_top() + Vec2::new(0.0, 10.0),
                Align2::CENTER_CENTER,
                label,
                FontId::proportional(12.0),
                ACCENT_MAIN,
            );
        }
    }
}

/// Which input(s) are holding the gate shut, as display text.
pub(crate) fn quiet_label(main_quiet: bool, ref_quiet: bool) -> &'static str {
    match (main_quiet, ref_quiet) {
        (true, true) => "both inputs quiet",
        (true, false) => "main quiet",
        (false, true) => "ref quiet — is the sidechain connected?",
        // Transient: both envelopes crossed the threshold this very block.
        (false, false) => "opening…",
    }
}

/// 1/2/5 × 10^k step targeting roughly `target` divisions.
pub fn nice_step(span: f64, target: f64) -> f64 {
    let raw = (span / target).max(1e-9);
    let mag = 10f64.powf(raw.log10().floor());
    let norm = raw / mag;
    let step = if norm <= 1.0 {
        1.0
    } else if norm <= 2.0 {
        2.0
    } else if norm <= 5.0 {
        5.0
    } else {
        10.0
    };
    step * mag
}

pub fn fmt_time(t: f64) -> String {
    let ms = t * 1000.0;
    if t.abs() >= 1.0 {
        format!("{t:.2} s")
    } else if ms.abs() >= 10.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{ms:.2} ms")
    }
}
