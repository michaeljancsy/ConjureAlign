# Vendored `clap-wrapper-rs`

`deps/clap-wrapper-rs` is [blepfx/clap-wrapper-rs](https://github.com/blepfx/clap-wrapper-rs)
**0.3.1**, exactly as published to crates.io, with two deliberate differences:

1. `external/vst3sdk/` is deleted. We never compile it — the crate's `vst3` feature is off
   (see `Cargo.toml`: enabling it would export `GetPluginFactory`, `bundleEntry` and
   `bundleExit`, the three symbols `nih_export_vst3!` already owns), and `build.rs` only
   references those paths inside `build_vst3()`, which never runs. Dropping it takes the
   vendored tree from 5.5 MB to 2.0 MB. **If the `vst3` feature is ever turned on, the SDK
   has to come back.**
2. The AUv2 mono patch, below.

It is a path dependency rather than a crates.io one because upstream's
`CLAP_WRAPPER_CPP_DIR` escape hatch — which exists precisely so you can build against your
own clap-wrapper checkout — landed *after* 0.3.1 was published. The released `build.rs`
hardcodes `./external/clap-wrapper/...`, so there is no way to inject a patched tree without
either vendoring or taking an unreleased git rev. Vendoring keeps us on the exact C++ we
validated.

## The patch: let the AU reach every channel layout

**Problem.** AudioAlign publishes two `AUDIO_IO_LAYOUTS` — Stereo (2 in / 2 out plus a
2-channel "Reference" sidechain) and Mono (1/1 plus a 1-channel sidechain). nih-plug exposes
these through CLAP's `audio-ports-config` extension, with the layout index as the config id.

clap-wrapper's AUv2 wrapper never looks at that extension. It derives everything from
`audio-ports`, which reports only the *currently selected* config, and it never calls
`audio-ports-config::select`. So the AU was pinned to layout 0 and advertised channel
capabilities `[2, 2]` and nothing else.

That is not a cosmetic limitation: **Logic filters its Audio FX menu by what a plugin can
actually instantiate as, so a stereo-only AudioAlign simply does not appear on a mono
track.** That was the reported symptom.

**Fix**, all in existing files (no files added or removed):

| File | Change |
| --- | --- |
| `src/clap_proxy.h` | Hold `_audioports_config` in `ClapPluginExtensions`. |
| `src/clap_proxy.cpp` | Fetch it with `getExtension(..., CLAP_EXT_AUDIO_PORTS_CONFIG)`. |
| `src/detail/auv2/auv2_base_classes.h` | Declare `WrapAsAUV2::selectAudioPortsConfigForMain`. |
| `src/wrapasauv2.cpp` | `SupportedNumChannels` builds `AUChannelInfo` from *every* config; `ValidFormat` also accepts a main-bus width belonging to any config; `ChangeStreamFormat` and `Initialize` select the matching config. |

Two anonymous-namespace helpers (`forEachAudioPortsConfig`, `mainChannelsFor`) sit above
`ValidFormat` because that is the first use site — C++ needs them declared before all three
users.

`audio-ports-config::select` is only legal while the plugin is deactivated. Both call sites
satisfy that: AU stream-format negotiation happens before `Initialize()`, and the
`Initialize()` call is placed before `activateCLAP()`. After selecting, `setupAudioBusses()`
is re-run so the AU elements agree with the plugin's new port layout — this is what makes
the sidechain bus follow the main bus down to 1 channel.

Everything degrades to upstream behaviour when the extension is absent: `SupportedNumChannels`
falls through to the original `audio-ports` path, `ValidFormat`'s extra clause matches
nothing, and `selectAudioPortsConfigForMain` returns false immediately.

## Verifying the patch after a change

```bash
cargo xtask bundle audio_align --release   # see CLAUDE.md for the worktree caveat
# install to ~/Library/Audio/Plug-Ins/Components/, then:
killall -9 AudioComponentRegistrar && auval -v aufx ALGN CONJ
```

`auval` must report **`Reported Channel Capabilities (explicit): [2, 2]  [1, 1]`** and mark
both `1-1` and `2-2` in its channel-handling grid. `auval` only ever *renders* the default
(stereo) config, so it cannot prove the mono path works — the harness at
`tests/au_mono_host.c` does. Build and run it with both widths:

```bash
clang -O1 -framework AudioToolbox -framework CoreFoundation -o /tmp/au_mono_host tests/au_mono_host.c
/tmp/au_mono_host 1 && /tmp/au_mono_host 2
```

The load-bearing assertion is that **in the mono run, input bus 1 ("Reference") reports 1
channel**. If it still reports 2, `select()` did not reach the plugin: the AU would be
handing mono buffers to a plugin that believes it is stereo, which is an out-of-bounds read
on the audio thread rather than a visible failure.

## Upstream

Worth revisiting whenever clap-wrapper-rs publishes a release newer than 0.3.1: if it ships
`CLAP_WRAPPER_CPP_DIR`, this could become a much smaller patch applied to an unmodified
crate. Upstream clap-wrapper has no issue tracking the missing `audio-ports-config` support
as of this writing; the related AUv2 channel-count bug is PR #496, which is merged only to
the `next` branch and is a different defect (it is harmless here, because AUSDK's default
element format is stereo and matches our layout-0 ports).
