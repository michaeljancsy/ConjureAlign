//! The capture gate: decides, per sample, whether a gated capture records.
//!
//! Alignment needs correlated signal on BOTH inputs, so the gate opens only
//! when both envelopes exceed the open threshold. Peak followers with
//! instant attack (the recorded chunk starts on the first loud sample, so
//! onset transients are kept) and an exponential release; a hysteresis gap
//! plus a hold time keep the gate from chattering, which also keeps recorded
//! chunks long — good both for splice bookkeeping (few seams) and for the
//! analysis, whose per-seam guard regions cost signal.
//!
//! Allocation-free; every method is safe on the audio thread.

/// Envelope release time constant.
pub const RELEASE_SECONDS: f32 = 0.08;
/// The close threshold sits this far below the open threshold, so a signal
/// hovering at the threshold can't toggle the gate per-block.
pub const HYSTERESIS_DB: f32 = 6.0;
/// Once open, the gate stays open this long after the signal drops below the
/// close threshold, bridging short gaps between phrases.
pub const HOLD_SECONDS: f32 = 0.25;

/// What the gate is doing, for the editor's status display. `*_below`
/// compare against the OPEN threshold — the UI wants "which input is holding
/// the gate shut", not the hysteresis internals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GateStatus {
    pub open: bool,
    pub main_below: bool,
    pub ref_below: bool,
}

pub struct CaptureGate {
    env_main: f32,
    env_ref: f32,
    /// Per-sample envelope decay factor for [`RELEASE_SECONDS`].
    decay: f32,
    open_thresh: f32,
    close_thresh: f32,
    hold_samples: u32,
    hold_left: u32,
    open: bool,
    /// Consecutive samples [`step`](Self::step) has returned `false` — how
    /// long the gate has currently been closed. The capture uses this to
    /// auto-finish after sustained silence (a short clip ends, nothing more
    /// is coming).
    closed_streak: u32,
}

impl CaptureGate {
    /// `open_threshold_amp` is a linear amplitude (the caller converts from
    /// dB), keeping this module free of any plugin-framework helpers.
    pub fn new(sample_rate: f32, open_threshold_amp: f32) -> Self {
        let sr = sample_rate.max(1.0);
        Self {
            env_main: 0.0,
            env_ref: 0.0,
            decay: (-1.0 / (RELEASE_SECONDS * sr)).exp(),
            open_thresh: open_threshold_amp,
            close_thresh: open_threshold_amp * 10f32.powf(-HYSTERESIS_DB / 20.0),
            hold_samples: (HOLD_SECONDS * sr) as u32,
            hold_left: 0,
            open: false,
            closed_streak: 0,
        }
    }

    /// Advances the envelopes by one sample of each mono-summed input and
    /// returns whether this sample should be recorded.
    pub fn step(&mut self, main: f32, reference: f32) -> bool {
        // A ±Inf sample must not enter an envelope: `Inf * decay` is still
        // `Inf`, so a single one would pin the envelope (and the gate) open
        // forever. Treat any non-finite sample as absent and just decay —
        // which is exactly what `max` already does for NaN (it returns the
        // other operand) — so the envelopes themselves stay finite for any
        // input.
        let main_abs = main.abs();
        let ref_abs = reference.abs();
        self.env_main = if main_abs.is_finite() {
            main_abs.max(self.env_main * self.decay)
        } else {
            self.env_main * self.decay
        };
        self.env_ref = if ref_abs.is_finite() {
            ref_abs.max(self.env_ref * self.decay)
        } else {
            self.env_ref * self.decay
        };
        if self.open {
            if self.env_main >= self.close_thresh && self.env_ref >= self.close_thresh {
                self.hold_left = self.hold_samples;
            } else if self.hold_left > 0 {
                self.hold_left -= 1;
            } else {
                self.open = false;
            }
        }
        // Re-opening (or the very first opening) needs the full threshold on
        // both inputs; instant attack means this can trigger on the same
        // sample the signal arrives.
        if !self.open && self.env_main >= self.open_thresh && self.env_ref >= self.open_thresh {
            self.open = true;
            self.hold_left = self.hold_samples;
        }
        if self.open {
            self.closed_streak = 0;
        } else {
            self.closed_streak = self.closed_streak.saturating_add(1);
        }
        self.open
    }

    /// How many consecutive samples the gate has currently been closed
    /// (0 while open).
    pub fn closed_streak(&self) -> u32 {
        self.closed_streak
    }

