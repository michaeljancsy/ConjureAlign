//! The frequency-domain ("Spectrum") panel: comb-filter visualization.
//!
//! Draws the magnitude spectrum of the two captures *summed*, twice: as
//! captured (the comb filter the user gets mixing both mics with no
//! correction) and with the currently applied shift + polarity (which
//! follows trim drags live). Both curves are synthesized per display width
//! from the snapshot's Welch spectra — see `spectrum.rs` for the math; no
//! FFT runs on the GUI thread per frame. The one exception is the header's
//! FFT-size selector: picking a size that differs from the snapshot's runs a
//! single Welch re-estimate here, cached per (snapshot, size), so switching
//! back and forth never repeats an FFT.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use nih_plug_egui::egui::{self, Align2, FontId, Pos2, Sense, Stroke, StrokeKind, Ui, Vec2};

use super::decimate::MinMax;
use super::freq_scale::{bucket_curve, bucket_edges, fmt_hz, log_ticks};
use super::waveform_view::{legend_chip, nice_step};
use super::{
    view_math, LowerPanelTab, PanelOutput, ACCENT_LIVE, CURVE_COLOR, GRID_COLOR, PANEL_BG,
    TEXT_DIM,
};
use crate::params::SPECTRUM_NFFT_OPTIONS;
use crate::shared::AnalysisSnapshot;
use crate::spectrum::{
    estimate_with_seam_fallback, nfft_fits, pick_nfft, prealign_lag, synth_sum_db, SpectrumData,
};

/// Fixed height of the dB axis; the top adapts to the data (`y_top_db`).
const SPAN_DB: f32 = 60.0;
/// Lower bound of the log-frequency axis.
const LOG_F_LO: f64 = 20.0;

/// Pan/zoom state for the spectrum panel. The dB axis is plugin-scaled,
/// always.
#[derive(Default)]
pub struct SpecViewState {
    /// `(f_lo, f_hi)` in Hz; `None` = the axis mode's full range. Reset when
    /// the log/linear mode toggles (the two spaces don't share a view).
    pub view: Option<(f64, f64)>,
}

/// Everything that only changes with the snapshot or the view itself.
/// (`span_db` is deliberately absent: vertical zoom only moves the y
/// mapping, never the cached curves.)
#[derive(PartialEq, Clone, Copy)]
struct SpecStaticKey {
    snap: usize,
    /// The resolved segment size — (snap, nfft) identifies the source
    /// spectra, so an FFT-selector change invalidates both curves.
    nfft: usize,
    log: bool,
    cols: usize,
    f_lo_bits: u64,
    f_hi_bits: u64,
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
    /// The persisted FFT-size selection (0 = Auto, else a fixed segment
    /// size); read every frame and written by the header's selector.
    pub nfft_choice: &'a AtomicU32,
}

/// Per-snapshot state for the FFT-size selector: the pre-alignment lag and
/// fit bound (computed ONCE per snapshot — `prealign_lag` scans the whole
/// correlation curve for a rejected-with-curve capture, far too much for a
/// per-frame path) plus the GUI-side Welch re-estimates keyed by nfft
/// (inner `None` = that size failed, don't retry). `snap` is the snapshot's
/// raw `Arc` pointer and is only a same-snapshot check for the frames while
/// this tab is visible — it does NOT keep the snapshot alive, so a freed
/// address can recur on a later snapshot (ABA). The editor's snapshot-changed
/// block therefore clears this cache like every other one.
pub struct SpectrumReestimates {
    snap: usize,
    /// Integer pre-alignment lag, identical to what the analysis task used
    /// (same inputs, same `prealign_lag`).
    d: i32,
    /// Largest segment that fully fits: `min(len) − |d|`; see
    /// [`nfft_fits`].
    usable: usize,
    entries: Vec<(usize, Option<SpectrumData>)>,
}

/// The segment size the panel displays for a stored selection: a listed,
/// fitting fixed choice is honored; anything else — Auto (0), an unlisted
/// or garbage persisted value, a size this capture can't fit — resolves to
/// the automatic [`pick_nfft`] size, the same degradation
/// `welch_for_capture` applies, so the GUI and the analysis task can never
/// disagree about the same stored value.
fn resolve_nfft(choice: u32, sample_rate: f32, usable: usize) -> Option<usize> {
    if SPECTRUM_NFFT_OPTIONS.contains(&choice) && nfft_fits(choice as usize, usable) {
        return Some(choice as usize);
    }
    pick_nfft(sample_rate, usable)
}

