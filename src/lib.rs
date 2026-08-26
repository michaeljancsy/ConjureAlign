//! ConjureAlign: time-aligns the main input to a sidechain reference.
//!
//! The three load-bearing design decisions — the persisted-atomics state
//! model, the latency/PDC trick, and the GUI threading rules (the editor
//! never touches the capture buffers) — are documented in `params.rs`,
//! `shared.rs`, and `CLAUDE.md`.

pub mod analysis;
pub mod analytics;
pub mod capture;
pub mod crash;
pub mod dsp;
pub mod editor;
pub mod params;
pub mod shared;
pub mod spectrum;

use nih_plug::prelude::*;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use capture::{
    CaptureState, GATE_MAIN_QUIET, GATE_OPEN, GATE_REF_QUIET, PHASE_ANALYZING, PHASE_ARMED,
    PHASE_CAPTURING, PHASE_IDLE,
};
use dsp::delay::{AlignDelay, TapSpec};
use dsp::fractional::FIR_CENTER;
use dsp::gate::CaptureGate;
use params::{ConjureAlignParams, PolarityMode, CAPTURE_MAX_SECS, MAX_SHIFT_MAX_MS, TRIM_RANGE_MS};
use shared::{AnalysisSnapshot, GuiShared};

const CROSSFADE_SECONDS: f32 = 0.05;

/// A capture that has already recorded signal auto-finishes (stops and
/// analyzes) once the gate has been closed this long — the short-clip
/// workflow: play the clip once and the analysis fires by itself instead of
/// pausing forever. With the gate's release + hold in front of it, this
/// amounts to ≈2.8 s of real-world silence. Armed never times out; with
/// nothing recorded it keeps waiting for signal.
const CAPTURE_AUTO_FINISH_SECONDS: f32 = 2.0;

pub struct ConjureAlign {
    params: Arc<ConjureAlignParams>,
    capture: Arc<CaptureState>,
    shared: Arc<GuiShared>,
    delay: AlignDelay,
    /// Capture gate; rebuilt (allocation-free) each time a capture arms.
    gate: CaptureGate,
    sample_rate: f32,
    prev_capture: bool,
    /// Whether the previous gated sample was recorded — a false→true
    /// transition with samples already written marks a splice seam.
    prev_record: bool,
    last_latency: u32,
    /// Opt-in usage analytics. Inert until the user consents; never touched
    /// from `process()`.
    analytics: Arc<analytics::AnalyticsHandle>,
    /// Opt-in crash reporting, on the same consent answer as `analytics`.
    /// `process()` takes one of its scope guards, which is a thread-local
    /// counter and nothing else — see the rules in `crash.rs`.
    crash: Arc<crash::CrashHandle>,
}

pub enum Task {
    Analyze,
}

/// Packs the gate status into the display bitfield the editor reads
/// ([`GATE_OPEN`] | [`GATE_MAIN_QUIET`] | [`GATE_REF_QUIET`]).
fn gate_bits(gate: &CaptureGate) -> u8 {
    let st = gate.status();
    (if st.open { GATE_OPEN } else { 0 })
        | (if st.main_below { GATE_MAIN_QUIET } else { 0 })
        | (if st.ref_below { GATE_REF_QUIET } else { 0 })
}

impl Default for ConjureAlign {
    fn default() -> Self {
        Self {
            params: Arc::new(ConjureAlignParams::default()),
            capture: Arc::new(CaptureState::new()),
            shared: Arc::new(GuiShared::default()),
            // Placeholders until `initialize()` knows the real sample rate.
            delay: AlignDelay::new(2, 1024, 64),
            gate: CaptureGate::new(48_000.0, 1e-3),
            sample_rate: 48_000.0,
            prev_capture: false,
            prev_record: false,
            last_latency: 0,
            analytics: Arc::new(analytics::AnalyticsHandle::new()),
            crash: Arc::new(crash::CrashHandle::new()),
        }
    }
}

