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

use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use nih_plug::prelude::{
    BoolParam, Editor, EnumParam, GuiContext, ParamSetter, ParentWindowHandle,
};
use nih_plug_egui::{
    create_egui_editor, egui, resizable_window::ResizableWindow, widgets, EguiState,
};

use crate::analysis::{RejectReason, CONFIDENCE_THRESHOLD};
use crate::analytics;
use crate::capture::{
    CaptureHandle, GATE_MAIN_QUIET, GATE_OPEN, GATE_REF_QUIET, PHASE_ANALYZING, PHASE_ARMED,
    PHASE_CAPTURING, PHASE_IDLE,
};
use crate::config;
use crate::crash;
use crate::params::{ConjureAlignParams, PolarityMode, TRIM_RANGE_MS};
use crate::session_marker::{MarkerHandle, Stage};
use crate::shared::{AnalysisSnapshot, GuiShared};
use crate::update;

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
            // Ctrl on every platform, macOS included: that flag is stamped
            // on the scroll event itself, so the gesture works whether or not
            // the host has given the editor keyboard focus. Cmd would have to
            // be latched from a key event, which Logic never delivers, so the
            // baseview patch drops it rather than leave zoom host-dependent.
            const LEGEND: &str = "drag / scroll: pan  ·  pinch or ctrl-scroll: zoom  ·  \
                                  arrow keys: nudge trim  ·  double-click: fit";
            ui.add(
                egui::Label::new(egui::RichText::new(LEGEND).small().color(TEXT_DIM)).truncate(),
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
    /// Per-snapshot FFT-selector state (fit bound + GUI-side Welch
    /// re-estimates). Cleared in the snapshot-changed block like every other
    /// cache: its own pointer check runs only while the Spectrum tab is
    /// drawn, so it cannot be trusted to self-invalidate — a freed
    /// snapshot's address can be reused by a later one (ABA) and would serve
    /// stale spectra.
    spectrum_reestimates: Option<spectrum_view::SpectrumReestimates>,
    /// Last trim value this editor sent. `set_parameter` only queues the
    /// change until the audio thread drains the event queue, so the nudge
    /// bases follow-up edits on this instead of the (possibly stale)
    /// parameter; cleared once the parameter catches up.
    pending_trim: Option<f32>,
    /// Measured width of the Capture/Align/Polarity row, for centering it.
    capture_row_w: f32,
    /// Latched by [`guarded_frame`] once a frame has panicked: from then on
    /// the editor draws [`panic_screen`] instead of re-entering the drawing
    /// code, which at 60 Hz would mean a Sentry report and a blocking flush
    /// per frame.
    panicked: bool,
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
            spectrum_reestimates: None,
            pending_trim: None,
            // Estimate for the first frame only; measured thereafter.
            capture_row_w: 300.0,
            panicked: false,
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

    /// What a panicked frame leaves behind. Everything the failed frame held
    /// is dropped rather than carried forward: it may be half-updated (a
    /// freshly published snapshot swapped in with its caches not yet
    /// cleared, say), and the snapshot is the likeliest *input* to whatever
    /// panicked — the Ableton crash this containment exists for was
    /// arithmetic on capture data.
    pub fn after_panic() -> Self {
        Self {
            panicked: true,
            ..Self::default()
        }
    }

    /// Whether a frame has panicked and the editor is showing its error
    /// message. Public for `tests/editor_panic.rs`.
    pub fn has_panicked(&self) -> bool {
        self.panicked
    }
}

