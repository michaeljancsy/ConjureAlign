# Upstream bugs and the local patches that work around them

Do not chase these as local bugs. deps/PATCHES.md is the full account of each patch and how to re-vendor; this page says what each one is for. (Moved from CLAUDE.md: Commands > Toolchain, and Known upstream issues except the clap-wrapper quirks, which are in docs/audio-unit.md.)

## Toolchain and vendored dependencies

- Toolchain: stable Rust. nih_plug is a git dependency (not on crates.io); Cargo.lock pins the
  rev — but the `nih_plug` crate itself is `[patch]`ed onto the vendored copy at
  `deps/nih-plug` (teardown fixes for the shared background worker, and a `flush()` the AU
  wrapper can call from the main thread; see Known upstream issues and deps/PATCHES.md), and
  so is `nih_plug_egui`, at `deps/nih_plug_egui` (editor resize and slider gestures).
  `nih_plug_xtask` still resolves from the git source, so the vendored trees must move in
  lockstep with the pinned rev. `atomic_float` in Cargo.toml
  must stay on the same version nih_plug uses, because its `AtomicF32` implements nih_plug's
  `PersistentField`.

## clap-validator, nih-plug worker teardown

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
draft. It is not filed: only Michael decides whether to file it, and agents never post it. nih-plug itself is in maintenance
mode; the active community successor, nice-plug (codeberg.org/RustAudio/nice-plug), fixed the
reference-cycle leak of nih-plug#222 but still carries both of these — so the draft's header
directs the report there first. Note #222 does NOT shield us: that cycle only closes for
plugins that retain the `AsyncExecutor` from `Plugin::editor()`, and ours ignores it
(`_async_executor`), so our instances genuinely tear down — which is what makes both bugs
reachable here, and equally what keeps our own Drop-chain invariants (joined analytics/Sentry
threads) working.

## nih-plug VST3 aux-bus bounds

