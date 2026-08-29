//! Renders the editor's waveform and correlation panels with synthetic data
//! to a PNG, for visual inspection without a DAW (the nih-plug standalone
//! wrapper crashes on recent macOS — see src/bin/standalone.rs).
//!
//! Usage: `cargo run --example gui_preview --features gui-preview [-- out.png]`

use std::sync::Arc;

use conjure_align::analysis;
use conjure_align::editor::correlation_view::{self, CorrArgs, CorrViewState};
use conjure_align::editor::spectrum_view::{self, SpecViewState, SpectrumArgs};
use conjure_align::editor::waveform_view::{self, CaptureOverlay, WaveArgs, WaveViewState};
use conjure_align::editor::LowerPanelTab;
use conjure_align::capture::CaptureState;
use conjure_align::params::ConjureAlignParams;
use conjure_align::shared::{AnalysisSnapshot, GuiShared};
use conjure_align::spectrum;
use nih_plug::prelude::{GuiContext, ParamPtr, ParamSetter, PluginApi, PluginState};
use nih_plug_egui::egui;

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "gui_preview.png".into());

    // Render against a scratch preferences directory rather than the
    // developer's own. Two reasons, both load-bearing for the consent scene:
    // it must show a *virgin* install (every question unanswered — on this
    // machine the analytics one is long since answered, so the real file would
    // render half the dialog), and a preview must never write to the file that
    // holds a real consent decision. Set before anything reads it: the config
    // is cached in a `OnceLock` on first access.
    let scratch = std::env::temp_dir().join("conjure-align-gui-preview-home");
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).expect("scratch preferences dir");
    std::env::set_var("HOME", &scratch);
    std::env::set_var("APPDATA", &scratch);

    // Synthetic capture: bursty band-limited noise; reference = main delayed
    // by 240 samples (5 ms at 48 kHz), i.e. main LEADS and the detected
    // offset must come out at +5 ms.
    let sr = 48_000.0f32;
    let n = 96_000; // 2 s
    let main = bursty_noise(n, 42);
    let k = 240usize;
    let mut reference = vec![0.0f32; n];
    reference[k..n].copy_from_slice(&main[..n - k]);
    let report = analysis::analyze_detailed(&main, &reference, 960); // ±20 ms
    let detected_ms = report
        .outcome
        .as_ref()
        .map(|r| (r.offset_samples / sr as f64 * 1000.0) as f32)
        .unwrap_or(0.0);
    println!(
        "synthetic capture: outcome ok = {}, detected = {detected_ms:.3} ms (expected +5)",
        report.outcome.is_ok()
    );
    let spectrum = spectrum::welch_for_capture(&main, &reference, sr, &report, &[], None);
    let snapshot = Arc::new(AnalysisSnapshot {
        main,
        reference,
        sample_rate: sr,
        max_shift_samples: report.max_shift_samples,
        corr: report.corr_curve,
        splices: Vec::new(),
        spectrum,
        outcome: report.outcome,
    });

    // Live marker deliberately off-peak: trim = +1.5 ms on top of detection.
    let trim_ms = 1.5f32;
    let net_ms = detected_ms + trim_ms;

    // Scene 1: full-capture view.
    render_scene(
        &out,
        &snapshot,
        detected_ms,
        net_ms,
        None,
        None,
        LowerPanelTab::Correlation,
    );
    // Scene 2: zoomed to one burst + correlation zoomed to the peak (the
    // same ±15 ms view the Peak button sets) — the view where the trim slide
    // and the ghost offset are actually visible.
    let zoom = waveform_view::TimeView {
        start_s: 0.98,
        span_s: 0.05,
    };
    let out_zoom = out.replace(".png", "_zoom.png");
    render_scene(
        &out_zoom,
        &snapshot,
        detected_ms,
        net_ms,
        Some(zoom),
        Some((-10.0, 30.0)),
        LowerPanelTab::Correlation,
    );
    // Scene 3: spectrum tab at trim 0 — the captured sum combs with notches
    // at 100, 300, 500, … Hz (the 5 ms offset), the corrected sum is flat.
    let out_spec = out.replace(".png", "_spectrum.png");
    render_scene(
        &out_spec,
        &snapshot,
        detected_ms,
        detected_ms,
        None,
        None,
        LowerPanelTab::Spectrum,
    );
    // Scene 4: trim knocks the corrected sum off-peak by 1.5 ms — it re-combs
    // at ≈333 Hz spacing, showing the live trim-follow.
    let out_spec_trim = out.replace(".png", "_spectrum_trim.png");
    render_scene(
        &out_spec_trim,
        &snapshot,
        detected_ms,
        net_ms,
        None,
        None,
        LowerPanelTab::Spectrum,
    );
    // Scene 5: the WHOLE editor — status strip, both panels and the control
    // bar — which is the only way to see the vertical budget (dead space at
    // the window bottom, a clipped control bar) rather than just the graphs.
    render_full(
        &out.replace(".png", "_full.png"),
        &snapshot,
        detected_ms,
        Overlay::None,
    );
    // Scene 6: the first-run analytics prompt over that same editor. It is
    // drawn outside `draw_ui` so it can never appear in the scenes above,
    // which makes this the only way to review its copy and fit headless.
    render_full(
        &out.replace(".png", "_consent.png"),
        &snapshot,
        detected_ms,
        Overlay::Consent,
    );
    // Scene 7: the ⚙ popover, the standing way to change that answer. It
    // needs a click, so nothing else would ever render it — and it opens
    // upward from a control-bar button close to the window edge, which is
    // exactly the fit worth checking.
    render_full(
        &out.replace(".png", "_settings.png"),
        &snapshot,
        detected_ms,
        Overlay::Settings,
    );
    // Scene 8: the same popover with an update waiting, which is also the only
    // way to see the "\u{2699} Update" label in the control bar behind it. The
    // fit worth checking is that label against the centred row's spare width at
    // the 600 px minimum.
    render_full(
        &out.replace(".png", "_settings_update.png"),
        &snapshot,
        detected_ms,
        Overlay::SettingsUpdate,
    );
    // Scene 9: what an editor that has panicked shows instead of the panels
    // (see `editor::guarded_frame`). It replaces the body rather than floating
    // over it, and reaching it in a DAW takes an actual crash, so this is the
    // only review of its copy and of how it wraps at the minimum width.
    render_panic_screen(&out.replace(".png", "_panic.png"));
}

