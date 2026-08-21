//! The frequency-domain ("Spectrum") panel: comb-filter visualization.
//!
//! Draws the magnitude spectrum of the two captures *summed*, twice: as
//! captured (the comb filter the user gets mixing both mics with no
//! correction) and with the currently applied shift + polarity (which
//! follows trim drags live). Both curves are synthesized per display width
//! from the snapshot's Welch spectra — see `spectrum.rs` for the math; no
//! FFT runs on the GUI thread.

use std::sync::Arc;

use nih_plug_egui::egui::{self, Align2, FontId, Pos2, Sense, Stroke, StrokeKind, Ui, Vec2};

use super::decimate::MinMax;
use super::freq_scale::{bucket_curve, bucket_edges, fmt_hz, log_ticks};
use super::waveform_view::{legend_chip, nice_step};
use super::{
    LowerPanelTab, PanelOutput, ACCENT_LIVE, CURVE_COLOR, GRID_COLOR, PANEL_BG, TEXT_DIM,
};
use crate::shared::AnalysisSnapshot;
use crate::spectrum::synth_sum_db;

/// Fixed height of the dB axis; the top adapts to the data (see `y_top_db`).
const SPAN_DB: f32 = 60.0;
/// Lower bound of the log-frequency axis.
const LOG_F_LO: f64 = 20.0;

/// Everything that only changes with the snapshot or the view itself.
#[derive(PartialEq, Clone, Copy)]
struct SpecStaticKey {
    snap: usize,
    log: bool,
    cols: usize,
}

/// Additionally what the corrected curve depends on. Split from
/// [`SpecStaticKey`] so a trim drag re-synthesizes only that one curve.
#[derive(PartialEq, Clone, Copy)]
struct SpecLiveKey {
    stat: SpecStaticKey,
    net_bits: u32,
    flip: bool,
}

pub struct SpectrumCache {
    static_key: SpecStaticKey,
    live_key: SpecLiveKey,
    /// Bucket edges in bin-index units, shared by both curves.
    edges: Vec<f64>,
    captured_db: Vec<f32>,
    captured_env: Vec<MinMax>,
    corrected_db: Vec<f32>,
    corrected_env: Vec<MinMax>,
    /// Top of the dB axis, from the captured curve only — its comb peaks
    /// already reach the fully-coherent level, and pinning the axis to the
    /// shift-independent curve keeps it still during drags.
    y_top_db: f32,
}

pub struct SpectrumArgs<'a> {
    pub snapshot: Option<&'a Arc<AnalysisSnapshot>>,
    /// Currently applied shift (detected + trim, clamped), in ms.
    pub net_ms: f32,
    /// Polarity as displayed (mirrors the waveform panel's flip).
    pub flip_main: bool,
    pub align_on: bool,
}