pub fn create(
    params: Arc<ConjureAlignParams>,
    shared: Arc<GuiShared>,
    capture: CaptureHandle,
    crash: Arc<crash::CrashHandle>,
    updates: Arc<update::UpdateHandle>,
    markers: Arc<MarkerHandle>,
) -> Option<Box<dyn Editor>> {
    let egui_state: Arc<EguiState> = params.editor_state.clone();
    let build_updates = updates.clone();
    let inner = create_egui_editor(
        egui_state.clone(),
        EditorState::default(),
        move |ctx, state| {
            // Same guard as the draw closure below: first-frame/context setup
            // is a prime spot for a panic, and it must be attributed to us.
            let _scope = crash::scope();
            // ...and the same containment, for the same reason: this closure
            // is called from the window's own callback, so an escaping panic
            // unwinds through an `extern "C"` frame and aborts the host.
            guarded_frame(state, |_state| {
                let mut style = (*ctx.style()).clone();
                style.visuals = egui::Visuals::dark();
                style.visuals.panel_fill = Color32::from_gray(18);
                ctx.set_style(style);

                // The ONLY automatic update check, and deliberately here
                // rather than in `initialize()`: a window has opened, so a
                // human is present. `auval`, `pluginval` and Logic's plugin
                // scan all instantiate and initialize the plugin headlessly,
                // and a scan has no business making network requests. It is a
                // no-op unless the user granted update checks and the interval
                // has elapsed.
                build_updates.check(update::Trigger::Auto);
            });
        },
        move |ctx, setter, state| {
            // Marks this thread as ours for the frame, so the hook in `crash`
            // attributes editor panics to us — the editor is where a panic is
            // most likely to be user-triggered. (The `crash` parameter does
            // not shadow the module here: locals live in the value namespace.)
            //
            // Taken OUTSIDE the containment below, which is what keeps that
            // attribution intact: the panic hook runs at the panic site,
            // before the unwind `guarded_frame` catches even starts, so it
            // still sees this guard on the stack.
            let _scope = crash::scope();

            let completed = guarded_frame(state, |state| {
                // An earlier frame panicked. Say so, and do not re-enter the
                // drawing code until the user asks for it: the input that
                // panicked (a snapshot, a zoom range) usually still would.
                if state.panicked {
                    panic_screen(ctx, state);
                    return;
                }

                // Pick up a freshly published snapshot; invalidate the caches and
                // refit the waveform view when it changes. `Some → None` is a
                // change too: a state load restoring a pre-capture session
                // clears the cell, and the graphs must follow.
                let latest = shared.snapshot.get();
                let changed = match (&state.snapshot, &latest) {
                    (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                    (None, Some(_)) | (Some(_), None) => true,
                    (None, None) => false,
                };
                if changed {
                    state.snapshot = latest;
                    state.wave = WaveViewState::default();
                    state.corr_view = CorrViewState::default();
                    state.corr_cache = None;
                    state.spec_view = SpecViewState::default();
                    state.spectrum_cache = None;
                    // Explicitly, NOT left to spectrum_view's own pointer check:
                    // that check runs only while the Spectrum tab is drawn, and a
                    // freed snapshot's address can be reused by a later one (ABA)
                    // — which rendered an old capture's spectra as the new one's.
                    state.spectrum_reestimates = None;
                }

                ResizableWindow::new("conjure-align-resize")
                    .min_size(egui::Vec2::new(600.0, 460.0))
                    .show(ctx, egui_state.as_ref(), |ui| {
                        // Breathing room against the window border.
                        egui::Frame::new()
                            .inner_margin(egui::Margin::symmetric(10, 8))
                            .show(ui, |ui| {
                                draw_ui(ui, setter, state, &params, &shared, &capture, &updates);
                            });
                    });

                // Asked once per install, the first time anyone opens the editor.
                // Deliberately outside `draw_ui`: the gui-preview example renders
                // that directly, and a consent dialog has no business in a
                // screenshot of the panels.
                if config::needs_prompt() {
                    consent_modal(ctx);
                }

                // After both surfaces that can change the answer — the modal above
                // and the settings popover inside `draw_ui` — so a click takes
                // effect on this frame rather than at the next activation. Costs an
                // atomic load and an uncontended lock when nothing has changed.
                crash.sync_consent();

                // Keep animating through captures/analysis and drags even if the
                // host stops delivering input events. An in-flight update check
                // is on that list for the same reason: its result arrives on
                // the network worker, and without this the "Checking…" line
                // would sit there until the user happened to move the mouse.
                if capture.phase() != PHASE_IDLE || update::status() == update::Status::Checking {
                    ctx.request_repaint();
                }
            });

            if !completed {
                // The frame died half-drawn, so what is on screen is a
                // fragment of it. Repaint now rather than waiting for the
                // host's next input event to show the message.
                ctx.request_repaint();
            }
        },
    )?;
    Some(Box::new(StageStamped { inner, markers }))
}

/// Wraps the egui editor so the session marker records that a window is being
/// created *before* the attempt starts.
///
/// That window is the one place the panic hook is least useful: `spawn` hands
/// the parent window to baseview, which on Windows creates an OpenGL context
/// through the graphics driver. A fault in there is not a Rust panic, so
/// nothing reports it — but the marker left on disk says `editor_creating`,
/// and the next launch reports that. See `session_marker`.
///
/// Six delegating methods and nothing else; the wrapper deliberately holds no
/// state of its own beyond the handle.
struct StageStamped {
    inner: Box<dyn Editor>,
    markers: Arc<MarkerHandle>,
}

impl Editor for StageStamped {
    fn spawn(
        &self,
        parent: ParentWindowHandle,
        context: Arc<dyn GuiContext>,
    ) -> Box<dyn Any + Send> {
        self.markers.set_stage(Stage::EditorCreating);
        let handle = self.inner.spawn(parent, context);
        self.markers.set_stage(Stage::EditorOpen);
        Box::new(OpenEditor {
            _inner: handle,
            markers: self.markers.clone(),
        })
    }

    fn size(&self) -> (u32, u32) {
        self.inner.size()
    }

    fn set_scale_factor(&self, factor: f32) -> bool {
        self.inner.set_scale_factor(factor)
    }

    fn param_value_changed(&self, id: &str, normalized_value: f32) {
        self.inner.param_value_changed(id, normalized_value)
    }

    fn param_modulation_changed(&self, id: &str, modulation_offset: f32) {
        self.inner.param_modulation_changed(id, modulation_offset)
    }

    fn param_values_changed(&self) {
        self.inner.param_values_changed()
    }
}

/// The handle nih-plug drops when the editor window closes. Its only job is to
/// move the stage back, so a crash after the window closed is not misreported
/// as a crash while it was open.
struct OpenEditor {
    /// Dropped first, closing the real window; declaration order is what puts
    /// the stage change after it.
    _inner: Box<dyn Any + Send>,
    markers: Arc<MarkerHandle>,
}

impl Drop for OpenEditor {
    fn drop(&mut self) {
        self.markers.set_stage(Stage::EditorClosed);
    }
}

/// Runs one editor frame with an unwind boundary around it, so that a panic
/// in the drawing code cannot reach the host.
///
/// This is not belt-and-braces. Both editor closures are called from an
/// `extern "C"` frame — a CFRunLoop timer callback on macOS, a window
/// procedure on Windows — and a panic unwinding out of one aborts the whole
/// process: an arithmetic bug in `view_math` (Sentry CONJUREALIGN-3) took
/// Ableton Live down instantly, with no chance to save. Contained here, the
/// same bug costs the editor's contents and nothing else — the audio thread
/// never runs this code, so the alignment already applied keeps running and
/// every parameter stays reachable from the host's generic controls.
///
/// Catching does not weaken the panic hook's attribution: the hook runs at the
/// panic site, before the unwind begins, so the caller's `crash::scope` guard
/// (taken outside this call, deliberately) is still on the stack and the
/// report is still filed as ours. It does bound the *cost* of reporting, via
/// the latch in [`EditorState::after_panic`]: a panic that recurs every frame
/// would otherwise mean a Sentry report plus a blocking 2 s flush at 60 Hz.
///
/// `AssertUnwindSafe` is load-bearing, and sound for the reason the assertion
/// demands: `state` may well be half-updated when the unwind arrives, so it is
/// not carried forward — the whole `EditorState` is replaced.
///
/// Returns whether `body` ran to completion.
pub fn guarded_frame(state: &mut EditorState, body: impl FnOnce(&mut EditorState)) -> bool {
    if catch_unwind(AssertUnwindSafe(|| body(&mut *state))).is_ok() {
        return true;
    }
    *state = EditorState::after_panic();
    false
}

/// What the editor shows once a frame has panicked. Deliberately plain: it is
/// drawn through [`guarded_frame`] like everything else, so a panic here would
/// merely re-latch, but there is nothing to gain from risking it.
///
/// Public for the same reason as [`consent_modal`]: it takes a crash to reach
/// it in a DAW, so `examples/gui_preview.rs` is the only way to review its
/// copy and its fit at the minimum window size (`gui_preview_panic.png`).
pub fn panic_screen(ctx: &egui::Context, state: &mut EditorState) {
    // The default central-panel frame plus generous side margins: the second
    // line is a full sentence, and at the 600 px minimum window it would
    // otherwise wrap against both window edges.
    let frame =
        egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin::symmetric(48, 8));
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(60.0);
            ui.label(
                egui::RichText::new("The editor hit an internal error.")
                    .color(ACCENT_WARN)
                    .strong(),
            );
            ui.add_space(8.0);
            // The reassurance is the point: what the user can see is broken,
            // and what they cannot see is not.
            ui.label(
                egui::RichText::new(
                    "Audio is unaffected — the alignment already applied keeps \
                     running, and every control is still available from the \
                     host's own plugin parameters.",
                )
                .small()
                .color(TEXT_DIM),
            );
            ui.add_space(14.0);
            if ui.button("Reload the editor").clicked() {
                *state = EditorState::default();
            }
        });
    });
}

