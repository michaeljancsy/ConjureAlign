//! The `analysis-snapshot` persist field: DAW-session persistence for
//! [`AnalysisSnapshot`].
//!
//! nih-plug serializes each `#[persist]` field to JSON, so the sample buffers
//! ride as base64 of their raw little-endian `f32` bytes — ~1.33× raw, versus
//! ~10 bytes per sample as a JSON number array. Full fidelity on purpose: a
//! reopened session draws bit-exactly what the analysis saw. The cost is
//! state size — ≈2.3 MB per instance at 48 kHz (≈9 MB at 192 kHz), rewritten
//! on every host save/autosave — accepted as an explicit product decision.
//!
//! Decoding trusts nothing. Hosts and validators feed plugins arbitrary
//! state (pluginval's strictness-10 pass fuzzes it outright), so every
//! length is capped and cross-checked *before* the allocation it sizes, and
//! any failure degrades to "no snapshot" — never a panic, never an unbounded
//! allocation, and never a lost load for the fields that matter (nih-plug
//! restores each persist field independently, so the detected offset
//! survives a corrupt snapshot).
//!
//! Compatibility falls out of nih-plug's field handling: an old session
//! without the key restores no snapshot, an old plugin ignores the unknown
//! key, and a future format bumps [`BLOB_VERSION`] — an unknown version is
//! discarded here rather than misread.

use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use nih_plug::nih_log;
use nih_plug::params::persist::PersistentField;
use realfft::num_complex::Complex;
use serde::{Deserialize, Serialize};

use crate::analysis::{AnalysisResult, RejectReason};
use crate::capture::MAX_SPLICES;
use crate::params::{CAPTURE_MAX_SECS, MAX_SHIFT_MAX_MS};
use crate::shared::{AnalysisSnapshot, SnapshotCell};
use crate::spectrum::{SpectrumData, MIN_NFFT};

pub const BLOB_VERSION: u32 = 1;

/// Decode-side allocation caps. 384 kHz is far above any rate a supported
/// host runs at; the caps only need to bound hostile input, not to be tight.
const MAX_SAMPLE_RATE: f32 = 384_000.0;
const MAX_WAVE_SAMPLES: usize = CAPTURE_MAX_SECS * MAX_SAMPLE_RATE as usize;
const MAX_SHIFT_SAMPLES_CAP: usize =
    (MAX_SHIFT_MAX_MS / 1000.0 * MAX_SAMPLE_RATE) as usize;
/// `pick_nfft` at the rate cap: `(384_000 / 6).next_power_of_two()`.
const MAX_NFFT: usize = 65_536;

/// What actually lands in the session state, as one JSON object (nih-plug
/// wraps it in a string). `snap: None` round-trips a pre-capture save.
#[derive(Serialize, Deserialize)]
pub struct SnapshotBlob {
    v: u32,
    snap: Option<BlobSnapshot>,
}

#[derive(Serialize, Deserialize)]
struct BlobSnapshot {
    sample_rate: f32,
    max_shift_samples: u64,
    /// Raw little-endian `f32` bytes, base64.
    main: String,
    reference: String,
    corr: String,
    splices: Vec<u64>,
    spectrum: Option<BlobSpectrum>,
    outcome: BlobOutcome,
}

#[derive(Serialize, Deserialize)]
struct BlobSpectrum {
    nfft: u64,
    prealign_samples: i32,
    segments: u32,
    pmm: String,
    prr: String,
    /// Interleaved re/im little-endian `f32` bytes, base64.
    pmr: String,
}

/// `Result<AnalysisResult, RejectReason>` flattened into one enum so the
/// wire format does not depend on those types' own (underived) serde shape.
#[derive(Serialize, Deserialize)]
enum BlobOutcome {
    Ok {
        offset_samples: f64,
        inverted: bool,
        confidence: f32,
    },
    TooShort,
    Silence,
    NonFinite,
    LowConfidence,
}

fn f32s_to_b64(v: &[f32]) -> String {
    let mut bytes = Vec::with_capacity(v.len() * 4);
    for x in v {
        bytes.extend_from_slice(&x.to_le_bytes());
    }
    B64.encode(bytes)
}

fn complexes_to_b64(v: &[Complex<f32>]) -> String {
    let mut bytes = Vec::with_capacity(v.len() * 8);
    for c in v {
        bytes.extend_from_slice(&c.re.to_le_bytes());
        bytes.extend_from_slice(&c.im.to_le_bytes());
    }
    B64.encode(bytes)
}

