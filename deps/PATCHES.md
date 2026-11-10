# Vendored `clap-wrapper-rs`

`deps/clap-wrapper-rs` is [blepfx/clap-wrapper-rs](https://github.com/blepfx/clap-wrapper-rs)
**0.3.1**, exactly as published to crates.io, with five deliberate differences:

1. `external/vst3sdk/` is deleted. We never compile it — the crate's `vst3` feature is off
   (see `Cargo.toml`: enabling it would export `GetPluginFactory`, `bundleEntry` and
   `bundleExit`, the three symbols `nih_export_vst3!` already owns), and `build.rs` only
   references those paths inside `build_vst3()`, which never runs. Dropping it takes the
   vendored tree from 5.5 MB to 2.0 MB. **If the `vst3` feature is ever turned on, the SDK
   has to come back.**
2. `vst3` is removed from the vendored crate's own `default` feature set. Turning the feature
   off on the dependency edge in the root `Cargo.toml` is not enough: the crate lives inside
   the workspace directory, so Cargo makes it an implicit workspace member, and a member is
   built with *its own* defaults. With `vst3` still in that list, `cargo build --workspace`,
   `cargo clippy --workspace`, `-p clap-wrapper`, or any cargo command run from inside
   `deps/clap-wrapper-rs` panics in `build.rs` on the `external/vst3sdk` tree that difference 1
   deleted — and feature unification can hand the linked copy a `vst3` it must not have.
3. The AUv2 mono patch, below.
4. The stopped-host patch, below. Its hunks are marked `ConjureWidth patch` (taken unchanged
   from ConjureWidth, which found the faults) or `ConjureAlign patch` (only here).
5. Two small fixes, below: the editor's idle timer is released, and release builds no longer
   print the wrapper's trace. Marked `ConjureAlign patch`.

It is a path dependency rather than a crates.io one because upstream's
`CLAP_WRAPPER_CPP_DIR` escape hatch — which exists precisely so you can build against your
own clap-wrapper checkout — landed *after* 0.3.1 was published. The released `build.rs`
hardcodes `./external/clap-wrapper/...`, so there is no way to inject a patched tree without
either vendoring or taking an unreleased git rev. Vendoring keeps us on the exact C++ we
validated.

## The patch: let the AU reach every channel layout

**Problem.** ConjureAlign publishes two `AUDIO_IO_LAYOUTS` — Stereo (2 in / 2 out plus a
2-channel "Reference" sidechain) and Mono (1/1 plus a 1-channel sidechain). nih-plug exposes
these through CLAP's `audio-ports-config` extension, with the layout index as the config id.

clap-wrapper's AUv2 wrapper never looks at that extension. It derives everything from
`audio-ports`, which reports only the *currently selected* config, and it never calls
`audio-ports-config::select`. So the AU was pinned to layout 0 and advertised channel
capabilities `[2, 2]` and nothing else.

That is not a cosmetic limitation: **Logic filters its Audio FX menu by what a plugin can
actually instantiate as, so a stereo-only ConjureAlign simply does not appear on a mono
track.** That was the reported symptom.

**Fix**, all in existing files (no files added or removed):

| File | Change |
| --- | --- |
| `src/clap_proxy.h` | Hold `_audioports_config` in `ClapPluginExtensions`. |
| `src/clap_proxy.cpp` | Fetch it with `getExtension(..., CLAP_EXT_AUDIO_PORTS_CONFIG)`. |
| `src/detail/auv2/auv2_base_classes.h` | Declare `WrapAsAUV2::selectAudioPortsConfigForMain`. |
| `src/wrapasauv2.cpp` | `SupportedNumChannels` builds `AUChannelInfo` from *every* config; `ValidFormat` also accepts a main-bus width belonging to any config, and accepts any width on a sidechain bus; `ChangeStreamFormat` and `Initialize` select the matching config, selection is skipped when the plugin already presents that width, and a select that goes through snapshots every element's width around `setupAudioBusses()` and fires the `PropertyChanged(kAudioUnitProperty_StreamFormat, …)` that raw re-setup never emits. |
| `src/detail/auv2/auv2_base_classes.h` | `StreamFormatWritable` returns `!IsInitialized()` (was unconditional `true`), so formats cannot change on a running unit. |
| `src/detail/auv2/process.cpp` | Value-initialize the substitute silent buffers, so an unconnected bus really does read as silence. |
| `build.rs` | Add `rerun-if-changed` for the vendored C++ (see below). |

The sidechain clause matters in practice: a mono instance fed from a **stereo** reference bus
is an ordinary Logic setup, and upstream's strict channel-count equality refuses it. So is the
mirror image — a stereo instance fed from a **mono** bus — so the clause accepts any width on
an aux input element, in either direction. Nothing sizes a buffer from the CLAP port: the
process loop re-reads the channel count from the AU element's buffer list every block
(`detail/auv2/process.cpp`), and a CLAP plugin has to honour that count, which nih-plug does by
copying with `.take(n)` and zero-filling the channels the host did not supply. What makes that
safe is that the element width is frozen before `ProcessAdapter::setupProcessing()` sizes its
pointer arrays — see the `StreamFormatWritable` change below.

`WrapAsAUV2::StreamFormatWritable` returned an unconditional `true`, where AUSDK's own
`AUEffectBase` returns `!IsInitialized()`; it now does the same. `AUBase`'s
`kAudioUnitProperty_StreamFormat` / `_SampleRate` cases carry no initialized-state gate of
their own, so without this a host could change a stream format on a running unit — which under
this patch meant `select()` on an *active* plugin (CLAP marks it `[main-thread &
plugin-deactivated]`, and nih-plug swaps the layout without rebuilding its buffers), an element
growing past the pointer array `setupProcessing()` already sized, and `ReallocateBuffers()`
freeing element buffers under a live render thread.

`selectAudioPortsConfigForMain` returns early when the plugin already presents the requested
main width. Re-selecting the current config is not a no-op: it re-runs `setupAudioBusses()`,
which would revert a sidechain width the host set and was told was accepted. `Initialize()`
and sample-rate-only sets both arrive with the config already correct.

A select that *does* go through re-runs `setupAudioBusses()` too, and there the rewrite is the
point — a genuine mono↔stereo main switch snaps the sidechain to the new config's width. But
`addInputBus`/`addOutputBus` write those formats through raw `AUIOElement::SetStreamFormat`,
which is only half of AUSDK's own dispatch: `AUBase::ChangeStreamFormat` is `SetStreamFormat`
*plus* `PropertyChanged(kAudioUnitProperty_StreamFormat, scope, element)`. A host that
negotiated a width (and heard noErr for it) must learn when the AU rewrites it — otherwise it
keeps rendering with the stale format, `PullInput` fails against the changed element, and the
wrapper substitutes silence on that bus with nothing anywhere saying why (for ConjureAlign:
the capture is forever rejected as "ref quiet"). So the select path snapshots every input and
output element's channel count before `setupAudioBusses()` and fires the missing
`PropertyChanged` for each element whose count changed afterwards — including indices present
in only one snapshot, since a config switch can change the element count (for a vanished
index, the old index is the only address the host knows the element by). Unchanged elements
announce nothing, so a re-setup that moves nothing stays quiet.

**`build.rs` only declared `rerun-if-changed=build.rs`.** Editing the vendored C++ therefore
did not rebuild anything: cargo reported `Finished` in 0.1s and you kept testing the previous
binary. Since the whole point of vendoring is to patch these sources, the build script now
watches `external` and `src` — the whole vendored tree, deliberately, rather than the handful
of directories the patch happens to touch today. Emitting *any* `rerun-if-changed` turns off
cargo's default whole-package watch, so a hand-picked list leaves the same trap open for
everything it omits, and `build_auv2()` also compiles all twelve `external/AudioUnitSDK/src`
translation units and includes `external/clap` and `external/filesystem`. If you ever
re-vendor from upstream, re-apply this or you will chase ghosts.

Two anonymous-namespace helpers (`forEachAudioPortsConfig`, `mainChannelsFor`) sit above
`ValidFormat` because that is the first use site — C++ needs them declared before all three
users.

`audio-ports-config::select` is only legal on the main thread while the plugin is deactivated.
Both call sites satisfy that, and it is now enforced rather than assumed: stream-format
negotiation happens before `Initialize()` *because* `StreamFormatWritable` refuses it
afterwards, and the `Initialize()` call is placed before `activateCLAP()`. The main-thread
guard is taken before the config enumeration, not just before `select()` — Logic drives
`ChangeStreamFormat` from other threads, and that call site holds no guard of its own. After
selecting, `setupAudioBusses()` is re-run so the AU elements agree with the plugin's new port
layout — this is what makes the sidechain bus follow the main bus down to 1 channel.

Most of this degrades to upstream behaviour when the extension is absent: `SupportedNumChannels`
falls through to the original `audio-ports` path, `ValidFormat`'s *main-bus* clause matches
nothing, and `selectAudioPortsConfigForMain` returns false immediately. The sidechain clause is
the exception, and worth knowing when you audit this patch: it reads only `audio-ports`, never
`audio-ports-config`, so it relaxes upstream's strict channel-count equality on aux input
elements for every plugin and every host, extension or no extension.

## The stopped-host patch: keep the plugin and the host in step when no audio runs

Found in ConjureWidth, which shares this wrapper, while testing in Logic Pro, and ported here
on 2026-11-08 together with the matching `nih_plug` and `nih_plug_egui` patches further down.
The hunks marked `ConjureWidth patch` are byte-for-byte ConjureWidth's (as of 2026-11-08,
items 1 to 7), so `diff -r ConjureAlign/deps/clap-wrapper-rs ConjureWidth/deps/clap-wrapper-rs`
shows only the `ConjureAlign patch` hunks described at the end of this section. Keep it that way when either
side changes.