/// The whole editor body, one window margin in. Public so the `gui-preview`
/// example can render it headless (see examples/gui_preview.rs).
#[allow(clippy::too_many_arguments)]
pub fn draw_ui(
    ui: &mut egui::Ui,
    setter: &ParamSetter,
    state: &mut EditorState,
    params: &ConjureAlignParams,
    shared: &GuiShared,
    capture: &CaptureHandle,
    updates: &update::UpdateHandle,
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

    status_strip(
        ui,
        params,
        capture,
        shared,
        state.snapshot.as_deref(),
        net_ms,
        net_clamped,
    );
    ui.separator();

    // The control bar sizes itself as a bottom panel and the graphs take
    // exactly what is left, so nothing has to budget a height it can only
    // guess at — which is what used to leave dead black space under the bar.
    // Bottom first: a central panel claims whatever the panels above and
    // below it did not.
    egui::TopBottomPanel::bottom("conjure-align-controls")
        .frame(egui::Frame::new())
        .show_inside(ui, |ui| {
            ui.add_space(6.0);
            control_bar(ui, setter, params, updates, &mut state.capture_row_w);
        });

    egui::CentralPanel::default()
        .frame(egui::Frame::new())
        .show_inside(ui, |ui| {
            graphs(
                ui,
                setter,
                state,
                params,
                shared,
                capture,
                net_ms,
                net_clamped,
                phase,
            );
        });
}

