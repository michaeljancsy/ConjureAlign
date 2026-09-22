//! Capture buffers shared between the audio thread and the analysis task.
//!
//! Exclusive access is enforced by the phase machine, not by locking:
//!
//! ```text
//! Idle ──(start edge, audio thread)──▶ Armed ──(gate opens, audio)──▶ Capturing
//! Armed ──(stop w/ nothing recorded, audio; cancel, GUI)──▶ Idle
//! Capturing ──(stop / buffers full, audio)──▶ Analyzing ──(done, background)──▶ Idle
//! ```
//!
//! Armed means "capture session running, nothing recorded yet" (`filled == 0`
//! exactly while Armed); once Capturing, momentary gate closes pause writing
//! but do NOT return to Armed — the gate state is separate display info. The
//! audio thread only borrows `data` in Idle/Armed/Capturing; the background
//! task only borrows it in Analyzing, so in steady state the `AtomicRefCell`
//! borrows never contend. The one deliberate exception is `initialize()`
//! reclaiming a wedged Analyzing phase (a lost or slow task): it bumps
//! [`CaptureState::generation`] and probes with `try_borrow_mut`, and the
//! task side takes its borrow with `try_borrow` and re-checks the generation
//! inside it — so a collision resolves as "stale task exits" instead of the
//! `AtomicRefCell` panic a plain borrow would raise across the FFI boundary.
//!
//! The GUI thread is deliberately NOT part of this scheme: the editor only
//! ever holds a [`CaptureHandle`], which exposes the atomics but cannot reach
//! `data`. Waveforms reach the GUI via the snapshot the background task
//! publishes (see `shared.rs`), never by borrowing these buffers.

use atomic_refcell::AtomicRefCell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

pub const PHASE_IDLE: u8 = 0;
pub const PHASE_CAPTURING: u8 = 1;
pub const PHASE_ANALYZING: u8 = 2;
pub const PHASE_ARMED: u8 = 3;
/// Display-only pseudo-phase returned by [`CaptureHandle::display_phase`] for
/// an Idle phase with an unconsumed [`CaptureState::request`]. Never stored
/// in `phase`: the audio thread alone leaves Idle.
pub const PHASE_PENDING: u8 = 4;

/// Bits of [`CaptureState::gate_state`] — the display protocol between the
/// audio thread (writer, once per block) and the editor.
pub const GATE_OPEN: u8 = 1;
pub const GATE_MAIN_QUIET: u8 = 2;
pub const GATE_REF_QUIET: u8 = 4;

/// How many gate re-openings (seams) a capture can track. The gate's hold
/// time (≥ 250 ms) bounds real seams to ~16 per 4 s of signal; when the list
/// is full the audio thread records continuously instead of splicing
/// untracked (always safe, see `process()`).
pub const MAX_SPLICES: usize = 64;

pub struct CaptureState {
    pub phase: AtomicU8,
    /// Bumped by `initialize()` when it reclaims a wedged Analyzing phase
    /// before reallocating the buffers. Every queued analysis task carries
    /// the value current at dispatch and exits without touching `data` when
    /// it no longer matches — see the module docs.
    pub generation: AtomicU64,
    /// GUI capture request. `process()` consumes (swaps to false) this every
    /// block and treats a `true` like a rising edge on the `capture` param;
    /// a request that races a non-idle phase is simply dropped.
    ///
    /// It deliberately survives `reset()` and `initialize()`, and `reset()`
    /// turns an Armed capture (nothing recorded) back into one: the editor
    /// renders an unconsumed request as armed ([`CaptureHandle::display_phase`]
    /// → [`PHASE_PENDING`]), so it is standing, cancelable intent, not a
    /// stale click. Two kinds of host make that matter. Logic Pro does not
    /// run a stopped audio track's `process()` in a freshly opened session
    /// until the track has played once (input-monitoring or record-enabled
    /// tracks always process; after one playback it kept processing for at
    /// least a minute), so a click made before Play sat invisible until
    /// playback — the "Capture needs two clicks" report (#42). And a
    /// VST3/CLAP host that suspends processing while idle (Cubase-style)
    /// reaches its first block after Play through `setProcessing(true)` /
    /// `start_processing`, from which nih-plug calls `reset()` — clearing the
    /// request there would drop the capture the editor had just promised.
    pub request: AtomicBool,
    /// GUI stop request ("analyze what was recorded"); consumed by
    /// `process()` every block like `request`. Unlike cancel it cannot be
    /// honored from the GUI thread — dispatching the analysis task needs the
    /// process context — so if the host stops processing it stays pending
    /// until playback resumes (Cancel is the escape hatch).
    pub stop_request: AtomicBool,
    /// [`GATE_OPEN`] | [`GATE_MAIN_QUIET`] | [`GATE_REF_QUIET`], written once
    /// per block while Armed/Capturing (Relaxed, display only).
    pub gate_state: AtomicU8,
    /// Accumulated (gated) samples, for display only (Relaxed, approximate).
    pub progress: AtomicU32,
    /// Capture capacity in samples, for display only.
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
    /// Positions where the gate re-opened after a silent stretch was spliced
    /// out — each starts a new contiguous chunk. Capacity reserved in
    /// `allocate()`; the audio thread pushes only below capacity.
    pub splices: Vec<usize>,
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