/// Header row + canvas. `height` is the panel's TOTAL height, header row
/// included.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut Ui,
    height: f32,
    args: &SpectrumArgs,
    tab: &mut LowerPanelTab,
    log_axis: &mut bool,
    vs: &mut SpecViewState,
    reestimates: &mut Option<SpectrumReestimates>,
    cache: &mut Option<SpectrumCache>,
) -> PanelOutput {
    // Refresh the per-snapshot selector state on snapshot change (a pointer
    // compare per frame). `None` until a capture with a correlation curve
    // exists (mirrors the rejected-before-analysis treatment of the canvas).
    let snap_key = args.snapshot.map(|s| Arc::as_ptr(s) as usize);
    if reestimates.as_ref().map(|r| r.snap) != snap_key {
        *reestimates = args.snapshot.and_then(|s| {
            if s.corr.is_empty() {
                return None;
            }
            let d = prealign_lag(&s.outcome, &s.corr, s.max_shift_samples);
            let usable = s
                .main
                .len()
                .min(s.reference.len())
                .saturating_sub(d.unsigned_abs() as usize);
            Some(SpectrumReestimates {
                snap: Arc::as_ptr(s) as usize,
                d,
                usable,
                entries: Vec::new(),
            })
        });
    }
    let usable = reestimates.as_ref().map(|r| r.usable);

    let header = ui.horizontal(|ui| {
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
                // The two axis spaces don't share a meaningful view.
                vs.view = None;
            }
            nfft_selector(ui, args.nfft_choice, usable);
            // Fills the row's dead middle, so the legend costs no height.
            ui.add_space(8.0);
            super::gesture_legend(ui);
        });
    });
    let log = *log_axis;

    let (response, painter) = ui.allocate_painter(
        Vec2::new(
            ui.available_width(),
            super::canvas_height(ui, height, &header.response),
        ),
        Sense::click_and_drag(),
    );
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
    let auto_spec = match args.snapshot {
        Some(s) => match &s.spectrum {
            Some(spec) => spec,
            None => {
                center_message("No spectrum — the capture was rejected before analysis");
                return PanelOutput { response };
            }
        },
        None => {
            center_message("The spectrum appears after a capture");
            return PanelOutput { response };
        }
    };
    let snap = args.snapshot.unwrap();

    // Resolve the stored selection against this snapshot; whenever the
    // resolved size differs from the size the snapshot's spectrum was built
    // at — a fixed size picked after the capture, or Auto after a capture
    // that baked a fixed override in — re-estimate at the resolved size.
    // The estimate runs synchronously here, an accepted ONE-SHOT frame
    // hitch (worst case a few tens of ms at 192 kHz / nfft 32768), cached
    // per (snapshot, size) in `reestimates` so it never repeats; it is the
    // only FFT that ever runs on the GUI thread.
    let choice = args.nfft_choice.load(Ordering::Relaxed);
    let spec: &SpectrumData = match reestimates.as_mut() {
        Some(re) => match resolve_nfft(choice, snap.sample_rate, re.usable) {
            Some(n) if n != auto_spec.nfft => {
                let i = match re.entries.iter().position(|(k, _)| *k == n) {
                    Some(i) => i,
                    None => {
                        let s = estimate_with_seam_fallback(
                            &snap.main,
                            &snap.reference,
                            re.d,
                            n,
                            &snap.splices,
                        );
                        re.entries.push((n, s));
                        re.entries.len() - 1
                    }
                };
                // Defensive only: `resolve_nfft` guarantees a segment fits
                // and the seam fallback covers the all-crossed case.
                re.entries[i].1.as_ref().unwrap_or(auto_spec)
            }
            _ => auto_spec,
        },
        // Unreachable once `auto_spec` exists (a spectrum implies a
        // non-empty correlation curve), but harmless.
        None => auto_spec,
    };

    let sr = snap.sample_rate.max(1.0) as f64;
    let base_hi = sr / 2.0;
    let base_lo = if log { LOG_F_LO.min(base_hi / 2.0) } else { 0.0 };

    // Resolve the view, then apply this frame's gestures to it. Horizontal
    // pan/zoom operates in ln-f space on the log axis so gestures feel
    // uniform across the decades.
    let (mut v_lo, mut v_hi) = match vs.view {
        // Ordered bounds only: `lo + 1e-6` exceeds `base_hi` for a stored view
        // sitting at the top of the range (or one stored before the sample
        // rate dropped), and `clamp` panics on `min > max`. Non-finite is
        // dropped rather than clamped — NaN survives `clamp` and would reach
        // the next frame's bounds.
        Some((lo, hi)) if lo.is_finite() && hi.is_finite() => {
            let lo = lo.clamp(base_lo, (base_hi - 1e-6).max(base_lo));
            (lo, hi.clamp((lo + 1e-6).min(base_hi), base_hi))
        }
        _ => (base_lo, base_hi),
    };
    if response.hovered() {
        let (zoom, scroll) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta));
        if zoom != 1.0 {
            let frac = response
                .hover_pos()
                .map(|p| ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0))
                .unwrap_or(0.5) as f64;
            if log {
                let (bl, bh) = (base_lo.max(1e-9).ln(), base_hi.ln());
                let l = v_lo.max(1e-9).ln();
                let (s, sp) = view_math::zoom_about(
                    l,
                    v_hi.ln() - l,
                    frac,
                    zoom as f64,
                    std::f64::consts::LN_2 / 2.0, // half an octave
                    bl,
                    bh,
                );
                v_lo = s.exp();
                v_hi = (s + sp).exp();
            } else {
                let (s, sp) = view_math::zoom_about(
                    v_lo,
                    v_hi - v_lo,
                    frac,
                    zoom as f64,
                    200.0,
                    base_lo,
                    base_hi,
                );
                v_lo = s;
                v_hi = s + sp;
            }
            vs.view = Some((v_lo, v_hi));
        }
        let pan_px = if scroll.x.abs() > scroll.y.abs() {
            scroll.x
        } else {
            scroll.y
        };
        if pan_px != 0.0 && rect.width() > 1.0 {
            let (lo, hi) = pan_view(log, v_lo, v_hi, pan_px, rect.width(), base_lo, base_hi);
            v_lo = lo;
            v_hi = hi;
            vs.view = Some((v_lo, v_hi));
        }
    }
    if response.dragged() {
        let dx = response.drag_delta().x;
        if dx != 0.0 && rect.width() > 1.0 {
            let (lo, hi) = pan_view(log, v_lo, v_hi, dx, rect.width(), base_lo, base_hi);
            v_lo = lo;
            v_hi = hi;
            vs.view = Some((v_lo, v_hi));
        }
    }
    if response.double_clicked() {
        vs.view = None;
        v_lo = base_lo;
        v_hi = base_hi;
    }

    // --- Curves (cached; the corrected one is keyed separately so a trim
    // drag re-synthesizes only what changed) ---
    let cols = (rect.width().floor() as usize).max(8);
    let static_key = SpecStaticKey {
        snap: Arc::as_ptr(snap) as usize,
        nfft: spec.nfft,
        log,
        cols,
        f_lo_bits: v_lo.to_bits(),
        f_hi_bits: v_hi.to_bits(),
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
        c.edges = bucket_edges(v_lo.max(bin_hz * 1e-3), v_hi, bin_hz, cols, log);
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
            (f / v_lo.max(1e-9)).ln() / (v_hi / v_lo.max(1e-9)).ln()
        } else {
            (f - v_lo) / (v_hi - v_lo)
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
        log_ticks(v_lo, v_hi)
    } else {
        let step = nice_step(v_hi - v_lo, 8.0);
        let mut ticks = Vec::new();
        let mut f = (v_lo / step).ceil() * step;
        if f <= 0.0 {
            f = step;
        }
        while f < v_hi {
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

    PanelOutput { response }
}

/// The header's FFT segment-size ComboBox. Writes the persisted selection
/// (0 = Auto); `usable` gates which fixed sizes are selectable for the
/// current capture (`None` = no analyzed capture yet, whole combo disabled —
/// the choice stays visible so the row doesn't jump when the first capture
/// lands). An unlisted persisted value (hand-edited or other-version
/// session) resolves to Auto everywhere, so it displays as Auto too — and
/// gets normalized back to 0 in the store below. A kept choice this capture
/// can't fit shows a `*`: the canvas is rendering the automatic size, and
/// the next capture may fit the choice again.
fn nfft_selector(ui: &mut Ui, choice: &AtomicU32, usable: Option<usize>) {
    let current = choice.load(Ordering::Relaxed);
    let effective = if SPECTRUM_NFFT_OPTIONS.contains(&current) {
        current
    } else {
        0
    };
    let honored = usable.is_none_or(|u| effective == 0 || nfft_fits(effective as usize, u));
    let mut selected = effective;
    ui.add_enabled_ui(usable.is_some(), |ui| {
        egui::ComboBox::from_id_salt("spec-nfft")
            .width(94.0)
            .selected_text(match (effective, honored) {
                (0, _) => "FFT: Auto".to_owned(),
                (n, true) => format!("FFT: {n}"),
                (n, false) => format!("FFT: {n}*"),
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut selected, 0, "Auto")
                    .on_hover_text("≈6 Hz bins (8192 at 44.1/48 kHz)");
                for &n in &SPECTRUM_NFFT_OPTIONS {
                    let fits = usable.is_some_and(|u| nfft_fits(n as usize, u));
                    ui.add_enabled_ui(fits, |ui| {
                        ui.selectable_value(&mut selected, n, n.to_string())
                    })
                    .inner
                    .on_disabled_hover_text("Capture too short for this FFT size");
                }
            })
            .response
            .on_hover_text(if honored {
                "FFT segment size: larger = finer frequency resolution \
                 but fewer averaged segments (noisier curve)"
            } else {
                "* This capture is too short for the selected FFT size; \
                 showing the automatic size instead. The choice is kept \
                 for the next capture."
            });
    });
    if selected != current {
        choice.store(selected, Ordering::Relaxed);
    }
}

