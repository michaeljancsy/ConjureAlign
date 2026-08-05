//! AudioAlign: time-aligns the main input to a sidechain reference.
//!
//! Headless plugin. The two load-bearing design decisions — the persisted-
//! atomics state model and the latency/PDC trick — are documented in
//! `params.rs` and `CLAUDE.md`.

pub mod analysis;
pub mod capture;
pub mod dsp;
pub mod params;

use nih_plug::prelude::*;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use capture::{CaptureState, PHASE_ANALYZING, PHASE_CAPTURING, PHASE_IDLE};
use dsp::delay::{AlignDelay, TapSpec};
use dsp::fractional::FIR_CENTER;
use params::{AudioAlignParams, PolarityMode, CAPTURE_MAX_SECS, MAX_SHIFT_MAX_MS, TRIM_RANGE_MS};

const CROSSFADE_SECONDS: f32 = 0.05;

pub struct AudioAlign {
    params: Arc<AudioAlignParams>,
    capture: Arc<CaptureState>,
    delay: AlignDelay,
    sample_rate: f32,
    prev_capture: bool,
    last_latency: u32,
}

pub enum Task {
    Analyze,
}

impl Default for AudioAlign {
    fn default() -> Self {
        Self {
            params: Arc::new(AudioAlignParams::default()),
            capture: Arc::new(CaptureState::new()),
            // Placeholder until `initialize()` knows the real sample rate.
            delay: AlignDelay::new(2, 1024, 64),
            sample_rate: 48_000.0,
            prev_capture: false,
            last_latency: 0,
        }
    }
}

impl AudioAlign {
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

impl Plugin for AudioAlign {
    const NAME: &'static str = "AudioAlign";
    const VENDOR: &'static str = "Michael Jancsy";
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

    fn task_executor(&mut self) -> TaskExecutor<Self> {
        let capture = self.capture.clone();
        let params = self.params.clone();
        Box::new(move |task| match task {
            Task::Analyze => {
                {
                    let data = capture.data.borrow();
                    match analysis::analyze(
                        &data.main[..data.filled],
                        &data.reference[..data.filled],
                        data.max_shift_samples,
                    ) {
                        Some(result) => {
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
                                "AudioAlign: detected offset {:.3} ms ({:.2} samples), \
                                 polarity {}, confidence {:.2}",
                                offset_ms,
                                result.offset_samples,
                                if result.inverted { "inverted" } else { "normal" },
                                result.confidence
                            );
                        }
                        None => {
                            nih_log!(
                                "AudioAlign: analysis rejected (silence or low correlation); \
                                 keeping previous offset"
                            );
                        }
                    }
                }
                capture.phase.store(PHASE_IDLE, Ordering::Release);
            }
        })
    }

    fn initialize(
        &mut self,
        audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        context: &mut impl InitContext<Self>,
    ) -> bool {
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
        self.capture
            .allocate(CAPTURE_MAX_SECS * self.sample_rate as usize, self.sample_rate);

        let latency = self.latency_samples();
        context.set_latency_samples(latency);
        self.last_latency = latency;

        // Jump straight to the restored session's target instead of fading in
        // from a default position.
        self.delay.retarget(self.current_target());
        self.delay.reset();

        // If the session was saved with Capture left on, don't fire a
        // spurious re-analysis on the first process() call.
        self.prev_capture = self.params.capture.value();

        true
    }

    fn reset(&mut self) {
        self.delay.reset();
        // Abort a capture in flight; a running analysis keeps its buffers and
        // finishes on its own.
        let _ = self.capture.phase.compare_exchange(
            PHASE_CAPTURING,
            PHASE_IDLE,
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let num_samples = buffer.samples();
        if num_samples == 0 {
            return ProcessStatus::Normal;
        }

        // NOTE: latency is deliberately NOT updated here. CLAP only permits
        // latency changes around activation; nih-plug's runtime notification
        // path violates that (clap-validator flags it). Max Shift edits apply
        // on the next initialize() — until then all math uses last_latency.

        // Rising edge on Capture starts a new capture, but only from Idle: a
        // toggle during a running capture/analysis is ignored.
        let capture_on = self.params.capture.value();
        if capture_on
            && !self.prev_capture
            && self.capture.phase.load(Ordering::Acquire) == PHASE_IDLE
        {
            {
                let mut data = self.capture.data.borrow_mut();
                let wanted = (self.params.capture_time.value().seconds() as f32
                    * self.sample_rate) as usize;
                data.target_len = wanted.min(data.main.len());
                data.filled = 0;
                data.max_shift_samples = self.reported_window_samples();
                data.sample_rate = self.sample_rate;
            }
            self.capture.phase.store(PHASE_CAPTURING, Ordering::Release);
        }
        self.prev_capture = capture_on;

        // Record the pre-delay input while capturing.
        if self.capture.phase.load(Ordering::Acquire) == PHASE_CAPTURING {
            let full = {
                let mut data_guard = self.capture.data.borrow_mut();
                // Reborrow as a plain &mut so disjoint field borrows work.
                let data = &mut *data_guard;
                let main_channels = buffer.as_slice_immutable();
                let to_copy = (data.target_len - data.filled).min(num_samples);
                for i in 0..to_copy {
                    let mono = main_channels.iter().map(|ch| ch[i]).sum::<f32>()
                        / main_channels.len() as f32;
                    data.main[data.filled + i] = mono;
                }
                match aux.inputs.first() {
                    Some(reference) => {
                        let ref_channels = reference.as_slice_immutable();
                        let ref_samples = reference.samples().min(to_copy);
                        for i in 0..ref_samples {
                            let mono = ref_channels.iter().map(|ch| ch[i]).sum::<f32>()
                                / ref_channels.len() as f32;
                            data.reference[data.filled + i] = mono;
                        }
                        for i in ref_samples..to_copy {
                            data.reference[data.filled + i] = 0.0;
                        }
                    }
                    // No sidechain connected: record silence; analysis will
                    // reject it rather than produce a bogus offset.
                    None => {
                        for i in 0..to_copy {
                            data.reference[data.filled + i] = 0.0;
                        }
                    }
                }
                data.filled += to_copy;
                data.filled >= data.target_len
            };
            if full {
                self.capture.phase.store(PHASE_ANALYZING, Ordering::Release);
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

impl ClapPlugin for AudioAlign {
    const CLAP_ID: &'static str = "com.michaeljancsy.audio-align";
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

impl Vst3Plugin for AudioAlign {
    const VST3_CLASS_ID: [u8; 16] = *b"MJancsyAudioAlgn";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Fx, Vst3SubCategory::Tools];
}

nih_export_clap!(AudioAlign);
nih_export_vst3!(AudioAlign);