In a new project Logic doesn't call `Render` for a stopped track until it has played once, but
the AU wrapper activates the plugin once and never deactivates it, and upstream relies on
`Render` for everything. So on a stopped track, with audio otherwise healthy:

- The editor's changes wait in nih-plug's queue. nih-plug asks for a flush, but
  `param_request_flush()` was empty. Align, Polarity and the sliders appear dead, because a
  parameter's value only changes when the queue is emptied. Everything then applies at once on
  the first `Render`.
- The host's changes wait in `ProcessAdapter::_events`, so the editor disagrees with the host.
  A queued change can also overwrite a newer one from the editor when `Render` finally comes.

The patch, all in `wrapasauv2.cpp` and `auv2_base_classes.h` except where noted:

1. `param_request_flush()` sets a flag. `onIdle()`, on the main thread every 10 ms, calls
   `params->flush()` if the flag is set and `Render` has been quiet for 200 ms, which is longer
   than any block. Its output events go straight to `onBeginEdit`, `onPerformEdit` and
   `onEndEdit`. While audio runs, `process()` empties the queue as before.
2. `SetParameter()` on the main thread hands the value to the plugin at once through `flush()`
   instead of `_events`, with audio stopped or running, and before `Initialize` too. On any
   other thread it stores the value in its `Parameter` (`parameter.h`) and sets flags, all
   atomics. The next `Render` moves waiting values into `_events`, or with audio stopped
   `onIdle()` passes them to `flush()`. This matters because Logic hosts the plugin in a helper
   process (`AUHostingServiceXPC_arrow`) and calls `SetParameter()` on a thread of its own
   (`AUOOPRenderingServer-...`), for its Controls view and its Undo, with or without audio
   running. Upstream pushed into `_events` from that thread while the audio thread could be
   reading it. Only the latest value of each parameter is kept until it is handed over.
