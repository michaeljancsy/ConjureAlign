//! The custom editor: overlaid capture waveforms, a cross-correlation graph
//! with live markers, and the parameter controls.
//!
//! Threading rules (see also `shared.rs` / `capture.rs`): everything here runs
//! on the GUI thread. It reads params and the persisted atomics, the
//! `AnalysisSnapshot` published by the background task, and the capture
//! atomics through [`CaptureHandle`] — and must never touch
//! `CaptureState::data`.

// Public so the `gui-preview` example can render the panels headless with
// synthetic data (see examples/gui_preview.rs).
pub mod correlation_view;
pub mod decimate;
pub mod freq_scale;
pub mod spectrum_view;
pub mod view_math;
pub mod waveform_view;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use nih_plug::prelude::{BoolParam, Editor, EnumParam, ParamSetter};
use nih_plug_egui::{
    create_egui_editor, egui, resizable_window::ResizableWindow, widgets, EguiState,
};

use crate::analysis::{RejectReason, CONFIDENCE_THRESHOLD};
use crate::capture::{
    CaptureHandle, GATE_MAIN_QUIET, GATE_OPEN, GATE_REF_QUIET, PHASE_ANALYZING, PHASE_ARMED,
    PHASE_CAPTURING, PHASE_IDLE,
};
use crate::params::{AudioAlignParams, PolarityMode, TRIM_RANGE_MS};
use crate::shared::{AnalysisSnapshot, GuiShared};

use correlation_view::{CorrArgs, CorrCache, CorrViewState};
use spectrum_view::{SpecViewState, SpectrumArgs, SpectrumCache};
use waveform_view::{quiet_label, CaptureOverlay, WaveArgs, WaveViewState};

use egui::Color32;

// Shared palette (dark theme).
pub(crate) const PANEL_BG: Color32 = Color32::from_gray(24);
pub(crate) const GRID_COLOR: Color32 = Color32::from_gray(48);
pub(crate) const TEXT_DIM: Color32 = Color32::from_gray(150);
pub(crate) const ACCENT_MAIN: Color32 = Color32::from_rgb(0xff, 0xb7, 0x4d); // amber
pub(crate) const ACCENT_REF: Color32 = Color32::from_rgb(0x4d, 0xd0, 0xe1); // cyan
pub(crate) const ACCENT_DETECTED: Color32 = Color32::from_rgb(0x81, 0xc7, 0x84); // green
pub(crate) const ACCENT_LIVE: Color32 = Color32::from_rgb(0xff, 0xd5, 0x4f); // yellow
pub(crate) const ACCENT_WARN: Color32 = Color32::from_rgb(0xe5, 0x73, 0x73); // red
pub(crate) const CURVE_COLOR: Color32 = Color32::from_rgb(0x64, 0xb5, 0xf6); // blue
// The capture button's fills — deliberately the loudest thing in the window,
// since nothing else happens until a user finds it. Green when idle (press
// me), red while a capture is running (recording, press to stop).
const CAPTURE_GREEN: Color32 = Color32::from_rgb(0x34, 0xc7, 0x59);
const CAPTURE_RED: Color32 = Color32::from_rgb(0xff, 0x3b, 0x30);

/// What a panel hands back so the shared arrow-key trim nudge can run on it.
pub struct PanelOutput {
    pub response: egui::Response,
}

/// Which graph occupies the lower panel.
#[derive(Default, Debug, PartialEq, Eq, Clone, Copy)]
pub enum LowerPanelTab {
    #[default]
    Correlation,
    Spectrum,
}

/// The tab strip both lower panels place at the left of their header row (a
/// row of its own would eat into the graph below it).
pub(crate) fn lower_tab_selector(ui: &mut egui::Ui, tab: &mut LowerPanelTab) {
    for (value, label) in [
        (LowerPanelTab::Correlation, "Correlation"),
        (LowerPanelTab::Spectrum, "Spectrum"),
    ] {
        if ui
            .selectable_label(*tab == value, egui::RichText::new(label).small())
            .clicked()
        {
            *tab = value;
        }
    }
}

/// What is left of a panel's total height once its header row and the
/// spacing below it are taken off. Panels measure their own header instead
/// of the caller assuming one, so the split below always adds up and the
/// window bottom never shows dead space.
pub(crate) fn canvas_height(ui: &egui::Ui, total: f32, header: &egui::Response) -> f32 {
    (total - header.rect.height() - ui.spacing().item_spacing.y).max(40.0)
}

