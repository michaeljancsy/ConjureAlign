# Testing ConjureAlign

Host tests (DAWs, anything that opens a window) run only on the test laptop, through a GitHub issue brief to "TEST Conjure Test Runner" (see ~/Projects/ConjureDSP/architecture.md section 9). On the main Mac, claim a turn with `screen-turn` before installing into a plug-in folder or opening a window. Any program that loads the plug-in on this Mac (auval, pluginval, clap-validator, Plug-in Bench, tests/au_host_test.m) runs with HOME pointing at a scratch folder whose analytics.json declines both questions, plus the three override addresses pointing nowhere; otherwise it sends real analytics, crash and update traffic. (Moved from CLAUDE.md: Commands detail, AU host test, Plug-in Bench, DAW testing notes; validator downloads moved from the memory note validators-need-download.md.)

## Build, test, lint and previews

- Build + bundle (debug): `cargo xtask bundle conjure_align`
- Build + bundle (release): `cargo xtask bundle conjure_align --release`
  - Bundles land in `target/bundled/ConjureAlign.clap`, `.vst3` and (macOS)
    `.component` — one cdylib with three entry points, copied into three bundles
- macOS universal binary: `cargo xtask bundle-universal conjure_align --release`
- Install locally (macOS): copy bundles to `~/Library/Audio/Plug-Ins/CLAP/`,
  `~/Library/Audio/Plug-Ins/VST3/` and `~/Library/Audio/Plug-Ins/Components/`
  (the AU must live in one of the two `Components` directories; nowhere else works)
- Unit + integration tests (pure DSP + GUI decimation + the macOS packaging scripts, no
  plugin host involved):
  `cargo test --release` (release mode: the analysis tests run multi-second captures)
- Lint: `cargo clippy --all-targets` (and `--features gui-preview,standalone` to cover the
  dev-only targets)
- GUI visual check without a DAW: `cargo run --example gui_preview --features gui-preview`
  renders synthetic data (known +5 ms offset) through egui_kittest's offscreen wgpu
  renderer: the panels alone (`gui_preview.png`, `_zoom`, `_spectrum`, `_spectrum_trim`)
  and — via a stub `GuiContext` behind a `ParamSetter` — the WHOLE editor at its 600×460
  minimum window (`_full.png`), which is the only scene that shows the vertical budget
  (dead space under the control bar, a clipped bar); the same window with a Capture click
  the audio thread has not consumed yet (`_pending.png`) and mid-capture
  (`_capturing.png`, `_capturing_paused.png`), which is the status strip at its tightest —
  Stop+Cancel up and the longest messages, where every label must elide at the button edge
  rather than paint through it; plus the floating surfaces that
  live outside `draw_ui` and would otherwise need a DAW and a click to see: the first-run
  prompt (`_consent.png`, both questions — the example points HOME at a scratch dir so it
  renders a virgin install rather than this machine's answers) and the ⚙ popover, with an
  update waiting and without (`_settings_update.png`, `_settings.png`). Or run the plugin
  interactively with `cargo run --bin standalone --features standalone -- --backend dummy`
  (works thanks to the baseview `[patch]` — see Known upstream issues).
- Bundling does not change what a DAW loads. Use the `/reinstall` skill to install all three
  bundles together.
- CLAP validation: `clap-validator validate target/bundled/ConjureAlign.clap`
  (needs rustc ≥1.95 to `cargo install`; otherwise download the binary from
  free-audio/clap-validator GitHub releases)
- VST3 validation: `pluginval --strictness-level 10 target/bundled/ConjureAlign.vst3`

## AU host test (no install)

- AU host test, no install needed: `tests/au_host_test.m` loads the built `.component` into
  its own process, opens the real editor, clicks and drags its controls, and checks that
  changes from the editor, the host's main thread, a background thread and the audio thread
  all reach both sides with audio stopped, running and switching between the two, plus gesture
  messages, parameter units and choice names, restoring settings, resizing, and that opening
  and closing the editor keeps no memory. That last check gives each open/close its own
  autorelease pool, as each turn of a host's event loop does: the test program runs no event
  loop, and the wrapper hands its view over autoreleased, so without the pools no view would
  ever be freed and the check could not tell a leak from a fix. Build and run
  commands are at the top of the file. Always run it with `HOME` set to a scratch folder whose
  settings decline analytics and updates, as shown there — it refuses the real `HOME`, because
  loading the plugin with this Mac's consent answers sends real analytics, crash-reporting and
  update-check traffic. Its click positions are measured from the editor's layout (Align and
  Polarity centred in the bottom panel, Trim at its left), so moving those controls means
  re-measuring the constants near `align_x()`.