/// The two stacked graph panels, filling the central region exactly: the
/// waveform takes its share of the height, the lower panel takes the rest.
#[allow(clippy::too_many_arguments)]
fn graphs(
    ui: &mut egui::Ui,
    setter: &ParamSetter,
    state: &mut EditorState,
    params: &ConjureAlignParams,
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
                paused_reason: (gate & GATE_OPEN == 0)
                    .then(|| quiet_label(gate & GATE_MAIN_QUIET != 0, gate & GATE_REF_QUIET != 0)),
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
                held: state.snapshot.as_ref().is_some_and(|s| s.outcome.is_err()),
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
                nfft_choice: &params.spectrum_nfft,
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
                    &mut state.spectrum_reestimates,
                    &mut state.spectrum_cache,
                )
            })
            .inner
        }
    };
    handle_trim_nudge(ui, &lower_out, setter, params, &mut state.pending_trim);
}

/// The currently applied shift in ms and whether the window clamp kicked in.
/// Mirrors `ConjureAlign::current_target()` — keep the two in sync.
fn net_shift(params: &ConjureAlignParams, shared: &GuiShared) -> (f32, bool) {
    if !params.align_on.value() {
        return (0.0, false);
    }
    let raw = params.detected_offset_ms.load(Ordering::Relaxed) + params.trim.value();
    // Mirrors the non-finite guard in `current_target()` (a generic-UI text
    // entry can make `trim` NaN); the two must stay in sync.
    let raw = if raw.is_finite() { raw } else { 0.0 };
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

/// A status-strip label: truncates (with the full text on hover) instead of
/// running under the capture buttons when the row is narrow. EVERY label in
/// the strip goes through this — the row is at its longest mid-capture,
/// exactly when the wider Stop/Cancel pair is up, so any plain label after
/// the phase message would otherwise be drawn through the buttons.
fn status_label(ui: &mut egui::Ui, text: impl Into<egui::RichText>) {
    ui.add(egui::Label::new(text.into()).truncate());
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

/// The one-time first-run prompt. Two independent questions — usage analytics
/// plus crash reporting (one answer, one identifier, one thing to explain), and
/// the update check — and it renders only the ones still unanswered. That is
/// what lets a new question be added without re-litigating a settled one: an
/// install upgrading from a version that only ever asked about analytics sees
/// the update question alone, with its stored answer untouched.
///
/// Every question has two explicit buttons, no default and no dismiss. Closing
/// the plugin window records nothing and asks again next time, which is the
/// only reading of silence that isn't a "yes" by attrition.
///
/// Public for the same reason as [`draw_ui`] — it is deliberately drawn
/// outside `draw_ui`, so the `gui-preview` example is the only way to see it
/// without a DAW.
pub fn consent_modal(ctx: &egui::Context) {
    let cfg = config::snapshot();
    egui::Modal::new(egui::Id::new("conjure-align-consent")).show(ctx, |ui| {
        ui.set_max_width(400.0);
        // Two questions at the 600x460 minimum is the tightest this window
        // ever gets. The scroll area is insurance, not layout: with the copy
        // as written nothing scrolls, but a translation or a larger host font
        // must not be able to push a button off the bottom where it cannot be
        // clicked at all.
        egui::ScrollArea::vertical()
            .max_height((ctx.screen_rect().height() - 90.0).max(200.0))
            .show(ui, |ui| {
                let mut asked_something = false;

                if cfg.analytics.is_none() {
                    analytics_question(ui);
                    asked_something = true;
                }

                if cfg.updates.is_none() {
                    if asked_something {
                        ui.add_space(10.0);
                        ui.separator();
                        ui.add_space(10.0);
                    }
                    updates_question(ui);
                }

                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new("You can change either of these any time under \u{2699}.")
                        .small()
                        .color(TEXT_DIM),
                );
            });
    });
}

/// Question one. Deliberately just the question and the two answers: the
/// enumeration of what is collected was removed by product decision on
/// 2026-08-28, so nothing in the UI describes the payload — not here, and not
/// on the [`privacy_section`] checkbox either. `CLAUDE.md` and the
/// `analytics`/`host` module docs are the only remaining record of it.
///
/// Question two below is also bare, but for an unrelated reason — prompt
/// length, not disclosure — and its copy still exists in [`privacy_section`]
/// and the README.
fn analytics_question(ui: &mut egui::Ui) {
    ui.heading("Share anonymous usage and crash data?");
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        if ui.button("No thanks").clicked() {
            analytics::set_consent(false);
        }
        if ui
            .add(capture_toggle(
                "Share anonymous data",
                CAPTURE_GREEN,
                Color32::BLACK,
            ))
            .clicked()
        {
            analytics::set_consent(true);
        }
    });
}

