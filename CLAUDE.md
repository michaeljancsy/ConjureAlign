# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

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
  (dead space under the control bar, a clipped bar); the same window mid-capture
  (`_capturing.png`, `_capturing_paused.png`), which is the status strip at its tightest —
  Stop+Cancel up and the longest messages, where every label must elide at the button edge
  rather than paint through it; plus the floating surfaces that
  live outside `draw_ui` and would otherwise need a DAW and a click to see: the first-run
  prompt (`_consent.png`, both questions — the example points HOME at a scratch dir so it
  renders a virgin install rather than this machine's answers) and the ⚙ popover, with an
  update waiting and without (`_settings_update.png`, `_settings.png`). Or run the plugin
  interactively with `cargo run --bin standalone --features standalone -- --backend dummy`
  (works thanks to the baseview `[patch]` — see Known upstream issues).
- CLAP validation: `clap-validator validate target/bundled/ConjureAlign.clap`
  (needs rustc ≥1.95 to `cargo install`; otherwise download the binary from
  free-audio/clap-validator GitHub releases)
- VST3 validation: `pluginval --strictness-level 10 target/bundled/ConjureAlign.vst3`
- Windows: no local toolchain — `.github/workflows/windows.yml` builds, tests, bundles and
  validates (pluginval `--skip-gui-tests`; clap-validator tolerating exactly the 4 known
  upstream failures) on `windows-latest`, packages `ConjureAlign-<v>-Windows-Setup.exe` with
  Inno Setup (`packaging/windows/ConjureAlign.iss`; Inno 6 is preinstalled on the runner, so
  no install step), uploads it as an artifact, and attaches it to the GitHub Release on `v*`
  tag pushes (warns if the release doesn't exist yet — create it and re-run the job). The
  editor is untestable in CI (no GPU); GUI checks need a real Windows machine.
  - The runner IS a real Windows machine and the workflow uses it as one: the installer is
    **smoke-tested end to end** (install over a planted loose-DLL mis-install → install again
    over a planted `ConjureAlign-1.2.0\` leftover and a per-user copy, with a decoy that must
    survive → uninstall), and pluginval is re-run against the *installed* bundle. That is not
    belt-and-braces: an Inno 6 installer cannot be inspected offline — 7-Zip does not read the
    format and `innoextract` lags Inno releases by years — so running it is the only
    verification that exists.
  - `AppId` in the `.iss` is a permanent identity, exactly like the AU `subtype`. Change it
    and a later installer stops recognising existing installs: two Add/Remove entries, two
    uninstallers, no upgrade path. The smoke test hard-codes the same GUID on purpose, so
    changing one without the other fails CI instead of shipping.
  - Two Inno settings are load-bearing and silently no-op if wrong.
    `CloseApplicationsFilter` defaults to `*.exe,*.dll,*.chm`, which matches neither `.vst3`
    nor `.clap` — without the override, `CloseApplications=yes` checks nothing and an install
    against a running DAW half-fails. And `ignoreversion` on `[Files]` is mandatory now that
    the DLL carries a version resource, or Inno skips a file whose installed copy is
    equal-or-newer and a same-version reinstall installs nothing.
  - Passing the version: `#define AppVersion GetEnv("CONJUREALIGN_VERSION")`, **not**
    `ISCC /DAppVersion=…`. The step is `shell: bash`, i.e. Git Bash, whose MSYS layer rewrites
    a leading-slash argument into a Windows path — `/DAppVersion=1.3.0` arrives as
    `D:\AppVersion=1.3.0`. The same hazard is why the installer itself is driven from `pwsh`
    (`/VERYSILENT` would be mangled too).
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
  `--no-notarize` builds without the multi-minute notarization step; it is checked as `$1`,
  so it must be the FIRST argument.
  - Each format package carries a **`preinstall` that sweeps its own format** out of
    `/Library` and every real user's `~/Library` before its payload lands (generated once by
    `make_scripts`, which stamps `SUBDIR`/`BUNDLE`/`CLEAR_AU_CACHE` onto one shared body).
    `BundleIsRelocatable=false` stops Installer *redirecting* onto a hand-installed
    `~/Library` copy but cannot remove it, and that shadow copy is the classic "my update
    didn't take" bug. Per-format, not one global sweep: a component's own preinstall is
    guaranteed to run before its own payload, whereas a separate sweep package would depend
    on components installing in `choices-outline` order — undocumented, and not something to
    put in front of an `rm -rf` of what you just installed. It also means a format
    *deselected* under Customize is left alone rather than silently uninstalled.
  - That script must never fail an install (a non-zero preinstall aborts the whole
    installation), so it runs with neither `set -e` nor `set -u` and always `exit 0`. That
    makes its two guards load-bearing rather than decorative: it refuses an empty
    `$SUBDIR`/`$BUNDLE` (which would turn the `rm -rf` into one that takes every plug-in on
    the machine) and refuses any `BUNDLE` not named `ConjureAlign.*`. Test it without
    installing anything: build a plug-in tree under a scratch directory, pass that directory
    as `$3`, and check that only the matching format disappears while the other two formats,
    another vendor's bundle, and (for non-AU packages) the AU cache all survive. Honouring
    `$3` is what makes that possible — and it is also required for real, since
    `enable_localSystem` still lets the user pick a non-boot volume.
  - A fourth, hidden component installs `/Applications/ConjureDSP/Uninstall
    ConjureAlign.command` (`scripts/uninstall-macos.sh`). It is `visible="false"
    start_selected="true" start_enabled="false"` — always installed, never a checkbox,
    because an uninstaller that exists only when someone ticked it is missing exactly when
    it is needed. It still needs a `<line>` in `choices-outline` (an unreferenced choice is
    inert) **and** both `<pkg-ref>`s: the inner one inside the choice and the outer one
    naming `uninstall.pkg`, or `productbuild --package-path` cannot resolve it.
  - That component deliberately does NOT go through `build_component()`. Its payload is a
    shell script, not a bundle, so `pkgbuild --analyze` emits an empty array and the
    helper's `PlistBuddy -c "Add :0:…"` fails — under `set -e` that kills the release.
    `--component-plist` is optional and configures bundle-specific behaviour only, so it is
    omitted. `--install-location` is the leaf `/Applications/ConjureDSP`, not
    `/Applications`: pkgbuild puts a `.` entry in the BOM carrying the payload root's own
    mode, and `.` maps to the install-location, so aiming at `/Applications` would have that
    entry describe `/Applications` itself. Verified: the BOM contains only `.` and the
    `.command` at `root:wheel 0755`, and `/Applications` never appears.
  - The uninstaller re-execs itself under `sudo` for one privileged phase rather than using
    `osascript … with administrator privileges`, which attributes the password dialog to
    "osascript" — indistinguishable from malware — and fails with no window server. That is
    safe only because the pkg installs the file `root:wheel 0755` inside a `root:wheel 0755`
    directory; relocating it anywhere user-writable would make it a privilege escalation.
    Its body is one `{ … }` group so the self-delete at the end cannot leave bash reading a
    truncated file. `/Applications/ConjureDSP` is removed with `rmdir`, never `rm -rf` — it
    is shared with the other ConjureDSP products — and only the `ConjureAlign` child of
    `~/Library/Application Support/ConjureDSP` is ever deleted, for the same reason.
- Toolchain: stable Rust. nih_plug is a git dependency (not on crates.io); Cargo.lock pins the
  rev — but the `nih_plug` crate itself is `[patch]`ed onto the vendored copy at
  `deps/nih-plug` (teardown fixes for the shared background worker; see Known upstream issues
  and deps/PATCHES.md). `nih_plug_egui`/`nih_plug_xtask` still resolve from the git source, so
  the vendored tree must move in lockstep with the pinned rev. `atomic_float` in Cargo.toml
  must stay on the same version nih_plug uses, because its `AtomicF32` implements nih_plug's
  `PersistentField`.
- `build.rs` exists for exactly one thing: on a Windows host it stamps a `VS_VERSION_INFO`
  resource into the cdylib (via `winresource` → the Windows SDK's `rc.exe`), so an installed
  `ConjureAlign.vst3` / `.clap` — both of which are this DLL renamed, with no other identity
  on disk — can be asked which build it is. It is a no-op on every other target. Note the
  two version shapes: the STRING `FileVersion` is `CARGO_PKG_VERSION` verbatim (three-part,
  matching the release tag and the only shape `update::parse_version` accepts), while the
  binary `VS_FIXEDFILEINFO` block is always `MAJOR.MINOR.PATCH.0`. Explorer will NOT display
  either — its property sheet resolves per-extension and `.vst3`/`.clap` have no registered
  handler — so the check is `(Get-Item '<path>').VersionInfo.FileVersion` in PowerShell, and
  CI asserts both blocks on the bundled files after every build.
- Debug builds enable nih_plug's `assert_process_allocs`: any allocation on the audio thread
  panics. Keep it that way; fix the code, not the feature flag.

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
   There are now **two** independent answers in that one file (`src/config.rs`): this one, under
   the JSON key `"consent"`, and the update check's, under `"updates"`. The key did not get
   renamed to match its Rust field (`Config::analytics`) on purpose — renaming it would read
   every existing install's answer as "never asked". The prompt appears while *either* is `None`
   and renders only the unanswered questions, which is what let the second question be added
   without re-asking the first.
3. **No thread outlives the dylib.** All of this now lives in `src/net.rs`, shared with the
   update check — one worker thread per process for both, via a `Mutex<Weak<Worker>>` registry;
   each `AnalyticsHandle`/`UpdateHandle` holds a `net::WorkerHandle`. `Worker::drop` sets a
   shutdown flag, drops the sender (which is what wakes a worker parked in `recv()` — joining
   before disconnecting would deadlock), then joins; while that flag is set queued jobs are
   drained *unrun*, which is what bounds the join to at most one in-flight request. Jobs are
   opaque `Box<dyn FnOnce()>` on a 32-slot bounded `try_send` channel: a wedged network drops
   work, never blocks a caller or grows a backlog. **Never let a job own the last strong ref to
   the worker** — dropping it on the worker thread makes `Worker::drop` join its own thread,
   which is precisely the nih-plug teardown bug below; `WorkerHandle` is the only route in, and
   it lives on a plugin instance. The drop-side join is bounded because every stage of a request
   is: DNS runs on a throwaway helper thread awaited with a timeout (`resolve_bounded` — the ONE
   deliberate exception to this rule, mirroring ureq's resolver: on timeout that thread is
   abandoned inside `getaddrinfo`, accepted because the alternative was a certain DAW-unload
   hang, and the image outlives it in practice — macOS pins ObjC-bearing images), every resolved
   address is tried, connect/read/write carry per-op timeouts, and the response read has a
   wall-clock deadline plus a size cap.

Transport (`src/net.rs`) is a hand-written HTTP/1.1 client over `native-tls` rather than an HTTP
client crate: the analytics request is one fire-and-forget JSON body to a fixed host, and
native-tls binds the OS stack (Security.framework / SChannel) so there is no `ring`/nasm
build-tooling risk on the Windows CI and no bundled root store to go stale. **`native-tls` is
target-gated to macOS+Windows** — on other targets it would drag in openssl, and Linux is a
build-from-source platform here, so `config_path()` returns `None`, both consent answers report
a settled `Some(false)`, and all three features are inert. `CONJURE_ALIGN_ANALYTICS_URL`
overrides the endpoint for a local sink.

`post()` returns the response *raw*, headers and all, because analytics discards it and the
smoke test wants to grep it. `get()` does not: it decodes the framing (status line stripped,
`Transfer-Encoding: chunked` de-framed) on **bytes**, not on a `String`. Both halves matter —
GitHub chunks, and chunk-size lines sit inside the byte stream, so an un-de-framed body is not
valid JSON; and chunk boundaries fall at arbitrary byte offsets, so decoding to text first and
slicing by byte index would corrupt any multi-byte character split across two chunks (an emoji
in a release note). The response size cap is per-call for the same reason: 64 KB is right for a
status reply and would silently truncate a release document into a parse failure.

The payload is bucketed on purpose (`confidence_bucket`, `offset_bucket`,
`capture_seconds_bucket`, `splice_count_bucket`): raw figures would describe the user's
material. Both capture events also carry `capture_length` and `splices` — read once from
`data.filled`/`data.sample_rate` and `data.splices.len()` inside the borrow that
`analyze_and_publish` already holds, so they cost nothing and touch no new state — and
`Capture Completed` adds `polarity_inverted`, the one un-bucketed value, because a single
bit about mic wiring cannot be coarsened further. They are on the *rejected* event too on
purpose: `reason` alone cannot separate "the user never played anything" from "they played
and it did not correlate", and a high `splices` bucket is the readable symptom of a
`gate_threshold` default that is chopping takes up. `splice_count_bucket` keeps `"max"`
distinct because at `MAX_SPLICES` the seam list stops growing and the rest of the capture
records continuously — past that the count is a floor, not a total. `capture_seconds_bucket`
routes non-finite input to the TOP bucket, not the bottom: a NaN fails every `<` guard, and
silently reporting a broken reading as the shortest capture would be worse than as the
longest. `MIXPANEL_TOKEN` holds the ConjureAlign project's token;
client-side tokens are public by design (write-only ingestion, no read access).

Every event also carries the environment, assembled in `EventContext`: `plugin_version`, `os`
(the *build target*, which cannot move), `os_version` and `daw`/`daw_version` from
`src/host.rs`, and `plugin_format` from `context.plugin_api()` — CLAP, VST3 or standalone,
with **AU folded into CLAP** because clap-wrapper translates AU calls onto our own
`clap_entry` and nih-plug genuinely never sees an AU (`daw` is what separates them again:
Logic and GarageBand load only the AU). `build_payload` takes that context as a parameter
rather than reading globals, which is what keeps its assertions valid on any machine. An
unresolved value is OMITTED, never sent as null — a null lands in Mixpanel's lexicon as a
real value and pollutes every breakdown on the property.

`src/host.rs` resolves the environment once per process into a `OnceLock`, and three rules
hold it together. **Nothing there may panic**: it runs inside `initialize()`, inside the
host's `extern "C"` activation frame, where an unwind aborts the DAW — which is why the
macOS bundle lookup uses the raw `core-foundation-sys` externs with hand-written null checks
instead of the safe `core-foundation` wrappers, whose `wrap_under_get_rule` asserts on the
NULL that `CFBundleGetMainBundle()` legitimately returns in an unbundled host (`auval`,
`clap-validator`). **The DAW is an allowlisted label, never a raw name or path** — the
`current_exe()` path can contain the user's home directory, and even a bare unrecognized
stem is a fingerprint; anything off the list is `"other"` and carries NO version, since an
unknown program plus its version identifies far more than either half. **The labels are wire
values**: renaming one splits its history in Mixpanel, so add freely and rename never.

Why `current_exe()` and not the host name the plugin API offers: nih-plug exposes neither
CLAP's `clap_host.name` nor VST3's `IHostApplication::getName`, so reaching them would mean a
fourth patch on the vendored tree — and it would be *wrong* for AU, where clap-wrapper is the
CLAP host and would name the wrapper rather than Logic. Platform sources: macOS reads
`CFBundleShortVersionString` from the main bundle and `kern.osproductversion` via `sysctl`
(note macOS serves a capped "10.16" to processes linked against a pre-Big-Sur SDK, so a very
old DAW under-reports); Windows reads the executable's `VS_FIXEDFILEINFO` block via
`version.dll` and the OS version from `os_info`. All four deps were already compiled into
their targets before being declared — `os_info` is Windows-ONLY in this graph, because
`sentry-contexts` uses `uname` elsewhere, which is why macOS goes through sysctl instead.
The `daw` labels include `auval`, `pluginval` and `clap-validator`: they load the plugin on
a machine that may well have consented, and labelling them is what lets them be filtered out
of the real usage figures rather than silently inflating the "other" bucket.

The bundle lookup cannot be reached by a unit test — a test binary has no `.app` around it.
`host::tests::print_resolved_host_info` (`#[ignore]`d) exists for that: copy the test binary
into `Foo.app/Contents/MacOS/<an allowlisted name>` with an Info.plist beside it and run it
from there, which is the same shape as a DAW loading the plugin.

Three events: **`Plugin Loaded`** (once per instance, from `initialize()`), **`Capture
Completed`**, **`Capture Rejected`**. The first is deliberately NOT called "Session Start" —
Mixpanel ships a built-in virtual event, `$session_start`, whose *display name* is exactly
"Session Start", so a custom event by that name is indistinguishable from it in the event
picker. Check `Get-Events` before naming anything new. Verify ingestion with
`cargo test --release -- --ignored --nocapture smoke_test`, which asks Mixpanel for
`verbose=1` and asserts `"status": 1` — a bad token otherwise reads as a silent bare `0`.
**It writes one real event to the live project**, tagged `smoke-test` — and deliberately
with the REAL host context, so a newly added `daw` or `os_version` value gets seen once in
the project before it arrives from a user.

`Plugin Loaded` also carries **`upgraded_from`** — the version that ran on this install
before this one — backed by `config::note_running_version` and the `last_version` key in
`analytics.json`. It is present only on the launch that first observes a version change, so
one upgrade produces one marked event however many instances the host loads; absent means
"first run, or an upgrade we cannot prove", and the two are deliberately indistinguishable
(an install arriving from a build that predates the field looks like a first run). The
write happens behind the `enabled()` check in `flush_session`, so a declined install stores
nothing, and only when the version actually changed, so an ordinary launch touches no disk.

It exists because **the cohort-level version breakdown is the wrong instrument.** Asking
Mixpanel "has any device been seen on two versions?" cannot separate "nobody upgraded" from
"nobody was told there was an upgrade" — and on 2026-08-31 the answer was the latter: the
update check itself first shipped in 1.2.0 (`src/update.rs` landed in `c14c06e`, after the
v1.1.0 tag), so the 24 installs then on 1.1.0 had no in-plugin way to learn 1.2.0 existed.
That breakdown is also silently degraded by anything resetting the device id, including the
`/reinstall` skill and the uninstallers. A per-device before/after has none of those
problems. Do not re-derive upgrade rates from disjoint version buckets; read
`upgraded_from`. (Disclosure: a previously-running version is strictly less revealing than
`plugin_version`, already on every event, so this needed no change to the prompt or README —
which is exactly why it has to be written down here.)

UI: the first-run prompt (`editor::consent_modal`) and the ⚙ popover (`settings_menu`) are both
drawn OUTSIDE `draw_ui` / from the control bar respectively, and both are `pub` so
`examples/gui_preview.rs` can render them headless (`_consent.png`, `_settings.png`,
`_settings_update.png`) — a consent dialog has no business in the panel screenshots. The prompt
is each question's heading plus its two buttons and nothing else, with one closing line saying
the answers are changeable under ⚙; what detail survives is in the README, plus the popover for
the update check. It renders only its unanswered questions, so the preview points `HOME` at a
scratch directory to get a virgin install; without that it would render whatever this machine
has already answered. Two layout constraints learned the
hard way: the status strip has **zero slack at the 600×460 minimum** (its labels already reach
the Capture button; every one goes through the truncating `status_label` and the row clips at
the button edge — full text on hover — so parking anything else there costs status text),
which is why the gear rides the centered control-bar row's spare width instead; and
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
`net::Worker`, and dropping the last strong ref closes the client, which ends the release-health
session and joins Sentry's transport thread). Three things are specific to panics:

1. **The hook is per-image, gated on consent only, and captures via `Hub::main()`.** A panic
   hook lives in the panicking image's own statically-linked std, so the host's panics and
   other Rust plugins' can physically never reach ours — every panic our hook sees was raised
   inside this dylib, and all of those are reported (a panic in egui/baseview/nih-plug code we
   ship is still our crash). The `crash::scope()` guards in `initialize()`, `process()`,
   `reset()`, the `task_executor` closure and the editor's build and draw closures feed the
   `in_scope` tag — attribution (known callback vs GUI event loop / helper thread / dependency
   internals), NOT a gate. The one skip: panics on Sentry's own `sentry-*` threads, which are
   must-not-panic and where a capture+flush would wait on the failing machinery itself.
   `PanicIntegration` is only ever *constructed* (for `event_from_panic_info`), never
   registered — registering would install a second, ungated hook next to ours
   (`default_integrations` is `false`, the other four listed by hand; `attach_stacktrace` is
   on so `report_issue` messages carry a stack). Captures and the hook's bounded flush go
   through `Hub::main()`, and `reporter()` re-binds the fresh client on that hub after every
   `sentry::init` — init alone binds only the *calling* thread's hub while every other
   thread's hub is a never-re-synced snapshot, so a consent decline→re-grant from the editor
   would otherwise leave audio/main/bg-worker panics captured into the closed client for the
   rest of the process (pinned by `tests/crash_regrant_threads.rs`). Everything the hook
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

Crash reports carry the same environment as the analytics events — `plugin_api`,
`sample_rate`, `daw`, `daw_version`, `os_version` as tags. `set_host_context` resolves ALL of
it on the main thread and stores plain data, so `scrub` (which runs on the *panicking* thread)
only clones out of the mutex; calling `host::info()` from there instead would run
`current_exe()` and a CoreFoundation lookup inside a panic hook. `os_version` is not
redundant with sentry's own OS context: `sentry-contexts` fills that from `uname`, which on
macOS reports the Darwin version ("25.3.0"), not the one users and release notes talk about
("26.3.1").

`before_send` (`crash::scrub`) is the last gate before anything leaves: `server_name` is
nulled (`sentry-contexts` fills it from the `hostname` crate), `user` is reduced to the device
id, and `debug_meta.images` is trimmed to our own dylib — `debug-images` otherwise enumerates
every shared library in the process, i.e. every other plugin the user owns. The README's crash
paragraph still backs the last two of those; the rest of `scrub` is now an unstated guarantee.

**The analytics prompt no longer enumerates what is collected.** `analytics_question` is a
bare question and its `privacy_section` checkbox a bare checkbox, by an explicit product
decision on 2026-08-28 — so nothing in the UI describes the payload, and the README's
Sent/Never-sent table that used to is gone. Adding a property therefore changes no
user-facing text, which is exactly why it is worth noticing: this file and the
`analytics`/`host` module docs are the only remaining record of what leaves the machine.
Re-check the disclosure question before shipping a property more revealing than the bucketed
outcomes already sent.

That decision covers question ONE only, and it is about *disclosure*. `updates_question` is
bare in the first-run prompt as well, but for an unrelated reason — the prompt was cut to
headings and buttons on length grounds — and its half of `privacy_section` and the README
still carry its copy. That copy is still a promise: "sends nothing about you, never installs
anything" has to stay true of `update.rs`.

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
authenticated machine still fails the upload. The second likeliest is scope: uploads need a
token carrying `project:releases`, and `sentry-cli info` passes on a read-only token — the
upload then 403s and the script warns and continues, so a release built that way is
permanently unsymbolicatable. `sentry-cli info` lists the token's scopes; an org auth token
(sentry.io → Settings → Auth Tokens) is the easy fix.

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

### Crashes that are not panics (`src/session_marker.rs`, `src/crash/veh.rs`)

`crash.rs` above is a **panic hook and nothing else** — `std::panic::set_hook` is the only
handler in the tree. An access violation, a stack overflow, an `abort()`, or a fault inside
linked C/C++ (clap-wrapper, the GL driver, SChannel) kills the host with nothing sent. On
Windows that is most of what "crash" means, and it is how a 1.2.0 install could crash a DAW
repeatedly while the Sentry project stayed completely empty (2026-08-30, LUNA/Windows).

Release health does not fill the gap either, and the reason is not obvious: sentry-core
enqueues a *healthy* session only from `Session::drop` (`Drop` → `close(Exited)` →
`enqueue_session`; there is no periodic send). A host that dies never reports one. So **"no
Sentry data for version X" is ambiguous** between "nobody runs X" and "everyone who runs X
dies" — do not read an empty release as an unused release.

The fix is to stop reporting *from* the dying process. `session_marker` writes one file per
pid under `<config dir>/sessions/`, holding the environment and a `stage`; a clean teardown
deletes it. Anything left behind by a process that no longer exists was an abnormal
termination, and the next launch reports it. Four rules:

1. **Consent gates the file, not just the report.** A declined user gets nothing on disk.
   A fault record counts as evidence only when it is **non-empty**: `veh::install` opens
   that file with `OPEN_ALWAYS`, so on Windows every consenting session has an empty one
   from the moment reporting arms, and testing for existence kept every marker across every
   clean exit — a false unclean-shutdown report on every launch (caught by CI, 2026-08-31).
2. **Never from the audio thread, and never on the `initialize()` fast path.** The stages
   are `initializing` / `initialized` (from `initialize()`), `editor_creating` /
   `editor_open` (around `Editor::spawn`, via the delegating `StageStamped` wrapper in
   `editor/mod.rs`) and `editor_closed` (that handle's `Drop`). None corresponds to
   `process()`, and none may. Both `initialize()` stamps go through `set_stage_if`: the
   entry one only on the slow path and only when the current stage is not an editor one
   (a state load with a window open must neither write a file on the path that exists to
   avoid dropouts, nor claim there was no window), and the exit one only if that entry
   stamp actually landed.
3. **A live pid is never reported** — that would turn pid reuse into an invented crash.
   Skipping one can only *lose* a report, which is the safe direction.
4. **The report is `Level::Warning`, and carries the dead session's own release.** Warning
   because `Session::update_from_event` marks a session errored at `>= Error`, and the
   session being captured into belongs to the *healthy* process doing the reporting — `Error`
   would corrupt the very crash-free rate this exists to make trustworthy. The release
   override (`conjure_align@<the version that died>`) is what makes a version whose every
   session crashed visible at all. Its environment tags are `prev_`-prefixed because `scrub`
   stamps `daw`/`daw_version`/`os_version` from the *current* process.

The sweep runs once per process, dispatched onto the shared `net::Worker` (so `initialize()`
never scans a directory) from the arming branch of `CrashHandle::sync_consent`. It deletes
each marker as it reads it — a marker that somehow crashed the reporter must not be re-read
forever — and caps itself at 32 files and 8 reports.

Crash reporting also arms in **`Plugin::editor()`**, not just `initialize()`: hosts may build
the view before activating, and opening a window is the heaviest native work this plugin
does. It deliberately does NOT arm in `Plugin::default()` — that would start Sentry and make
a network request during a plugin scan, for an instance nobody uses.

`src/crash/veh.rs` (Windows only) adds a vectored exception handler for the hardware faults
themselves. It cannot report — a crashing process is no place for an HTTPS request — so it
writes a fixed-width record into `sessions/<pid>.fault` and the sweep folds it into the next
launch's event. **It is process-global**, so it is written to be invisible to everyone else:
`AddVectoredExceptionHandler` (chainable) never `SetUnhandledExceptionFilter` (a single
global slot the host may own); a fatal-code allowlist that excludes `0xE06D7363` (C++
`throw`) and the other codes hosts raise routinely; an "is the faulting address inside our
own image" test; a re-entrancy guard; no allocation, no locks, no CRT; and it always returns
`EXCEPTION_CONTINUE_SEARCH`. **`uninstall()` runs from `Reporter::drop`** — a handler left
registered across a DLL unload means the OS calls into freed memory at the next exception
anywhere in the process, which would be far worse than the bug it exists to find.

Two things it deliberately cannot do: a fault *inside the GPU driver* called from our code
has its address in the driver, so the ownership filter rejects it; and
`EXCEPTION_STACK_OVERFLOW` leaves so little stack that the write may not complete. The
marker's `stage` is the coarse answer in both cases.

`tests/veh_windows.rs` is the only place the handler ever runs before a release (no Windows
toolchain locally): it re-execs the test binary, arms through the real `sync_consent` path,
and dereferences null. The child calls `SetErrorMode(SEM_NOGPFAULTERRORBOX)` first, or a WER
dialog would hang CI instead of failing it.

### Update checks (opt-in, `src/update.rs`)

Asks GitHub's `/repos/michaeljancsy/ConjureAlign/releases/latest` for `tag_name` and compares
it to `CARGO_PKG_VERSION`. **It notifies and nothing else** — no download, no install, no
self-update. The bundle is mapped into a running host, `/Library/Audio/Plug-Ins` needs admin
rights, and the shipped pkg is signed/notarized/stapled; re-implementing any part of that
inside a plugin buys nothing. The REST API rather than a manifest committed here, deliberately:
a manifest is one more thing to bump at release time, and forgetting it fails silently in the
"nobody hears about the new version" direction, i.e. the exact failure the feature exists to
prevent. `/releases/latest` already skips drafts and pre-releases, so un-advertising a bad
release means marking it pre-release — which is the right thing to do to it anyway. No release
script changes; the notice appears as soon as the Release is published.

Its own consent answer (`"updates"` in `analytics.json`), asked as a **second question in the
same first-run prompt**, deliberately not folded into the analytics one: this check shares no
data and mints no identifier, so tying it to the analytics answer would cost update notices to
users who declined for reasons that have nothing to do with it.

Four invariants:

1. **Checks run from the editor only, NEVER from `initialize()`.** `auval`, `pluginval` and
   Logic's scan all instantiate and initialize the plugin headlessly; a network request during
   a plugin scan is bad manners and can slow it. The one automatic check is in `editor::create`'s
   *build* closure (a window opened, so a human is present), gated on a granted answer and on
   `config::should_check` (24 h after a success, 6 h after a failure, and a clock that went
   backwards forces one rather than locking it out). This is the same reasoning that forbids a
   native consent dialog.
2. **A manual check ("Check now" in the ⚙ popover) runs whatever the stored answer is** — the
   click is the consent for that one request — and must never write an answer the user did not
   give. `tests/update_check.rs` pins that.
3. **The link is a compile-time constant, never a URL from the response.** Clicking it hands a
   URL to the OS browser via `open::that_detached`, so `html_url` is read from the JSON by
   nobody: `parse_release` takes `tag_name`, parses it to three integers, and renders them back
   out, so nothing attacker-shaped can reach the UI, the config file or the browser.
4. **A queued check that never runs must not wedge anything.** `net`'s worker drops whatever is
   still queued at shutdown, and refuses work when its queue is full or its thread never
   spawned. Left alone that would strand `IN_FLIGHT` (no further check for the life of the
   process) and the status on `Checking` — which the editor treats as "animate", i.e. a
   permanent 60 Hz repaint in a DAW. A `CheckGuard` moved into the closure releases both from
   `Drop`, so dropping the job unrun is self-correcting.

UI: the whole notification is the ⚙ button's label — plain `⚙`, or `⚙ Update` in
`ACCENT_DETECTED` when there is something to see. It rides the centred control row's spare
width (~140 px at the 600 px minimum) for the same reason the gear itself does; the status
strip has none. Nothing louder: an update notice must never interrupt a session, and a banner
would cost a row out of the graphs' budget. "Skip this version" stores `update_skipped`, and
anything newer notifies again — an unparseable stored value fails *open* (notify), because a
silenced-forever update the user cannot see or clear is the worse bug. `Status::Failed` is
surfaced only inside the popover, which the user opened on purpose; a failed automatic check
produces no label, no banner and no dialog.

`CONJURE_ALIGN_UPDATE_URL` overrides the endpoint for a local sink.

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
`michaeljancsy/baseview` fork's `magnify-as-ctrl-scroll` branch, which is that fix PLUS TWO
LOCAL COMMITS. (1) Stock baseview registers no `magnifyWithEvent:` handler, so macOS
trackpad pinches produce no events at all in the editor — the commit re-encodes them
as ctrl-scroll `WheelScrolled` events (K = 200 calibrates to egui's default scroll-zoom
speed of 1/200, giving exp(magnification) — the native AppKit convention). That is what
makes pinch-zoom work in every host. (2) The view claims first responder on `mouseDown:`
(stock baseview asks once, from `viewWillMoveToWindow:`, and Logic takes it straight back),
delivers every key but hands all of them EXCEPT ←/→ up the responder chain as well —
egui-baseview answers every event `Captured`, so without that the host would lose its own
key commands — and never delivers Cmd at all, which is what pins zoom to Ctrl/pinch.
See deps/PATCHES.md. Do NOT remove the patch when
nih-plug/egui-baseview advance past the #204 fix — REBASE both commits onto the new
rev instead (without #204 the editor crashes every host on current macOS; without the
magnify commit pinch silently dies; without the focus commit ←/→ are dead in Logic). Beware when pushing to the fork: its GitHub refs
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