/// The gesture legend, drawn in the lower panel's header row — that row's
/// height is already budgeted, so the legend costs none of its own. It
/// truncates rather than wrapping when the window is narrow. Modifiers are
/// spelled out: egui's default font renders ⌘ but not ⌥/⇧/arrows (boxes).
pub(crate) fn gesture_legend(ui: &mut egui::Ui) {
    // Allocated to exactly what is left of the row: a bare truncating label
    // asks for the full available width, which inside a right-to-left layout
    // grows the row and shoves its buttons off the panel's right edge.
    let size = egui::Vec2::new(ui.available_width(), ui.spacing().interact_size.y);
    ui.allocate_ui_with_layout(
        size,
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(
                        "drag / scroll: pan  ·  pinch or cmd-scroll: zoom  ·  \
                         arrow keys: nudge trim  ·  double-click: fit",
                    )
                    .small()
                    .color(TEXT_DIM),
                )
                .truncate(),
            );
        },
    );
}

/// GUI-only state, alive as long as the editor object (survives window
/// close/reopen). Public for the same reason as [`draw_ui`].
pub struct EditorState {
    /// Latest snapshot the GUI has seen; compared to the published one by
    /// pointer each frame.
    snapshot: Option<Arc<AnalysisSnapshot>>,
    wave: WaveViewState,
    corr_view: CorrViewState,
    corr_cache: Option<CorrCache>,
    spec_view: SpecViewState,
    show_raw: bool,
    lower_tab: LowerPanelTab,
    spectrum_log: bool,
    spectrum_cache: Option<SpectrumCache>,
    /// Last trim value this editor sent. `set_parameter` only queues the
    /// change until the audio thread drains the event queue, so the nudge
    /// bases follow-up edits on this instead of the (possibly stale)
    /// parameter; cleared once the parameter catches up.
    pending_trim: Option<f32>,
    /// Measured width of the Capture/Align/Polarity row, for centering it.
    capture_row_w: f32,
}

// Manual because `spectrum_log` defaults to true; everything else matches
// what derive(Default) produced before.
impl Default for EditorState {
    fn default() -> Self {
        Self {
            snapshot: None,
            wave: WaveViewState::default(),
            corr_view: CorrViewState::default(),
            corr_cache: None,
            spec_view: SpecViewState::default(),
            show_raw: false,
            lower_tab: LowerPanelTab::default(),
            spectrum_log: true,
            spectrum_cache: None,
            pending_trim: None,
            // Estimate for the first frame only; measured thereafter.
            capture_row_w: 300.0,
        }
    }
}

impl EditorState {
    /// Preview-only: state that already holds a snapshot, as if an analysis
    /// had just published one (see [`draw_ui`]).
    pub fn with_snapshot(snapshot: Arc<AnalysisSnapshot>) -> Self {
        Self {
            snapshot: Some(snapshot),
            ..Self::default()
        }
    }
}

pub fn create(
    params: Arc<AudioAlignParams>,
    shared: Arc<GuiShared>,
    capture: CaptureHandle,
) -> Option<Box<dyn Editor>> {
    let egui_state: Arc<EguiState> = params.editor_state.clone();
    create_egui_editor(
        egui_state.clone(),
        EditorState::default(),
        |ctx, _state| {
            let mut style = (*ctx.style()).clone();
            style.visuals = egui::Visuals::dark();
            style.visuals.panel_fill = Color32::from_gray(18);
            ctx.set_style(style);
        },
        move |ctx, setter, state| {
            // Pick up a freshly published snapshot; invalidate the caches and
            // refit the waveform view when it changes.
            let latest = shared.snapshot.lock().unwrap().clone();
            let changed = match (&state.snapshot, &latest) {
                (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                (None, Some(_)) => true,
                _ => false,
            };
            if changed {
                state.snapshot = latest;
                state.wave = WaveViewState::default();
                state.corr_view = CorrViewState::default();
                state.corr_cache = None;
                state.spec_view = SpecViewState::default();
                state.spectrum_cache = None;
            }

            ResizableWindow::new("audio-align-resize")
                .min_size(egui::Vec2::new(600.0, 460.0))
                .show(ctx, egui_state.as_ref(), |ui| {
                    // Breathing room against the window border.
                    egui::Frame::new()
                        .inner_margin(egui::Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            draw_ui(ui, setter, state, &params, &shared, &capture);
                        });
                });

            // Keep animating through captures/analysis and drags even if the
            // host stops delivering input events.
            if capture.phase() != PHASE_IDLE {
                ctx.request_repaint();
            }
        },
    )
}