/// Question two. Deliberately separate from the one above rather than folded
/// into it: this one shares no data and mints no identifier, and rolling the
/// two together would mean a user who declines analytics also loses update
/// notices for a reason that has nothing to do with why they declined.
///
/// Heading and buttons only, for the same reason as [`analytics_question`];
/// what the check does is described in [`privacy_section`] and the README.
fn updates_question(ui: &mut egui::Ui) {
    ui.heading("Check for new versions?");
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        if ui.button("No thanks").clicked() {
            config::set_update_consent(false);
        }
        if ui
            .add(capture_toggle(
                "Check for updates",
                CAPTURE_GREEN,
                Color32::BLACK,
            ))
            .clicked()
        {
            config::set_update_consent(true);
        }
    });
}

/// About + privacy, tucked into a popover. Consent must be as easy to withdraw
/// as it was to give, but it doesn't earn permanent space in the main UI.
fn settings_popup_id() -> egui::Id {
    egui::Id::new("conjure-align-settings")
}

/// Preview-only: opens the ⚙ popover, which otherwise needs a click. Lets the
/// `gui-preview` example render it (see [`draw_ui`]).
pub fn open_settings_popup(ctx: &egui::Context) {
    ctx.memory_mut(|mem| mem.open_popup(settings_popup_id()));
}

