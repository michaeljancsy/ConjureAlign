//! Renders the editor's waveform and correlation panels with synthetic data
//! to a PNG, for visual inspection without a DAW (the nih-plug standalone
//! wrapper crashes on recent macOS — see src/bin/standalone.rs).
//!
//! Usage: `cargo run --example gui_preview --features gui-preview [-- out.png]`

use std::sync::Arc;

use audio_align::analysis;
use audio_align::editor::correlation_view::{self, CorrArgs};
use audio_align::editor::waveform_view::{self, WaveArgs, WaveViewState};
use audio_align::shared::AnalysisSnapshot;
use nih_plug_egui::egui;

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "gui_preview.png".into());

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
    let snapshot = Arc::new(AnalysisSnapshot {
        main,
        reference,
        sample_rate: sr,
        max_shift_samples: report.max_shift_samples,
        corr: report.corr_curve,
        outcome: report.outcome,
    });

    // Live marker deliberately off-peak: trim = +1.5 ms on top of detection.
    let trim_ms = 1.5f32;
    let net_ms = detected_ms + trim_ms;

    // Scene 1: full-capture view.
    render_scene(&out, &snapshot, detected_ms, net_ms, None, false);
    // Scene 2: zoomed to one burst + correlation zoomed to the peak — the
    // view where the trim slide and the ghost offset are actually visible.
    let zoom = waveform_view::TimeView {
        start_s: 0.98,
        span_s: 0.05,
    };
    let out_zoom = out.replace(".png", "_zoom.png");
    render_scene(&out_zoom, &snapshot, detected_ms, net_ms, Some(zoom), true);
}

fn render_scene(
    out: &str,
    snapshot: &Arc<AnalysisSnapshot>,
    detected_ms: f32,
    net_ms: f32,
    wave_view: Option<waveform_view::TimeView>,
    zoom_peak: bool,
) {
    let snapshot = snapshot.clone();
    let mut wave_state = WaveViewState {
        view: wave_view,
        cache: None,
    };
    let mut show_raw = true;
    let mut corr_cache = None;
    let mut zoom_peak = zoom_peak;

    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::Vec2::new(820.0, 560.0))
        .build_ui(move |ui| {
            ui.ctx().set_visuals(egui::Visuals::dark());
            let wave_args = WaveArgs {
                snapshot: Some(&snapshot),
                net_ms,
                flip_main: false,
                capturing: None,
            };
            waveform_view::show(ui, 270.0, &wave_args, &mut show_raw, &mut wave_state);
            ui.add_space(6.0);
            let corr_args = CorrArgs {
                snapshot: Some(&snapshot),
                detected_ms: Some(detected_ms),
                held: false,
                net_ms,
                clamped: false,
                align_on: true,
                active_window_ms: Some(20.0),
            };
            correlation_view::show(ui, 200.0, &corr_args, &mut zoom_peak, &mut corr_cache);
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