/// The whole editor body, one window margin in. Public so the `gui-preview`
/// example can render it headless (see examples/gui_preview.rs).
pub fn draw_ui(
    ui: &mut egui::Ui,
    setter: &ParamSetter,
    state: &mut EditorState,
    params: &AudioAlignParams,
    shared: &GuiShared,
    capture: &CaptureHandle,
) {
    let (net_ms, net_clamped) = net_shift(params, shared);
    let phase = capture.phase();

    // Once the host has applied our queued trim edit, the parameter is
    // authoritative again (also lets external automation take over).
    if let Some(p) = state.pending_trim {
        if (params.trim.value() - p).abs() < 0.005 {
            state.pending_trim = None;
        }
    }

    status_strip(ui, params, capture, shared, state.snapshot.as_deref(), net_ms, net_clamped);
    ui.separator();

    // The control bar sizes itself as a bottom panel and the graphs take
    // exactly what is left, so nothing has to budget a height it can only
    // guess at — which is what used to leave dead black space under the bar.
    // Bottom first: a central panel claims whatever the panels above and
    // below it did not.
    egui::TopBottomPanel::bottom("audio-align-controls")
        .frame(egui::Frame::new())
        .show_inside(ui, |ui| {
            ui.add_space(6.0);
            control_bar(ui, setter, params, &mut state.capture_row_w);
        });

    egui::CentralPanel::default()
        .frame(egui::Frame::new())
        .show_inside(ui, |ui| {
            graphs(ui, setter, state, params, shared, capture, net_ms, net_clamped, phase);
        });
}

/// The two stacked graph panels, filling the central region exactly: the
/// waveform takes its share of the height, the lower panel takes the rest.
#[allow(clippy::too_many_arguments)]
fn graphs(
    ui: &mut egui::Ui,
    setter: &ParamSetter,
    state: &mut EditorState,
    params: &AudioAlignParams,
    shared: &GuiShared,
    capture: &CaptureHandle,
    net_ms: f32,
    net_clamped: bool,
    phase: u8,
) {
    const PANEL_GAP: f32 = 4.0;
    let wave_h = ((ui.available_height() - PANEL_GAP) * 0.58).max(110.0);

    let gate = capture.gate_state();
    let overlay = match phase {
        PHASE_ARMED => CaptureOverlay::Armed {
            main_quiet: gate & GATE_MAIN_QUIET != 0,
            ref_quiet: gate & GATE_REF_QUIET != 0,
        },
        PHASE_CAPTURING => {
            let (filled, target) = capture.progress();
            // Valid for any in-flight capture: arming happens in process(),
            // which only runs under the activation that wrote this pair.
            let (_, sr) = shared.window();
            CaptureOverlay::Capturing {
                frac: if target > 0 {
                    filled as f32 / target as f32
                } else {
                    0.0
                },
                secs: filled as f32 / sr.max(1.0),
                paused_reason: (gate & GATE_OPEN == 0).then(|| {
                    quiet_label(gate & GATE_MAIN_QUIET != 0, gate & GATE_REF_QUIET != 0)
                }),
            }
        }
        _ => CaptureOverlay::Idle,
    };
    let flip_main = params.align_on.value()
        && match params.polarity.value() {
            PolarityMode::Auto => params.detected_polarity.load(Ordering::Relaxed),
            PolarityMode::Normal => false,
            PolarityMode::Inverted => true,
        };

    let wave_args = WaveArgs {
        snapshot: state.snapshot.as_ref(),
        net_ms,
        flip_main,
        overlay,
    };
    // push_id: the rejection banner above is emitted conditionally, and
    // without an explicit scope its appearance would shift the panels'
    // auto-Ids — killing an in-flight drag's Response identity mid-gesture.
    let wave_out = ui
        .push_id("wave-panel", |ui| {
            waveform_view::show(ui, wave_h, &wave_args, &mut state.show_raw, &mut state.wave)
        })
        .inner;
    handle_trim_nudge(ui, &wave_out, setter, params, &mut state.pending_trim);

    ui.add_space(PANEL_GAP);
    // Whatever the waveform panel and the gap left: no leftover, by
    // construction.
    let corr_h = ui.available_height().max(80.0);

    let lower_out = match state.lower_tab {
        LowerPanelTab::Correlation => {
            let detected_ms = (params.detected_confidence.load(Ordering::Relaxed) > 0.0)
                .then(|| params.detected_offset_ms.load(Ordering::Relaxed));
            let corr_args = CorrArgs {
                snapshot: state.snapshot.as_ref(),
                detected_ms,
                held: state
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.outcome.is_err()),
                net_ms,
                clamped: net_clamped,
                align_on: params.align_on.value(),
                active_window_ms: active_window_ms(shared),
            };
            ui.push_id("corr-panel", |ui| {
                correlation_view::show(
                    ui,
                    corr_h,
                    &corr_args,
                    &mut state.lower_tab,
                    &mut state.corr_view,
                    &mut state.corr_cache,
                )
            })
            .inner
        }
        LowerPanelTab::Spectrum => {
            let spec_args = SpectrumArgs {
                snapshot: state.snapshot.as_ref(),
                net_ms,
                flip_main,
                align_on: params.align_on.value(),
            };
            // Its own stable push_id (vs "corr-panel"): switching tabs must
            // not alias the two panels' widget state or Response identity.
            ui.push_id("spectrum-panel", |ui| {
                spectrum_view::show(
                    ui,
                    corr_h,
                    &spec_args,
                    &mut state.lower_tab,
                    &mut state.spectrum_log,
                    &mut state.spec_view,
                    &mut state.spectrum_cache,
                )
            })
            .inner
        }
    };
    handle_trim_nudge(ui, &lower_out, setter, params, &mut state.pending_trim);
}

