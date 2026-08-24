# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

ConjureAlign: a VST3 + CLAP + AudioUnit v2 plugin built on nih-plug that time-aligns a mic signal (the
plugin's main input) to a reference mic (the sidechain "Reference" input) via FFT
cross-correlation, with sub-sample precision and automatic polarity detection. Typical use:
two microphones on one guitar amp, one plugin instance on the track to be shifted, the other
track routed into the sidechain. Has a custom egui editor (overlaid capture waveforms, a
cross-correlation graph with live markers, a comb-filter spectrum panel; all graphs share one
gesture set — drag/scroll pans, pinch or ⌘-scroll zooms the x-axis, the y-axis is always
plugin-scaled, double-click fits; Trim is adjusted via its slider or ←/→ while hovering a
graph, there is no drag-to-trim; the lower panel's header row carries a legend spelling
the gesture set out — it rides in that already-budgeted row rather than a line of its own,
and words the modifiers because egui's default font renders ⌘ but not ⌥/⇧/←/→) but
remains fully operable headless from the host's generic parameter UI. Capture/Stop/Cancel
sit at the top right of the status strip — the primary button is bright green when idle and
bright red while a capture runs, because it is the one control a new user must find, and it
fits the strip's existing row height. The
editor body is
a bottom panel (control bar, sizes itself) plus a central panel (the graphs, take exactly
what is left); panels are handed a TOTAL height and subtract their own measured header
row, so nothing budgets a guessed height and no dead space collects at the window bottom.

## Commands

- Build + bundle (debug): `cargo xtask bundle conjure_align`
- Build + bundle (release): `cargo xtask bundle conjure_align --release`
  - Bundles land in `target/bundled/ConjureAlign.clap`, `.vst3` and (macOS)
    `.component` — one cdylib with three entry points, copied into three bundles
- macOS universal binary: `cargo xtask bundle-universal conjure_align --release`
- Install locally (macOS): copy bundles to `~/Library/Audio/Plug-Ins/CLAP/`,
  `~/Library/Audio/Plug-Ins/VST3/` and `~/Library/Audio/Plug-Ins/Components/`
  (the AU must live in one of the two `Components` directories; nowhere else works)
- Unit + integration tests (pure DSP + GUI decimation, no plugin host involved):
  `cargo test --release` (release mode: the analysis tests run multi-second captures)
- Lint: `cargo clippy --all-targets` (and `--features gui-preview,standalone` to cover the
  dev-only targets)
- GUI visual check without a DAW: `cargo run --example gui_preview --features gui-preview`
  renders synthetic data (known +5 ms offset) through egui_kittest's offscreen wgpu
  renderer: the panels alone (`gui_preview.png`, `_zoom`, `_spectrum`, `_spectrum_trim`)
  and — via a stub `GuiContext` behind a `ParamSetter` — the WHOLE editor at its 600×460
  minimum window (`_full.png`), which is the only scene that shows the vertical budget
  (dead space under the control bar, a clipped bar). Or run the plugin
  interactively with `cargo run --bin standalone --features standalone -- --backend dummy`
  (works thanks to the baseview `[patch]` — see Known upstream issues).
- CLAP validation: `clap-validator validate target/bundled/ConjureAlign.clap`
  (needs rustc ≥1.95 to `cargo install`; otherwise download the binary from
  free-audio/clap-validator GitHub releases)
- VST3 validation: `pluginval --strictness-level 10 target/bundled/ConjureAlign.vst3`
- AU validation: install the `.component`, then
  `killall -9 AudioComponentRegistrar; auval -v aufx ALGN CONJ` (add `-strict` for the
  pedantic pass). The `;` is deliberate — `AudioComponentRegistrar` is an on-demand daemon,
  so `killall` exits non-zero whenever it happens to be idle, and `&&` would skip `auval`
  without printing anything that looks like a failure. A rebuild at an unchanged version needs the `killall` to be re-scanned,
  and the host must be restarted; `rm -rf ~/Library/Caches/AudioUnitCache` is the
  sledgehammer. Note `auval` renders the main bus only, so it never exercises the
  sidechain, and it loads the plugin in-process without building the Cocoa view — a green
  `auval` says nothing about the editor or about AU sandboxing. Logic's Plug-in Manager is
  the real test.
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

Flow (gated capture): the user clicks Capture in the editor (sets `CaptureState::request`,
an `AtomicBool` the audio thread consumes every block) or turns on the `capture` BoolParam
(host generic UI/automation; the plugin never un-toggles it) → `process()` treats either as a
start edge and ARMS a capture → samples are recorded into the pre-allocated buffers only
while a gate (`dsp/gate.rs`: peak envelopes on both mono sums, instant attack, −6 dB
hysteresis, 250 ms hold; threshold = the non-automatable `gate_threshold` param, default
−60 dBFS) is open on BOTH inputs — silent stretches are spliced out and each gate re-opening
records a seam in `CaptureData::splices` (fixed capacity; when full the rest records
continuously, since an untracked seam could corrupt the analysis and extra silence cannot).
The capture stops on the editor's Stop button (`stop_request`), the `capture` param's falling
edge, when 4 s of accumulated signal fills the buffer, or automatically once signal has been
recorded and the gate has stayed closed for `CAPTURE_AUTO_FINISH_SECONDS` (2 s; ≈2.8 s of
real silence including the gate's release+hold) — so playing a short clip once analyzes by
itself instead of pausing forever. Armed never times out (an off-edge after any auto-stop is
a no-op). Phase machine: Idle → Armed → Capturing → Analyzing → Idle, an `AtomicU8`; Armed
means nothing recorded yet, and every transition out of Armed/Capturing is a CAS so a GUI
cancel always wins (`cancel_capture` tries ARMED→IDLE first — the phase only moves forward,
so that order can't drop a cancel). The buffers live in an `AtomicRefCell`; the audio thread
borrows in Idle/Armed/Capturing, the background task in Analyzing, never overlapping. On stop
with data, `context.execute_background(Task::Analyze)` → the `task_executor()` closure (owns
Arc clones) runs `analyze_spliced` (zeroes ±max_shift guard regions around each seam in both
working copies — exactly the cross-seam products — before the FFT cross-correlation), refines
the peak to sub-sample precision by maximizing the continuous cross-correlation (DTFT of the
cross-spectrum — deliberately NOT a parabolic fit, which is biased on sinc-shaped peaks),
detects polarity from the peak sign, then stores the atomics → `process()` notices the
changed target and crossfades (~50 ms, dual delay-line taps, coalescing rapid changes) to the
new delay. Too-short or low-confidence captures are rejected and the previous offset kept;
the editor shows the reason. A missing sidechain shows up LIVE as "Armed — waiting for signal
(ref quiet)" from the `gate_state` bitfield rather than only as a post-hoc rejection. Results
are logged via `nih_log!` and shown in the editor's status strip.

### GUI threading rules

The editor NEVER touches `CaptureState::data` — the `AtomicRefCell` borrow discipline covers
only the audio thread (Idle/Armed/Capturing) and the background task (Analyzing); a GUI
borrow would panic the audio thread. Enforced by construction: the editor only receives a
`CaptureHandle` (phase/progress/gate-state reads + capture/stop requests + cancel; cannot
reach `data`). Waveform and correlation data reach the GUI exclusively through
`shared::AnalysisSnapshot` — full raw copies of the captures (un-zeroed; splice seam
positions ride along for the waveform markers) plus the normalized correlation curve per
integer lag, built by the
background task at the end of `Task::Analyze` (allocation is fine there) and published via
`Mutex<Option<Arc<AnalysisSnapshot>>>` before the phase returns to Idle. The audio thread
never touches that mutex; its only GUI-related work is a handful of atomic loads/stores per
block (capture request swap, progress). The snapshot is deliberately not persisted: after a
session reload the GUI shows the restored detected values but no waveforms until the next
capture. The editor decimates the raw snapshot per zoom level GUI-side
(`editor/decimate.rs`, pure + unit-tested); the applied-shift math in `editor::net_shift`
mirrors `ConjureAlign::current_target` — keep the two in sync. Editor window size persists via
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

### AudioUnit v2 (clap-wrapper)

`clap_wrapper::export_auv2!()` at the bottom of `lib.rs` adds a third exported entry point,
`GetPluginFactoryAUV2`, to the same cdylib that already exports `clap_entry` and
`GetPluginFactory`; clap-wrapper's vendored C++ translates AU calls into CLAP calls against
our own `clap_entry`. NEVER enable clap-wrapper's `vst3` feature — it exports
`GetPluginFactory`, `bundleEntry` and `bundleExit`, the exact three symbols
`nih_export_vst3!` already owns, so `default-features = false` in Cargo.toml is
load-bearing.

The AU identity (`aufx` / `ALGN` / `CONJ`) lives ONLY in `bundler.toml`. nih-plug does not
export CLAP's `clap.plugin-factory-info-as-auv2.draft0` factory, so the plist is the only
channel an AU host has for the four-character codes. `subtype` and `manufacturer` are what
saved Logic sessions key on — changing either orphans every project that used the plugin.
(`CONJ` is a shared manufacturer namespace with the other ConjureDSP plugins; identity is
the type/subtype/manufacturer *triple*, so `aufx/ALGN/CONJ` is what must stay unique.)

`nih_plug_xtask` has no AU support — it sniffs exported symbols and knows only
clap/vst2/vst3, and its Info.plist has no `AudioComponents` array — so `xtask/src/main.rs`
writes the `.component` itself. It copies the binary nih_plug_xtask already placed in the
CLAP bundle rather than reconstructing target/profile paths: that binary is already lipo'd
for `bundle-universal`, so one code path covers single-arch, cross-compiled Darwin and
universal builds.

The "Reference" aux port becomes AU input bus 1 (clap-wrapper creates one AU input element
per CLAP input port and names it from the CLAP port name), which is what Logic uses as the
side chain for `aufx` units. An unconnected bus 1 arrives as silence — clap-wrapper
substitutes a silent buffer when `PullInput` fails — so `aux.inputs.first()` is
`Some(silence)` and the capture is rejected, exactly as on the CLAP/VST3 path.

**Both `AUDIO_IO_LAYOUTS` are reachable from AU, but only because of a local patch.** Stock
clap-wrapper derives `AUChannelInfo` from the *current* CLAP audio-ports config, never calls
`audio-ports-config::select`, and rejects in `ValidFormat` any stream format whose channel
count differs from that config's port — pinning the AU to layout 0 and advertising `[2, 2]`
only. Since Logic filters its Audio FX menu by what a plugin can actually instantiate as,
that made ConjureAlign invisible on mono tracks. `deps/clap-wrapper-rs` is therefore a
vendored copy of the crate carrying a patch that enumerates every config into
`AUChannelInfo`, accepts their formats, and selects the matching one before activation —
see `deps/PATCHES.md`. `auval` must report `[2, 2]  [1, 1]`.

`auval` only ever *renders* the default (stereo) config, so it cannot cover the mono path.
`tests/au_mono_host.c` is a small AU host that does: it forces mono, initializes, and
renders. Its load-bearing assertion is that the "Reference" sidechain bus follows the main
bus down to 1 channel — if it does not, `select()` never reached the plugin and the AU is
feeding mono buffers to a plugin that believes it is stereo.

The generated `AudioComponents` entry deliberately carries `resourceUsage` and NO
`sandboxSafe` key, matching what clap-wrapper's own CMake build-helper emits — claiming
untested sandbox-safety only buys a stricter hosting path that can fail silently, and
upstream clap-wrapper-rs dropped the flag from its bundler for the same reason. It also
carries `tags = ["Effects"]`: Logic files plugins into the Audio FX menu by those, and it
understands only a fixed vocabulary, so they are spelled out in `bundler.toml` rather than
derived from `CLAP_FEATURES`.

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
`[patch."https://github.com/RustAudio/baseview.git"]` pointing at the
`michaeljancsy/baseview` fork's `magnify-as-ctrl-scroll` branch (rev `c1ff57c`), which is
that fix PLUS ONE LOCAL COMMIT: stock baseview registers no `magnifyWithEvent:` handler, so
macOS trackpad pinches produce no events at all in the editor — the commit re-encodes them
as ctrl-scroll `WheelScrolled` events (K = 200 calibrates to egui's default scroll-zoom
speed of 1/200, giving exp(magnification) — the native AppKit convention). That is what
makes pinch-zoom work in every host; see deps/PATCHES.md. Do NOT remove the patch when
nih-plug/egui-baseview advance past the #204 fix — REBASE the magnify commit onto the new
rev instead (without #204 the editor crashes every host on current macOS; without the
magnify commit pinch silently dies). Beware when pushing to the fork: its GitHub refs
predate the pinned revs (objects resolve via the fork network), so pushing a branch uploads
upstream history that touches `.github/workflows/` and needs a gh token with the `workflow`
scope. The AU path is the likeliest of the three to trip the null-deref: clap-wrapper's
`wrappedview.asinclude.mm` calls `gui->set_parent()` on an NSView that is not yet in a
window, which is exactly the scenario that null-derefs.

clap-wrapper 0.14.0 (vendored by the `clap-wrapper` 0.3.1 crate) has these AUv2 quirks —
all verified against the vendored sources, none worth patching for us:
- `WrapAsAUV2::PostConstructor` calls `SetNumberOfElements`, which resets each bus's stream
  format, and then only re-applies the *name*, never the channel count (upstream PR #496,
  merged to `next` only). Harmless here: AUSDK's default element format is stereo and matches
  our *layout-0* ports, which are the ones current at `PostConstructor` time, so both input
  busses come up 2-channel and `auval`'s default pass reports `[2, 2]`. The mono layout is
  reached later, through `select()` + `setupAudioBusses()`, which sets every element's channel
  count explicitly — that is what stops the missing re-apply from biting. It would bite if
  layout 0 were ever mono or >2 channels.
- `SaveState`/`RestoreState` both early-return `kAudioUnitErr_Uninitialized` when
  `!IsInitialized()` (upstream issue #490; the guards are still present on every branch).
  If a Logic project ever fails to restore the detected offset, this is the first suspect —
  patch it directly in `deps/clap-wrapper-rs/external/clap-wrapper/`, which is a path
  dependency, so an edit there rebuilds. (Upstream's `CLAP_WRAPPER_CPP_DIR` hook would be the
  tidier route, but it landed after 0.3.1 and the vendored `build.rs` ignores the variable —
  see `deps/PATCHES.md`.)
- `auval` warns `AU implements MusicDeviceMIDIEvent but is of type 'aufx'`. Cosmetic:
  clap-wrapper registers every AU type through `AUSDK_COMPONENT_ENTRY(AUMusicDeviceFactory,
  …)`. Validation still succeeds.
- Tail Time is not implemented, so `auval` warns that a recommended property is missing.
- AUv2 offline rendering is unsupported upstream.
- nih-plug's `ext_gui_can_resize`/`get_resize_hints` return `false`, so an AU host cannot
  resize the editor; the plugin-driven path (`ResizableWindow` → `gui_request_resize`)
  works.
- The AU wrapper is compiled into the one dylib behind all three bundles, so its ObjC
  classes register even in a CLAP/VST3 host. Loading two of our own bundles in one process
  can log an ObjC "implemented in both" warning — harmless, both copies are the same code
  from the same build (0.3.1 already suffixes the class names to reduce this).

`cargo xtask bundle` is UNSAFE IN A `.claude/worktrees/` WORKTREE: nih_plug_xtask's
`chdir_workspace_root()` picks the TOPMOST ancestor directory containing a `Cargo.toml`,
which from a nested worktree is the main checkout — it silently builds and bundles whatever
branch the main checkout is on (this shipped a stale headless build once). Workaround: build
the xtask binary, then run it with `CARGO_MANIFEST_DIR` pointing at a symlink to the worktree
that lives outside the repo tree, e.g.
`ln -s <worktree> /tmp/aa && CARGO_MANIFEST_DIR=/tmp/aa ./target/release/xtask bundle
conjure_align --release`; bundles then land in the worktree's own `target/bundled/`. From the
main checkout, plain `cargo xtask bundle` is fine.

## DAW testing notes

- REAPER: add ConjureAlign to the target track; open the plugin's pin connector ("2 in 2 out"
  button), add input channels 3/4, and route the reference track there via a send. Fastest host
  for iteration; loads both CLAP and VST3.
- Ableton Live: VST3 build; the device's header exposes a sidechain routing chooser for plugins
  declaring aux inputs — set "Audio From" to the reference track.
- Bitwig: CLAP build; sidechain chooser in the device header.
- Logic Pro: AU build. Copy `ConjureAlign.component` to `~/Library/Audio/Plug-Ins/Components/`,
  `killall -9 AudioComponentRegistrar`, restart Logic, and confirm it validates in
  Settings → Plug-in Manager (under **ConjureDSP**; "Reset & Rescan Selection" if not).
  Insert it as Audio FX → ConjureDSP → ConjureAlign → Stereo, then pick the reference track
  in the **Side Chain** menu at the top right of the plugin header — if the track is not
  listed, send it to a bus and pick the bus. Works on both mono and stereo tracks — if it
  is missing from the Audio FX menu, that is the first symptom of the channel-layout
  problem the `deps/` patch fixes; see the AudioUnit v2 section.
- Null test recipe: duplicate a track, nudge the copy by a known amount (track delay or clip
  nudge), sidechain the original into ConjureAlign on the copy, click Capture (or toggle the
  Capture parameter) and play — recording accumulates only while both inputs clear the Gate
  threshold, so the click can precede playback; click Stop (or toggle the parameter off, or
  let 4 s of signal fill the buffer); after the crossfade, invert one track and sum. Expected depths (measured in
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
(`nih_export_vst3!`) are GPLv3. Do not add GPL-incompatible dependencies. The `clap-wrapper`
crate is MIT/Apache-2.0 and vendors free-audio/clap-wrapper (MIT) plus Apple's AudioUnitSDK
(Apache-2.0); all are one-way compatible with GPL-3.0-or-later, and its VST3 SDK sources are
never compiled because the `vst3` feature is off. A CLAP-only build
could be relicensed by disabling VST3 export, but VST3 is a product requirement.
