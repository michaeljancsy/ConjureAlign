//! Opt-in usage analytics (Mixpanel).
//!
//! Three rules shape everything here:
//!
//! 1. **The audio thread never touches this module.** Events are raised from
//!    `initialize()` (main thread) and from the background analysis task, both
//!    of which may allocate. `process()` has no analytics code at all, so
//!    `assert_process_allocs` stays meaningful.
//! 2. **Nothing is sent, and nothing is written to disk, until the user says
//!    yes.** Consent is tri-state — never asked / granted / declined — and
//!    lives in a config file next to no other state, because `#[persist]`
//!    fields are per-DAW-session and consent is per-install. The answer and its
//!    storage live in [`crate::config`]; the accessors below are re-exports, so
//!    that `analytics::enabled()` still reads as the question it is.
//! 3. **No thread outlives the dylib.** Hosts unload plugin bundles in-process.
//!    Sending goes through the shared worker in [`crate::net`], which is held
//!    alive per-instance by the [`AnalyticsHandle`] and joined when the last
//!    one drops.
//!
//! The payload is deliberately thin: a random device id, the plugin version,
//! the OS, the sample rate, and *bucketed* capture outcomes. No audio, no file
//! names, no host name, no raw measurements.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use atomic_float::AtomicF32;

use crate::analysis::RejectReason;
use crate::net::{self, Endpoint};

// Consent is one question covering analytics and crash reporting, stored
// install-wide. Re-exported rather than re-implemented so the editor, `crash`
// and the integration tests keep asking `analytics::…` for the analytics
// answer.
pub use crate::config::{
    analytics_consent as consent, analytics_enabled as enabled,
    analytics_enabled_in_hook as enabled_in_hook, config_path, device_id, device_id_in_hook,
    is_supported, set_analytics_consent as set_consent,
};

/// Mixpanel project token for the "ConjureAlign" project. Client-side tokens
/// are public by design — this one is write-only ingestion, it grants no read
/// access, and it ships in every binary regardless of what we do here.
pub const MIXPANEL_TOKEN: &str = "33c5c2d1578f3275ec2985bf4c92ad22";

const DEFAULT_ENDPOINT: &str = "https://api.mixpanel.com/track";
/// Points the sender at a local sink for tests and manual QA.
const ENDPOINT_ENV: &str = "CONJURE_ALIGN_ANALYTICS_URL";

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AnalyticsEvent {
    /// Once per plugin instance. Goes over the wire as "Plugin Loaded", NOT
    /// "Session Start" — Mixpanel ships a built-in virtual event,
    /// `$session_start`, whose display name is exactly "Session Start", and
    /// two identically labelled entries in the event picker is a trap.
    PluginLoaded { sample_rate: f32 },
    CaptureCompleted { confidence: f32, offset_ms: f32 },
    CaptureRejected { reason: RejectReason },
}

/// Bucketed, never raw: a precise confidence figure would say more about the
/// user's material than we have any reason to know.
pub fn confidence_bucket(confidence: f32) -> &'static str {
    match confidence {
        c if c < 0.5 => "<0.5",
        c if c < 0.7 => "0.5-0.7",
        c if c < 0.9 => "0.7-0.9",
        _ => "0.9+",
    }
}

/// Magnitude only — the sign says which mic was closer, which is the user's
/// business, and the bucket is all we need to know if the shift window is
/// sized sensibly.
pub fn offset_bucket(offset_ms: f32) -> &'static str {
    match offset_ms.abs() {
        m if m < 1.0 => "<1ms",
        m if m < 10.0 => "1-10ms",
        m if m < 50.0 => "10-50ms",
        _ => "50ms+",
    }
}

pub fn reason_str(reason: RejectReason) -> &'static str {
    match reason {
        RejectReason::TooShort => "too_short",
        RejectReason::Silence => "silence",
        RejectReason::NonFinite => "non_finite",
        RejectReason::LowConfidence => "low_confidence",
    }
}