/// The currently applied shift in ms and whether the window clamp kicked in.
/// Mirrors `AudioAlign::current_target()` — keep the two in sync.
fn net_shift(params: &AudioAlignParams, shared: &GuiShared) -> (f32, bool) {
    if !params.align_on.value() {
        return (0.0, false);
    }
    let raw = params.detected_offset_ms.load(Ordering::Relaxed) + params.trim.value();
    match active_window_ms(shared) {
        Some(w_ms) => {
            let clamped = raw.clamp(-w_ms, w_ms);
            (clamped, clamped != raw)
        }
        // Not activated yet — nothing is being applied, show the raw target.
        None => (raw, false),
    }
}

/// The active clamp window in ± ms; `None` before the first activation.
fn active_window_ms(shared: &GuiShared) -> Option<f32> {
    let (w, sr) = shared.window();
    if w == 0 {
        return None;
    }
    Some(w as f32 / sr.max(1.0) * 1000.0)
}

/// A status message that truncates instead of running under the capture
/// button when the window is narrow.
fn status_label(ui: &mut egui::Ui, color: Color32, text: String) {
    ui.add(egui::Label::new(egui::RichText::new(text).color(color)).truncate());
}

/// The capture control, top right of the status strip. Its buttons are
/// egui's default height, which is also the strip's minimum row height, so
/// it rides along without making that section any taller.
fn capture_button(ui: &mut egui::Ui, capture: &CaptureHandle, phase: u8) {
    match phase {
        // Stop analyzes what was recorded; Cancel discards. A host that
        // stops processing mid-capture freezes the phase machine, and a
        // Stop stays pending until playback resumes — Cancel, which acts
        // directly from the GUI thread, is the escape hatch.
        PHASE_ARMED | PHASE_CAPTURING => {
            if ui
                .add(capture_toggle("⏹ Stop", CAPTURE_RED, Color32::WHITE))
                .on_hover_text("Stop and analyze what was recorded")
                .clicked()
            {
                capture.request_stop();
            }
            if ui
                .button("✕ Cancel")
                .on_hover_text("Discard the capture in progress")
                .clicked()
            {
                capture.cancel_capture();
            }
        }
        PHASE_ANALYZING => {
            ui.add_enabled(
                false,
                capture_toggle("⏺ Capture…", CAPTURE_GREEN, Color32::BLACK),
            );
        }
        _ => {
            if ui
                .add(capture_toggle("⏺ Capture", CAPTURE_GREEN, Color32::BLACK))
                .on_hover_text(
                    "Arm a gated capture: records while both inputs are above \
                     the Gate threshold, then re-detects the offset",
                )
                .clicked()
            {
                capture.request_capture();
            }
        }
    }
}