fn settings_menu(ui: &mut egui::Ui, updates: &update::UpdateHandle) {
    let popup_id = settings_popup_id();

    // The whole notification, and all of it: the gear grows a word when there
    // is something to look at. It rides the centred control row's spare width
    // (~140 px at the 600 px minimum) because the status strip has none — its
    // labels already reach the Capture button and overflow rather than
    // truncate — and it is drawn after `capture_row_w` has been measured, so
    // widening it cannot disturb that row's centring.
    //
    // Nothing louder than this: an update notice must never interrupt a
    // session, and a banner would cost a row out of the graphs' budget.
    let pending = update::pending_version();
    let button = match &pending {
        Some(version) => ui
            .small_button(
                egui::RichText::new("\u{2699} Update")
                    .color(ACCENT_DETECTED)
                    .strong(),
            )
            .on_hover_text(format!(
                "ConjureAlign {version} is available — click for details"
            )),
        None => ui
            .small_button("\u{2699}")
            .on_hover_text("About and privacy"),
    };
    if button.clicked() {
        ui.memory_mut(|mem| mem.toggle_popup(popup_id));
    }
    // Upward: egui places popups where it is told and never flips them, and
    // this gear sits in the control bar a few dozen pixels off the window
    // bottom — downward would open it straight out of the window.
    egui::popup_above_or_below_widget(
        ui,
        popup_id,
        &button,
        egui::AboveOrBelow::Above,
        egui::PopupCloseBehavior::CloseOnClickOutside,
        |ui| {
            ui.set_min_width(300.0);
            ui.label(
                egui::RichText::new(concat!("ConjureAlign v", env!("CARGO_PKG_VERSION"))).strong(),
            );
            ui.separator();
            update_section(ui, updates);
            ui.separator();
            privacy_section(ui);
        },
    );
}

/// The update status and its two buttons. Failures are reported *here* and
/// nowhere else: the popover is a surface the user opened on purpose, so
/// telling them the check failed is informative rather than intrusive — while
/// a failed automatic check still produces no banner, no gear label and no
/// dialog.
fn update_section(ui: &mut egui::Ui, updates: &update::UpdateHandle) {
    if !config::is_supported() {
        ui.label(
            egui::RichText::new("Update checks are not available on this platform.")
                .small()
                .color(TEXT_DIM),
        );
        return;
    }

    let status = update::status();
    let skipped = config::update_skipped();
    let notifying = update::notifies(&status, skipped.as_deref());

    match &status {
        update::Status::Checking => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    egui::RichText::new("Checking for updates\u{2026}")
                        .small()
                        .color(TEXT_DIM),
                );
            });
        }
        update::Status::Available { version } => {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(ACCENT_DETECTED, format!("Version {version} is available"));
                // A constant URL, never one from the response — see the rules
                // in `update.rs`. egui-baseview turns this into the OS's
                // default browser via the `open` crate.
                ui.hyperlink_to("Release notes \u{2197}", update::RELEASES_URL);
            });
            if !notifying {
                ui.label(
                    egui::RichText::new("You chose to skip this one.")
                        .small()
                        .color(TEXT_DIM),
                );
            }
        }
        update::Status::UpToDate => {
            ui.label(
                egui::RichText::new("You have the latest version.")
                    .small()
                    .color(TEXT_DIM),
            );
        }
        update::Status::Failed => {
            ui.label(
                egui::RichText::new("Couldn't reach GitHub to check for updates.")
                    .small()
                    .color(TEXT_DIM),
            );
        }
        update::Status::Unknown => {}
    }

    ui.horizontal(|ui| {
        // Available whatever the stored answer is: clicking it *is* the
        // consent for this one request, and it never writes an answer the user
        // did not give.
        if ui
            .small_button("Check now")
            .on_hover_text("Ask GitHub once whether a newer release exists")
            .clicked()
        {
            updates.check(update::Trigger::Manual);
        }
        if notifying
            && ui
                .small_button("Skip this version")
                .on_hover_text("Stop showing this one. Anything newer still appears.")
                .clicked()
        {
            update::skip_current();
        }
    });
}

