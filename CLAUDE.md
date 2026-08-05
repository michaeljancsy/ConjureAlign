# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

AudioAlign: a headless (no custom GUI) VST3 + CLAP plugin built on nih-plug that time-aligns a
mic signal (the plugin's main input) to a reference mic (the sidechain "Reference" input) via
FFT cross-correlation, with sub-sample precision and automatic polarity detection. Typical use:
two microphones on one guitar amp, one plugin instance on the track to be shifted, the other
track routed into the sidechain.

## Commands

- Build + bundle (debug): `cargo xtask bundle audio_align`
- Build + bundle (release): `cargo xtask bundle audio_align --release`
  - Bundles land in `target/bundled/AudioAlign.clap` and `target/bundled/AudioAlign.vst3`
- macOS universal binary: `cargo xtask bundle-universal audio_align --release`
- Install locally (macOS): copy bundles to `~/Library/Audio/Plug-Ins/CLAP/` and
  `~/Library/Audio/Plug-Ins/VST3/`
- Unit + integration tests (pure DSP, no plugin host involved): `cargo test --release`
  (release mode: the analysis tests run multi-second captures)
- Lint: `cargo clippy --all-targets`
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
  `src/capture.rs` — capture buffers + phase machine; `src/analysis.rs` — offset estimation
  (background thread only); `src/dsp/` — delay line + fractional windowed-sinc interpolator
  (audio thread only). `analysis` and `dsp` contain no nih-plug types and are covered by
  `cargo test`, including an end-to-end capture→analyze→correct test in `tests/end_to_end.rs`.

### Threading / state model (the important part)

This plugin is HEADLESS: there is no GuiContext/ParamSetter, so the plugin CANNOT write host
parameters. Detected results are therefore NOT parameters — they are `#[persist = "..."]` fields
on the Params struct (`Arc<AtomicF32>` offset in ms, `Arc<AtomicBool>` polarity,
`Arc<AtomicF32>` confidence). nih-plug serializes these into the DAW session automatically.

Flow: the user flips the `capture` BoolParam in the host's generic UI → `process()` edge-detects
it → the audio thread copies mono-summed main + sidechain into pre-allocated buffers (phase
machine: Idle → Capturing → Analyzing → Idle, an `AtomicU8`; the buffers live in an
`AtomicRefCell` but the phases guarantee borrows never overlap) → when full,
`context.execute_background(Task::Analyze)` → the `task_executor()` closure (owns Arc clones)
runs FFT cross-correlation, refines the peak to sub-sample precision by maximizing the
continuous cross-correlation (DTFT of the cross-spectrum — deliberately NOT a parabolic fit,
which is biased on sinc-shaped peaks), detects polarity from the peak sign, then stores the
atomics → `process()` notices the changed target and crossfades (~50 ms, dual delay-line taps,
coalescing rapid changes) to the new delay. Silent or low-confidence captures are rejected and
the previous offset kept. The plugin cannot un-toggle `capture`; users toggle off/on to
re-analyze. Detected values are not visible in the DAW until there is a GUI (they are logged
via `nih_log!`).

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

## DAW testing notes

- REAPER: add AudioAlign to the target track; open the plugin's pin connector ("2 in 2 out"
  button), add input channels 3/4, and route the reference track there via a send. Fastest host
  for iteration; loads both CLAP and VST3.
- Ableton Live: VST3 build; the device's header exposes a sidechain routing chooser for plugins
  declaring aux inputs — set "Audio From" to the reference track.
- Bitwig: CLAP build; sidechain chooser in the device header.
- Logic Pro: NOT SUPPORTED yet (Logic only loads AU; planned via clap-wrapper's AUv2 target).
- Null test recipe: duplicate a track, nudge the copy by a known amount (track delay or clip
  nudge), sidechain the original into AudioAlign on the copy, play a few seconds, toggle
  Capture; after the crossfade the two tracks should null (invert one and sum) well below
  −60 dB. Session save/reload must preserve the correction.
- macOS Gatekeeper only affects downloaded bundles; locally built ones load fine.

## Licensing

GPL-3.0-or-later, mandatorily: nih-plug itself is ISC but its VST3 bindings
(`nih_export_vst3!`) are GPLv3. Do not add GPL-incompatible dependencies. A CLAP-only build
could be relicensed by disabling VST3 export, but VST3 is a product requirement.
