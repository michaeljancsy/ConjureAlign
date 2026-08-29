//! State shared with the GUI thread.
//!
//! The editor must never touch `CaptureState::data` (see `capture.rs` for the
//! borrow discipline). Instead, the background analysis task publishes an
//! immutable [`AnalysisSnapshot`] here, and `initialize()` mirrors the two
//! values the GUI needs for its own alignment math. The audio thread never
//! touches the `Mutex`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::analysis::{AnalysisResult, RejectReason};

/// Everything the last analysis produced, frozen for display. Built on the
/// background thread at the end of `Task::Analyze`; immutable afterwards.
/// Holds full raw copies of the captures (≤ ~6 MB at 192 kHz / 4 s) so the
/// GUI can decimate per zoom level. Persisted into the DAW session via the
/// `analysis-snapshot` field on the Params struct (see `snapshot_persist.rs`
/// for the wire format and its size cost), so a reopened project shows the
/// graphs behind its restored detected values.
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
    /// Positions in the (spliced, signal-time) capture buffers where the
    /// gate re-opened — each starts a new contiguous chunk. The raw captures
    /// above are NOT guard-zeroed; the analysis works on its own copies.
    pub splices: Vec<usize>,
    /// Welch spectra for the Spectrum panel (~64 KB at 48 kHz, ~262 KB at
    /// 192 kHz). `None` when the capture was rejected before analysis ran
    /// (mirrors `corr.is_empty()`) or no full Welch segment fits.
    pub spectrum: Option<crate::spectrum::SpectrumData>,
    pub outcome: Result<AnalysisResult, RejectReason>,
}

/// Home of the snapshot `Mutex`. One `Arc<SnapshotCell>` is shared between
/// [`GuiShared`] (the background task publishes through it, the editor reads
/// it every frame) and the `#[persist = "analysis-snapshot"]` field on the
/// Params struct — which is what lets a host save pick up whatever the last
/// analysis published without any extra hand-off, and a state load land where
/// the editor already looks.
#[derive(Default)]
pub struct SnapshotCell(Mutex<Option<Arc<AnalysisSnapshot>>>);

impl SnapshotCell {
    /// Clone out the current snapshot. Poison-tolerant: a panic in the
    /// analysis task while publishing must not turn every subsequent GUI
    /// frame (or host save) into a panic of its own.
    pub fn get(&self) -> Option<Arc<AnalysisSnapshot>> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Replace the snapshot: the background task on publish, a state load on
    /// restore (`None` when the saved session had no capture).
    pub fn store(&self, snapshot: Option<Arc<AnalysisSnapshot>>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = snapshot;
    }
}

/// Cross-thread channel between the plugin and its editor. The audio thread
/// touches only the atomics (written in `initialize()`); the `Mutex` is
/// shared only by the background task (one pointer store per analysis), the
/// GUI (one clone of the `Arc` per frame), and the host's state save/load on
/// the main thread.
pub struct GuiShared {
    /// Latest analysis snapshot; the same cell the Params struct persists.
    pub snapshot: Arc<SnapshotCell>,
    /// Mirror of `reported_window_samples()` (high 32 bits) packed with the
    /// sample rate's bits (low 32). The two are one logical value, and a
    /// single atomic keeps a GUI frame from ever pairing a new window with a
    /// stale sample rate across a rate change. Written from `initialize()`;
    /// a window of 0 means "not activated yet". The GUI must derive its
    /// clamping from this, never from the Max Shift knob, to stay in sync
    /// with the latency actually reported to the host.
    window_and_rate: AtomicU64,
}

impl GuiShared {
    /// `snapshot` is the Params struct's persisted cell — the plugin passes
    /// `params.snapshot.clone()` so both ends are one object.
    pub fn new(snapshot: Arc<SnapshotCell>) -> Self {
        Self {
            snapshot,
            window_and_rate: AtomicU64::new(48_000.0f32.to_bits() as u64),
        }
    }

    /// Called from `initialize()` only.
    pub fn set_window(&self, window_samples: u32, sample_rate: f32) {
        let packed = ((window_samples as u64) << 32) | sample_rate.to_bits() as u64;
        self.window_and_rate.store(packed, Ordering::Relaxed);
    }

    /// `(window_samples, sample_rate)` as one consistent pair.
    pub fn window(&self) -> (u32, f32) {
        let packed = self.window_and_rate.load(Ordering::Relaxed);
        ((packed >> 32) as u32, f32::from_bits(packed as u32))
    }
}

/// A standalone channel with its own (un-persisted) cell — the previews and
/// tests that never load host state.
impl Default for GuiShared {
    fn default() -> Self {
        Self::new(Arc::default())
    }
}