/// Black text on green, white on red: each is the readable pairing on its
/// own fill.
fn capture_toggle(label: &str, fill: Color32, text: Color32) -> egui::Button<'_> {
    egui::Button::new(egui::RichText::new(label).strong().color(text)).fill(fill)
}

fn status_strip(
    ui: &mut egui::Ui,
    params: &AudioAlignParams,
    capture: &CaptureHandle,
    shared: &GuiShared,
    snapshot: Option<&AnalysisSnapshot>,
    net_ms: f32,
    net_clamped: bool,
) {
    ui.horizontal(|ui| {
        // Right-to-left outer layout: the capture button claims the right
        // edge first, so a long status line crowds itself rather than
        // squeezing the one control a new user has to find.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            capture_button(ui, capture, capture.phase());
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                match capture.phase() {
                    PHASE_ARMED => {
                        let gate = capture.gate_state();
                        // Truncating, not wrapping: these messages are the
                        // long ones, and the row now ends at the capture
                        // button rather than at the window edge.
                        status_label(
                            ui,
                            ACCENT_LIVE,
                            format!(
                                "● Armed — waiting for signal ({})",
                                quiet_label(gate & GATE_MAIN_QUIET != 0, gate & GATE_REF_QUIET != 0)
                            ),
                        );
                    }
                    PHASE_CAPTURING => {
                        let (filled, _) = capture.progress();
                        let (_, sr) = shared.window();
                        let secs = filled as f32 / sr.max(1.0);
                        let gate = capture.gate_state();
                        if gate & GATE_OPEN == 0 {
                            status_label(
                                ui,
                                ACCENT_MAIN,
                                format!(
                                    "● Capturing {secs:.1} s (paused — {}; analyzes after ~2 s of silence)",
                                    quiet_label(gate & GATE_MAIN_QUIET != 0, gate & GATE_REF_QUIET != 0)
                                ),
                            );
                        } else {
                            ui.colored_label(ACCENT_MAIN, format!("● Capturing {secs:.1} s"));
                        }
                    }
                    PHASE_ANALYZING => {
                        ui.colored_label(ACCENT_LIVE, "● Analyzing…");
                        ui.spinner();
                    }
                    _ => {
                        ui.colored_label(TEXT_DIM, "● Idle");
                    }
                }
                ui.separator();

                let conf = params.detected_confidence.load(Ordering::Relaxed);
                if conf > 0.0 {
                    let off = params.detected_offset_ms.load(Ordering::Relaxed);
                    let pol = params.detected_polarity.load(Ordering::Relaxed);
                    ui.label(format!("Detected {off:+.2} ms"));
                    ui.label(format!("Confidence {:.0}%", conf * 100.0));
                    ui.label(if pol {
                        "Polarity inverted"
                    } else {
                        "Polarity normal"
                    });
                } else {
                    ui.colored_label(TEXT_DIM, "No offset detected yet");
                }
                ui.separator();

                if !params.align_on.value() {
                    ui.colored_label(TEXT_DIM, "Alignment off");
                } else if net_clamped {
                    ui.colored_label(ACCENT_WARN, format!("Applied {net_ms:+.2} ms (clamped)"));
                } else {
                    ui.label(format!("Applied {net_ms:+.2} ms"));
                }
            });
        });
    });

    if let Some(Err(reason)) = snapshot.map(|s| s.outcome) {
        let msg = match reason {
            RejectReason::TooShort => {
                "Last capture rejected: not enough contiguous signal was captured.".to_string()
            }
            RejectReason::Silence => {
                "Last capture rejected: input silent — is the sidechain connected and playing?"
                    .to_string()
            }
            RejectReason::LowConfidence => {
                let peak = snapshot
                    .map(|s| s.corr.iter().fold(0.0f32, |m, &v| m.max(v.abs())))
                    .unwrap_or(0.0);
                format!(
                    "Last capture rejected: signals don't correlate \
                     (peak {peak:.2} < {CONFIDENCE_THRESHOLD:.1}). Keeping the previous offset."
                )
            }
        };
        ui.colored_label(ACCENT_WARN, msg);
    }
}