/// Renders the post-panic message at the real minimum window size. It draws
/// straight onto the context, like the editor's own closure does once the
/// latch is set — no `draw_ui`, which is the point of the scene.
fn render_panic_screen(out: &str) {
    let mut state = conjure_align::editor::EditorState::after_panic();
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::Vec2::new(600.0, 460.0))
        .build(move |ctx| {
            ctx.set_visuals(egui::Visuals::dark());
            conjure_align::editor::panic_screen(ctx, &mut state);
        });

    harness.run();
    let image = harness.render().expect("wgpu offscreen render");
    image.save(out).expect("write png");
    println!("wrote {out}");
}

/// Which of the editor's two floating surfaces a full-editor scene draws on
/// top; both live outside `draw_ui`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Overlay {
    None,
    Consent,
    Settings,
    /// The popover with an update on offer — which also puts the "\u{2699} Update"
    /// label on the control bar behind it, the only notification the plugin
    /// ever shows.
    SettingsUpdate,
}

impl Overlay {
    /// The update status this scene wants. Forced per scene because the status
    /// is process-wide and every scene renders in the same process.
    fn update_status(self) -> conjure_align::update::Status {
        match self {
            Overlay::SettingsUpdate => conjure_align::update::Status::Available {
                version: "9.9.9".into(),
            },
            _ => conjure_align::update::Status::Unknown,
        }
    }
}

/// A `GuiContext` that goes nowhere: `ParamSetter` needs one, and the editor
/// only ever writes parameters through it, which a preview must not do.
struct StubGuiContext;

impl GuiContext for StubGuiContext {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Clap
    }
    fn request_resize(&self) -> bool {
        true
    }
    unsafe fn raw_begin_set_parameter(&self, _param: ParamPtr) {}
    unsafe fn raw_set_parameter_normalized(&self, _param: ParamPtr, _normalized: f32) {}
    unsafe fn raw_end_set_parameter(&self, _param: ParamPtr) {}
    fn get_state(&self) -> PluginState {
        unimplemented!("the editor never asks a preview for plugin state")
    }
    fn set_state(&self, _state: PluginState) {
        unimplemented!("the editor never restores state in a preview")
    }
}