3. `RestoreState()` tells the AU's own parameter store and any listeners about the loaded
   values. Upstream never did, so a host reading the values back got the old ones. It also
   discards values still waiting from step 2, since the loaded settings replace them.
4. `process.cpp` reports the end of a gesture even if `ProcessAdapter` didn't see the start. A
   drag can start with audio stopped and end with it running.
5. `auv2shared_mm_request_resize()` in `auv2_shared.mm` returns `true`. Upstream changed the
   view's frame and then reported failure, so `nih_plug_egui` never resized its contents or
   recorded the new size: dragging the editor's corner did nothing in an AU host. It also keeps
   the host's origin and skips a frame that wouldn't change.
6. `parameter.cpp` reports what kind of control each stepped parameter is, and
   `GetParameterValueStrings()` in `wrapasauv2.cpp` names the steps. Upstream added the unit to
   the flags, where those numbers mean something else, so a host saw every control as
   continuous and the arrows in Logic's Controls view moved an on/off control by 0.0001, too
   little to switch it. Only a parameter whose two steps read Off and On counts as on/off,
   because CLAP has no such type. Here that makes Capture and Alignment On on/off controls and
   Polarity a list of three named choices (Auto, Normal, Inverted); Manual Trim, Gate and Max
   Shift stay continuous. This doesn't touch the values, so saved projects and their
   automation still load.
7. `parameter.cpp` doesn't flag on/off controls as naming their values (the
   `kAudioUnitParameterFlag_ValuesHaveStrings` flag, which upstream sets under its older name
   `HasName` on every parameter whose `value_to_text` works), so a host draws its own checkbox.
   This works around a Logic bug: its Controls view shows "Off", "On (Base Channel 1)" and
   "On (Base Channel 16)" for values 0, 1 and 2 of the second-to-last control if that control
   names its values. Those are the words of a MIDI Mono Mode control that Logic adds to some
   instruments in that position, and Logic doesn't check that it added one
   (`AudioUnitHost::AU2PlugIn::GetControlValueString` in Logic Pro 12's `MAAudioEngine`; found
   in ConjureWidth). ConjureWidth also makes `GetParameterList()` report the plugin's own
   order and keeps an on/off control second to last there. ConjureAlign does NOT take that half
   (see the `ConjureAlign patch` hunks below): it keeps upstream's order, sorted by parameter
   ID, which is what every released version showed, and that order already puts Alignment On,
   an on/off control, second to last. In 1.4.0 Logic would therefore have shown "On (Base
   Channel 1)" instead of "On" for Alignment On; Polarity was never in that position.

Calling `flush()` from the main thread is only safe because of the matching `nih_plug` patch
below. `flush()` may overlap a `Render` that starts just then, and with that patch it only
touches atomics and a lock-free queue. No lock was added, so the audio thread never waits.

**The `ConjureAlign patch` hunks.** There are two.

*Parameter order* (`GetParameterList()` in `wrapasauv2.cpp`). ConjureWidth always reports the
plugin's declaration order. For ConjureAlign that is Capture, Alignment On, Polarity, Manual
Trim, Max Shift, Gate, which would change the order of Logic's Controls view from every
released version and put Max Shift second to last, where Logic would show "Off" at its 10 ms
minimum and "On (Base Channel 1)" at its 200 ms maximum. So this hunk keeps upstream's rule:
the plugin's order only if it provides one, which nih-plug never does, so in practice sorted by
parameter ID. The IDs come from the `#[id]` strings in `src/params.rs` and never change, so the
order is fixed — until a parameter is added. The host test checks that the order is still
sorted, has six controls, and has Alignment On second to last without value names; adding a
parameter fails it, and then whatever lands second to last must be an on/off control (or the
order question has to be settled first).

*Positions within the block* (`parameter.h`, and `SetParameter()` and `Render()` in
`wrapasauv2.cpp`). ConjureWidth drops the position within the block that a host gives with
each change (`inBufferOffsetInFrames`) and hands every waiting change over at the start of the
block. That is harmless there because ConjureWidth has `SAMPLE_ACCURATE_AUTOMATION = false`.
ConjureAlign has it set to `true`: nih-plug applies the first change of a block at its start,
splits the block at the position of each later change, and the delay target is re-read for each
piece. Upstream kept those positions, so this patch keeps them too: a waiting change is stored
with its position and handed over at that position in the next block. Two differences from
upstream remain. If a host sends several changes to the same parameter for one block, only the
last one arrives, at its own position. And a position past the end of the block is moved to
its last sample. Upstream passed it through unchecked, and nih-plug then processed past the end
of the block's buffers: the host test's last section, two changes in one block with the second
placed at sample 4096 of 512, crashes the unpatched build with a memory fault. No host is known
to send such a position, so this is protection rather than a fix for something seen in use.

**Not yet checked in Logic itself:** that Polarity shows Auto, Normal and Inverted, and that
Alignment On shows as a plain checkbox with no "Base Channel" wording.

`tests/au_host_test.m` checks all of this, including changes from a background thread and
from the audio thread part-way into a block. See "Verifying" below.

## Two small fixes: the idle timer, and quiet release builds