/// Header row + canvas. `ms_per_px` is always `None`: a drag along a
/// frequency axis has no meaning in milliseconds, so the shared trim-drag
/// gesture stays inert on this panel.
pub fn show(
    ui: &mut Ui,
    height: f32,
    args: &SpectrumArgs,
    tab: &mut LowerPanelTab,
    log_axis: &mut bool,
    cache: &mut Option<SpectrumCache>,
) -> PanelOutput {
    ui.horizontal(|ui| {
        super::lower_tab_selector(ui, tab);
        ui.add_space(8.0);
        legend_chip(ui, CURVE_COLOR.gamma_multiply(0.55), "Captured sum");
        legend_chip(ui, ACCENT_LIVE, "Corrected sum");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .selectable_label(*log_axis, "Log f")
                .on_hover_text("Logarithmic frequency axis (comb notches are evenly spaced on a linear one)")
                .clicked()
            {
                *log_axis = !*log_axis;
            }
        });
    });
    let log = *log_axis;

    let (response, painter) =
        ui.allocate_painter(Vec2::new(ui.available_width(), height), Sense::click_and_drag());
    let rect = response.rect.shrink(1.0);
    painter.rect_filled(rect, 4.0, PANEL_BG);
    painter.rect_stroke(rect, 4.0, Stroke::new(1.0, GRID_COLOR), StrokeKind::Inside);

    let center_message = |text: &str| {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            text,
            FontId::proportional(13.0),
            TEXT_DIM,
        );
    };
    let spec = match args.snapshot {
        Some(s) => match &s.spectrum {
            Some(spec) => spec,
            None => {
                center_message("No spectrum — the capture was rejected before analysis");
                return PanelOutput {
                    response,
                    ms_per_px: None,
                };
            }
        },
        None => {
            center_message("The spectrum appears after a capture");
            return PanelOutput {
                response,
                ms_per_px: None,
            };
        }
    };
    let snap = args.snapshot.unwrap();

    let sr = snap.sample_rate.max(1.0) as f64;
    let f_hi = sr / 2.0;
    let f_lo = if log { LOG_F_LO.min(f_hi / 2.0) } else { 0.0 };

    // --- Curves (cached; the corrected one is keyed separately so a trim
    // drag re-synthesizes only what changed) ---
    let cols = (rect.width().floor() as usize).max(8);
    let static_key = SpecStaticKey {
        snap: Arc::as_ptr(snap) as usize,
        log,
        cols,
    };
    let live_key = SpecLiveKey {
        stat: static_key,
        net_bits: args.net_ms.to_bits(),
        flip: args.flip_main,
    };
    let rebuild_static = !cache.as_ref().is_some_and(|c| c.static_key == static_key);
    let rebuild_live =
        rebuild_static || !cache.as_ref().is_some_and(|c| c.live_key == live_key);
    let mut c = cache.take().unwrap_or_else(|| SpectrumCache {
        static_key,
        live_key,
        edges: Vec::new(),
        captured_db: Vec::new(),
        captured_env: Vec::new(),
        corrected_db: Vec::new(),
        corrected_env: Vec::new(),
        y_top_db: 0.0,
    });
    if rebuild_static {
        let bin_hz = sr / spec.nfft as f64;
        c.edges = bucket_edges(f_lo.max(bin_hz * 1e-3), f_hi, bin_hz, cols, log);
        synth_sum_db(spec, 0.0, false, &mut c.captured_db);
        c.captured_env = bucket_curve(&c.captured_db, &c.edges);
        let max_db = c.captured_db.iter().fold(f32::MIN, |m, &v| m.max(v));
        c.y_top_db = (max_db / 5.0).ceil() * 5.0 + 5.0;
        c.static_key = static_key;
    }
    if rebuild_live {
        let delta_samples = args.net_ms as f64 / 1000.0 * sr;
        synth_sum_db(spec, delta_samples, args.flip_main, &mut c.corrected_db);
        c.corrected_env = bucket_curve(&c.corrected_db, &c.edges);
        c.live_key = live_key;
    }
    *cache = Some(c);
    let c = cache.as_ref().unwrap();

    let x_of = |f: f64| -> f32 {
        let t = if log {
            (f / f_lo.max(1e-9)).ln() / (f_hi / f_lo.max(1e-9)).ln()
        } else {
            (f - f_lo) / (f_hi - f_lo)
        };
        rect.left() + (t as f32).clamp(0.0, 1.0) * rect.width()
    };
    let y_of = |v: f32| {
        let t = ((c.y_top_db - v) / SPAN_DB).clamp(0.0, 1.0);
        rect.top() + 3.0 + t * (rect.height() - 6.0)
    };

    // --- Grid ---
    let stroke = Stroke::new(1.0, GRID_COLOR);
    let ticks: Vec<f64> = if log {
        log_ticks(f_lo, f_hi)
    } else {
        let step = nice_step(f_hi - f_lo, 8.0);
        let mut ticks = Vec::new();
        let mut f = step;
        while f < f_hi {
            ticks.push(f);
            f += step;
        }
        ticks
    };
    for &f in &ticks {
        let x = x_of(f);
        painter.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())], stroke);
        // Skip labels that would collide with the "Hz" unit in the corner.
        if x < rect.right() - 30.0 {
            painter.text(
                Pos2::new(x + 3.0, rect.bottom() - 3.0),
                Align2::LEFT_BOTTOM,
                fmt_hz(f),
                FontId::proportional(10.0),
                TEXT_DIM,
            );
        }
    }
    for i in 0..=(SPAN_DB as i32 / 10) {
        let db = c.y_top_db - 10.0 * i as f32;
        let y = y_of(db);
        painter.line_segment([Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)], stroke);
        let label = if i == 0 {
            format!("{db:.0} dB")
        } else {
            format!("{db:.0}")
        };
        painter.text(
            Pos2::new(rect.left() + 4.0, y + 2.0),
            Align2::LEFT_TOP,
            label,
            FontId::proportional(10.0),
            TEXT_DIM,
        );
    }
    painter.text(
        Pos2::new(rect.right() - 4.0, rect.bottom() - 3.0),
        Align2::RIGHT_BOTTOM,
        "Hz",
        FontId::proportional(10.0),
        TEXT_DIM,
    );

    // --- The two curves: captured behind, corrected on top ---
    // At the max-shift extreme the comb spacing drops below the bin spacing;
    // min/max bucketing then honestly renders a filled band instead of
    // aliased wiggles.
    draw_envelope(&painter, rect, &c.captured_env, y_of, CURVE_COLOR.gamma_multiply(0.55));
    draw_envelope(&painter, rect, &c.corrected_env, y_of, ACCENT_LIVE);

    // --- Readout, mirroring the correlation panel's live-marker language ---
    let (readout, color) = if !args.align_on {
        ("alignment off — corrected = captured".to_string(), TEXT_DIM)
    } else {
        let pol = if args.flip_main { ", inverted" } else { "" };
        (format!("corrected @ {:+.2} ms{pol}", args.net_ms), ACCENT_LIVE)
    };
    painter.text(
        Pos2::new(rect.right() - 6.0, rect.top() + 4.0),
        Align2::RIGHT_TOP,
        readout,
        FontId::proportional(11.0),
        color,
    );

    PanelOutput {
        response,
        ms_per_px: None,
    }
}

fn draw_envelope(
    painter: &egui::Painter,
    rect: egui::Rect,
    env: &[MinMax],
    y_of: impl Fn(f32) -> f32,
    color: egui::Color32,
) {
    let stroke = Stroke::new(1.0, color);
    let mut top_pts = Vec::with_capacity(env.len());
    let mut bot_pts = Vec::with_capacity(env.len());
    for (i, mm) in env.iter().enumerate() {
        let x = rect.left() + i as f32 + 0.5;
        if x > rect.right() {
            break;
        }
        let y_top = y_of(mm.max);
        // At least ~a pixel tall so flat stretches still draw a line.
        let y_bot = y_of(mm.min).max(y_top + 0.75);
        painter.line_segment([Pos2::new(x, y_top), Pos2::new(x, y_bot)], stroke);
        top_pts.push(Pos2::new(x, y_top));
        bot_pts.push(Pos2::new(x, y_bot));
    }
    // A log axis mixes sub-bin buckets (low decades) with many-bin buckets
    // (top decade) in one view, so there is no global stub/line mode switch
    // like the other panels'; outlining the envelope's edges instead keeps
    // the over-zoomed stretches reading as a continuous curve.
    painter.add(egui::Shape::line(top_pts, stroke));
    painter.add(egui::Shape::line(bot_pts, stroke));
}