/// Pans the `(v_lo, v_hi)` frequency view by `px` display pixels, operating
/// in ln-f space on the log axis so the pan speed is uniform per decade.
fn pan_view(
    log: bool,
    v_lo: f64,
    v_hi: f64,
    px: f32,
    width: f32,
    base_lo: f64,
    base_hi: f64,
) -> (f64, f64) {
    if log {
        let (bl, bh) = (base_lo.max(1e-9).ln(), base_hi.ln());
        let l = v_lo.max(1e-9).ln();
        let span = v_hi.ln() - l;
        let s = view_math::pan(l, span, -(px as f64) * span / width as f64, bl, bh);
        (s.exp(), (s + span).exp())
    } else {
        let span = v_hi - v_lo;
        let s = view_math::pan(v_lo, span, -(px as f64) * span / width as f64, base_lo, base_hi);
        (s, s + span)
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

#[cfg(test)]
mod tests {
    use super::resolve_nfft;

    /// Pins the selector's resolution policy: honored when listed and
    /// fitting, otherwise the automatic size — including the Auto choice
    /// itself, an unlisted/garbage persisted value, and a kept choice this
    /// capture can't fit.
    #[test]
    fn nfft_resolution() {
        // Auto → pick_nfft's ≈6 Hz-bin size.
        assert_eq!(resolve_nfft(0, 48_000.0, 1 << 20), Some(8192));
        // A listed, fitting choice is honored — even when it differs from
        // what a capture-time override baked into the snapshot.
        assert_eq!(resolve_nfft(2048, 48_000.0, 1 << 20), Some(2048));
        assert_eq!(resolve_nfft(32_768, 48_000.0, 1 << 20), Some(32_768));
        // Doesn't fit this capture → automatic size, choice kept elsewhere.
        assert_eq!(resolve_nfft(32_768, 48_000.0, 5000), Some(4096));
        // Unlisted persisted values (hand-edited/other-version session)
        // behave exactly like Auto — the same degradation the analysis
        // task's `welch_for_capture` applies.
        assert_eq!(resolve_nfft(100, 48_000.0, 1 << 20), Some(8192));
        assert_eq!(resolve_nfft(3000, 48_000.0, 1 << 20), Some(8192));
        // Too short for any segment at all.
        assert_eq!(resolve_nfft(0, 48_000.0, 200), None);
    }
}