impl ConjureAlign {
    /// Reported latency: the PDC trick, computed from the Max Shift parameter.
    /// Only ever *reported* during `initialize()` — CLAP allows latency changes
    /// only around activation, so a Max Shift edit takes effect the next time
    /// the host (re)activates the plugin. See CLAUDE.md.
    fn latency_samples(&self) -> u32 {
        let max_shift_samples =
            (self.params.max_shift.value() / 1000.0 * self.sample_rate).round() as usize;
        (max_shift_samples + FIR_CENTER) as u32
    }

    /// The usable shift window: everything must clamp to the latency the host
    /// was actually told about, not the current knob position, or the applied
    /// shift would no longer equal `D_total − reported_latency`.
    fn reported_window_samples(&self) -> usize {
        (self.last_latency as usize).saturating_sub(FIR_CENTER)
    }

    /// Where the delay line should sit right now, given params and the last
    /// analysis result.
    fn current_target(&self) -> TapSpec {
        let latency = self.last_latency as f64;
        if !self.params.align_on.value() {
            // Honest A/B: identical latency, no shift, no polarity flip.
            return TapSpec {
                delay_samples: latency,
                inverted: false,
            };
        }
        let offset_ms = self.params.detected_offset_ms.load(Ordering::Relaxed)
            + self.params.trim.value();
        let max_shift = self.reported_window_samples() as f64;
        let offset = (offset_ms as f64 / 1000.0 * self.sample_rate as f64)
            .clamp(-max_shift, max_shift);
        let inverted = match self.params.polarity.value() {
            PolarityMode::Auto => self.params.detected_polarity.load(Ordering::Relaxed),
            PolarityMode::Normal => false,
            PolarityMode::Inverted => true,
        };
        TapSpec {
            delay_samples: latency + offset,
            inverted,
        }
    }
}