/// The two consent checkboxes. Consent must be as easy to withdraw as it was
/// to give, but it doesn't earn permanent space in the main UI.
///
/// The copy in both explainers is a promise. Changing what either feature sends
/// means changing this, [`consent_modal`], and the README's Privacy table.
fn privacy_section(ui: &mut egui::Ui) {
    if !config::is_supported() {
        ui.label(
            egui::RichText::new("Usage and crash reporting are not available on this platform.")
                .small()
                .color(TEXT_DIM),
        );
        return;
    }

    let mut share = analytics::enabled();
    if ui
        .checkbox(&mut share, "Share anonymous usage and crash data")
        .changed()
    {
        analytics::set_consent(share);
    }

    ui.add_space(6.0);

    let mut check_updates = config::update_checks_enabled();
    if ui
        .checkbox(&mut check_updates, "Check for new versions")
        .changed()
    {
        config::set_update_consent(check_updates);
    }
    ui.label(
        egui::RichText::new(
            "Once a day, asks GitHub whether a newer release exists. Sends \
             nothing about you, and never installs anything.",
        )
        .small()
        .color(TEXT_DIM),
    );
}

fn status_strip(
    ui: &mut egui::Ui,
    params: &ConjureAlignParams,
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
                // Backstop for status_label's truncation: once the width is
                // exhausted, whatever still gets placed (a separator, the
                // "…" stub of a later label) lands past the row's right
                // edge, where the buttons already sit — clip it away.
                ui.shrink_clip_rect(egui::Rect::everything_left_of(ui.max_rect().right()));
                match capture.phase() {
                    PHASE_ARMED => {
                        let gate = capture.gate_state();
                        status_label(
                            ui,
                            egui::RichText::new(format!(
                                "● Armed — waiting for signal ({})",
                                quiet_label(gate & GATE_MAIN_QUIET != 0, gate & GATE_REF_QUIET != 0)
                            ))
                            .color(ACCENT_LIVE),
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
                                egui::RichText::new(format!(
                                    "● Capturing {secs:.1} s (paused — {}; analyzes after ~2 s of silence)",
                                    quiet_label(gate & GATE_MAIN_QUIET != 0, gate & GATE_REF_QUIET != 0)
                                ))
                                .color(ACCENT_MAIN),
                            );
                        } else {
                            status_label(
                                ui,
                                egui::RichText::new(format!("● Capturing {secs:.1} s"))
                                    .color(ACCENT_MAIN),
                            );
                        }
                    }
                    PHASE_ANALYZING => {
                        status_label(ui, egui::RichText::new("● Analyzing…").color(ACCENT_LIVE));
                        ui.spinner();
                    }
                    _ => {
                        status_label(ui, egui::RichText::new("● Idle").color(TEXT_DIM));
                    }
                }
                ui.separator();

                let conf = params.detected_confidence.load(Ordering::Relaxed);
                if conf > 0.0 {
                    let off = params.detected_offset_ms.load(Ordering::Relaxed);
                    let pol = params.detected_polarity.load(Ordering::Relaxed);
                    status_label(ui, format!("Detected {off:+.2} ms"));
                    status_label(ui, format!("Confidence {:.0}%", conf * 100.0));
                    status_label(
                        ui,
                        if pol {
                            "Polarity inverted"
                        } else {
                            "Polarity normal"
                        },
                    );
                } else {
                    status_label(
                        ui,
                        egui::RichText::new("No offset detected yet").color(TEXT_DIM),
                    );
                }
                ui.separator();

                if !params.align_on.value() {
                    status_label(ui, egui::RichText::new("Alignment off").color(TEXT_DIM));
                } else if net_clamped {
                    status_label(
                        ui,
                        egui::RichText::new(format!("Applied {net_ms:+.2} ms (clamped)"))
                            .color(ACCENT_WARN),
                    );
                } else {
                    status_label(ui, format!("Applied {net_ms:+.2} ms"));
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
            RejectReason::NonFinite => {
                "Last capture rejected: the input contained non-finite samples (NaN/Inf) \
                 — an upstream plugin may be misbehaving."
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
    params: &ConjureAlignParams,
    updates: &update::UpdateHandle,
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
        // Rides the slack this centered row leaves on its right. The status
        // strip has none at the 600 px minimum — its labels already reach the
        // Capture button and overflow rather than truncate — so a gear there
        // would be drawn through. Measured above, so this cannot disturb the
        // centering.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            settings_menu(ui, updates);
        });
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
    params: &ConjureAlignParams,
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
