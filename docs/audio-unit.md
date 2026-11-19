# The Audio Unit v2 build (clap-wrapper)

Reference for the AU entry point, its identity, the vendored clap-wrapper patches and auval. (Moved from CLAUDE.md: Commands > AU validation, AudioUnit v2 section, and the clap-wrapper part of Known upstream issues.)

## Validating the AU

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

## How the AU is built

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

**Parameters reach the plugin with no audio running only because of a second local patch**
(the stopped-host patch in `deps/PATCHES.md`, ported from ConjureWidth in 1.4.1). Stock clap-wrapper relies on `Render` for every parameter change in both directions,
and Logic doesn't render a stopped track in a new project, so the editor's buttons and sliders,
Logic's Controls view and Logic's Undo all did nothing until Play. Logic also calls
`SetParameter` on a background thread of its out-of-process host, with or without audio. The
patch hands changes over from the main thread's idle timer while `Render` is quiet, keeps
background-thread changes in atomics until the next block or idle tick, reports restored
settings to the host, makes the editor resizable in AU hosts, and gives the host real units
(Capture and Alignment On are on/off, Polarity is a named list of three). It leans on the
`nih_plug` flush patch and the vendored `nih_plug_egui`. ConjureAlign differs from ConjureWidth
in two places: it keeps each host change's position within the block, because this plugin has
`SAMPLE_ACCURATE_AUTOMATION = true`, and it keeps upstream's AU parameter order (sorted by ID,
as every release had) where ConjureWidth reports declaration order — see deps/PATCHES.md for
why the order decides which control Logic mislabels. `tests/au_host_test.m` covers all of it.

Two smaller local fixes ride in the same vendored tree (deps/PATCHES.md, "Two small fixes"):
the editor's 50 ms idle timer is released when the editor closes (one leaked per open), and
release builds define `CLAP_WRAPPER_LOGLEVEL=0`. Upstream's build defines no `NDEBUG`, so every
AU build printed the wrapper's trace onto the host's standard output.

The generated `AudioComponents` entry deliberately carries `resourceUsage` and NO
`sandboxSafe` key, matching what clap-wrapper's own CMake build-helper emits — claiming
untested sandbox-safety only buys a stricter hosting path that can fail silently, and
upstream clap-wrapper-rs dropped the flag from its bundler for the same reason. It also
carries `tags = ["Effects"]`: Logic files plugins into the Audio FX menu by those, and it
understands only a fixed vocabulary, so they are spelled out in `bundler.toml` rather than
derived from `CLAP_FEATURES`.

## clap-wrapper quirks (known, not worth patching)

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
- `auval` warns `Can Initialize Unit to un-supported num channels:InputChan:1, OutputChan:2`
  — it found the unit will also initialize to a 1-in/2-out configuration, which is not one of
  the declared `[2, 2]`/`[1, 1]` rows of our `AUChannelInfo` matrix. Harmless: it stems from
  clap-wrapper's `ValidFormat` not rejecting an input/output channel-count *mismatch* that the
  declared `AUChannelInfo` does not cover — the local patch enumerates every config into the
  matrix and accepts their formats (see the AudioUnit v2 section and `deps/PATCHES.md`), but
  does not reject an unlisted in/out pairing. Logic only ever instantiates matching in/out —
  mono→1-1, stereo→2-2 — so the un-declared config is never reached in practice. A known
  clap-wrapper validation gap, not a local bug; the deeper fix would tighten `ValidFormat` in
  `deps/clap-wrapper-rs` to reject in/out channel-count mismatches, but that touches the
  load-bearing AU validation path and buys nothing for a config no host ever requests.
- `SaveState`/`RestoreState` both early-return `kAudioUnitErr_Uninitialized` when
  `!IsInitialized()` (upstream issue #490; the guards are still present on every branch).
  If a Logic project ever fails to restore the detected offset, this is the first suspect —
  patch it directly in `deps/clap-wrapper-rs/external/clap-wrapper/`, which is a path
  dependency, so an edit there rebuilds. (Upstream's `CLAP_WRAPPER_CPP_DIR` hook would be the
  tidier route, but it landed after 0.3.1 and the vendored `build.rs` ignores the variable —
  see `deps/PATCHES.md`.)
- `auval` warns `Preset name is not retained in retrieved class data` — the AU's
  current-preset *name* does not survive a `SaveState`/`RestoreState` round trip, part of the
  same clap-wrapper AUv2 state handling as the issue-#490 guard above. Cosmetic here:
  ConjureAlign ships no factory presets, and the detected offset (the only state that matters)
  rides nih-plug's own persisted state, so no user-visible preset name is ever lost.
- `auval` warns `AU implements MusicDeviceMIDIEvent but is of type 'aufx'`. Cosmetic:
  clap-wrapper registers every AU type through `AUSDK_COMPONENT_ENTRY(AUMusicDeviceFactory,
  …)`. Validation still succeeds.
- Tail Time is not implemented, so `auval` warns that a recommended property is missing.
- AUv2 offline rendering is unsupported upstream.
- nih-plug's `ext_gui_can_resize`/`get_resize_hints` return `false`, so an AU host cannot
  resize the editor; the plugin-driven path (`ResizableWindow` → `gui_request_resize`)
  works, but in an AU host only since 1.4.1: the wrapper's `request_resize` reported failure
  and `nih_plug_egui`'s resize was broken on Retina screens (both patched, see
  `deps/PATCHES.md`).
- The AU wrapper is compiled into the one dylib behind all three bundles, so its ObjC
  classes register even in a CLAP/VST3 host. Loading two of our own bundles in one process
  can log an ObjC "implemented in both" warning — harmless, both copies are the same code
  from the same build (0.3.1 already suffixes the class names to reduce this).

## Identity and build rules (short form)

Word for word from the old CLAUDE.md.

- The AU identity `aufx` / `ALGN` / `CONJ` lives only in `bundler.toml`, and never changes: saved
  Logic projects depend on it. The same goes for the CLAP ID, the VST3 class ID `MJancsyConjrAlgn`
  and the Windows installer `AppId`.
- Never enable clap-wrapper's `vst3` feature. `default-features = false` is load-bearing.