impl Plugin for ConjureAlign {
    const NAME: &'static str = "ConjureAlign";
    const VENDOR: &'static str = "ConjureDSP";
    const URL: &'static str = env!("CARGO_PKG_HOMEPAGE");
    const EMAIL: &'static str = "michaeljancsy@gmail.com";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(2),
            main_output_channels: NonZeroU32::new(2),
            aux_input_ports: &[new_nonzero_u32(2)],
            names: PortNames {
                layout: Some("Stereo"),
                aux_inputs: &["Reference"],
                ..PortNames::const_default()
            },
            ..AudioIOLayout::const_default()
        },
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(1),
            main_output_channels: NonZeroU32::new(1),
            aux_input_ports: &[new_nonzero_u32(1)],
            names: PortNames {
                layout: Some("Mono"),
                aux_inputs: &["Reference"],
                ..PortNames::const_default()
            },
            ..AudioIOLayout::const_default()
        },
    ];

    const MIDI_INPUT: MidiConfig = MidiConfig::None;
    const MIDI_OUTPUT: MidiConfig = MidiConfig::None;
    const SAMPLE_ACCURATE_AUTOMATION: bool = true;

    type SysExMessage = ();
    type BackgroundTask = Task;

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        editor::create(
            self.params.clone(),
            self.shared.clone(),
            self.capture.handle(),
            self.crash.clone(),
        )
    }

    fn task_executor(&mut self) -> TaskExecutor<Self> {
        let capture = self.capture.clone();
        let params = self.params.clone();
        let shared = self.shared.clone();
        let analytics = self.analytics.clone();
        Box::new(move |task| match task {
            Task::Analyze => {
                // Everything below runs on nih-plug's shared `bg-worker`
                // thread, which serves every plugin instance in the process.
                // The guard is what tells the panic hook that a panic on this
                // thread right now is ours and not another plugin's.
                let _scope = crash::scope();
                // Built inside the borrow below, sent after it drops:
                // serializing and queueing an event is cheap, but it has no
                // business happening while the capture buffers are borrowed.
                let event;
                // Everything that reads `data` stays inside this scope; the
                // snapshot mutex is deliberately touched only AFTER the
                // borrow drops. The GUI locks that mutex every frame, and
                // blocking here while holding the borrow would extend the
                // window in which `initialize()`'s `allocate()` could
                // collide with it.
                let snapshot = {
                    let data = capture.data.borrow();
                    let report = analysis::analyze_spliced(
                        &data.main[..data.filled],
                        &data.reference[..data.filled],
                        data.max_shift_samples,
                        &data.splices,
                    );
                    match report.outcome {
                        Ok(result) => {
                            let offset_ms =
                                (result.offset_samples / data.sample_rate as f64 * 1000.0) as f32;
                            params
                                .detected_offset_ms
                                .store(offset_ms, Ordering::Relaxed);
                            params
                                .detected_polarity
                                .store(result.inverted, Ordering::Relaxed);
                            params
                                .detected_confidence
                                .store(result.confidence, Ordering::Relaxed);
                            nih_log!(
                                "ConjureAlign: detected offset {:.3} ms ({:.2} samples), \
                                 polarity {}, confidence {:.2}",
                                offset_ms,
                                result.offset_samples,
                                if result.inverted { "inverted" } else { "normal" },
                                result.confidence
                            );
                            event = analytics::AnalyticsEvent::CaptureCompleted {
                                confidence: result.confidence,
                                offset_ms,
                            };
                        }
                        Err(reason) => {
                            nih_log!(
                                "ConjureAlign: analysis rejected ({:?}); keeping previous offset",
                                reason
                            );
                            event = analytics::AnalyticsEvent::CaptureRejected { reason };
                        }
                    }
                    // Freeze everything the GUI needs. Copying here — on the
                    // background thread, inside the Analyzing phase where this
                    // task owns the borrow — is the only path waveform data
                    // ever takes to the editor.
                    // Honor the panel's persisted FFT-size selection so a
                    // fresh snapshot already matches it; the GUI only
                    // re-estimates when the selector changes afterwards.
                    let nfft_override = match params.spectrum_nfft.load(Ordering::Relaxed) {
                        0 => None,
                        n => Some(n as usize),
                    };
                    let spectrum = spectrum::welch_for_capture(
                        &data.main[..data.filled],
                        &data.reference[..data.filled],
                        data.sample_rate,
                        &report,
                        &data.splices,
                        nfft_override,
                    );
                    Arc::new(AnalysisSnapshot {
                        main: data.main[..data.filled].to_vec(),
                        reference: data.reference[..data.filled].to_vec(),
                        sample_rate: data.sample_rate,
                        max_shift_samples: report.max_shift_samples,
                        corr: report.corr_curve,
                        splices: data.splices.clone(),
                        spectrum,
                        outcome: report.outcome,
                    })
                };
                *shared.snapshot.lock().unwrap() = Some(snapshot);
                capture.phase.store(PHASE_IDLE, Ordering::Release);
                analytics.track(event);
            }
        })
    }

    fn initialize(
        &mut self,
        audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        context: &mut impl InitContext<Self>,
    ) -> bool {
        let _scope = crash::scope();

        self.sample_rate = buffer_config.sample_rate;
        let channels = audio_io_layout
            .main_input_channels
            .map(|c| c.get())
            .unwrap_or(2) as usize;

        // Size everything for the parameter maxima so no later change ever
        // allocates on the audio thread.
        let max_shift_max =
            (MAX_SHIFT_MAX_MS / 1000.0 * self.sample_rate).ceil() as usize;
        let trim_max = (TRIM_RANGE_MS / 1000.0 * self.sample_rate).ceil() as usize;
        let max_delay = 2 * max_shift_max + trim_max + FIR_CENTER + 1;
        let fade_len = (CROSSFADE_SECONDS * self.sample_rate) as usize;
        self.delay = AlignDelay::new(channels, max_delay, fade_len);

        // An in-flight analysis holds a borrow of the capture buffers on the
        // background thread, and `allocate()`'s `borrow_mut()` would panic
        // across the FFI boundary if it collided (hosts re-run initialize()
        // after state loads with no task drain). Analysis takes tens of
        // milliseconds — wait it out, bounded in case something is wedged.
        let mut wedged = true;
        for _ in 0..500 {
            if self.capture.phase.load(Ordering::Acquire) != PHASE_ANALYZING {
                wedged = false;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        if wedged {
            // The spin above is the guard against `allocate()`'s `borrow_mut()`
            // colliding with the background task's borrow. Reaching the end of
            // it still holding PHASE_ANALYZING means the guard has failed and
            // the `AtomicRefCell` panic — across the FFI boundary — is the next
            // thing that happens. It is invisible in the field otherwise.
            nih_log!("ConjureAlign: analysis still running after 500 ms; proceeding anyway");
            crash::report_issue("initialize(): capture buffers still borrowed after 500 ms");
        }
        // Drop any stale GUI capture/stop request and progress so a click
        // made while the host wasn't processing can't fire a surprise
        // capture on the first block, and the editor can't show a stale
        // percentage or gate state.
        self.capture.request.store(false, Ordering::Relaxed);
        self.capture.stop_request.store(false, Ordering::Relaxed);
        self.capture.gate_state.store(0, Ordering::Relaxed);
        self.capture.progress.store(0, Ordering::Relaxed);
        self.capture.target.store(0, Ordering::Relaxed);
        self.capture
            .allocate(CAPTURE_MAX_SECS * self.sample_rate as usize, self.sample_rate);

        let latency = self.latency_samples();
        context.set_latency_samples(latency);
        self.last_latency = latency;

        // Mirror the values the GUI needs for its alignment math (one packed
        // atomic, so no frame ever pairs a new window with a stale rate).
        // Like every other clamp, this derives from the *reported* latency.
        self.shared
            .set_window(self.reported_window_samples() as u32, self.sample_rate);

        // Jump straight to the restored session's target instead of fading in
        // from a default position.
        self.delay.retarget(self.current_target());
        self.delay.reset();

        // If the session was saved with Capture left on, don't fire a
        // spurious re-analysis on the first process() call.
        self.prev_capture = self.params.capture.value();

        // Idempotent per instance, so the host re-initializing (state loads,
        // sample-rate changes) doesn't inflate the session count.
        self.analytics.note_session(self.sample_rate);
        // Arms or tears down crash reporting to match the stored answer. The
        // editor re-syncs every frame, which is what picks up a consent change
        // made while the plugin is already running.
        self.crash.sync_consent();
        // AU reports as CLAP: clap-wrapper translates AU calls into calls on
        // our own `clap_entry`, so nih-plug never sees an AU-specific api.
        self.crash
            .set_host_context(&context.plugin_api().to_string(), self.sample_rate);

        true
    }

    fn reset(&mut self) {
        self.delay.reset();
        // Abort a capture in flight and drop any queued GUI requests:
        // reset() fires when processing resumes, and a click made while the
        // host wasn't processing must not start a surprise capture now. A
        // running analysis keeps its buffers and finishes on its own.
        // ARMED first — the phase only moves Armed→Capturing, so this order
        // can't miss (see CaptureHandle::cancel_capture).
        self.capture.request.store(false, Ordering::Relaxed);
        self.capture.stop_request.store(false, Ordering::Relaxed);
        for phase in [PHASE_ARMED, PHASE_CAPTURING] {
            let _ = self.capture.phase.compare_exchange(
                phase,
                PHASE_IDLE,
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
        }
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        // Marks this thread as ours for the duration of the block, so that a
        // panic here — the `AtomicRefCell` borrows below being the likeliest —
        // is reported as ours rather than left invisible. A thread-local
        // increment and decrement: no allocation, no syscall, nothing that
        // weakens `assert_process_allocs`.
        let _scope = crash::scope();

        let num_samples = buffer.samples();
        if num_samples == 0 {
            return ProcessStatus::Normal;
        }

        // NOTE: latency is deliberately NOT updated here. CLAP only permits
        // latency changes around activation; nih-plug's runtime notification
        // path violates that (clap-validator flags it). Max Shift edits apply
        // on the next initialize() — until then all math uses last_latency.

        // Consume the GUI requests and the Capture param edges every block —
        // even while running, and cleared by reset()/initialize(), so a
        // stale click can't fire long after the fact. prev_capture updates
        // unconditionally: a toggle that lands while non-idle must not
        // re-fire later.
        let gui_request = self.capture.request.swap(false, Ordering::AcqRel);
        let gui_stop = self.capture.stop_request.swap(false, Ordering::AcqRel);
        let capture_on = self.params.capture.value();
        let rising = (capture_on && !self.prev_capture) || gui_request;
        let falling = !capture_on && self.prev_capture;
        self.prev_capture = capture_on;

        // Stop BEFORE start, so a stop and a fresh capture request arriving
        // in one block resolve in click order. Needs no borrow: Armed means
        // nothing was recorded (back to Idle, previous snapshot kept), and
        // both transitions are CASes so a GUI cancel that already landed
        // wins — the analysis task fires only if OUR transition succeeds.
        if gui_stop || falling {
            let _ = self.capture.phase.compare_exchange(
                PHASE_ARMED,
                PHASE_IDLE,
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
            if self
                .capture
                .phase
                .compare_exchange(
                    PHASE_CAPTURING,
                    PHASE_ANALYZING,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                context.execute_background(Task::Analyze);
            }
        }

        // A rising edge arms a gated capture, but only from Idle: a toggle
        // during a running capture or analysis is ignored.
        if rising && self.capture.phase.load(Ordering::Acquire) == PHASE_IDLE {
            {
                let mut data = self.capture.data.borrow_mut();
                // The whole buffer: the gate makes this "4 s of signal", not
                // 4 s of wall time, and a Stop can end the capture earlier.
                data.target_len = data.main.len();
                data.filled = 0;
                data.splices.clear();
                data.max_shift_samples = self.reported_window_samples();
                data.sample_rate = self.sample_rate;
                self.capture
                    .target
                    .store(data.target_len as u32, Ordering::Relaxed);
            }
            self.gate = CaptureGate::new(
                self.sample_rate,
                nih_plug::util::db_to_gain(self.params.gate_threshold.value()),
            );
            self.prev_record = false;
            self.capture
                .gate_state
                .store(gate_bits(&self.gate), Ordering::Relaxed);
            self.capture.progress.store(0, Ordering::Relaxed);
            // Blind store is fine here: only the audio thread leaves Idle.
            self.capture.phase.store(PHASE_ARMED, Ordering::Release);
        }

        // Record the pre-delay input through the gate while armed/capturing.
        let phase_now = self.capture.phase.load(Ordering::Acquire);
        if phase_now == PHASE_ARMED || phase_now == PHASE_CAPTURING {
            let (filled, target_len) = {
                let mut data_guard = self.capture.data.borrow_mut();
                // Reborrow as a plain &mut so disjoint field borrows work.
                let data = &mut *data_guard;
                let main_channels = buffer.as_slice_immutable();
                let ref_buf = aux.inputs.first();
                let ref_channels = ref_buf.map(|b| b.as_slice_immutable());
                // An absent or short sidechain feeds 0.0 into the gate, so
                // it never opens and the editor shows "ref quiet" — the live
                // replacement for the old post-hoc Silence rejection.
                let ref_len = ref_buf.map(|b| b.samples()).unwrap_or(0);
                let cap = data.splices.capacity();
                for i in 0..num_samples {
                    if data.filled >= data.target_len {
                        break;
                    }
                    let mono_main = main_channels.iter().map(|ch| ch[i]).sum::<f32>()
                        / main_channels.len() as f32;
                    let mono_ref = match &ref_channels {
                        Some(chs) if i < ref_len => {
                            chs.iter().map(|ch| ch[i]).sum::<f32>() / chs.len() as f32
                        }
                        _ => 0.0,
                    };
                    // Once the splice list is full, record continuously (an
                    // untracked seam could corrupt the analysis; extra
                    // silence cannot). The force-open term keys on
                    // `== capacity` with gate.step still run: a gap can only
                    // START below capacity, and the push terminating it
                    // finds len == cap−1 — so no gap ever goes untracked.
                    let record =
                        self.gate.step(mono_main, mono_ref) || data.splices.len() == cap;
                    if record {
                        if !self.prev_record && data.filled > 0 && data.splices.len() < cap {
                            // Within capacity: push never allocates.
                            data.splices.push(data.filled);
                        }
                        data.main[data.filled] = mono_main;
                        data.reference[data.filled] = mono_ref;
                        data.filled += 1;
                    }
                    self.prev_record = record;
                }
                (data.filled, data.target_len)
            };
            self.capture
                .gate_state
                .store(gate_bits(&self.gate), Ordering::Relaxed);
            self.capture
                .progress
                .store(filled as u32, Ordering::Relaxed);
            // Promote Armed→Capturing once something was recorded. A CAS,
            // not a store: a GUI cancel that landed mid-block must not be
            // overwritten (a blind store would resurrect the capture).
            let mut phase_now = phase_now;
            if phase_now == PHASE_ARMED && filled > 0 {
                phase_now = match self.capture.phase.compare_exchange(
                    PHASE_ARMED,
                    PHASE_CAPTURING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => PHASE_CAPTURING,
                    // Cancelled mid-block; the samples already written are
                    // harmless (filled resets at the next capture start).
                    Err(actual) => actual,
                };
            }
            let auto_finish = self.gate.closed_streak()
                >= (CAPTURE_AUTO_FINISH_SECONDS * self.sample_rate) as u32;
            if phase_now == PHASE_CAPTURING
                && (filled >= target_len || auto_finish)
                && self
                    .capture
                    .phase
                    .compare_exchange(
                        PHASE_CAPTURING,
                        PHASE_ANALYZING,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                    .is_ok()
            {
                context.execute_background(Task::Analyze);
            }
        }

        // Apply the correction. `retarget` is a no-op unless the target
        // actually changed; changes crossfade click-free inside the line.
        self.delay.retarget(self.current_target());
        self.delay.process(buffer.as_slice());

        ProcessStatus::Normal
    }
}

impl ClapPlugin for ConjureAlign {
    const CLAP_ID: &'static str = "com.michaeljancsy.conjure-align";
    // The AudioUnit build repeats this string in `bundler.toml`; keep the two in
    // sync. xtask can't read it from here without depending on this crate, which
    // would mean building egui just to run the bundler.
    const CLAP_DESCRIPTION: Option<&'static str> =
        Some("Time-aligns a mic signal to a sidechain reference");
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::AudioEffect,
        ClapFeature::Utility,
        ClapFeature::Stereo,
        ClapFeature::Mono,
    ];
}

impl Vst3Plugin for ConjureAlign {
    const VST3_CLASS_ID: [u8; 16] = *b"MJancsyConjrAlgn";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Fx, Vst3SubCategory::Tools];
}

nih_export_clap!(ConjureAlign);
nih_export_vst3!(ConjureAlign);

// AudioUnit v2, macOS only. clap-wrapper wraps the `clap_entry` above behind
// `GetPluginFactoryAUV2`, an AudioComponentFactoryFunction — so this one dylib
// now carries three entry points and gets copied into three bundles. Nothing
// AU-specific can live here: nih-plug's `get_factory` only answers the standard
// CLAP plugin factory, never `clap.plugin-factory-info-as-auv2.draft0`, so the
// four-character type/subtype/manufacturer codes are only expressible in the
// `.component`'s Info.plist, which xtask generates from `bundler.toml`.
#[cfg(target_os = "macos")]
clap_wrapper::export_auv2!();