Both found on 2026-11-09 by testing with Plug-in Bench and macOS's `leaks` tool.

- **The idle timer.** `wrappedview.asinclude.mm` creates a 50 ms run-loop timer for every
  editor it opens (`CFRunLoopTimerCreate`, so we own a reference) and on close only
  invalidates it, which takes away the run loop's reference and leaves ours. One 320-byte
  timer leaked per open. Both places that invalidate it now `CFRelease` it as well.
- **Quiet release builds.** `build.rs` never defines `NDEBUG`, and `detail/os/log.h` turns
  full logging on whenever `NDEBUG` is missing, so every AU build, release included, printed
  the wrapper's trace (`Setup Busses: …`, `PostConstructor: …`) onto the host's standard
  output. That spammed host logs and broke any tool that reads the host's output as data:
  Plug-in Bench's `--json` output for the AU started with those lines. `build.rs` now defines
  `CLAP_WRAPPER_LOGLEVEL=0` when Cargo's `PROFILE` is `release` (not `DEBUG`, which is "true"
  in release too, because the release profile keeps limited debug info). One trace line in
  `wrapasauv2.cpp` printed with `std::cout` directly, so it sits behind the same switch. The
  `[ERROR]` lines still print. `NDEBUG` stays undefined on purpose, so the C++ `assert`s
  behave as before.

## Verifying the patch after a change

```bash
cargo xtask bundle conjure_align --release   # see CLAUDE.md for the worktree caveat
# install to ~/Library/Audio/Plug-Ins/Components/, then:
killall -9 AudioComponentRegistrar; auval -v aufx ALGN CONJ
```

(`;`, not `&&`: `AudioComponentRegistrar` is an on-demand daemon, so `killall` exits non-zero
whenever it is idle and `&&` would silently skip the `auval` run below.)

`auval` must report **`Reported Channel Capabilities (explicit): [2, 2]  [1, 1]`** and mark
both `1-1` and `2-2` in its channel-handling grid. `auval` only ever *renders* the default
(stereo) config, so it cannot prove the mono path works — the harness at
`tests/au_mono_host.c` does. Build and run it with both widths:

```bash
clang -O1 -framework AudioToolbox -framework CoreFoundation -o /tmp/au_mono_host tests/au_mono_host.c
/tmp/au_mono_host 1 && /tmp/au_mono_host 2
```

The load-bearing assertion is that **in the mono run, input bus 1 ("Reference") reports 1
channel**. The harness checks it (and the main busses) and exits non-zero on a mismatch, so
the `&&` chain above really does gate on it. If it reports 2, `select()` did not reach the
plugin, and the AU is handing mono buffers to a plugin that believes it is stereo — a silent
wrong-shape read on the audio thread rather than a visible failure.

The stopped-host patch, the `nih_plug` flush patch and the `nih_plug_egui` patches are covered
by `tests/au_host_test.m`, which needs nothing installed: it loads the built `.component` into
its own process, opens the real editor in a window behind every other window, and sends it
mouse events directly. The commands are at the top of the file. Run it with `HOME` pointing at
a scratch folder whose settings decline analytics and update checks, as shown there: it loads
the plugin, so with the real settings it would send a "Plugin Loaded" event, start a
crash-reporting session and check for updates (it refuses to start with the real `HOME`). It
prints one line per check and exits with 0 only if all pass. Against 1.4.0, the last build
without these patches, 38 checks fail and its final section crashes the program; 1.4.1 passes
all of them. It also checks that opening and closing the editor keeps no memory, which is the
baseview fork's third commit (below): against a build without that commit the check fails with
about 46 MB kept per open/close, and with it 0.06 MB.

## Upstream

Worth revisiting whenever clap-wrapper-rs publishes a release newer than 0.3.1: if it ships
`CLAP_WRAPPER_CPP_DIR`, this could become a much smaller patch applied to an unmodified
crate. Upstream clap-wrapper has no issue tracking the missing `audio-ports-config` support
as of this writing; the related AUv2 channel-count bug is PR #496, which is merged only to
the `next` branch and is a different defect (it is harmless here, because AUSDK's default
element format is stereo and matches our layout-0 ports).

---

# Patched `baseview` fork (not vendored — a `[patch]` in the root Cargo.toml)

`[patch."https://github.com/RustAudio/baseview.git"]` points at
`michaeljancsy/baseview`, branch `magnify-as-ctrl-scroll`. That branch is
upstream's RustAudio/baseview#204 null-deref fix (`3e12973`, the rev the fork originally
pinned unmodified) **plus three commits**:

## The patch: deliver macOS trackpad pinches at all

**Problem.** baseview's macOS view registers `scrollWheel:` but no `magnifyWithEvent:`
handler, and its event enum has no pinch/zoom variant. A trackpad pinch over the editor
therefore produces **no events whatsoever** — pinch-zoom is unimplementable from inside the
plugin, in every host.

**Fix.** One added handler in `src/macos/view.rs` that re-encodes each magnify event as a
precise scroll-wheel event with the CTRL modifier forced on:

```text
magnifyWithEvent: → WheelScrolled { Pixels { x: 0, y: 200 · magnification }, mods | CTRL }
```

No new event variants, no egui-baseview changes: egui's `is_zoom` convention
(`ctrl || command` + scroll = zoom) picks it up through the existing pipeline. The factor
200 calibrates to egui's default `scroll_zoom_speed` of 1/200, so a pinch applies
`exp(magnification)` — the native AppKit `1 + magnification` convention, compounded across
the gesture. The event's real modifiers ride along. Magnify gestures have no momentum phase;
the began/ended phases carry `magnification == 0` and synthesize harmless zero deltas.