/// One Mixpanel event object. `time` is milliseconds since the epoch, which is
/// what the ingestion API expects.
pub fn build_payload(
    event: &AnalyticsEvent,
    device_id: &str,
    now_millis: u64,
) -> serde_json::Value {
    let mut props = serde_json::Map::new();
    props.insert("token".into(), MIXPANEL_TOKEN.into());
    props.insert("distinct_id".into(), device_id.into());
    props.insert("time".into(), now_millis.into());
    props.insert("plugin_version".into(), env!("CARGO_PKG_VERSION").into());
    props.insert("os".into(), std::env::consts::OS.into());

    let name = match event {
        AnalyticsEvent::PluginLoaded { sample_rate } => {
            props.insert("sample_rate".into(), (*sample_rate as u64).into());
            "Plugin Loaded"
        }
        AnalyticsEvent::CaptureCompleted {
            confidence,
            offset_ms,
        } => {
            props.insert("confidence".into(), confidence_bucket(*confidence).into());
            props.insert("offset".into(), offset_bucket(*offset_ms).into());
            "Capture Completed"
        }
        AnalyticsEvent::CaptureRejected { reason } => {
            props.insert("reason".into(), reason_str(*reason).into());
            "Capture Rejected"
        }
    };

    serde_json::json!({ "event": name, "properties": serde_json::Value::Object(props) })
}

/// The default endpoint, unless the environment points somewhere else (a local
/// sink for tests and manual QA). Read once: the environment is process-wide
/// and mutating it underneath running threads is a data race anyway.
fn endpoint() -> Option<&'static Endpoint> {
    static ENDPOINT: OnceLock<Option<Endpoint>> = OnceLock::new();
    ENDPOINT
        .get_or_init(|| {
            std::env::var(ENDPOINT_ENV)
                .ok()
                .as_deref()
                .and_then(net::parse_endpoint)
                .or_else(|| net::parse_endpoint(DEFAULT_ENDPOINT))
        })
        .as_ref()
}

// ---------------------------------------------------------------------------
// Per-instance handle
// ---------------------------------------------------------------------------

/// One per plugin instance. Constructing it does no I/O and starts no thread,
/// so plugin scanners and opted-out users pay nothing.
#[derive(Default)]
pub struct AnalyticsHandle {
    /// Holds the process-wide network worker alive for this instance's
    /// lifetime.
    worker: net::WorkerHandle,
    session_sent: AtomicBool,
    /// Zero until `note_session` has run; also the Plugin Loaded payload.
    sample_rate: AtomicF32,
}