/// The cap is enforced on the *encoded* length first — an oversized field is
/// rejected before any decode buffer exists.
fn b64_to_bytes(s: &str, max_bytes: usize) -> Option<Vec<u8>> {
    // 4 base64 chars per 3 bytes, plus padding slack.
    if s.len() > max_bytes.div_ceil(3) * 4 + 4 {
        return None;
    }
    B64.decode(s).ok()
}

fn b64_to_f32s(s: &str, max_elems: usize) -> Option<Vec<f32>> {
    let bytes = b64_to_bytes(s, max_elems * 4)?;
    if bytes.len() % 4 != 0 || bytes.len() / 4 > max_elems {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
            .collect(),
    )
}

fn b64_to_complexes(s: &str, max_elems: usize) -> Option<Vec<Complex<f32>>> {
    let bytes = b64_to_bytes(s, max_elems * 8)?;
    if bytes.len() % 8 != 0 || bytes.len() / 8 > max_elems {
        return None;
    }
    Some(
        bytes
            .chunks_exact(8)
            .map(|c| Complex {
                re: f32::from_le_bytes(c[0..4].try_into().unwrap()),
                im: f32::from_le_bytes(c[4..8].try_into().unwrap()),
            })
            .collect(),
    )
}

/// Deterministic (fixed alphabet, ordered struct fields), which is what keeps
/// save→save byte-identical for the hosts and validators that diff state.
pub fn encode(snapshot: Option<&AnalysisSnapshot>) -> SnapshotBlob {
    SnapshotBlob {
        v: BLOB_VERSION,
        snap: snapshot.map(|s| BlobSnapshot {
            sample_rate: s.sample_rate,
            max_shift_samples: s.max_shift_samples as u64,
            main: f32s_to_b64(&s.main),
            reference: f32s_to_b64(&s.reference),
            corr: f32s_to_b64(&s.corr),
            splices: s.splices.iter().map(|&p| p as u64).collect(),
            spectrum: s.spectrum.as_ref().map(|sp| BlobSpectrum {
                nfft: sp.nfft as u64,
                prealign_samples: sp.prealign_samples,
                segments: sp.segments,
                pmm: f32s_to_b64(&sp.pmm),
                prr: f32s_to_b64(&sp.prr),
                pmr: complexes_to_b64(&sp.pmr),
            }),
            outcome: match s.outcome {
                Ok(r) => BlobOutcome::Ok {
                    offset_samples: r.offset_samples,
                    inverted: r.inverted,
                    confidence: r.confidence,
                },
                Err(RejectReason::TooShort) => BlobOutcome::TooShort,
                Err(RejectReason::Silence) => BlobOutcome::Silence,
                Err(RejectReason::NonFinite) => BlobOutcome::NonFinite,
                Err(RejectReason::LowConfidence) => BlobOutcome::LowConfidence,
            },
        }),
    }
}