/// Renders `editor::draw_ui` at the real minimum window size, one window
/// margin in — the same wrapping the editor's own `create()` uses.
fn render_full(out: &str, snapshot: &Arc<AnalysisSnapshot>, detected_ms: f32, overlay: Overlay) {
    let params = ConjureAlignParams::default();
    params
        .detected_offset_ms
        .store(detected_ms, std::sync::atomic::Ordering::Relaxed);
    params
        .detected_confidence
        .store(0.9, std::sync::atomic::Ordering::Relaxed);
    let shared = GuiShared::default();
    shared.set_window(960, snapshot.sample_rate);
    *shared.snapshot.lock().unwrap() = Some(snapshot.clone());
    let capture = Arc::new(CaptureState::new()).handle();
    let updates = Arc::new(conjure_align::update::UpdateHandle::new());
    // Scenes render in one process and the status is process-wide, so each one
    // states what it wants rather than inheriting the last scene's.
    conjure_align::update::set_status_for_preview(overlay.update_status());
    let mut state = conjure_align::editor::EditorState::with_snapshot(snapshot.clone());

    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::Vec2::new(600.0, 460.0))
        .build_ui(move |ui| {
            ui.ctx().set_visuals(egui::Visuals::dark());
            let ctx = StubGuiContext;
            let setter = ParamSetter::new(&ctx);
            egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    conjure_align::editor::draw_ui(
                        ui, &setter, &mut state, &params, &shared, &capture, &updates,
                    );
                });
            match overlay {
                Overlay::None => {}
                Overlay::Consent => conjure_align::editor::consent_modal(ui.ctx()),
                // Opened before the bar is drawn on the next pass, so the
                // popover is already up when the harness renders.
                Overlay::Settings | Overlay::SettingsUpdate => {
                    conjure_align::editor::open_settings_popup(ui.ctx())
                }
            }
        });

    harness.run();
    let image = harness.render().expect("wgpu offscreen render");
    image.save(out).expect("write png");
    println!("wrote {out}");
}

#[allow(clippy::too_many_arguments)]
fn render_scene(
    out: &str,
    snapshot: &Arc<AnalysisSnapshot>,
    detected_ms: f32,
    net_ms: f32,
    wave_view: Option<waveform_view::TimeView>,
    corr_view: Option<(f64, f64)>,
    lower: LowerPanelTab,
) {
    let snapshot = snapshot.clone();
    let mut wave_state = WaveViewState {
        view: wave_view,
        ..Default::default()
    };
    let mut show_raw = true;
    let mut corr_cache = None;
    let mut corr_state = CorrViewState { view: corr_view };
    let mut tab = lower;
    let mut spectrum_log = true;
    let mut spec_state = SpecViewState::default();
    let spectrum_nfft = std::sync::atomic::AtomicU32::new(0);
    let mut spectrum_reestimates = None;
    let mut spectrum_cache = None;

    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::Vec2::new(820.0, 560.0))
        .build_ui(move |ui| {
            ui.ctx().set_visuals(egui::Visuals::dark());
            let wave_args = WaveArgs {
                snapshot: Some(&snapshot),
                net_ms,
                flip_main: false,
                overlay: CaptureOverlay::Idle,
            };
            // Panel heights are TOTALS (header row included).
            waveform_view::show(ui, 296.0, &wave_args, &mut show_raw, &mut wave_state);
            ui.add_space(6.0);
            match tab {
                LowerPanelTab::Correlation => {
                    let corr_args = CorrArgs {
                        snapshot: Some(&snapshot),
                        detected_ms: Some(detected_ms),
                        held: false,
                        net_ms,
                        clamped: false,
                        align_on: true,
                        active_window_ms: Some(20.0),
                    };
                    correlation_view::show(
                        ui,
                        226.0,
                        &corr_args,
                        &mut tab,
                        &mut corr_state,
                        &mut corr_cache,
                    );
                }
                LowerPanelTab::Spectrum => {
                    let spec_args = SpectrumArgs {
                        snapshot: Some(&snapshot),
                        net_ms,
                        flip_main: false,
                        align_on: true,
                        nfft_choice: &spectrum_nfft,
                    };
                    spectrum_view::show(
                        ui,
                        226.0,
                        &spec_args,
                        &mut tab,
                        &mut spectrum_log,
                        &mut spec_state,
                        &mut spectrum_reestimates,
                        &mut spectrum_cache,
                    );
                }
            }
        });

    harness.run();
    let image = harness.render().expect("wgpu offscreen render");
    image.save(out).expect("write png");
    println!("wrote {out}");
}

/// Deterministic band-limited noise with a slow burst envelope, so the
/// waveform looks like real audio with visually trackable transients.
fn bursty_noise(len: usize, seed: u64) -> Vec<f32> {
    let mut state = seed.max(1);
    let mut white = Vec::with_capacity(len + 16);
    for _ in 0..len + 16 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        white.push(((state >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) as f32);
    }
    (0..len)
        .map(|i| {
            let s: f32 = white[i..i + 16].iter().sum::<f32>() / 8.0;
            let t = i as f32 / 48_000.0;
            let gate = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * 1.5 * t).cos();
            s * (0.1 + 0.9 * gate.powi(3))
        })
        .collect()
}