## The second patch: keyboard focus for an embedded editor

**Problem.** An embedded plugin view asks to become first responder exactly once, from
`viewWillMoveToWindow:`. Hosts that keep first responder on their own views for key commands
take it straight back — Logic Pro does — and the editor then never receives a keystroke.
That kills the ←/→ Trim nudge, and it also kills ⌘-scroll zoom, because egui-baseview only
learns Cmd from a key event: its `update_modifiers` reads Shift/Option/Control off each
mouse event and drops the Command bit that macOS stamps there (window.rs:279). Ctrl-scroll
and pinch are unaffected — they ride that same stamp, or force it (see above) — which is why
the symptom is host-dependent and modifier-dependent at once.

**Fix.** Three parts, one commit, all in `src/macos/view.rs`:

1. `mouseDown:` claims first responder (skipping the call when the view already holds it —
   `makeFirstResponder:` otherwise cycles resign/become, which surfaces as focus flapping).
2. Every key event is still delivered, but everything EXCEPT ←/→ is *also* passed up the
   responder chain, so the host keeps transport, save and menu shortcuts while the editor is
   focused. The view has to name those keys itself: egui-baseview answers every event with
   `EventStatus::Captured` (window.rs:605), so baseview's stock "forward only when the
   handler ignored it" rule would forward nothing.
3. Cmd (`MetaLeft`/`MetaRight`) is never delivered to the handler. A latched Cmd is a
   scroll-to-zoom modifier that desyncs from focus in both directions — unreachable when the
   host withholds keys, and never cleared when focus leaves mid-gesture, which turns the next
   ordinary scroll into a zoom. Zoom is pinned to Ctrl/pinch instead. **Cost:** ⌘-shortcuts,
   including egui's clipboard ones, no longer reach the GUI. Harmless here — the editor has
   no text entry — but a view that gains a text field wants this behind a flag.

Both 2 and 3 are policy this plugin gets to set because it owns the fork; neither belongs
upstream as an unconditional default.

## The third patch: release the OpenGL view when the editor closes

**Problem.** `GlContext::create` in `src/gl/macos.rs` took an extra `retain` on the
`NSOpenGLView` it had just allocated: +1 from `alloc`, +1 from that `retain`, +1 from
`addSubview:`, against only the parent view's release and the one release in `Drop`. So every
closed editor left its OpenGL view alive, with the OpenGL context and the window-sized
drawing surface the view owns. Plug-in Bench's "Editor open / close cycles" check reported
about 4.3 MB per open/close (it counts resident memory only). A host that opens and closes the
AU editor 24 times grew 52.6 MB per open/close in total memory (1.26 GB in all, a straight
line), since most of the loss is graphics memory. It was there in every release.

**Fix.** Keep only the `alloc` reference, and have `Drop` take the view out of its parent
before releasing it, so that release is the last one even if the parent outlives the context.
Measured the same way afterwards: 0.14 MB per open/close, and macOS's `leaks` finds nothing
left per cycle. Upstream's later objc2 rewrite holds the view in a `Retained<>` and does not
have this bug. `tests/au_host_test.m` guards it.

**Maintenance.** When nih-plug/egui-baseview move past `3e12973`, rebase all three commits
onto the new upstream rev and update the `[patch]` rev — do not drop the fork. Note the
fork's GitHub refs are years older than the pinned revs (the objects resolve via GitHub's
fork network), so pushing any branch based on modern upstream uploads history touching
`.github/workflows/` and requires a gh token with the `workflow` scope
(`gh auth refresh -h github.com -s workflow`).

---

# Vendored `nih_plug` (three patches: worker teardown, flush, aux-bus bounds)