/// `Ok(None)` is a valid "no capture in this session"; `Err` is a blob that
/// cannot be trusted and must restore nothing.
pub fn decode(blob: &SnapshotBlob) -> Result<Option<Arc<AnalysisSnapshot>>, &'static str> {
    if blob.v != BLOB_VERSION {
        return Err("unknown format version");
    }
    let Some(b) = &blob.snap else {
        return Ok(None);
    };

    if !(b.sample_rate.is_finite() && b.sample_rate > 0.0 && b.sample_rate <= MAX_SAMPLE_RATE) {
        return Err("sample rate out of range");
    }
    let max_shift_samples = usize::try_from(b.max_shift_samples)
        .ok()
        .filter(|&m| m <= MAX_SHIFT_SAMPLES_CAP)
        .ok_or("max shift out of range")?;

    let main = b64_to_f32s(&b.main, MAX_WAVE_SAMPLES).ok_or("bad main buffer")?;
    let reference = b64_to_f32s(&b.reference, MAX_WAVE_SAMPLES).ok_or("bad reference buffer")?;
    if main.len() != reference.len() {
        return Err("capture length mismatch");
    }

    // Empty = rejected before the FFT ran; otherwise one value per lag in
    // `-max_shift..=max_shift`, the invariant the correlation view indexes by.
    let corr = b64_to_f32s(&b.corr, 2 * MAX_SHIFT_SAMPLES_CAP + 1).ok_or("bad corr buffer")?;
    if !corr.is_empty() && corr.len() != 2 * max_shift_samples + 1 {
        return Err("corr length mismatch");
    }

    if b.splices.len() > MAX_SPLICES {
        return Err("too many splices");
    }
    let mut splices = Vec::with_capacity(b.splices.len());
    for &p in &b.splices {
        let p = usize::try_from(p)
            .ok()
            .filter(|&p| p <= main.len())
            .ok_or("splice out of range")?;
        splices.push(p);
    }

    let spectrum = match &b.spectrum {
        None => None,
        Some(sp) => {
            let nfft = usize::try_from(sp.nfft)
                .ok()
                .filter(|&n| (MIN_NFFT..=MAX_NFFT).contains(&n) && n.is_power_of_two())
                .ok_or("nfft out of range")?;
            if sp.prealign_samples.unsigned_abs() as usize > MAX_WAVE_SAMPLES {
                return Err("prealign out of range");
            }
            let bins = nfft / 2 + 1;
            let pmm = b64_to_f32s(&sp.pmm, bins).ok_or("bad pmm")?;
            let prr = b64_to_f32s(&sp.prr, bins).ok_or("bad prr")?;
            let pmr = b64_to_complexes(&sp.pmr, bins).ok_or("bad pmr")?;
            if pmm.len() != bins || prr.len() != bins || pmr.len() != bins {
                return Err("spectrum bin count mismatch");
            }
            Some(SpectrumData {
                nfft,
                prealign_samples: sp.prealign_samples,
                segments: sp.segments,
                pmm,
                prr,
                pmr,
            })
        }
    };

    let outcome = match b.outcome {
        BlobOutcome::Ok {
            offset_samples,
            inverted,
            confidence,
        } => {
            if !(offset_samples.is_finite() && confidence.is_finite()) {
                return Err("non-finite result");
            }
            Ok(AnalysisResult {
                offset_samples,
                inverted,
                confidence,
            })
        }
        BlobOutcome::TooShort => Err(RejectReason::TooShort),
        BlobOutcome::Silence => Err(RejectReason::Silence),
        BlobOutcome::NonFinite => Err(RejectReason::NonFinite),
        BlobOutcome::LowConfidence => Err(RejectReason::LowConfidence),
    };

    Ok(Some(Arc::new(AnalysisSnapshot {
        main,
        reference,
        sample_rate: b.sample_rate,
        max_shift_samples,
        corr,
        splices,
        spectrum,
        outcome,
    })))
}

impl<'a> PersistentField<'a, SnapshotBlob> for Arc<SnapshotCell> {
    fn set(&self, blob: SnapshotBlob) {
        match decode(&blob) {
            Ok(snapshot) => SnapshotCell::store(self, snapshot),
            Err(why) => {
                nih_log!("ConjureAlign: discarding persisted analysis snapshot ({why})");
                // `None`, not leave-as-is: a reused instance must not keep
                // showing a previous session's graphs as this one's.
                SnapshotCell::store(self, None);
            }
        }
    }

