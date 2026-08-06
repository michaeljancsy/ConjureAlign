# AudioAlign

A free VST3 / CLAP / AudioUnit plugin that automatically time-aligns two recordings of the same source
captured by different microphones — e.g., a close mic and a room mic on one guitar amp —
eliminating the phase cancellation and comb filtering caused by the distance between them.
Sub-sample precision, automatic polarity detection, and support for *negative* shifts
(moving a track earlier) via latency compensation.

The plugin window shows both captured waveforms overlaid (the main signal slides as the
alignment changes) and the cross-correlation curve with a marker at the detected peak plus a
live marker that tracks the applied shift — drag either display left/right to adjust Manual
Trim, hold Shift for fine control, scroll/pinch to pan and zoom.

## How to use it

1. Put **AudioAlign on the track you want to shift** (usually the more distant mic).
2. Route the **reference track into the plugin's sidechain** ("Reference") input:
   - **REAPER**: open the plugin's pin connector, add inputs 3/4, send the reference track there.
   - **Ableton Live** (VST3): choose the reference track in the device header's sidechain chooser.
   - **Bitwig** (CLAP): same, via the device's sidechain chooser.
   - **Logic Pro** (AU): open the **Side Chain** menu at the top right of the plugin
     window's header and pick the reference track. If the track isn't listed, send it to a
     bus and pick the bus instead.
3. **Play a loud, representative section** of the song.
4. While it plays, **click "Capture"** in the plugin window (or toggle the "Capture"
   parameter on, e.g. from host automation). The plugin records a few seconds of both
   signals, measures the offset by cross-correlation, and glides click-free onto the
   corrected alignment. The waveforms, correlation curve, detected offset, polarity, and
   confidence appear in the window, and the result is stored with your session.
5. To re-analyze, click Capture again (or toggle the parameter off and on).

### Parameters

| Parameter | What it does |
|---|---|
| Capture | Rising edge starts a capture + analysis. Toggle off/on to re-run. |
| Alignment On | Bypass the correction without changing latency, for honest A/B comparison. |
| Polarity | Auto (use detected), Normal, or Inverted. |
| Manual Trim | ±10 ms added on top of the detected offset. |
| Max Shift | Search window and reported latency (10–200 ms). Takes effect on next plugin activation (e.g., session reload). |
| Capture Time | 1/2/4 s of audio analyzed per capture. |

Captures made during silence, or where the two signals don't actually correlate, are rejected
and the previous alignment is kept.

**A note on null tests**: if you verify alignment by inverting and summing, whole-sample
offsets null below −80 dB, but *fractional* offsets only null to around −20…−30 dB broadband —
the residual is entirely above ~19 kHz, where fractional delay is mathematically impossible for
any plugin. In the audible band the null is −70 dB or deeper (measured in
`tests/null_depth.rs`). A lowpass at ~19 kHz on the null bus shows the true audible-band depth.

The Audio Unit build is produced by [clap-wrapper](https://github.com/free-audio/clap-wrapper),
which re-exports the CLAP plugin behind an AU entry point. It carries a local patch so that
both the mono and stereo layouts are reachable from AU hosts — without it Logic hides the
plugin on mono tracks. See `deps/PATCHES.md`.

## Building

Stable Rust required.

```bash
cargo xtask bundle audio_align --release
```

Bundles appear in `target/bundled/`; on macOS copy `AudioAlign.clap` to
`~/Library/Audio/Plug-Ins/CLAP/`, `AudioAlign.vst3` to `~/Library/Audio/Plug-Ins/VST3/`, and
`AudioAlign.component` to `~/Library/Audio/Plug-Ins/Components/`. After replacing an
installed `.component`, run `killall -9 AudioComponentRegistrar` and restart the host, or
macOS will keep serving the cached registration.

Run the DSP test suite with `cargo test --release`. To eyeball the GUI panels without a DAW,
`cargo run --example gui_preview --features gui-preview` renders them with synthetic data to
`gui_preview.png` / `gui_preview_zoom.png`.

## License

GPL-3.0-or-later.
