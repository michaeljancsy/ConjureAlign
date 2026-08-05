# AudioAlign

A free VST3/CLAP plugin that automatically time-aligns two recordings of the same source
captured by different microphones — e.g., a close mic and a room mic on one guitar amp —
eliminating the phase cancellation and comb filtering caused by the distance between them.
Sub-sample precision, automatic polarity detection, and support for *negative* shifts
(moving a track earlier) via latency compensation.

## How to use it

1. Put **AudioAlign on the track you want to shift** (usually the more distant mic).
2. Route the **reference track into the plugin's sidechain** ("Reference") input:
   - **REAPER**: open the plugin's pin connector, add inputs 3/4, send the reference track there.
   - **Ableton Live** (VST3): choose the reference track in the device header's sidechain chooser.
   - **Bitwig** (CLAP): same, via the device's sidechain chooser.
3. **Play a loud, representative section** of the song.
4. While it plays, **toggle the "Capture" parameter on**. The plugin records a few seconds of
   both signals, measures the offset by cross-correlation, and glides click-free onto the
   corrected alignment. The result is stored with your session.
5. To re-analyze, toggle Capture **off and then on** again.

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

**Logic Pro is not supported yet** — Logic only loads Audio Units. An AU build via
[clap-wrapper](https://github.com/free-audio/clap-wrapper) is planned.

## Building

Stable Rust required.

```bash
cargo xtask bundle audio_align --release
```

Bundles appear in `target/bundled/`; copy `AudioAlign.clap` to `~/Library/Audio/Plug-Ins/CLAP/`
and `AudioAlign.vst3` to `~/Library/Audio/Plug-Ins/VST3/` (macOS).

Run the DSP test suite with `cargo test --release`.

## License

GPL-3.0-or-later.
