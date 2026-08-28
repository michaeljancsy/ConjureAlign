# Vendored `clap-wrapper-rs`

`deps/clap-wrapper-rs` is [blepfx/clap-wrapper-rs](https://github.com/blepfx/clap-wrapper-rs)
**0.3.1**, exactly as published to crates.io, with three deliberate differences:

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
pinned unmodified) **plus two commits**:

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

**Maintenance.** When nih-plug/egui-baseview move past `3e12973`, rebase both commits
onto the new upstream rev and update the `[patch]` rev — do not drop the fork. Note the
fork's GitHub refs are years older than the pinned revs (the objects resolve via GitHub's
fork network), so pushing any branch based on modern upstream uploads history touching
`.github/workflows/` and requires a gh token with the `workflow` scope
(`gh auth refresh -h github.com -s workflow`).

---

# Vendored `nih_plug` (shared-background-worker teardown patch)

`deps/nih-plug` is [robbert-vdh/nih-plug](https://github.com/robbert-vdh/nih-plug) at
`f36931f7af4646065488a9845d8f8c2f95252c23` — the exact rev Cargo.lock pins — applied through
`[patch."https://github.com/robbert-vdh/nih-plug.git"]` in the root Cargo.toml. Same mechanism
as the baseview patch above, but pointing at a local path instead of a fork: nothing here needs
to be public, and a path under `deps/` follows the clap-wrapper-rs precedent. The `[patch]`
replaces `nih_plug` for the whole graph — `nih_plug_egui` and `nih_plug_xtask` still resolve
from the git source at the locked rev, and their internal `nih_plug` path dependency lands on
the vendored copy, so exactly one `nih_plug` exists in the build (verify with
`cargo tree -i nih_plug`: one node, two dependents).

Deliberate differences from upstream, in full:

1. **A subset, not the repo.** Only the root `nih_plug` crate plus `nih_plug_derive` (its one
   path dependency) are vendored; the sibling GUI/tooling crates, the bundled plugins,
   `.github` and `bundler.toml` are deleted, and the `[workspace]` member list in `Cargo.toml`
   is trimmed to match — the only manifest edit, marked `LOCAL PATCH`. The vendored
   `Cargo.lock` is upstream's, pruned by cargo to the trimmed workspace on the first
   vendored-test run (the surviving entries keep upstream's versions); it governs only the
   vendored test runs below — the plugin itself builds against the root workspace's lock.
2. **The teardown patch and its regression tests** in `src/event_loop/background_thread.rs` —
   the only Rust change. Every hunk is marked `LOCAL PATCH (ConjureAlign)`, so
   `grep -rn "LOCAL PATCH" deps/nih-plug` is the complete diff map.

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
non-unwinding panic. aborting." — the host-abort mechanism reproduced in miniature. The usual
gates (`cargo test --release`, `cargo clippy --all-targets`, a bundle build) cover
integration.

Cargo does not `--cap-lints allow` path dependencies the way it does git dependencies, so the
vendored crate surfaces three of upstream's own warnings in our builds (an unused import in
`src/wrapper/clap/util.rs`, and `unexpected cfg` noise from the objc `class!` macro). Left in
place deliberately: fixing upstream cosmetics here would widen the diff the `LOCAL PATCH`
markers are supposed to bound.

## Maintenance

The vendored tree and the git rev that `nih_plug_egui`/`nih_plug_xtask` pin MUST advance
together — `nih_plug_egui` (git) compiles against the vendored `nih_plug`, so drift between
the two revs means API mismatches at best and silent behavior skew at worst. To bump nih-plug:

1. Update the rev for the git dependencies as usual and let Cargo.lock re-pin.
2. Re-vendor: copy the new checkout (`~/.cargo/git/checkouts/nih-plug-*/<rev>/`) over
   `deps/nih-plug` with the same trim — keep the root files, `src/` and `nih_plug_derive/`,
   delete the rest — and re-trim the `[workspace]` members list.
3. Check whether upstream fixed the bugs: they are present while `WorkerThread::drop` still
   `.expect`s on send/join and the worker loop's upgrade-failure arm still `return`s. If both
   are fixed, drop the vendor and the `[patch]` entry instead of steps 4–5.
4. Re-apply the patch: every hunk is marked `LOCAL PATCH (ConjureAlign)` in
   `src/event_loop/background_thread.rs` (three hunks: the `Drop` impl, the worker loop's
   `None` arm, the `tests` module).
5. Re-run the verify commands above.

## Upstream

Not reported as of 2026-08-28. The complete issue draft — mechanism, permalinks,
reproduction tests, suggested diff — is
[`docs/upstream/nih-plug-worker-teardown.md`](../docs/upstream/nih-plug-worker-teardown.md),
written to be pasted into a robbert-vdh/nih-plug issue after review. File it manually;
nothing in this repo posts it anywhere.

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
