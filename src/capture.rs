//! Capture buffers shared between the audio thread and the analysis task.
//!
//! Exclusive access is enforced by the phase machine, not by locking:
//!
//! ```text
//! Idle ──(capture param rising edge, audio thread)──▶ Capturing
//! Capturing ──(buffers full, audio thread)──▶ Analyzing
//! Analyzing ──(analysis done, background thread)──▶ Idle
//! ```
//!
//! The audio thread only borrows `data` in Idle/Capturing; the background task
//! only borrows it in Analyzing. The phases never overlap, so the
//! `AtomicRefCell` borrows can never contend — a failed borrow would be a bug
//! in the state machine, and `AtomicRefCell` panics loudly in that case.
//!
//! The GUI thread is deliberately NOT part of this scheme: the editor only
//! ever holds a [`CaptureHandle`], which exposes the atomics but cannot reach
//! `data`. Waveforms reach the GUI via the snapshot the background task
//! publishes (see `shared.rs`), never by borrowing these buffers.

use atomic_refcell::AtomicRefCell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;

pub const PHASE_IDLE: u8 = 0;
pub const PHASE_CAPTURING: u8 = 1;
pub const PHASE_ANALYZING: u8 = 2;

pub struct CaptureState {
    pub phase: AtomicU8,
    /// GUI capture request. `process()` consumes (swaps to false) this every
    /// block and treats a `true` like a rising edge on the `capture` param;
    /// a request that races a non-idle phase is simply dropped.
    pub request: AtomicBool,
    /// Capture progress in samples, for display only (Relaxed, approximate).
    pub progress: AtomicU32,
    /// Capture target length in samples, for display only.
    pub target: AtomicU32,
    pub data: AtomicRefCell<CaptureData>,
}

pub struct CaptureData {
    /// Mono-summed main input, pre-delay. Fixed capacity after `allocate()`.
    pub main: Vec<f32>,
    /// Mono-summed sidechain (reference) input.
    pub reference: Vec<f32>,
    /// Valid samples in both buffers.
    pub filled: usize,
    /// Capture stops once `filled` reaches this.
    pub target_len: usize,
    /// Search window, snapshotted when the capture started.
    pub max_shift_samples: usize,
    pub sample_rate: f32,
}

/// The ONLY view of `CaptureState` the editor receives. It can read the phase
/// and progress and request a capture, but cannot reach `data` — so the
/// borrow discipline documented above is enforced by construction.
#[derive(Clone)]
pub struct CaptureHandle(Arc<CaptureState>);

impl CaptureHandle {
    pub fn phase(&self) -> u8 {
        self.0.phase.load(Ordering::Acquire)
    }

    /// `(filled, target)` in samples; approximate, for display only.
    pub fn progress(&self) -> (u32, u32) {
        (
            self.0.progress.load(Ordering::Relaxed),
            self.0.target.load(Ordering::Relaxed),
        )
    }

    pub fn request_capture(&self) {
        self.0.request.store(true, Ordering::Release);
    }
}

impl CaptureState {
    pub fn handle(self: &Arc<Self>) -> CaptureHandle {
        CaptureHandle(self.clone())
    }

    pub fn new() -> Self {
        Self {
            phase: AtomicU8::new(PHASE_IDLE),
            request: AtomicBool::new(false),
            progress: AtomicU32::new(0),
            target: AtomicU32::new(0),
            data: AtomicRefCell::new(CaptureData {
                main: Vec::new(),
                reference: Vec::new(),
                filled: 0,
                target_len: 0,
                max_shift_samples: 0,
                sample_rate: 0.0,
            }),
        }
    }

    /// Sizes the buffers for the longest possible capture. Called from
    /// `initialize()` (allocation is allowed there), never from `process()`.
    pub fn allocate(&self, max_len: usize, sample_rate: f32) {
        let mut data = self.data.borrow_mut();
        data.main.clear();
        data.main.resize(max_len, 0.0);
        data.reference.clear();
        data.reference.resize(max_len, 0.0);
        data.filled = 0;
        data.target_len = 0;
        data.sample_rate = sample_rate;
    }
}

impl Default for CaptureState {
    fn default() -> Self {
        Self::new()
    }
}