fn control_bar(
    ui: &mut egui::Ui,
    setter: &ParamSetter,
    params: &AudioAlignParams,
    capture_row_w: &mut f32,
) {
    ui.horizontal(|ui| {
        // Center the row using its measured width from the previous frame
        // (content-driven, so it converges immediately and never oscillates).
        let pad = ((ui.available_width() - *capture_row_w) / 2.0).max(0.0);
        ui.add_space(pad);
        let row_start = ui.cursor().left();
        bool_toggle(ui, setter, &params.align_on, "Align");
        ui.separator();
        ui.label(egui::RichText::new("Polarity").small().color(TEXT_DIM));
        enum_selector(
            ui,
            setter,
            &params.polarity,
            &[
                (PolarityMode::Auto, "Auto"),
                (PolarityMode::Normal, "Normal"),
                (PolarityMode::Inverted, "Invert"),
            ],
        );
        *capture_row_w = ui.cursor().left() - row_start - ui.spacing().item_spacing.x;
    });
    ui.horizontal(|ui| {
        ui.label("Trim");
        ui.add_sized(
            [180.0, 18.0],
            widgets::ParamSlider::for_param(&params.trim, setter),
        );
        ui.separator();
        ui.label("Gate");
        ui.add_sized(
            [100.0, 18.0],
            widgets::ParamSlider::for_param(&params.gate_threshold, setter),
        )
        .on_hover_text("Capture records only while both inputs exceed this level");
        ui.separator();
        ui.label("Max Shift");
        ui.add_sized(
            [130.0, 18.0],
            widgets::ParamSlider::for_param(&params.max_shift, setter),
        )
        .on_hover_text(
            "How far the track can be shifted, earlier or later, and how far the \
             analysis searches for the offset. Larger values add more latency for \
             the host to compensate. Takes effect the next time the session loads.",
        );
    });
}

fn bool_toggle(ui: &mut egui::Ui, setter: &ParamSetter, param: &BoolParam, label: &str) {
    let value = param.value();
    if ui.selectable_label(value, label).clicked() {
        setter.begin_set_parameter(param);
        setter.set_parameter(param, !value);
        setter.end_set_parameter(param);
    }
}

fn enum_selector<T: nih_plug::prelude::Enum + PartialEq + Copy + 'static>(
    ui: &mut egui::Ui,
    setter: &ParamSetter,
    param: &EnumParam<T>,
    options: &[(T, &str)],
) {
    let current = param.value();
    for (value, label) in options {
        if ui.selectable_label(current == *value, *label).clicked() && current != *value {
            setter.begin_set_parameter(param);
            setter.set_parameter(param, *value);
            setter.end_set_parameter(param);
        }
    }
}

/// Arrow-key trim nudge while hovering a panel: ±0.01 ms, Shift for
/// ±0.1 ms. Skipped while something else (e.g. a ParamSlider's text entry)
/// owns the keyboard, and during a pan drag.
fn handle_trim_nudge(
    ui: &egui::Ui,
    panel: &PanelOutput,
    setter: &ParamSetter,
    params: &AudioAlignParams,
    pending_trim: &mut Option<f32>,
) {
    let response = &panel.response;
    // With alignment off neither panel displays the shift, so a nudge would
    // silently slew trim with zero visual feedback; stay inert.
    if !params.align_on.value() {
        return;
    }
    if response.hovered() && !response.dragged() && !ui.ctx().wants_keyboard_input() {
        let (left, right, shift) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.modifiers.shift,
            )
        });
        if left || right {
            let step = if shift { 0.1 } else { 0.01 };
            let delta = if right { step } else { -step };
            let base = pending_trim.unwrap_or_else(|| params.trim.value());
            let new = snap_trim(base as f64 + delta);
            setter.begin_set_parameter(&params.trim);
            setter.set_parameter(&params.trim, new);
            setter.end_set_parameter(&params.trim);
            *pending_trim = Some(new);
        }
    }
}

/// Clamp to the trim range and snap to the parameter's 0.01 ms step.
/// Non-finite input (defense against a degenerate panel axis) maps to 0.
fn snap_trim(ms: f64) -> f32 {
    if !ms.is_finite() {
        return 0.0;
    }
    let clamped = ms.clamp(-TRIM_RANGE_MS as f64, TRIM_RANGE_MS as f64);
    ((clamped / 0.01).round() * 0.01) as f32
}