## Plug-in Bench

- Plug-in Bench opens editor windows, so on the main Mac it needs a `screen-turn` claim; prefer
  sending it to the Test Runner.
- Plug-in Bench (`~/.local/bin/plugin-bench`, a third-party host that stress-tests a VST3/AU and
  reports pass/warn/fail with numbers): run it with `HOME` pointing at a scratch folder whose
  settings decline analytics and updates, plus the three addresses pointing nowhere, exactly as
  for `tests/au_host_test.m`. Plug-in Bench finds its own catalog without `HOME`, and the
  processes it starts inherit it. Never run `plugin-bench scan` without asking, not even with
  `--folder`: it also retries every Audio Unit it has not catalogued, which loads third-party
  plug-ins (a PACE-protected one, on this Mac). `test` takes only catalogued plug-ins, by name
  or identifier, never a path. Its "Bus layouts" check is what found the VST3 sidechain crash,
  and "Editor open / close cycles" found the baseview leak.

## DAW notes (for Test Runner briefs)

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
  problem the `deps/` patch fixes; see the AudioUnit v2 section. In a freshly opened session
  Logic does not run a stopped audio track's `process()` until it has played once
  (input-monitoring — the `I` button — or record-enabled tracks always process; after one
  playback it kept processing for at least a minute), so a Capture click made before Play
  shows `⏺ Armed — waiting for playback` and arms for real on Play — expected, not a
  dropped click (and the reason the phase machine can freeze mid-capture when Logic is
  stopped; Cancel is the escape hatch). Parameters are different since 1.4.1: Align,
  Polarity, the sliders, Logic's Controls view and Logic's Undo all take effect on a stopped
  track (see the AudioUnit v2 section). Logic's Controls view replaces the value names of the
  SECOND-TO-LAST AU parameter with "Off" / "On (Base Channel 1)" / "On (Base Channel 16)" (a
  Logic bug, found in ConjureWidth). ConjureAlign's AU order is sorted by parameter ID, which
  puts Alignment On there, and on/off controls no longer advertise value names, so it is safe —
  but adding a parameter can move a different control into that slot; `tests/au_host_test.m`
  fails when that happens. Expected in Logic's Controls view: Polarity steps through Auto / Normal / Inverted, Capture
  and Alignment On are plain checkboxes with no "Base Channel" names, Alignment On is listed
  second-to-last, and changes made there on a stopped track reach the editor.
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

## Validators on the main Mac

- auval ships with macOS. Run it only on this plug-in (`auval -v aufx ALGN CONJ`), never `auval -a`.
- clap-validator and pluginval are not installed on the main Mac. ~/Projects/ConjureDSP/architecture.md
  section 9 says to build them from source into `target/validators/`; `cargo install clap-validator`
  needs a newer rustc than this Mac has (1.93), so until that changes download the official
  release binaries: `clap-validator-*-macos-universal.zip` from free-audio/clap-validator (the zip
  nests a tar.gz; the binary is `binaries/clap-validator`), and `pluginval_macOS.zip` from
  Tracktion/pluginval (strip quarantine with `xattr -dr com.apple.quarantine pluginval.app`, run
  `pluginval.app/Contents/MacOS/pluginval`). Keep them in `target/validators/`, not /tmp.
- On the main Mac run pluginval with `--skip-gui-tests`; its GUI tests open windows, which belong on
  the test laptop.
- Expected clap-validator result against this nih-plug build: exactly four failures
  (state-reproducibility-basic, -binary, -buffered FAILED; state-invalid-random CRASHED). More
  than four is a real regression.
- A validator run against the wrong binary looks exactly like success. After bundling from a
  worktree, check the bundled binary for a string unique to the new code (`strings ... | grep`)
  before trusting the result.