`deps/nih-plug` is [robbert-vdh/nih-plug](https://github.com/robbert-vdh/nih-plug) at
`f36931f7af4646065488a9845d8f8c2f95252c23` — the exact rev Cargo.lock pins — applied through
`[patch."https://github.com/robbert-vdh/nih-plug.git"]` in the root Cargo.toml. Same mechanism
as the baseview patch above, but pointing at a local path instead of a fork: nothing here needs
to be public, and a path under `deps/` follows the clap-wrapper-rs precedent. The `[patch]`
replaces `nih_plug` for the whole graph — `nih_plug_xtask` still resolves from the git source
at the locked rev, `nih_plug_egui` is vendored too (see the end of this file), and their
`nih_plug` path dependencies land on the vendored copy, so exactly one `nih_plug` exists in the
build (verify with `cargo tree -i nih_plug`: one node, two dependents).

Deliberate differences from upstream, in full:

1. **A subset, not the repo.** Only the root `nih_plug` crate plus `nih_plug_derive` (its one
   path dependency) are vendored; the sibling GUI/tooling crates, the bundled plugins,
   `.github` and `bundler.toml` are deleted, and the `[workspace]` member list in `Cargo.toml`
   is trimmed to match — the only manifest edit, marked `LOCAL PATCH`. The vendored
   `Cargo.lock` is upstream's, pruned by cargo to the trimmed workspace on the first
   vendored-test run (the surviving entries keep upstream's versions); it governs only the
   vendored test runs below — the plugin itself builds against the root workspace's lock.
2. **The teardown patch and the aux-bus bounds patch, each with a regression test.** The
   teardown patch lives in `src/event_loop/background_thread.rs`; the aux-bus bounds patch
   touches `src/wrapper/vst3/wrapper.rs`, `src/wrapper/clap/wrapper.rs` and
   `src/wrapper/util/buffer_management.rs`. Their hunks are marked `LOCAL PATCH (ConjureAlign)`.
3. **The flush patch** in `ext_params_flush()` in `src/wrapper/clap/wrapper.rs`, marked
   `LOCAL PATCH (ConjureWidth)` because it was written there and copied unchanged.
   `grep -rn "LOCAL PATCH" deps/nih-plug` is the complete diff map.

## The flush patch: `flush()` must be safe beside a starting block

The stopped-host patch to the AU wrapper calls `params->flush()` from the main thread, at
moments when the audio thread may be starting a block. Upstream's flush calls
`handle_in_events()`, which takes a mutable borrow of `input_events`. `process()` takes the
same borrow, and whichever thread loses that race panics inside the host. So flush now handles
parameter values only, which go through `update_plain_value_by_hash()` and touch only atomics.
The AU wrapper sends nothing else this way, and the output side already used only atomics and
a lock-free queue. Don't pass this path any other kind of event, or call `handle_in_events()`
from it, without adding a real guard against `process()`. The file is identical to
ConjureWidth's copy; keep it that way.

## The patch: teardown of the process-shared worker must not take the host down

`BackgroundThread` runs every instance's background tasks — our per-capture `Task::Analyze`,
and on Linux even GUI tasks (`LinuxEventLoop` delegates both queues to it) — on ONE
`bg-worker` thread shared by all instances of the plugin in the process. Tasks travel as
`(task, Weak<Wrapper>)`; the worker upgrades the `Weak` per task and holds the `Arc` while the
task runs. Teardown is reference-counted: the last instance to drop its handle sends
`Shutdown` and joins the thread. Two bugs at the pinned rev, both reachable from ordinary
host behavior whenever a task is in flight around instance destruction:

1. **Destroy-during-task makes the worker join itself.** The host's `destroy` drops the
   wrapper `Arc` while the worker holds the upgraded one, so when `execute()` returns, the
   *last* strong reference — owning the `BackgroundThread`, and through it the last
   `WorkerThread` handle — dies **on the worker thread**. `Drop` then joined its own thread:
   `pthread_join` returns EDEADLK and std panics (macOS/Linux, verified against std 1.93.1),
   `WaitForSingleObject(INFINITE)` hangs forever (Windows), with a latent access violation if
   the host unloads the DLL afterwards.
2. **One dead executor killed the worker for everyone.** A queued task whose instance was
   already destroyed fails `executor.upgrade()`, and the worker `return`ed — ending the
   thread every *other* instance shares. From then on `schedule()` fails silently in release
   builds (the only report is a `nih_debug_assert!` at the call sites), and the last
   instance's teardown ran `send(Shutdown).expect(…)` on the disconnected channel — a panic
   that unwinds out of the host's `extern "C"` destroy call: a host abort at project close.

The fix keeps behavior identical outside the failure paths: `Drop` no longer `.expect`s on
send or join, detaches instead of joining when it finds itself running on the worker thread
(the `try_send(Shutdown)` — or, if the queue is full, the channel disconnecting when this
struct's only `Sender` drops — ends the loop right after the current task), and the worker
loop drops a task whose executor is gone and keeps serving the rest.

What detaching does NOT fix: in the destroy-during-task scenario the worker still executes a
few final instructions of plugin code after `destroy()` has returned, so a host that unloads
the dylib immediately could still fault. That window exists upstream too — the worker is
inside plugin code when `destroy` returns, by definition of the scenario — and closing it
needs an upstream design change (destroy synchronizing with in-flight tasks); the issue draft
says so. The patch makes teardown non-fatal and keeps surviving instances working.

## The second patch: an omitted trailing bus must not be read past the host's array

Found 2026-09-09 while chasing a LUNA/Windows crash-on-insert (issues #18 and #37): the
plug-in dies the instant it is inserted, on mono and stereo tracks alike, on every release,
while Studio One is fine. Nothing in Sentry — the reporter never reached the editor, so they
were never asked for consent — so this came from reading the wrapper.

**The bug.** In the VST3 wrapper's `process()`, the loop that picks up the auxiliary
*input* busses from `ProcessData` guarded with

```rust
if aux_input_idx > data.num_outputs as usize { break; }
let audio_input = &*data.inputs.add(aux_input_idx);
```

— the *output* bus count, and `>` rather than `>=`. ConjureAlign's "Reference" sidechain is
input bus 1 and it has one output bus, so a host that passes `numInputs == 1` (main bus
only) sails through the guard and the wrapper reads `inputs[1]`: one `AudioBusBuffers`
past the end of the host's array. Whatever `buffers` pointer sits in that memory is then
dereferenced as channel data by `buffer_management.rs` — an access violation inside our
DLL on the very first process call. The aux *output* loop has the same off-by-one (`>` on
the right count), and the CLAP wrapper mirrors both (`>` on `audio_inputs_count` /
`audio_outputs_count`), though CLAP hosts must pass every port so that half is defensive.

**Why a host does that.** Steinberg's VST 3 FAQ on processing, verbatim: "When these
deactivated busses are the last busses (for input or output), the host do not need to
provide associated AudioBusBuffers, in this case the plug-in should check numInputs and
numOutputs and doesn't process these busses." An unrouted sidechain at insert time is
exactly a trailing deactivated input bus. A host that honours that sentence crashes every
nih-plug plug-in with a sidechain; a host that passes all busses with null pointers (REAPER,
Ableton, Studio One, Bitwig — everything the plug-in was tested in) never trips it. LUNA
processes continuously even with the transport stopped, so insert means an immediate
process call. All of that fits the report; none of it is confirmed by a LUNA crash dump.

**The fix.** Four one-token guards — `>= num_inputs` for the input loops, `>= num_outputs`
for the output loops (the VST3 ones clamp a negative count to zero, since `as usize` on a
negative `i32` would otherwise defeat the comparison) — so an omitted bus leaves its
`ChannelPointers` as `None`. The buffer manager then had to be taught what `None` means,
because upstream's handling of it was broken in both directions: on the aux *input* side a
`nih_debug_assert!(is_some())` contradicted the zero-fill right under it; on the aux *output*
side the `None` arm asserted `output_slices.len() == 0`, which can never hold (the slice
count is the port's declared channel count), and then emptied the channel slices while still
declaring the full block length — a `Buffer` whose `samples()` says 16 over channels of
length 0, which the type documents as a panic waiting to happen in its per-sample iterators.
Now an omitted input bus is silence of the block length and an omitted output bus is a
zero-length buffer, both asserts are gone, and the "every output slice must be non-empty"
validity scan in each wrapper's `process()` — which would otherwise have vetoed the whole
block for that zero-length aux output, silently stopping the plug-in — skips zero-length aux
outputs. Every hunk is marked `LOCAL PATCH`. (Both
asserts were also hard `debug_assert!`s under `cfg(test)`, where the regression test died on
each of them in turn before this was sorted out.)

**A second way in: a switched-off bus sent with null channel pointers** (found 2026-11-09).
The fix above was not enough for Plug-in Bench, whose VST3 "Bus layouts" check switches the
sidechain off: the patched build still crashed on every run, at the same line of
`buffer_management.rs`. That host (built on JUCE) does not omit the bus. It sends it with its
channel count intact, every channel pointer null and every silence flag set. nih-plug ignores
`activateBus`, so the wrapper took that as a live sidechain and copied from address zero. The
buffer manager now reads a null channel pointer as silence, on the aux inputs and also on the
main input. While there: a zero-filled aux input channel (an omitted bus, or a bus with fewer
channels than the port) kept whatever length it had before, `max_buffer_size` at first, so the
sidechain could be longer than the block. Such channels are now resized to the block length.
That stays within the capacity reserved up front, so nothing allocates on the audio thread.
After this, Plug-in Bench passed every check on three runs out of three.

## Verifying the patch after a change

```bash
cargo test --manifest-path deps/nih-plug/Cargo.toml -p nih_plug --lib background_thread
```

Both tests pass against the patched file, and they are load-bearing: reverting the two fix
hunks (keeping the tests) makes `destroying_the_last_instance_mid_task_does_not_self_join`
fail via its panic-hook counter — it counts panics on the `bg-worker` thread, which is where
the EDEADLK join panic lands, invisible to the harness otherwise — and makes
`dead_executor_does_not_kill_the_shared_worker` **abort the entire test process**: the
`.expect` in `Drop` fires while the failed assertion is already unwinding, "thread caused
non-unwinding panic. aborting." — the host-abort mechanism reproduced in miniature.

```bash
cargo test --manifest-path deps/nih-plug/Cargo.toml -p nih_plug --lib vst3::wrapper::tests
```

That is the aux-bus test: a real `Wrapper<P>` around a test plug-in with one sidechain input
and one aux output, driven through `initialize` → `setup_processing` → `set_active` →
`set_processing` → `process` with `numInputs == numOutputs == 1`, where a fully formed poison
bus sits in the memory right behind each one-element array — i.e. exactly where the old
guards read. It asserts that the plug-in saw silence on its sidechain and that the poison
output bus was never written to. Verified by mutation (2026-10-02): revert only the input
guard and it fails on the sidechain assertion with the poison value (12345); revert only the
output guard and it fails on the poison-output assertion. Three more tests in the same module
(2026-11-09) send a sidechain with null channel pointers, a main input with null channel
pointers, and an omitted sidechain in a block shorter than the maximum. Verified the same way:
without either null check the test program aborts, which is the fault Plug-in Bench hit, and
without the resize the length test fails. The usual gates
(`cargo test --release`, `cargo clippy --all-targets`, a bundle build) cover integration.

Cargo does not `--cap-lints allow` path dependencies the way it does git dependencies, so the
vendored crate surfaces three of upstream's own warnings in our builds (an unused import in
`src/wrapper/clap/util.rs`, and `unexpected cfg` noise from the objc `class!` macro). Left in
place deliberately: fixing upstream cosmetics here would widen the diff the `LOCAL PATCH`
markers are supposed to bound.

## Maintenance

The vendored trees (`deps/nih-plug` and `deps/nih_plug_egui`) and the git rev that
`nih_plug_xtask` pins MUST advance together — drift between them means API mismatches at best
and silent behavior skew at worst. To bump nih-plug:

1. Update the rev for the git dependencies as usual and let Cargo.lock re-pin.
2. Re-vendor: copy the new checkout (`~/.cargo/git/checkouts/nih-plug-*/<rev>/`) over
   `deps/nih-plug` with the same trim — keep the root files, `src/` and `nih_plug_derive/`,
   delete the rest — and re-trim the `[workspace]` members list.
3. Check whether upstream fixed the bugs: the teardown ones are present while
   `WorkerThread::drop` still `.expect`s on send/join and the worker loop's upgrade-failure
   arm still `return`s; the aux-bus one is present while the VST3 `process()` aux-input loop
   still compares against `data.num_outputs` (upstream master still did on 2026-10-02). Drop
   the hunks of any bug upstream fixed in step 4; if upstream fixed all three, including the
   flush, drop the vendor and the `[patch]` entry instead of steps 4–6.
4. Re-apply the patches: every hunk is marked `LOCAL PATCH (ConjureAlign)` or
   `LOCAL PATCH (ConjureWidth)` —
   `src/event_loop/background_thread.rs` (three hunks: the `Drop` impl, the worker loop's
   `None` arm, the `tests` module), `src/wrapper/vst3/wrapper.rs` (the two guards, the
   validity-scan filter and the `tests` module), `src/wrapper/clap/wrapper.rs` (the two
   guards, the validity-scan filter, and the `LOCAL PATCH (ConjureWidth)` hunk in
   `ext_params_flush()`) and `src/wrapper/util/buffer_management.rs` (the aux-input assert,
   the aux-output `None` arm, the two null-channel-pointer checks and the two resizes).
5. Re-copy and re-patch `deps/nih_plug_egui` from the same checkout (see its section below).
6. Re-run the verify commands above, and `tests/au_host_test.m`.

## Upstream

Not reported as of 2026-08-28. The complete issue draft — mechanism, permalinks,
reproduction tests, suggested diff — is
[`docs/upstream/nih-plug-worker-teardown.md`](../docs/upstream/nih-plug-worker-teardown.md),
written to be pasted into a robbert-vdh/nih-plug issue after review. File it manually;
nothing in this repo posts it anywhere. The aux-bus patch has its own draft,
[`docs/upstream/nih-plug-vst3-aux-bus-oob.md`](../docs/upstream/nih-plug-vst3-aux-bus-oob.md),
same arrangement, not filed as of 2026-10-02.

The nearest existing upstream report is
[nih-plug#222](https://github.com/robbert-vdh/nih-plug/issues/222) with open PR
[#225](https://github.com/robbert-vdh/nih-plug/pull/225): the wrappers'
`execute_background`/`execute_gui` closures hold strong `Arc`s to their own wrapper, a
reference cycle that stops the drop glue from running on destroy at all. A different defect,
and — this matters — **one that does not shield us**: the cycle only closes for plugins that
*retain* the `AsyncExecutor` handed to `Plugin::editor()`. Ours ignores it (`_async_executor`,
[src/lib.rs:355](../src/lib.rs)) and `nih_plug_egui`'s editor never stores one, so those
strong clones die when `editor()` returns, our instances really do tear down, and the two bugs
above are **live in the shipped build**, not latent. (Flip side, and the reason the shipped
build is otherwise sound: we do not suffer #222's leak either — our `Drop` chain runs, so the
analytics worker and Sentry reporter are still joined, as the "no thread outlives the dylib"
invariant requires.) #225 does not touch `background_thread.rs`, so it neither fixes nor
conflicts with this patch. `background_thread.rs` is unchanged on master since the pinned rev
(checked 2026-08-28), so the draft applies to master as-is.

Know also that robbert-vdh/nih-plug is officially **in maintenance mode** (its README
redirects framework users to the community successor,
[nice-plug](https://codeberg.org/RustAudio/nice-plug)), so #225 — and any issue we file
there — may never be acted on: this vendored patch is the durable state for as long as the
plugin stays on nih-plug, and the #222 leak likewise will not be fixed for us by upstream.
nice-plug has already fixed the leak (its closed issues #3/#4) but as of 2026-08-28 still
carries both teardown bugs verbatim in
`crates/nice-plug/src/event_loop/background_thread.rs` — armed there, since teardown really
runs. The draft's header says how to adapt the report for nice-plug's tracker, where it is
most useful. A future migration to nice-plug is a separate project (crate renames, egui
0.36, an overhauled editor API, crates.io baseview 0.3.1 — the pinch patch above needs
re-porting) and this patch must be ported or upstreamed to nice-plug as part of it.

---

# Vendored `nih_plug_egui` (resize and slider-gesture patch)

`deps/nih_plug_egui` is the crate of that name from the same nih-plug rev (`f36931f`), applied
through the same `[patch]` entry as `deps/nih-plug`, and copied unchanged from ConjureWidth's
`deps/nih_plug_egui` (ported 2026-11-08). It has to move together with `deps/nih-plug`, so see
Maintenance above. The differences from upstream are the `nih_plug` path in `Cargo.toml`, the
upstream licence file, one hunk in `src/editor.rs` and one change in
`src/widgets/param_slider.rs`. The code changes are marked `LOCAL PATCH (ConjureWidth)`.

Upstream's resize has two bugs, both visible in any host that accepts the request:

1. It takes `requested_size` before calling `context.request_resize()`, but `Editor::size()`
   reads `requested_size`, so the host is asked for the old size and ends up one step behind.
2. It passes the new size in points to `queue.resize()`, which takes pixels. Nothing corrects
   that on macOS, so on a Retina screen the contents collapse into the bottom left quarter of
   the window and the corner handle moves with them.

The hunk compares the wanted size with the size last applied, converts to pixels, and clears
`requested_size` only afterwards. Comparing sizes also covers a size that arrives by restoring
saved settings (ConjureAlign persists the editor size as `editor-state`), which upstream applied
to neither the contents nor the host. In an AU host none of this showed before, because the
wrapper reported every resize request as failed (stopped-host patch, item 5), so dragging the
corner did nothing at all.

The slider change is about gestures. Upstream sets the value on the press but only tells the
host a gesture has begun once egui counts the movement as a drag, and a plain click never gets
a gesture. So Logic's Undo after a drag went back to where the drag first touched, not to where
the slider had been. The gesture now opens before the first value and closes after the last
one, for clicks too. This covers ConjureAlign's three sliders (Trim, Gate, Max Shift); its
Align and Polarity buttons call `begin_set_parameter()`/`end_set_parameter()` themselves and
were already right.

To bump nih-plug, re-copy the crate and re-apply both. Drop the vendored copy and its
`[patch]` entry once upstream is fixed.