The same vendored tree carries another local patch, unrelated to teardown (and to the AU
flush patch): the VST3
wrapper's `process()` picked up auxiliary input busses behind the guard
`aux_input_idx > data.num_outputs` — the wrong count and the wrong operator — so a host that
hands over only the main input bus made it read one `AudioBusBuffers` past the end of the
host's array and dereference the pointer it found there. The VST3 spec explicitly lets a
host omit trailing deactivated busses ("the plug-in should check numInputs and numOutputs"),
and an unrouted sidechain at insert time is exactly that, which is the leading explanation
for the LUNA/Windows crash-on-insert (issues #18/#37; unconfirmed — no dump). It reproduces
on macOS too: Plug-in Bench's VST3 "Bus layouts" check, which disables the optional
sidechain, segfaulted at address 0 in `BufferManager::create_buffers` on every run of 1.3.0
and 1.4.1 (2026-11-09). It took a second fix in the same patch to stop it: that host does not
omit the switched-off bus but sends it with null channel pointers, and nih-plug ignores
`activateBus`, so the buffer manager now reads a null channel pointer as silence. The aux
output loop and both CLAP loops had the mirror off-by-one, and `buffer_management.rs`
mishandled an absent bus (a soft assert that could never hold, and an aux output buffer
declaring the block length over empty channels, which the wrappers' output validity scan then
turned into a silently skipped block). All patched and marked `LOCAL PATCH`, pinned
by `cargo test --manifest-path deps/nih-plug/Cargo.toml -p nih_plug --lib
vst3::wrapper::tests`, which drives a real wrapper with `numInputs == 1` and a poison bus
behind the array; upstream master still had the bug when last checked, and
docs/upstream/nih-plug-vst3-aux-bus-oob.md is the issue draft. deps/PATCHES.md has the full
account.

## baseview fork

baseview (both the rev egui-baseview pins, `9a0b42c`, and the older one nih-plug's
`standalone` feature pins) null-derefs in `becomeFirstResponder` on recent macOS and ABORTS
THE HOST when the editor window opens: AppKit now attaches the view before it has a window,
and `msg_send![nil, isKeyWindow]` trips rustc's inserted null check. Fixed upstream one
commit after `9a0b42c` (RustAudio/baseview#204, rev `3e12973`); Cargo.toml carries a
`[patch."https://github.com/RustAudio/baseview.git"]` pointing at the
`michaeljancsy/baseview` fork's `fix-gl-view-leak` branch (rev `ce2f86d`), which is that fix
PLUS FOUR LOCAL COMMITS. (The fork's `magnify-as-ctrl-scroll` branch holds the first two plus
`ee92660`, a second, equivalent fix for the same leak that nothing pins; rebase from
`fix-gl-view-leak`.) (1) Stock baseview registers no `magnifyWithEvent:` handler, so macOS
trackpad pinches produce no events at all in the editor — the commit re-encodes them
as ctrl-scroll `WheelScrolled` events (K = 200 calibrates to egui's default scroll-zoom
speed of 1/200, giving exp(magnification) — the native AppKit convention). That is what
makes pinch-zoom work in every host. (2) The view claims first responder on `mouseDown:`
(stock baseview asks once, from `viewWillMoveToWindow:`, and Logic takes it straight back),
delivers every key but hands all of them EXCEPT ←/→ up the responder chain as well —
egui-baseview answers every event `Captured`, so without that the host would lose its own
key commands — and never delivers Cmd at all, which is what pins zoom to Ctrl/pinch.
(3) The OpenGL view is released when the editor closes: stock baseview took an extra
`retain` on it, so every close leaked the view, its OpenGL context and its window-sized
drawing surface — about 50 MB per open/close in a host, in every release up to 1.4.1.
(4) The view makes ONE tracking area, in `create_view`, and never removes or releases one:
on window close it used to release whichever tracking area came first in the view's list,
and that list is added to by AppKit and by hosts too, so it could be one the view did not
own. Pro Tools crashed on it while testing. The dead `update_tracking_areas` (registered as
`updateTrackingAreas:` with a colon, but AppKit's method takes no argument, so AppKit never
called it) is gone.
See deps/PATCHES.md. Do NOT remove the patch when
nih-plug/egui-baseview advance past the #204 fix — REBASE all four commits onto the new
rev instead (without #204 the editor crashes every host on current macOS; without the
magnify commit pinch silently dies; without the focus commit ←/→ are dead in Logic; without
the release commit every editor close leaks about 50 MB; without the tracking-area commit a
window close can crash the host, Pro Tools being the one seen). Beware when pushing to the fork: its GitHub refs
predate the pinned revs (objects resolve via the fork network), so pushing a branch uploads
upstream history that touches `.github/workflows/` and needs a gh token with the `workflow`
scope. The AU path is the likeliest of the three to trip the null-deref: clap-wrapper's
`wrappedview.asinclude.mm` calls `gui->set_parent()` on an NSView that is not yet in a
window, which is exactly the scenario that null-derefs.

## `cargo xtask bundle` in a worktree

`cargo xtask bundle` is UNSAFE IN A `.claude/worktrees/` WORKTREE: nih_plug_xtask's
`chdir_workspace_root()` picks the TOPMOST ancestor directory containing a `Cargo.toml`,
which from a nested worktree is the main checkout — it silently builds and bundles whatever
branch the main checkout is on (this shipped a stale headless build once). Workaround: build
the xtask binary, then run it with `CARGO_MANIFEST_DIR` pointing at a symlink to the worktree
that lives outside the repo tree, e.g.
`ln -s <worktree> /tmp/aa && CARGO_MANIFEST_DIR=/tmp/aa ./target/release/xtask bundle
conjure_align --release`; bundles then land in the worktree's own `target/bundled/`. From the
main checkout, plain `cargo xtask bundle` is fine.

## Vendored code (short form)

Word for word from the old CLAUDE.md.

- `nih_plug`, `nih_plug_egui` and `clap-wrapper-rs` are vendored under `deps/`, and baseview is
  patched to the `michaeljancsy/baseview` fork. When upstream moves, rebase the local patches;
  never drop them. `deps/PATCHES.md` says what each one prevents.
- `atomic_float` stays on the version nih_plug uses.
- New dependencies must be permissively licensed (MIT, Apache-2.0, BSD, ISC, zlib). No copyleft code.
- Building from a worktree, the safe form of the recipe above (no fixed `/tmp` path):

```bash
cargo build --release -p xtask
LINK=$(mktemp -d)/aa && ln -s "$PWD" "$LINK" && CARGO_MANIFEST_DIR="$LINK" ./target/release/xtask bundle conjure_align --release
```

  Then check the timestamps in the worktree's own `target/bundled/`.