    fn map<F, R>(&self, f: F) -> R
    where
        F: Fn(&SnapshotBlob) -> R,
    {
        // `get` clones the Arc under the lock and releases it; the
        // multi-megabyte encode runs outside, so an editor frame never waits
        // behind a host save.
        let snapshot = self.get();
        f(&encode(snapshot.as_deref()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nih_plug::params::persist::{deserialize_field, serialize_field};

    fn sample_snapshot(outcome: Result<AnalysisResult, RejectReason>) -> AnalysisSnapshot {
        let max_shift_samples = 3;
        AnalysisSnapshot {
            main: vec![0.0, 1.0, -1.0, f32::MIN_POSITIVE, 1.0e-38, 0.25],
            reference: vec![0.5, -0.5, 0.125, -0.125, 3.0e5, -3.0e5],
            sample_rate: 48_000.0,
            max_shift_samples,
            corr: (0..2 * max_shift_samples + 1).map(|i| i as f32 * 0.1).collect(),
            splices: vec![2, 4],
            spectrum: Some(SpectrumData {
                nfft: 256,
                prealign_samples: -7,
                segments: 3,
                pmm: (0..129).map(|i| i as f32).collect(),
                prr: (0..129).map(|i| i as f32 * 2.0).collect(),
                pmr: (0..129)
                    .map(|i| Complex {
                        re: i as f32,
                        im: -(i as f32),
                    })
                    .collect(),
            }),
            outcome,
        }
    }

    /// Encode → nih-plug's own field serializer → deserializer → decode, the
    /// exact path a session save/reload takes.
    fn round_trip(snap: Option<&AnalysisSnapshot>) -> Option<Arc<AnalysisSnapshot>> {
        let json = serialize_field(&encode(snap)).unwrap();
        let blob: SnapshotBlob = deserialize_field(&json).unwrap();
        decode(&blob).unwrap()
    }

    fn assert_snapshots_equal(a: &AnalysisSnapshot, b: &AnalysisSnapshot) {
        assert_eq!(a.main, b.main);
        assert_eq!(a.reference, b.reference);
        assert_eq!(a.sample_rate, b.sample_rate);
        assert_eq!(a.max_shift_samples, b.max_shift_samples);
        assert_eq!(a.corr, b.corr);
        assert_eq!(a.splices, b.splices);
        assert_eq!(a.spectrum.is_some(), b.spectrum.is_some());
        if let (Some(x), Some(y)) = (&a.spectrum, &b.spectrum) {
            assert_eq!(x.nfft, y.nfft);
            assert_eq!(x.prealign_samples, y.prealign_samples);
            assert_eq!(x.segments, y.segments);
            assert_eq!(x.pmm, y.pmm);
            assert_eq!(x.prr, y.prr);
            assert_eq!(x.pmr, y.pmr);
        }
        assert_eq!(a.outcome, b.outcome);
    }

    #[test]
    fn round_trip_accepted_capture_is_bit_exact() {
        let snap = sample_snapshot(Ok(AnalysisResult {
            offset_samples: 12.345678901234,
            inverted: true,
            confidence: 0.87,
        }));
        let restored = round_trip(Some(&snap)).expect("snapshot should survive");
        assert_snapshots_equal(&snap, &restored);
    }

    #[test]
    fn round_trip_rejected_capture_keeps_reason_and_empty_corr() {
        let mut snap = sample_snapshot(Err(RejectReason::LowConfidence));
        snap.corr = Vec::new();
        snap.spectrum = None;
        let restored = round_trip(Some(&snap)).expect("snapshot should survive");
        assert_snapshots_equal(&snap, &restored);
    }

    /// NaN capture samples are real (a NonFinite-rejected capture publishes
    /// its raw buffers); only the *metadata* is finiteness-checked.
    #[test]
    fn round_trip_preserves_non_finite_samples() {
        let mut snap = sample_snapshot(Err(RejectReason::NonFinite));
        snap.main[1] = f32::NAN;
        snap.reference[0] = f32::INFINITY;
        let restored = round_trip(Some(&snap)).unwrap();
        assert!(restored.main[1].is_nan());
        assert_eq!(restored.reference[0], f32::INFINITY);
    }

    #[test]
    fn round_trip_none() {
        assert!(round_trip(None).is_none());
    }

    #[test]
    fn encode_is_deterministic() {
        let snap = sample_snapshot(Err(RejectReason::Silence));
        let a = serialize_field(&encode(Some(&snap))).unwrap();
        let b = serialize_field(&encode(Some(&snap))).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn unknown_version_is_rejected() {
        let mut blob = encode(Some(&sample_snapshot(Err(RejectReason::TooShort))));
        blob.v = BLOB_VERSION + 1;
        assert!(decode(&blob).is_err());
    }

    /// The length gate fires on the *encoded* string, before any decoding.
    #[test]
    fn oversized_buffer_is_rejected_before_allocation() {
        let mut blob = encode(Some(&sample_snapshot(Err(RejectReason::TooShort))));
        let huge = MAX_WAVE_SAMPLES * 4 / 3 * 4 + 64;
        blob.snap.as_mut().unwrap().main = "A".repeat(huge);
        assert!(decode(&blob).is_err());
    }

    #[test]
    fn truncated_byte_stream_is_rejected() {
        let mut blob = encode(Some(&sample_snapshot(Err(RejectReason::TooShort))));
        // 3 bytes: valid base64, not a whole number of f32s.
        blob.snap.as_mut().unwrap().main = B64.encode([1u8, 2, 3]);
        assert!(decode(&blob).is_err());
    }

    #[test]
    fn mismatched_lengths_are_rejected() {
        let base = sample_snapshot(Err(RejectReason::TooShort));

        let mut blob = encode(Some(&base));
        blob.snap.as_mut().unwrap().reference = f32s_to_b64(&[0.0; 3]);
        assert!(decode(&blob).is_err(), "main/reference mismatch");

        let mut blob = encode(Some(&base));
        blob.snap.as_mut().unwrap().corr = f32s_to_b64(&[0.0; 4]);
        assert!(decode(&blob).is_err(), "corr vs max_shift mismatch");

        let mut blob = encode(Some(&base));
        blob.snap.as_mut().unwrap().spectrum.as_mut().unwrap().pmm = f32s_to_b64(&[0.0; 5]);
        assert!(decode(&blob).is_err(), "spectrum bin mismatch");
    }

    #[test]
    fn hostile_metadata_is_rejected() {
        let base = sample_snapshot(Err(RejectReason::TooShort));

        let mut blob = encode(Some(&base));
        blob.snap.as_mut().unwrap().sample_rate = f32::NAN;
        assert!(decode(&blob).is_err(), "NaN sample rate");

        let mut blob = encode(Some(&base));
        blob.snap.as_mut().unwrap().max_shift_samples = u64::MAX;
        assert!(decode(&blob).is_err(), "absurd max shift");

        let mut blob = encode(Some(&base));
        blob.snap.as_mut().unwrap().splices = vec![0; MAX_SPLICES + 1];
        assert!(decode(&blob).is_err(), "too many splices");

        let mut blob = encode(Some(&base));
        blob.snap.as_mut().unwrap().splices = vec![u64::MAX];
        assert!(decode(&blob).is_err(), "splice past the buffer");

        let mut blob = encode(Some(&base));
        blob.snap.as_mut().unwrap().spectrum.as_mut().unwrap().nfft = 12_345;
        assert!(decode(&blob).is_err(), "non-power-of-two nfft");

        let mut blob = encode(Some(&base));
        blob.snap.as_mut().unwrap().outcome = BlobOutcome::Ok {
            offset_samples: f64::NAN,
            inverted: false,
            confidence: 0.5,
        };
        assert!(decode(&blob).is_err(), "NaN offset");
    }

    /// The whole derive-generated path: the `#[persist = "analysis-snapshot"]`
    /// attribute on the Params struct, through nih-plug's own
    /// `serialize_fields`/`deserialize_fields` — what a host save/load calls.
    #[test]
    fn params_derive_round_trips_the_snapshot() {
        use crate::params::ConjureAlignParams;
        use nih_plug::prelude::Params as _;

        let snap = sample_snapshot(Err(RejectReason::Silence));
        let source = ConjureAlignParams::default();
        source.snapshot.store(Some(Arc::new(sample_snapshot(Err(
            RejectReason::Silence,
        )))));
        let fields = source.serialize_fields();
        assert!(fields.contains_key("analysis-snapshot"));

        let target = ConjureAlignParams::default();
        target.deserialize_fields(&fields);
        let restored = target.snapshot.get().expect("snapshot should restore");
        assert_snapshots_equal(&snap, &restored);
    }

    /// The `PersistentField` seam itself: `map` feeds the serializer, `set`
    /// lands the restored snapshot in a fresh cell — and a hostile blob
    /// clears the cell instead of leaving a stale snapshot behind.
    #[test]
    fn persistent_field_round_trip_and_hostile_set() {
        let snap = Arc::new(sample_snapshot(Ok(AnalysisResult {
            offset_samples: -3.25,
            inverted: false,
            confidence: 0.44,
        })));

        let source = Arc::new(SnapshotCell::default());
        source.store(Some(snap.clone()));
        let json = PersistentField::map(&source, |blob| serialize_field(blob).unwrap());

        let target = Arc::new(SnapshotCell::default());
        target.store(Some(snap.clone())); // pre-existing state to overwrite
        PersistentField::set(&target, deserialize_field::<SnapshotBlob>(&json).unwrap());
        let restored = target.get().expect("snapshot should restore");
        assert_snapshots_equal(&snap, &restored);

        let mut hostile = encode(Some(&snap));
        hostile.v = 999;
        PersistentField::set(&target, hostile);
        assert!(target.get().is_none(), "hostile blob must clear the cell");
    }
}