    pub fn status(&self) -> GateStatus {
        GateStatus {
            open: self.open,
            main_below: self.env_main < self.open_thresh,
            ref_below: self.env_ref < self.open_thresh,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;
    const THRESH: f32 = 1e-3; // −60 dBFS

    fn run(gate: &mut CaptureGate, main: f32, reference: f32, n: usize) -> Vec<bool> {
        (0..n).map(|_| gate.step(main, reference)).collect()
    }

    #[test]
    fn opens_only_when_both_inputs_live() {
        let mut gate = CaptureGate::new(SR, THRESH);
        assert!(run(&mut gate, 0.5, 0.5, 100).iter().all(|&r| r));

        let mut gate = CaptureGate::new(SR, THRESH);
        assert!(run(&mut gate, 0.5, 0.0, 48_000).iter().all(|&r| !r));
        let st = gate.status();
        assert!(!st.open && !st.main_below && st.ref_below);

        let mut gate = CaptureGate::new(SR, THRESH);
        assert!(run(&mut gate, 0.0, 0.5, 48_000).iter().all(|&r| !r));
        let st = gate.status();
        assert!(!st.open && st.main_below && !st.ref_below);
    }

    #[test]
    fn opens_on_first_loud_sample() {
        let mut gate = CaptureGate::new(SR, THRESH);
        assert!(run(&mut gate, 0.0, 0.0, 1000).iter().all(|&r| !r));
        // Instant attack: the very first above-threshold sample records.
        assert!(gate.step(0.5, 0.5));
    }

    #[test]
    fn hold_bridges_short_gaps() {
        let mut gate = CaptureGate::new(SR, THRESH);
        run(&mut gate, 0.5, 0.5, 5000);
        // 8000-sample gap < 12000-sample hold: record stays true throughout.
        assert!(run(&mut gate, 0.0, 0.0, 8000).iter().all(|&r| r));
        assert!(run(&mut gate, 0.5, 0.5, 100).iter().all(|&r| r));
        // No splice would be recorded: record never went false.
    }

    #[test]
    fn closes_after_release_plus_hold_on_sustained_silence() {
        let mut gate = CaptureGate::new(SR, THRESH);
        run(&mut gate, 0.5, 0.5, 5000);
        let silence = run(&mut gate, 0.0, 0.0, 60_000);
        // Still open through the hold time...
        assert!(silence[..10_000].iter().all(|&r| r));
        // ...but closed well before a second of silence: the envelope decays
        // from 0.5 to the close threshold (~5e-4) in ln(1000)·τ ≈ 26.5 k
        // samples, plus 12 k hold.
        assert!(!silence[59_999]);
        // And it stays closed.
        assert!(!gate.step(0.0, 0.0));
    }

    #[test]
    fn closed_streak_tracks_silence_runs() {
        let mut gate = CaptureGate::new(SR, THRESH);
        // Closed from the start: silence counts up.
        run(&mut gate, 0.0, 0.0, 100);
        assert_eq!(gate.closed_streak(), 100);
        // Opening resets it, and it stays 0 while open — including through
        // the hold time bridging a short gap.
        run(&mut gate, 0.5, 0.5, 1000);
        assert_eq!(gate.closed_streak(), 0);
        run(&mut gate, 0.0, 0.0, 8000); // < hold: still recording
        assert_eq!(gate.closed_streak(), 0);
        // Sustained silence: after the gate closes the streak grows again.
        run(&mut gate, 0.0, 0.0, 52_000);
        assert!(gate.closed_streak() > 0);
        let before = gate.closed_streak();
        run(&mut gate, 0.0, 0.0, 500);
        assert_eq!(gate.closed_streak(), before + 500);
        // And reopening resets it once more.
        gate.step(0.5, 0.5);
        assert_eq!(gate.closed_streak(), 0);
    }

    #[test]
    fn hysteresis_band_never_toggles() {
        // 7e-4 sits between the close (~5e-4) and open (1e-3) thresholds.
        let mid = 7e-4f32;

        // From closed, the band must NOT open the gate.
        let mut gate = CaptureGate::new(SR, THRESH);
        assert!(run(&mut gate, mid, mid, 48_000).iter().all(|&r| !r));

        // From open, the band must never close it (hold keeps refreshing).
        let mut gate = CaptureGate::new(SR, THRESH);
        run(&mut gate, 0.5, 0.5, 1000);
        assert!(run(&mut gate, mid, mid, 100_000).iter().all(|&r| r));
    }

    #[test]
    fn inf_sample_does_not_pin_the_gate_open() {
        let mut gate = CaptureGate::new(SR, THRESH);
        // Open on real signal, then hit both inputs with one ±Inf sample.
        run(&mut gate, 0.5, 0.5, 5000);
        gate.step(f32::INFINITY, f32::NEG_INFINITY);
        // A pinned envelope would hold the gate open forever; instead the
        // same release+hold budget as plain silence must close it...
        let silence = run(&mut gate, 0.0, 0.0, 60_000);
        assert!(silence[..10_000].iter().all(|&r| r));
        assert!(!silence[59_999]);
        // ...and the closed streak keeps growing, so auto-finish stays
        // viable.
        let before = gate.closed_streak();
        assert!(before > 0);
        run(&mut gate, 0.0, 0.0, 500);
        assert_eq!(gate.closed_streak(), before + 500);

        // A non-finite sample is treated as absent, so from closed it does
        // not open the gate either.
        let mut gate = CaptureGate::new(SR, THRESH);
        assert!(!gate.step(f32::INFINITY, f32::INFINITY));
        assert_eq!(gate.closed_streak(), 1);
    }

    #[test]
    fn one_sided_inf_status_recovers() {
        let mut gate = CaptureGate::new(SR, THRESH);
        // Live on both sides, then a burst of Inf on main only.
        run(&mut gate, 0.5, 0.5, 1000);
        run(&mut gate, f32::INFINITY, 0.5, 10);
        // After sustained real silence main's below-threshold reading must
        // come back — a pinned envelope would report that input live (and
        // "waiting for signal" would blame the wrong side) forever.
        run(&mut gate, 0.0, 0.0, 60_000);
        let st = gate.status();
        assert!(!st.open && st.main_below && st.ref_below);
    }

    #[test]
    fn nan_input_still_decays_like_silence() {
        // NaN was already self-healing (`max` drops the NaN operand); the
        // finiteness guard must keep that: a NaN run behaves exactly like
        // silence.
        let mut gate = CaptureGate::new(SR, THRESH);
        run(&mut gate, 0.5, 0.5, 5000);
        let nans = run(&mut gate, f32::NAN, f32::NAN, 60_000);
        // Hold still bridges the start of the run...
        assert!(nans[..10_000].iter().all(|&r| r));
        // ...and release+hold still closes the gate on schedule.
        assert!(!nans[59_999]);
        // And NaN alone never opens a closed gate.
        let mut gate = CaptureGate::new(SR, THRESH);
        assert!(run(&mut gate, f32::NAN, f32::NAN, 48_000).iter().all(|&r| !r));
    }
}
