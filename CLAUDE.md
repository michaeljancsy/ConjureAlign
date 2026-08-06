# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

AudioAlign: a VST3 + CLAP plugin built on nih-plug that time-aligns a mic signal (the
plugin's main input) to a reference mic (the sidechain "Reference" input) via FFT
cross-correlation, with sub-sample precision and automatic polarity detection. Typical use:
two microphones on one guitar amp, one plugin instance on the track to be shifted, the other
track routed into the sidechain. Has a custom egui editor (overlaid capture waveforms, a
cross-correlation graph with live markers, drag-to-trim) but remains fully operable headless
from the host's generic parameter UI.

## Commands

- Build + bundle (debug): `cargo xtask bundle audio_align`
- Build + bundle (release): `cargo xtask bundle audio_align --release`
  - Bundles land in `target/bundled/AudioAlign.clap` and `target/bundled/AudioAlign.vst3`
- macOS universal binary: `cargo xtask bundle-universal audio_align --release`
- Install locally (macOS): copy bundles to `~/Library/Audio/Plug-Ins/CLAP/` and
  `~/Library/Audio/Plug-Ins/VST3/`
- Unit + integration tests (pure DSP + GUI decimation, no plugin host involved):
  `cargo test --release` (release mode: the analysis tests run multi-second captures)
- Lint: `cargo clippy --all-targets` (and `--features gui-preview,standalone` to cover the
  dev-only targets)
- GUI visual check without a DAW: `cargo run --example gui_preview --features gui-preview`
  renders the two panels with synthetic data (known +5 ms offset) to `gui_preview.png` and
  `gui_preview_zoom.png` via egui_kittest's offscreen wgpu renderer, or run the plugin
  interactively with `cargo run --bin standalone --features standalone -- --backend dummy`
  (works thanks to the baseview `[patch]` — see Known upstream issues).
- CLAP validation: `clap-validator validate target/bundled/AudioAlign.clap`
  (needs rustc ≥1.95 to `cargo install`; otherwise download the binary from
  free-audio/clap-validator GitHub releases)
- VST3 validation: `pluginval --strictness-level 10 target/bundled/AudioAlign.vst3`
- Toolchain: stable Rust. nih_plug is a git dependency (not on crates.io); Cargo.lock pins the
  rev. `atomic_float` in Cargo.toml must stay on the same version nih_plug uses, because its
  `AtomicF32` implements nih_plug's `PersistentField`.
- Debug builds enable nih_plug's `assert_process_allocs`: any allocation on the audio thread
  panics. Keep it that way; fix the code, not the feature flag.

## Architecture

- `src/lib.rs` — Plugin impl and orchestration; `src/params.rs` — the whole state model;
  `src/capture.rs` — capture buffers + phase machine + the GUI-safe `CaptureHandle`;
  `src/analysis.rs` — offset estimation (background thread only); `src/shared.rs` — the
  snapshot channel between the background task and the editor; `src/editor/` — the egui GUI
  (GUI thread only); `src/dsp/` — delay line + fractional windowed-sinc interpolator (audio
  thread only). `analysis`, `dsp`, and `editor::decimate` contain no nih-plug types and are
  covered by `cargo test`, including an end-to-end capture→analyze→correct test in
  `tests/end_to_end.rs`.

### Threading / state model (the important part)

The *background analysis task* cannot write host parameters (only an editor's ParamSetter
can), and results must update even with no editor open. Detected results are therefore NOT
parameters — they are `#[persist = "..."]` fields on the Params struct (`Arc<AtomicF32>`
offset in ms, `Arc<AtomicBool>` polarity, `Arc<AtomicF32>` confidence). nih-plug serializes
these into the DAW session automatically. The editor reads them lock-free every frame.

Flow: the user clicks Capture in the editor (sets `CaptureState::request`, an `AtomicBool`
the audio thread consumes every block) or flips the `capture` BoolParam (host generic
UI/automation; the plugin never un-toggles it) → `process()` treats either as a rising edge →
the audio thread copies mono-summed main + sidechain into pre-allocated buffers (phase
machine: Idle → Capturing → Analyzing → Idle, an `AtomicU8`; the buffers live in an
`AtomicRefCell` but the phases guarantee borrows never overlap) → when full,
`context.execute_background(Task::Analyze)` → the `task_executor()` closure (owns Arc clones)
runs FFT cross-correlation, refines the peak to sub-sample precision by maximizing the
continuous cross-correlation (DTFT of the cross-spectrum — deliberately NOT a parabolic fit,
which is biased on sinc-shaped peaks), detects polarity from the peak sign, then stores the
atomics → `process()` notices the changed target and crossfades (~50 ms, dual delay-line taps,
coalescing rapid changes) to the new delay. Silent or low-confidence captures are rejected and
the previous offset kept; the editor shows the reason. Results are logged via `nih_log!` and
shown in the editor's status strip.

### GUI threading rules

The editor NEVER touches `CaptureState::data` — the `AtomicRefCell` borrow discipline covers
only the audio thread (Idle/Capturing) and the background task (Analyzing); a GUI borrow
would panic the audio thread. Enforced by construction: the editor only receives a
`CaptureHandle` (phase/progress reads + capture request; cannot reach `data`). Waveform and
correlation data reach the GUI exclusively through `shared::AnalysisSnapshot` — full raw
copies of the captures plus the normalized correlation curve per integer lag, built by the
background task at the end of `Task::Analyze` (allocation is fine there) and published via
`Mutex<Option<Arc<AnalysisSnapshot>>>` before the phase returns to Idle. The audio thread
never touches that mutex; its only GUI-related work is a handful of atomic loads/stores per
block (capture request swap, progress). The snapshot is deliberately not persisted: after a
session reload the GUI shows the restored detected values but no waveforms until the next
capture. The editor decimates the raw snapshot per zoom level GUI-side
(`editor/decimate.rs`, pure + unit-tested); the applied-shift math in `editor::net_shift`
mirrors `AudioAlign::current_target` — keep the two in sync. Editor window size persists via
`#[persist = "editor-state"]` (`Arc<EguiState>`).

Sign convention (pinned by the `sign_convention` test in `analysis.rs`):
`detected offset = t_ref − t_main`; positive ⇒ main leads and gets delayed more.

### Latency / PDC trick (negative offsets)

Reported latency = `max_shift_samples + FIR_CENTER` (the 64-tap linear-phase interpolator's
31-sample center). Actual applied delay = `reported latency + clamp(detected_offset + trim,
±window)`. The host's PDC cancels the reported part, so the net shift is exactly
`detected_offset + trim` — which can be NEGATIVE (the track moves earlier). Never report the
actual applied delay as latency.

Latency is reported ONLY from `initialize()`: CLAP forbids latency changes outside activation,
and nih-plug's runtime notification path violates that (clap-validator flags it). Consequently a
Max Shift edit takes effect at the next (re)activation — session reload, or any host state load
(nih-plug re-initializes after state loads). At runtime, every clamp and the capture search
window derive from `last_latency` (the reported value), NOT from the current knob position;
breaking that invariant desynchronizes the applied shift from host PDC.

`detected_offset` is persisted in MILLISECONDS so sessions survive sample-rate changes. Delay
lines and capture buffers are sized in `initialize()` for the parameter maxima
(`MAX_SHIFT_MAX_MS`, 4 s captures), so no parameter change ever allocates on the audio thread.

## Known upstream issues (do not chase these as local bugs)

clap-validator 0.4.1 fails 3 state-reproducibility tests and crashes on state-invalid-random
against ANY nih-plug plugin (verified against the wrapper source, commit f36931f):
- After a host-initiated `clap_plugin_state::load`, nih-plug's `Task::ParameterValuesChanged`
  only notifies the plugin's own editor and never calls
  `clap_host_params::rescan(CLAP_PARAM_RESCAN_VALUES)`. State restores correctly; only the
  notification is missing.
- `ext_state_load` does `Vec::with_capacity` on an unvalidated u64 length prefix, so random
  bytes SIGABRT (state-invalid-random).
All other validator tests pass; re-run after bumping the nih_plug rev to see if these are fixed.

baseview (both the rev egui-baseview pins, `9a0b42c`, and the older one nih-plug's
`standalone` feature pins) null-derefs in `becomeFirstResponder` on recent macOS and ABORTS
THE HOST when the editor window opens: AppKit now attaches the view before it has a window,
and `msg_send![nil, isKeyWindow]` trips rustc's inserted null check. Fixed upstream one
commit after `9a0b42c` (RustAudio/baseview#204, rev `3e12973`); Cargo.toml carries a
`[patch."https://github.com/RustAudio/baseview.git"]` pinning that rev via the
`michaeljancsy/baseview` fork (an unmodified mirror — cargo forbids patching a git source
with its own URL). Do NOT remove the patch until nih-plug/egui-baseview advance past the fix;
without it the editor crashes every host on current macOS even though pluginval passed on
older systems.

`cargo xtask bundle` is UNSAFE IN A `.claude/worktrees/` WORKTREE: nih_plug_xtask's
`chdir_workspace_root()` picks the TOPMOST ancestor directory containing a `Cargo.toml`,
which from a nested worktree is the main checkout — it silently builds and bundles whatever
branch the main checkout is on (this shipped a stale headless build once). Workaround: build
the xtask binary, then run it with `CARGO_MANIFEST_DIR` pointing at a symlink to the worktree
that lives outside the repo tree, e.g.
`ln -s <worktree> /tmp/aa && CARGO_MANIFEST_DIR=/tmp/aa ./target/release/xtask bundle
audio_align --release`; bundles then land in the worktree's own `target/bundled/`. From the
main checkout, plain `cargo xtask bundle` is fine.

## DAW testing notes

- REAPER: add AudioAlign to the target track; open the plugin's pin connector ("2 in 2 out"
  button), add input channels 3/4, and route the reference track there via a send. Fastest host
  for iteration; loads both CLAP and VST3.
- Ableton Live: VST3 build; the device's header exposes a sidechain routing chooser for plugins
  declaring aux inputs — set "Audio From" to the reference track.
- Bitwig: CLAP build; sidechain chooser in the device header.
- Logic Pro: NOT SUPPORTED yet (Logic only loads AU; planned via clap-wrapper's AUv2 target).
- Null test recipe: duplicate a track, nudge the copy by a known amount (track delay or clip
  nudge), sidechain the original into AudioAlign on the copy, play a few seconds, click
  Capture in the plugin window (or toggle the Capture parameter); after the crossfade, invert
  one track and sum. Expected depths (measured in
  `tests/null_depth.rs`): integer-sample offsets null below −80 dB broadband; FRACTIONAL
  offsets are floored at only −20…−30 dB broadband because no real filter can fractionally
  delay content near Nyquist — but the audible band (<0.44·fs, ≈19 kHz) nulls at −69 dB
  (white noise) to −86 dB (dark material). A shallow broadband null with a fractional nudge
  is EXPECTED and inaudible; put a ~19 kHz lowpass on the null bus to see the audible-band
  depth, or nudge by whole samples to see the deep null. Gain/pan mismatches (e.g., a
  post-fader sidechain tap) also cap the null. Session save/reload must preserve the
  correction.
- macOS Gatekeeper only affects downloaded bundles; locally built ones load fine.

## Licensing

GPL-3.0-or-later, mandatorily: nih-plug itself is ISC but its VST3 bindings
(`nih_export_vst3!`) are GPLv3. Do not add GPL-incompatible dependencies. A CLAP-only build
could be relicensed by disabling VST3 export, but VST3 is a product requirement.
