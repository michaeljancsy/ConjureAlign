//! State shared with the GUI thread.
//!
//! The editor must never touch `CaptureState::data` (see `capture.rs` for the
//! borrow discipline). Instead, the background analysis task publishes an
//! immutable [`AnalysisSnapshot`] here, and `initialize()` mirrors the two
//! values the GUI needs for its own alignment math. The audio thread never
//! touches the `Mutex`.

use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};

use atomic_float::AtomicF32;

use crate::analysis::{AnalysisResult, RejectReason};

/// Everything the last analysis produced, frozen for display. Built on the
/// background thread at the end of `Task::Analyze`; immutable afterwards.
/// Holds full raw copies of the captures (≤ ~6 MB at 192 kHz / 4 s) so the
/// GUI can decimate per zoom level. Deliberately NOT persisted: after a
/// session reload the GUI shows the restored detected values but no
/// waveforms until the next capture.
pub struct AnalysisSnapshot {
    /// Mono-summed main input as captured (pre-delay).
    pub main: Vec<f32>,
    /// Mono-summed sidechain reference as captured.
    pub reference: Vec<f32>,
    /// Sample rate the capture was recorded at.
    pub sample_rate: f32,
    /// Search half-window at capture time (== the reported window then).
    pub max_shift_samples: usize,
    /// Normalized cross-correlation per integer lag; index `i` holds lag
    /// `i − max_shift_samples`. Empty when analysis was rejected before the
    /// FFT ran (too short / silence).
    pub corr: Vec<f32>,
    pub outcome: Result<AnalysisResult, RejectReason>,
}

/// Cross-thread channel between the plugin and its editor. The audio thread
/// touches only the atomics (written in `initialize()`); the `Mutex` is
/// shared only by the background task (one pointer store per analysis) and
/// the GUI (one clone of the `Arc` per frame).
pub struct GuiShared {
    /// Latest analysis snapshot.
    pub snapshot: Mutex<Option<Arc<AnalysisSnapshot>>>,
    /// Mirror of `reported_window_samples()` — the active clamp window. The
    /// GUI must derive its clamping from this, never from the Max Shift knob,
    /// to stay in sync with the latency actually reported to the host.
    pub window_samples: AtomicU32,
    /// Current sample rate, for converting the window to milliseconds.
    pub sample_rate: AtomicF32,
}

impl Default for GuiShared {
    fn default() -> Self {
        Self {
            snapshot: Mutex::new(None),
            window_samples: AtomicU32::new(0),
            sample_rate: AtomicF32::new(48_000.0),
        }
    }
}
