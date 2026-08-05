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

use atomic_refcell::AtomicRefCell;
use std::sync::atomic::AtomicU8;

pub const PHASE_IDLE: u8 = 0;
pub const PHASE_CAPTURING: u8 = 1;
pub const PHASE_ANALYZING: u8 = 2;

pub struct CaptureState {
    pub phase: AtomicU8,
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

impl CaptureState {
    pub fn new() -> Self {
        Self {
            phase: AtomicU8::new(PHASE_IDLE),
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
