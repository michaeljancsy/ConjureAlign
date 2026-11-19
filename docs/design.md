# How ConjureAlign works inside

Reference for changing the DSP, the capture/analysis threads or the editor. CLAUDE.md keeps the short rules; this page keeps the reasons. (Moved from CLAUDE.md: Project, Architecture, Threading/state model, GUI threading rules, Latency/PDC.)

## The editor

## Project

ConjureAlign: a VST3 + CLAP + AudioUnit v2 plugin built on nih-plug that time-aligns a mic signal (the
plugin's main input) to a reference mic (the sidechain "Reference" input) via FFT
cross-correlation, with sub-sample precision and automatic polarity detection. Typical use:
two microphones on one guitar amp, one plugin instance on the track to be shifted, the other
track routed into the sidechain. Has a custom egui editor (overlaid capture waveforms, a
cross-correlation graph with live markers, a comb-filter spectrum panel; all graphs share one
gesture set — drag/scroll pans, pinch or ctrl-scroll zooms the x-axis on every platform (Ctrl
is stamped on the scroll event itself, so it needs no keyboard focus; ⌘ would, and Logic
never grants it — see the baseview patch), the y-axis is always
plugin-scaled, double-click fits; Trim is adjusted via its slider or ←/→ while hovering a
graph — those DO need keyboard focus, so in Logic the editor must be clicked once first —
there is no drag-to-trim; the lower panel's header row carries a legend spelling
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

Every glyph in an editor string literal must exist in egui's bundled fonts — Ubuntu-Light,
Noto Emoji and emoji-icon-font, consulted in that order — because egui paints a missing one
as the ◻ replacement box and says nothing: the status strip's "●" phase marker and Cancel's
"✕" shipped as boxes in 1.3.0. They are now "⏺" and "✖", drawn from the same two fonts as
the ⏺/⏹ button icons, so they match those in weight. The unit test
`editor::tests::label_glyphs_exist_in_egui_default_fonts` scans `src/editor/*.rs` (comments
stripped) for exactly that, so a glyph the fonts lack fails `cargo test`, not a screenshot.

## Source layout

## Architecture

- `src/analytics.rs` — opt-in Mixpanel telemetry (see its own section below);
  `src/host.rs` — which DAW and OS the plugin is running in (see below);
  `src/update.rs` — the opt-in update check; `src/net.rs` — the one HTTP client and the one
  background network thread both of them (and nothing else) go through; `src/config.rs` —
  the install-wide preferences file holding both consent answers;
  `src/session_marker.rs` — the "did the last session end cleanly?" file, which is the only
  thing that can report a crash that is not a Rust panic (see below);
- `src/lib.rs` — Plugin impl and orchestration; `src/params.rs` — the whole state model;
  `src/capture.rs` — capture buffers + phase machine + the GUI-safe `CaptureHandle`;
  `src/analysis.rs` — offset estimation (background thread only); `src/shared.rs` — the
  snapshot channel between the background task and the editor; `src/editor/` — the egui GUI
  (GUI thread only); `src/dsp/` — delay line + fractional windowed-sinc interpolator (audio
  thread only). `analysis`, `dsp`, and `editor::decimate` contain no nih-plug types and are
  covered by `cargo test`, including an end-to-end capture→analyze→correct test in
  `tests/end_to_end.rs`.

## Threading / state model

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
a no-op). The editor renders an unconsumed `request` exactly like Armed (`⏺ Armed — waiting
for playback`, Stop/Cancel up): `CaptureHandle::display_phase` folds it in as the
display-only `PHASE_PENDING`, read ONCE per frame in `draw_ui` and threaded to the strip,
the button and the overlay so no frame can show them disagreeing. Arming happens on the
audio thread, and **in a freshly opened session Logic does not call `process()` on a
stopped audio track until it has played once** (afterwards it kept processing for at least
a minute; input-monitoring or record-enabled tracks always process), so a Capture click
made before Play showed nothing at all until playback — which users read as "Capture needs
two clicks" (issue #42, diagnosed with a real trackpad 2026-09-21; synthetic clicks kept
arming because that Logic session had already played). Two consequences. `reset()` and
`initialize()` KEEP the request, and `reset()` turns an Armed (nothing recorded) capture
back into one: nih-plug runs `reset()` from VST3 `setProcessing(true)` / CLAP
`start_processing`, which is how a host that suspends processing while idle (Cubase-style)
reaches its first block after Play — clearing there would drop the capture the editor had
just promised (AU never passes through it: clap-wrapper's `Start()`/`Stop()` are no-ops).
And Stop while pending cancels outright — a `stop_request` would be consumed only AFTER
the `request` it was meant to stop, since `process()` resolves stops before starts. The
pending display covers the GUI request only: a `capture` param edge from the host's generic
UI while stopped still reads Idle until the first block (the edge lives in `process()`'s
`prev_capture`, and a session saved with Capture on must not show as pending). Phase machine: Idle → Armed → Capturing → Analyzing → Idle, an `AtomicU8`; Armed
means nothing recorded yet, and every transition out of Armed/Capturing is a CAS so a GUI
cancel always wins (`cancel_capture` tries ARMED→IDLE first — the phase only moves forward,
so that order can't drop a cancel). The buffers live in an `AtomicRefCell`; the audio thread
borrows in Idle/Armed/Capturing, the background task in Analyzing, never overlapping. Two
hardenings guard that discipline where hosts stress it: `Task::Analyze` carries a capture
*generation* and runs its whole body under `catch_unwind` (an escaped panic would kill
nih-plug's process-shared worker — no analysis for ANY instance until the DAW restarts, plus a
host abort at the next teardown unless the vendored patch is in place — see Known upstream
issues),
taking its borrow with `try_borrow` and re-checking the generation inside it; and
`initialize()` — which hosts re-run on every state load while holding the same lock
`process()` takes — takes a fast path when (rate, channels) are unchanged (no delay-line
rebuild, no ~12 MB reallocation, no wait; latency is deliberately not in the key and is
re-reported either way), and otherwise waits up to 500 ms for an in-flight analysis, then
either reclaims a queued/lost task (generation bump + `try_borrow_mut` probe, reallocating
THROUGH the held guard → phase back to Idle; a stale task exits on its pre-borrow and
in-borrow generation checks) or, if the task actively holds the borrow, keeps the existing
buffers — clearing the fast-path key so the deferred reallocation really happens next time —
and the task finishes with valid results. No interleaving reaches the `AtomicRefCell` panic,
and every phase release on the task side is either value-checked (CAS from Analyzing only)
or generation-guarded, so a stale task can never demote a successor capture's phase. On stop
with data, `context.execute_background(Task::Analyze { generation })` → the `task_executor()` closure (owns
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

## GUI threading rules

The editor NEVER touches `CaptureState::data` — the `AtomicRefCell` borrow discipline covers
only the audio thread (Idle/Armed/Capturing) and the background task (Analyzing); a GUI
borrow would panic the audio thread. Enforced by construction: the editor only receives a
`CaptureHandle` (phase/display-phase/progress/gate-state reads + capture/stop requests +
cancel; cannot reach `data`). Waveform and correlation data reach the GUI exclusively through
`shared::AnalysisSnapshot` — full raw copies of the captures (un-zeroed; splice seam
positions ride along for the waveform markers) plus the normalized correlation curve per
integer lag, built by the
background task at the end of `Task::Analyze` (allocation is fine there) and published via
`shared::SnapshotCell` (a poison-tolerant `Mutex<Option<Arc<AnalysisSnapshot>>>`) before the
phase returns to Idle. The audio thread
never touches that mutex; its only GUI-related work is a handful of atomic loads/stores per
block (capture request swap, progress). The snapshot IS persisted into the DAW session: the
cell is also the `#[persist = "analysis-snapshot"]` field on the Params struct (one shared
`Arc`, wired in `ConjureAlign::default()`), so a host save picks up whatever the task last
published and a reload restores the graphs alongside the detected values.
`src/snapshot_persist.rs` owns the wire format — a versioned DTO with the sample buffers as
base64 raw-LE-f32 inside nih-plug's JSON state, full fidelity by explicit product decision
(2026-08-29) at ≈2.3 MB of state per instance at 48 kHz (≈9 MB at 192 kHz), rewritten every
host save. Its decode side trusts nothing (validators fuzz plugin state): every length is
capped before the allocation it sizes, and any failure — or a future format version —
degrades to "no snapshot" without disturbing the other persist fields. Encode runs outside
the cell's lock, so an editor frame never waits behind a save; a state load CAN store `None`
(pre-capture save), which is why the editor's change detector treats `Some → None` as a
change. The editor decimates the raw snapshot per zoom level GUI-side
(`editor/decimate.rs`, pure + unit-tested); the applied-shift math in `editor::net_shift`
mirrors `ConjureAlign::current_target` — keep the two in sync. Editor window size persists via
`#[persist = "editor-state"]` (`Arc<EguiState>`).

## Sign convention

Sign convention (pinned by the `sign_convention` test in `analysis.rs`):
`detected offset = t_ref − t_main`; positive ⇒ main leads and gets delayed more.

## Latency / PDC (negative offsets)

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

## Rules the code keeps (short form)

The short rules that used to sit in CLAUDE.md, word for word. The sections above, and
`docs/reporting.md`, `docs/audio-unit.md` and `docs/upstream-issues.md`, give the reasons.

Audio thread:
- Debug builds turn on `assert_process_allocs`: any allocation on the audio thread panics. Keep it
  on. Fix the code, not the flag.
- `process()` contains no analytics, crash-reporting or network code.

Threads and state:
- Detected results (offset in ms, polarity, confidence) are `#[persist]` atomics, not parameters,
  because the background task cannot set parameters.
- The editor never touches `CaptureState::data`. It gets a `CaptureHandle` and reads graphs only
  from `shared::AnalysisSnapshot`.
- Every phase change out of Armed or Capturing is a compare-and-swap, so a cancel always wins.
- `editor::net_shift` mirrors `ConjureAlign::current_target`. Change both together.
- Sign convention: detected offset = t_ref − t_main (pinned by the `sign_convention` test).

Latency:
- Report latency only from `initialize()`, as `max_shift_samples + FIR_CENTER`. Never report the
  applied delay. At runtime, clamps and the search window use `last_latency`, never the knob.

Editor:
- Every character in an editor string must exist in egui's bundled fonts, or it draws as a box.
  `editor::tests::label_glyphs_exist_in_egui_default_fonts` checks this.
- The status strip has no spare width at the 600×460 minimum window. Add nothing to it.
- Never show a native OS dialog (`NSAlert`, `MessageBox`). Headless scans would hang on it.
- Editor closures run inside `editor::guarded_frame`. A panic there is reported and swallowed;
  keep it that way.

## Licensing

GPL-3.0-or-later, mandatorily: nih-plug itself is ISC but its VST3 bindings
(`nih_export_vst3!`) are GPLv3. Do not add GPL-incompatible dependencies. The `clap-wrapper`
crate is MIT/Apache-2.0 and vendors free-audio/clap-wrapper (MIT) plus Apple's AudioUnitSDK
(Apache-2.0); all are one-way compatible with GPL-3.0-or-later, and its VST3 SDK sources are
never compiled because the `vst3` feature is off.