    /// The phase as the editor should show it: the real phase, except that
    /// an Idle phase with an unconsumed request reads as [`PHASE_PENDING`] —
    /// armed, waiting for the host to process audio (see
    /// [`CaptureState::request`]). Read it ONCE per frame and pass it down:
    /// the audio thread can consume the request and leave Idle between two
    /// loads, and a strip, button and overlay drawn from different loads
    /// would disagree within one frame.
    pub fn display_phase(&self) -> u8 {
        let phase = self.0.phase.load(Ordering::Acquire);
        if phase == PHASE_IDLE && self.0.request.load(Ordering::Acquire) {
            PHASE_PENDING
        } else {
            phase
        }
    }

    /// Requests a stop-and-analyze of the running capture; consumed by the
    /// audio thread (see `stop_request`).
    pub fn request_stop(&self) {
        self.0.stop_request.store(true, Ordering::Release);
    }

    /// Latest gate display bits ([`GATE_OPEN`] etc.).
    pub fn gate_state(&self) -> u8 {
        self.0.gate_state.load(Ordering::Relaxed)
    }

    /// Aborts (discards) a capture in progress. GUI escape hatch: if the
    /// host stops calling `process()` mid-capture, the phase machine freezes
    /// until processing resumes — this lets the user back out. Safe from the
    /// GUI thread: the audio thread's borrows never span blocks, and a
    /// capture that completes concurrently simply proceeds to analysis (the
    /// CASes just fail). ARMED must be tried FIRST: the phase only ever
    /// moves Armed→Capturing, so against that forward motion one of the two
    /// CASes always lands — the reverse order could miss both.
    pub fn cancel_capture(&self) {
        self.0.request.store(false, Ordering::Relaxed);
        self.0.stop_request.store(false, Ordering::Relaxed);
        for phase in [PHASE_ARMED, PHASE_CAPTURING] {
            let _ = self.0.phase.compare_exchange(
                phase,
                PHASE_IDLE,
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
        }
    }
}

impl CaptureState {
    pub fn handle(self: &Arc<Self>) -> CaptureHandle {
        CaptureHandle(self.clone())
    }

    pub fn new() -> Self {
        Self {
            phase: AtomicU8::new(PHASE_IDLE),
            generation: AtomicU64::new(0),
            request: AtomicBool::new(false),
            stop_request: AtomicBool::new(false),
            gate_state: AtomicU8::new(0),
            progress: AtomicU32::new(0),
            target: AtomicU32::new(0),
            data: AtomicRefCell::new(CaptureData {
                main: Vec::new(),
                reference: Vec::new(),
                filled: 0,
                target_len: 0,
                splices: Vec::new(),
                max_shift_samples: 0,
                sample_rate: 0.0,
            }),
        }
    }

    /// Sizes the buffers for the longest possible capture. Called from
    /// `initialize()` (allocation is allowed there), never from `process()`.
    pub fn allocate(&self, max_len: usize, sample_rate: f32) {
        Self::allocate_locked(&mut self.data.borrow_mut(), max_len, sample_rate);
    }

    /// [`Self::allocate`] through a borrow the caller already holds — the
    /// reclaim path in `initialize()` must not release its probe borrow
    /// before resizing (a raced stale task's `try_borrow` could land in the
    /// gap and collide with a fresh `borrow_mut`).
    pub fn allocate_locked(data: &mut CaptureData, max_len: usize, sample_rate: f32) {
        data.main.clear();
        data.main.resize(max_len, 0.0);
        data.reference.clear();
        data.reference.resize(max_len, 0.0);
        data.filled = 0;
        data.target_len = 0;
        data.splices.clear();
        data.splices.reserve(MAX_SPLICES);
        data.sample_rate = sample_rate;
    }
}

impl Default for CaptureState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract the editor's pending display rests on.
    #[test]
    fn a_request_reads_as_pending_until_consumed_or_cancelled() {
        let state = Arc::new(CaptureState::new());
        let handle = state.handle();
        assert_eq!(handle.display_phase(), PHASE_IDLE);

        handle.request_capture();
        assert_eq!(handle.display_phase(), PHASE_PENDING);
        assert_eq!(handle.phase(), PHASE_IDLE, "the GUI never leaves Idle itself");

        // Stop/Cancel while pending drop the request and move nothing else.
        handle.cancel_capture();
        assert_eq!(handle.display_phase(), PHASE_IDLE);
        assert!(!state.request.load(Ordering::Relaxed));

        // Once the audio thread has left Idle the real phase wins, whatever
        // the flag says.
        handle.request_capture();
        state.phase.store(PHASE_ARMED, Ordering::Relaxed);
        assert_eq!(handle.display_phase(), PHASE_ARMED);
        state.request.store(false, Ordering::Relaxed);
        assert_eq!(handle.display_phase(), PHASE_ARMED);
    }
}
