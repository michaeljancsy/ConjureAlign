//! The whole state model lives here.
//!
//! This plugin is headless: there is no GUI, hence no `ParamSetter`, hence NO
//! WAY for the plugin to write its own host-visible parameters. Analysis
//! results are therefore not parameters at all — they are `#[persist]` atomics
//! that the background task writes, the audio thread reads, and nih-plug
//! serializes into the DAW session.

use atomic_float::AtomicF32;
use nih_plug::prelude::*;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Upper bound of the Max Shift parameter; delay lines and capture buffers are
/// sized for this at initialization so parameter changes never allocate.
pub const MAX_SHIFT_MAX_MS: f32 = 200.0;
pub const MAX_SHIFT_MIN_MS: f32 = 10.0;
pub const TRIM_RANGE_MS: f32 = 10.0;
/// Longest selectable capture, seconds.
pub const CAPTURE_MAX_SECS: usize = 4;

#[derive(Enum, Debug, PartialEq, Clone, Copy)]
pub enum PolarityMode {
    /// Follow the polarity found by analysis.
    Auto,
    /// Never invert, regardless of analysis.
    Normal,
    /// Always invert.
    Inverted,
}

#[derive(Enum, Debug, PartialEq, Clone, Copy)]
pub enum CaptureTime {
    #[name = "1 s"]
    OneSecond,
    #[name = "2 s"]
    TwoSeconds,
    #[name = "4 s"]
    FourSeconds,
}

impl CaptureTime {
    pub fn seconds(self) -> usize {
        match self {
            CaptureTime::OneSecond => 1,
            CaptureTime::TwoSeconds => 2,
            CaptureTime::FourSeconds => 4,
        }
    }
}

#[derive(Params)]
pub struct AudioAlignParams {
    /// Rising edge starts a capture. The plugin cannot un-toggle this
    /// (headless); to re-analyze, the user toggles it off and on again.
    #[id = "capture"]
    pub capture: BoolParam,

    /// Off = apply no offset and no polarity flip, but keep reporting the same
    /// latency, so A/B comparison doesn't move the track around.
    #[id = "align_on"]
    pub align_on: BoolParam,

    #[id = "polarity"]
    pub polarity: EnumParam<PolarityMode>,

    /// Added on top of the detected offset. Positive delays the track further.
    #[id = "trim"]
    pub trim: FloatParam,

    /// Half-width of the correlation search window; also determines the
    /// reported latency. Non-automatable: changing it changes latency, which
    /// hosts handle by restarting playback.
    #[id = "max_shift"]
    pub max_shift: FloatParam,

    #[id = "capture_time"]
    pub capture_time: EnumParam<CaptureTime>,

    // --- Analysis results (not host parameters; see module docs). ---
    /// `t_ref − t_main` in milliseconds. Stored in ms, not samples, so a
    /// session reopened at a different sample rate stays aligned.
    #[persist = "detected-offset-ms"]
    pub detected_offset_ms: Arc<AtomicF32>,
    #[persist = "detected-polarity-inverted"]
    pub detected_polarity: Arc<AtomicBool>,
    /// Peak normalized correlation of the last accepted analysis, for display
    /// once there is a GUI. Not used for control.
    #[persist = "detected-confidence"]
    pub detected_confidence: Arc<AtomicF32>,
}

impl Default for AudioAlignParams {
    fn default() -> Self {
        Self {
            capture: BoolParam::new("Capture", false),
            align_on: BoolParam::new("Alignment On", true),
            polarity: EnumParam::new("Polarity", PolarityMode::Auto),
            trim: FloatParam::new(
                "Manual Trim",
                0.0,
                FloatRange::Linear {
                    min: -TRIM_RANGE_MS,
                    max: TRIM_RANGE_MS,
                },
            )
            .with_unit(" ms")
            .with_step_size(0.01),
            max_shift: FloatParam::new(
                "Max Shift",
                50.0,
                FloatRange::Linear {
                    min: MAX_SHIFT_MIN_MS,
                    max: MAX_SHIFT_MAX_MS,
                },
            )
            .with_unit(" ms")
            .with_step_size(1.0)
            .non_automatable(),
            capture_time: EnumParam::new("Capture Time", CaptureTime::TwoSeconds)
                .non_automatable(),
            detected_offset_ms: Arc::new(AtomicF32::new(0.0)),
            detected_polarity: Arc::new(AtomicBool::new(false)),
            detected_confidence: Arc::new(AtomicF32::new(0.0)),
        }
    }
}