impl AnalyticsHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Called from `initialize()`. Idempotent across host re-initializations,
    /// so a state load or sample-rate change never doubles the session count.
    pub fn note_session(&self, sample_rate: f32) {
        self.sample_rate.store(sample_rate, Ordering::Relaxed);
        self.flush_session();
    }

    pub fn track(&self, event: AnalyticsEvent) {
        if !enabled() {
            return;
        }
        // Consent usually arrives *after* `initialize()` — the prompt is shown
        // when the editor first opens — so the session event is emitted here
        // rather than being lost.
        self.flush_session();
        self.send(event);
    }

    fn flush_session(&self) {
        if !enabled() {
            return;
        }
        let sample_rate = self.sample_rate.load(Ordering::Relaxed);
        if sample_rate <= 0.0 {
            return;
        }
        if self
            .session_sent
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.send(AnalyticsEvent::PluginLoaded { sample_rate });
        }
    }

    fn send(&self, event: AnalyticsEvent) {
        let Some(device_id) = device_id() else {
            return;
        };
        let Some(endpoint) = endpoint() else {
            return;
        };
        let now_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let batch = serde_json::Value::Array(vec![build_payload(&event, &device_id, now_millis)]);
        let body = batch.to_string();

        // The return value is deliberately ignored: a dropped analytics event
        // is the designed behaviour when the queue is backed up, and there is
        // nothing to tell the user about it.
        self.worker.spawn_job(move || {
            let _ = net::post(endpoint, &body);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_buckets_cover_the_range() {
        assert_eq!(confidence_bucket(0.0), "<0.5");
        assert_eq!(confidence_bucket(0.49), "<0.5");
        assert_eq!(confidence_bucket(0.5), "0.5-0.7");
        assert_eq!(confidence_bucket(0.69), "0.5-0.7");
        assert_eq!(confidence_bucket(0.7), "0.7-0.9");
        assert_eq!(confidence_bucket(0.89), "0.7-0.9");
        assert_eq!(confidence_bucket(0.9), "0.9+");
        assert_eq!(confidence_bucket(1.0), "0.9+");
    }

    #[test]
    fn offset_buckets_use_magnitude() {
        assert_eq!(offset_bucket(0.4), "<1ms");
        assert_eq!(offset_bucket(-0.4), "<1ms");
        assert_eq!(offset_bucket(-5.0), "1-10ms");
        assert_eq!(offset_bucket(5.0), "1-10ms");
        assert_eq!(offset_bucket(-49.0), "10-50ms");
        assert_eq!(offset_bucket(120.0), "50ms+");
        assert_eq!(offset_bucket(-120.0), "50ms+");
    }

    #[test]
    fn payload_carries_only_bucketed_values() {
        let event = AnalyticsEvent::CaptureCompleted {
            confidence: 0.94,
            offset_ms: -3.75,
        };
        let v = build_payload(&event, "abc123", 1_700_000_000_000);
        assert_eq!(v["event"], "Capture Completed");
        let props = &v["properties"];
        assert_eq!(props["token"], MIXPANEL_TOKEN);
        assert_eq!(props["distinct_id"], "abc123");
        assert_eq!(props["time"], 1_700_000_000_000u64);
        assert_eq!(props["plugin_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(props["confidence"], "0.9+");
        assert_eq!(props["offset"], "1-10ms");

        // The raw measurements must not survive anywhere in the payload.
        let text = v.to_string();
        assert!(!text.contains("0.94"), "raw confidence leaked: {text}");
        assert!(!text.contains("3.75"), "raw offset leaked: {text}");
    }

    #[test]
    fn rejection_payload_names_the_reason() {
        let v = build_payload(
            &AnalyticsEvent::CaptureRejected {
                reason: RejectReason::LowConfidence,
            },
            "d",
            1,
        );
        assert_eq!(v["event"], "Capture Rejected");
        assert_eq!(v["properties"]["reason"], "low_confidence");
    }

    #[test]
    fn session_payload_carries_the_sample_rate() {
        let v = build_payload(
            &AnalyticsEvent::PluginLoaded {
                sample_rate: 48_000.0,
            },
            "d",
            1,
        );
        assert_eq!(v["event"], "Plugin Loaded");
        assert_eq!(v["properties"]["sample_rate"], 48_000u64);
    }

    #[test]
    fn default_endpoint_is_a_valid_https_url() {
        let endpoint = net::parse_endpoint(DEFAULT_ENDPOINT).expect("default endpoint must parse");
        assert!(endpoint.tls);
        assert_eq!(endpoint.port, 443);
    }

    /// The only test that leaves the machine, so it is opt-in:
    /// `cargo test --release -- --ignored --nocapture smoke_test`.
    ///
    /// Worth having because every real user takes the TLS path and no local
    /// sink can reach it — the plain-HTTP tests in `net` would not have caught,
    /// for instance, the branch that used to open a second TCP connection.
    /// Asking for `verbose=1` turns a silent rejection (a wrong token reads
    /// as a bare `0`) into a message that says which field Mixpanel disliked.
    ///
    /// It writes ONE event to the live project, tagged `smoke-test` so it can
    /// be filtered out; `MIXPANEL_TOKEN` must be real for it to pass.
    #[test]
    #[ignore = "sends one real event to the live Mixpanel project"]
    fn smoke_test_against_live_mixpanel() {
        let mut endpoint = net::parse_endpoint(DEFAULT_ENDPOINT).unwrap();
        endpoint.path = "/track?verbose=1".into();

        let now_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let body = serde_json::Value::Array(vec![build_payload(
            &AnalyticsEvent::PluginLoaded {
                sample_rate: 48_000.0,
            },
            "smoke-test",
            now_millis,
        )])
        .to_string();

        let response =
            net::post(&endpoint, &body).expect("TLS handshake and POST should complete");
        println!("--- Mixpanel response ---\n{response}\n---");
        assert!(
            response.contains("\"status\": 1") || response.contains("\"status\":1"),
            "Mixpanel rejected the event — check MIXPANEL_TOKEN:\n{response}"
        );
    }
}
