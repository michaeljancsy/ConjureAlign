# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

ConjureAlign: a VST3 + CLAP + AudioUnit v2 plugin built on nih-plug that time-aligns a mic signal (the
plugin's main input) to a reference mic (the sidechain "Reference" input) via FFT
cross-correlation, with sub-sample precision and automatic polarity detection. Typical use:
two microphones on one guitar amp, one plugin instance on the track to be shifted, the other
track routed into the sidechain. Has a custom egui editor (overlaid capture waveforms, a
cross-correlation graph with live markers, a comb-filter spectrum panel; all graphs share one
gesture set — drag/scroll pans, pinch or ⌘-scroll (ctrl-scroll off macOS) zooms the x-axis, the y-axis is always
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
  (dead space under the control bar, a clipped bar), plus the two floating surfaces that
  live outside `draw_ui` and would otherwise need a DAW and a click to see: the first-run
  privacy prompt (`_consent.png`) and the ⚙ popover (`_settings.png`). Or run the plugin
  interactively with `cargo run --bin standalone --features standalone -- --backend dummy`
  (works thanks to the baseview `[patch]` — see Known upstream issues).
- CLAP validation: `clap-validator validate target/bundled/ConjureAlign.clap`
  (needs rustc ≥1.95 to `cargo install`; otherwise download the binary from
  free-audio/clap-validator GitHub releases)
- VST3 validation: `pluginval --strictness-level 10 target/bundled/ConjureAlign.vst3`
- Windows: no local toolchain — `.github/workflows/windows.yml` builds, tests, bundles and
  validates (pluginval `--skip-gui-tests`; clap-validator tolerating exactly the 4 known
  upstream failures) on `windows-latest`, uploads `ConjureAlign-<v>-Windows.zip` as an
  artifact, and attaches it to the GitHub Release on `v*` tag pushes (warns if the release
  doesn't exist yet — create it and re-run the job). The editor is untestable in CI (no
  GPU); GUI checks need a real Windows machine.
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
- Release: `./scripts/release.sh` — universal build, sign, notarize, and package into
  `dist/ConjureAlign-<version>-macOS.pkg`, a signed installer whose component packages
  target `/Library/Audio/Plug-Ins/{VST3,CLAP,Components}` (AU postinstall clears the
  AudioComponentRegistrar cache). Signing the pkg needs the "Developer ID **Installer**"
  cert — a different cert from the "Developer ID Application" one that signs the bundles.
- Toolchain: stable Rust. nih_plug is a git dependency (not on crates.io); Cargo.lock pins the
  rev — but the `nih_plug` crate itself is `[patch]`ed onto the vendored copy at
  `deps/nih-plug` (teardown fixes for the shared background worker; see Known upstream issues
  and deps/PATCHES.md). `nih_plug_egui`/`nih_plug_xtask` still resolve from the git source, so
  the vendored tree must move in lockstep with the pinned rev. `atomic_float` in Cargo.toml
  must stay on the same version nih_plug uses, because its `AtomicF32` implements nih_plug's
  `PersistentField`.
- Debug builds enable nih_plug's `assert_process_allocs`: any allocation on the audio thread
  panics. Keep it that way; fix the code, not the feature flag.

## Architecture

- `src/analytics.rs` — opt-in Mixpanel telemetry (see its own section below);
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

### Analytics (opt-in, `src/analytics.rs`)

Anonymous Mixpanel events, OFF until the user consents. Three invariants, all load-bearing:

1. **The audio thread never touches analytics.** Events are raised from `initialize()` (main
   thread) and from the `Task::Analyze` arm of `task_executor()` — the capture event is built
   inside the `data` borrow but *sent* after it drops and after the phase returns to Idle, so
   neither the borrow nor `initialize()`'s spin is extended. `process()` has no analytics code,
   which is what keeps `assert_process_allocs` meaningful.
2. **Consent is tri-state and install-wide** — never asked / granted / declined — stored in
   `~/Library/Application Support/ConjureDSP/ConjureAlign/analytics.json` (`%APPDATA%\…` on
   Windows), NOT in `#[persist]` state, which is per-DAW-session. `None` (no file, or an
   unparseable one) is what shows the first-run prompt; **declining must write the file** or the
   prompt returns forever. Declining stores no device id. The file is read once per process into
   a `OnceLock`, so a change in one running DAW reaches other processes at their next launch.
3. **No thread outlives the dylib.** One worker thread per process, shared by every instance via
   a `Mutex<Weak<Worker>>` registry; each `AnalyticsHandle` holds a strong ref. `Worker::drop`
   sets a shutdown flag, drops the sender (which is what wakes a worker parked in `recv()` —
   joining before disconnecting would deadlock), then joins. Sends are `try_send` on a 32-slot
   bounded channel: a wedged network drops events, never blocks a caller or grows a backlog.
   The drop-side join is bounded because every stage of `post()` is: DNS runs on a throwaway
   helper thread awaited with a timeout (`resolve_bounded` — the ONE deliberate exception to
   this rule, mirroring ureq's resolver: on timeout that thread is abandoned inside
   `getaddrinfo`, accepted because the alternative was a certain DAW-unload hang, and the
   image outlives it in practice — macOS pins ObjC-bearing images), every resolved address is
   tried, connect/read/write carry per-op timeouts, and the response read has a wall-clock
   deadline plus a size cap.

Transport is a hand-written HTTP/1.1 POST over `native-tls` rather than an HTTP client crate:
the request is one fire-and-forget JSON body to a fixed host, and native-tls binds the OS stack
(Security.framework / SChannel) so there is no `ring`/nasm build-tooling risk on the Windows CI
and no bundled root store to go stale. **`native-tls` is target-gated to macOS+Windows** — on
other targets it would drag in openssl, and Linux is a build-from-source platform here, so
`config_path()` returns `None`, `consent()` reports a settled `Some(false)`, and the whole
feature is inert. `CONJURE_ALIGN_ANALYTICS_URL` overrides the endpoint for a local sink.

The payload is bucketed on purpose (`confidence_bucket`, `offset_bucket`): raw figures would
describe the user's material. `MIXPANEL_TOKEN` holds the ConjureAlign project's token;
client-side tokens are public by design (write-only ingestion, no read access).

Three events: **`Plugin Loaded`** (once per instance, from `initialize()`), **`Capture
Completed`**, **`Capture Rejected`**. The first is deliberately NOT called "Session Start" —
Mixpanel ships a built-in virtual event, `$session_start`, whose *display name* is exactly
"Session Start", so a custom event by that name is indistinguishable from it in the event
picker. Check `Get-Events` before naming anything new. Verify ingestion with
`cargo test --release -- --ignored --nocapture smoke_test`, which asks Mixpanel for
`verbose=1` and asserts `"status": 1` — a bad token otherwise reads as a silent bare `0`.
**It writes one real event to the live project**, tagged `smoke-test`.

UI: the first-run prompt (`editor::consent_modal`) and the ⚙ popover (`settings_menu`) are both
drawn OUTSIDE `draw_ui` / from the control bar respectively, and both are `pub` so
`examples/gui_preview.rs` can render them headless (`_consent.png`, `_settings.png`) — a
consent dialog has no business in the panel screenshots. Two layout constraints learned the
hard way: the status strip has **zero slack at the 600×460 minimum** (its labels already reach
the Capture button and *overflow* rather than truncate, so anything parked there gets drawn
through), which is why the gear rides the centered control-bar row's spare width instead; and
egui never flips a popup, so the gear's popover is explicitly `AboveOrBelow::Above` or it opens
out through the window bottom.

Never use a native OS dialog (`NSAlert`/`MessageBox`) for consent: headless scanners (`auval`,
`pluginval`, Logic's scan) instantiate the plugin with no GUI, and a modal there hangs the scan.

### Crash reporting (opt-in, `src/crash.rs`)

Sentry, gated on the **same** consent answer as analytics — `analytics::enabled()`, the same
`analytics.json`, one question in the UI. There is no second toggle and no second identifier:
reports are labelled with `analytics::device_id()`, the same random install id.

The two analytics invariants above hold here too (nothing before consent; no thread outliving
the dylib — `Reporter` is held through a `Mutex<Weak<Reporter>>` registry exactly like
`Worker`, and dropping the last strong ref closes the client, which ends the release-health
session and joins Sentry's transport thread). Three things are specific to panics:

1. **The hook is scoped, not blanket.** A plugin shares its process with the DAW and every other
   plugin. Sentry's stock `PanicIntegration` installs a hook that captures *any* panic anywhere
   in that process, which would file the host's bugs — and every other Rust plugin's — as ours.
   So `default_integrations` is `false`, the other four integrations are listed by hand, and
   `PanicIntegration` is only ever *constructed* (for `event_from_panic_info`), never
   registered. Our own hook reports only when `crash::in_plugin_code()` — a thread-local depth
   counter raised by the `crash::scope()` guards in `initialize()`, `process()`, `reset()`,
   the `task_executor` closure and the editor's build and draw closures. Everything the hook
   (and `crash::scrub`) touches on the panicking thread must use the `_in_hook` analytics
   accessors / `try_lock` — the panicking frame may hold those very locks, so a blocking
   `lock().unwrap()` there is a same-thread deadlock or a panic-inside-the-hook abort.
2. **The hook must chain, and must be installed late.** nih-plug installs a global hook of its
   own from the CLAP `clap_entry.init` / VST3 `bundleEntry` dylib entry points (`setup_logger()`
   → `log_panics()`), long before `Plugin::default()`. Ours is installed on consent, so
   `take_hook()` picks that one up and always calls it — nih-plug's stderr/`NIH_LOG` panic
   logging keeps working. Installing from a library constructor would run *before* nih-plug and
   be silently replaced.
3. **The hook body is wrapped in `permit_alloc`.** A panic can originate on the audio thread —
   the `AtomicRefCell` borrows in `process()` are the likeliest failure in this codebase — where
   `assert_process_allocs` would otherwise turn the report into a second panic inside the first.
   nih-plug's own hook does the same. The `scope()` guard itself is just a thread-local
   increment, so `process()` still allocates nothing.
4. **An editor panic is reported AND swallowed.** Both editor closures run their body inside
   `editor::guarded_frame` (`catch_unwind` + `AssertUnwindSafe`), because they are called from
   an `extern "C"` frame — a CFRunLoop timer on macOS, a window proc on Windows — and unwinding
   out of one aborts the host: an arithmetic bug in `view_math` killed Ableton Live instantly
   (CONJUREALIGN-3). The `crash::scope()` guard stays OUTSIDE the catch, which is what preserves
   attribution: the hook runs at the panic site, before the unwind. On a catch the whole
   `EditorState` is replaced by `EditorState::after_panic()` — it may be half-updated, and its
   snapshot is the likeliest input to the panic — which latches `panic_screen` (a message plus a
   "Reload the editor" button, previewed as `gui_preview_panic.png`) in place of the panels.
   The latch is not cosmetic: without it a panic that recurs every frame costs a Sentry report
   and a blocking 2 s flush at 60 Hz. Nothing here touches the audio thread, so a dead editor
   still leaves the correction running and every parameter reachable from the host's generic UI.

`before_send` (`crash::scrub`) is the last gate before anything leaves, and each line in it
backs a promise in the README/consent copy: `server_name` is nulled (`sentry-contexts` fills it
from the `hostname` crate), `user` is reduced to the device id, and `debug_meta.images` is
trimmed to our own dylib — `debug-images` otherwise enumerates every shared library in the
process, i.e. every other plugin the user owns. **Changing what is sent means changing
`consent_modal`, `settings_menu` and the README Privacy table too.**

`RejectReason` is NOT an error: it is an expected user outcome, already logged and already a
Mixpanel event. Routing it here would bury real crashes. Only panics and genuine
should-not-happen conditions (`crash::report_issue`) go to Sentry.

The `sentry` dep is target-gated to macOS+Windows for the same reason `native-tls` is, and uses
`default-features = false`: the default `transport` feature is `["reqwest", "native-tls"]` and
`reqwest` pulls `tokio`, i.e. an async runtime inside every DAW. The `ureq` transport is
declared `default-features = false` upstream, so no `rustls`, no `ring`, no nasm on the Windows
CI. `CONJURE_ALIGN_SENTRY_DSN` overrides the DSN for a local sink (see
`tests/crash_consent.rs`).

**`crash::BoundedUreq` exists because sentry's stock ureq agent is wrong for a plugin in two
ways.** Do not simplify it back to the default transport:

- **No timeouts.** Sentry's transport configures TLS and proxying but sets none, and every
  `ureq` timeout defaults to `None`. `TransportThread::drop` then joins its worker with
  `handle.join().unwrap()` — unbounded — so one request that never completes (captive portal, a
  firewall that blackholes the connection) blocks the drop forever, on whichever host thread is
  unloading the plugin. A DAW would hang instead of quitting. `CONNECT_TIMEOUT`/`REQUEST_TIMEOUT`
  bound the worst-case unload stall to `SHUTDOWN_TIMEOUT` plus one in-flight request.
- **`RootCerts` defaults to `WebPki`.** That makes ureq call `disable_built_in_roots(true)` and
  trust a *bundled Mozilla store* — the exact thing choosing native-tls was meant to avoid.
  `BoundedUreq` sets `RootCerts::PlatformVerifier` so TLS rides the OS trust store.

**`ureq` must be taken on the `native-tls` feature, never `native-tls-no-default`.** The latter
looks equivalent — it pulls the native-tls crate, and the dependency graph reads correctly — but
ureq's `src/tls/native_tls.rs` is gated on `#[cfg(feature = "native-tls")]`, so the backend is
simply not compiled. `TlsProvider::NativeTls` then panics on the first https request. Raised on
sentry's transport thread, that panic becomes a **host abort**: `TransportThread::drop` joins
with `handle.join().unwrap()`, so the `Err` from the dead worker unwraps on the host's main
thread and unwinds out of the VST3 `extern "C"` teardown. It crashed Ableton Live on plugin
removal. `crash::tls_backend_is_compiled_in_not_just_the_crate` guards it offline, by pointing
the real agent at a local listener that hangs up: a compiled backend errors, a missing one
panics. The cost is `webpki-root-certs` back in the tree — dead weight only, since
`PlatformVerifier` means those roots are never consulted.

That last mechanism generalizes: **any** panic on sentry's transport thread — TLS, envelope
serialization, anything — becomes an abort of the host at unload, because of that unguarded
`join().unwrap()` upstream. There is no way to catch it from our side, so the only defence is
that the transport must not panic. Treat changes to `BoundedUreq` or the `ureq` features as
crash-risk changes and exercise a real TLS connection, not just `cargo tree`.

Nor is the transport thread the only upstream landmine at that seam: sentry-core's session
flusher spawns its thread with `.unwrap()` (a panic on the consenting editor frame under
thread exhaustion) and its Drop unwraps its own mutexes after swallowing a worker panic — so
`sentry::init` is wrapped in `catch_unwind` inside `reporter()` (a failed init degrades to
no-reporting and retries later), `sync_consent` decides under the per-instance mutex but runs
init/teardown OUTSIDE it (a decline's client drain would otherwise freeze the editor frame
while `initialize()` convoys on the lock), and BOTH sentry threads are treated as
must-not-panic.

**Release builds symbolicate to nothing without a debug-file upload.** `[profile.release]` keeps
`strip = "symbols"`; `debug = "limited"` + `split-debuginfo = "packed"` leave a `.dSYM`/`.pdb`
beside the binary that the shipped bundle does not contain, and the Mach-O UUID / PE build id is
what matches the two up. `scripts/release.sh` and `.github/workflows/windows.yml` upload them
via `sentry-cli`, and both **skip with a warning** rather than failing when the credentials are
absent — a release that ships beats a symbolicated crash report, and the upload can be repeated
later from the same build tree.

The two gate on that differently, on purpose. CI reads `secrets.SENTRY_AUTH_TOKEN` plus the
`SENTRY_ORG`/`SENTRY_PROJECT` repository *variables* (slugs and ids are not credentials) and
checks the env vars directly, because on a runner that is the only route they can arrive by.
`release.sh` instead asks `sentry-cli info`, because locally the token usually comes from
`~/.sentryclirc` — which is the only place `sentry-cli login` writes — and an env-var check
there reports a perfectly authenticated machine as unconfigured and silently drops the symbols.
Note that `login` sets the token but NOT the org/project defaults, so those still need to be in
the environment or in `~/.sentryclirc`'s `[defaults]`; that is the likeliest way an
authenticated machine still fails the upload.

Both call sites name the plugin's own binary and dSYM/PDB explicitly rather than pointing at a
release directory. That is not tidiness: `sentry-cli` searches paths recursively, so a whole
release tree uploads every dependency's `build_script_build`, the proc-macro dylibs, the test
binaries and `xtask` — 334 files on the run that caught this, of which 3 were ours — and
`--include-sources` bundles their source too.

The Sentry `release` is `conjure_align@<CARGO_PKG_VERSION>` and must stay in step with what the
upload tags. **macOS has no CI**, so a macOS release only ever gets symbols if this machine is
configured — and debug files match by build id, so a release shipped without them cannot be
symbolicated afterwards without reproducing a byte-identical binary. Adding the
dep cost ~1.8 MB per architecture slice (4.996 → 6.803 MB), i.e. ~10.8 MB on the macOS pkg (two
architectures × three bundles) and ~3.6 MB on the Windows zip.

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

nih-plug's process-shared background worker (`event_loop/background_thread.rs`, same rev) has
two teardown flaws, both triggered by destroying an instance around an in-flight
`Task::Analyze`: (a) destroying it while the task RUNS can drop the last `Arc<Wrapper>` on
the worker thread itself (the worker holds the upgraded `Arc` for the task's duration), whose
`WorkerThread::drop` then joins its own thread — EDEADLK panic-in-Drop on macOS/Linux
(contained, leaks), permanent `WaitForSingleObject(INFINITE)` hang on Windows; (b) destroying
it while the task is still QUEUED makes the worker's `executor.upgrade()` fail and the whole
shared thread exit — later tasks from every instance are silently dropped in release, and the
last instance's teardown panics in Drop (`send(Shutdown).expect` on the disconnected channel)
inside the host's `destroy` → host abort at project close.

Two independent mitigations, both needed. The local containment (the `catch_unwind` +
generation reclaim in the threading section) removes OUR panic route into the worker and
un-wedges stuck instances. The destroy races themselves are upstream's, and are PATCHED
LOCALLY in the vendored `deps/nih-plug` (wired by the
`[patch."https://github.com/robbert-vdh/nih-plug.git"]` in Cargo.toml; hunks marked
`LOCAL PATCH`) — so they cannot bite this build, but they come back if a rev bump drops the
patch. Regression tests:
`cargo test --manifest-path deps/nih-plug/Cargo.toml -p nih_plug --lib background_thread` —
both abort/deadlock against unpatched upstream. The patch also makes teardown tolerate a
worker that died for any other reason, which is what keeps a panic that escapes the
`catch_unwind` from becoming a host abort at the next destroy. deps/PATCHES.md has the
invariants and the re-vendor procedure; docs/upstream/nih-plug-worker-teardown.md is the issue
draft to file upstream by hand (not filed as of 2026-08-28). nih-plug itself is in maintenance
mode; the active community successor, nice-plug (codeberg.org/RustAudio/nice-plug), fixed the
reference-cycle leak of nih-plug#222 but still carries both of these — so the draft's header
directs the report there first. Note #222 does NOT shield us: that cycle only closes for
plugins that retain the `AsyncExecutor` from `Plugin::editor()`, and ours ignores it
(`_async_executor`), so our instances genuinely tear down — which is what makes both bugs
reachable here, and equally what keeps our own Drop-chain invariants (joined analytics/Sentry
threads) working.

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
