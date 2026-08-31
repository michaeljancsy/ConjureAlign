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
//!
//! `Plugin Loaded` additionally carries `upgraded_from` — the version that ran
//! on this install before this one — on the one launch that first sees a
//! version change. It is strictly less revealing than `plugin_version`, which
//! every event already carries, so it needed no new disclosure; it is recorded
//! here because this module doc and CLAUDE.md are the only account of what
//! leaves the machine. It exists because the cohort-level version breakdown
//! cannot tell "nobody upgraded" from "nobody was told there was an upgrade".

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use atomic_float::AtomicF32;

use crate::analysis::RejectReason;
use crate::host::HostInfo;
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

// Not `Copy`: `PluginLoaded` carries an owned version string.
#[derive(Debug, Clone, PartialEq)]
pub enum AnalyticsEvent {
    /// Once per plugin instance. Goes over the wire as "Plugin Loaded", NOT
    /// "Session Start" — Mixpanel ships a built-in virtual event,
    /// `$session_start`, whose display name is exactly "Session Start", and
    /// two identically labelled entries in the event picker is a trap.
    PluginLoaded {
        sample_rate: f32,
        /// The version that ran on this install before this one, when this
        /// launch is an upgrade. `None` on a first run and on every launch
        /// after the first at a given version, so at most one event per
        /// upgrade carries it. See [`crate::config::note_running_version`].
        upgraded_from: Option<String>,
    },
    CaptureCompleted {
        confidence: f32,
        offset_ms: f32,
        /// Seconds of *gated* signal, not wall clock: silence is spliced out
        /// as it is recorded.
        capture_seconds: f32,
        splice_count: usize,
        polarity_inverted: bool,
    },
    CaptureRejected {
        reason: RejectReason,
        /// Carried here too, because it is what separates "the user never
        /// played anything" from "they played, and the correlation was bad".
        capture_seconds: f32,
        splice_count: usize,
    },
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

/// Bucketed like the rest: the exact length of a take describes the user's
/// material. The edges are the ones that change a decision — the top bucket is
/// "hit the `CAPTURE_MAX_SECS` buffer cap", the bottom is "barely anything got
/// through the gate".
pub fn capture_seconds_bucket(seconds: f32) -> &'static str {
    match seconds {
        s if s < 0.5 => "<0.5s",
        s if s < 1.0 => "0.5-1s",
        s if s < 2.0 => "1-2s",
        s if s < 4.0 => "2-4s",
        // Also catches a non-finite reading. That cannot come from a real
        // capture — the buffers are sized in `initialize()` and the sample
        // rate is known — but a NaN failing every guard above must not be
        // reported as the *shortest* bucket.
        _ => "4s+",
    }
}

/// How badly the gate chopped a take up, which is the readable symptom of a
/// wrong `gate_threshold` default. `"max"` is its own bucket because at
/// `MAX_SPLICES` the seam list stops growing and the rest of the capture
/// records continuously — past that point the count is a floor, not a total.
pub fn splice_count_bucket(count: usize) -> &'static str {
    match count {
        0 => "0",
        1..=3 => "1-3",
        4..=10 => "4-10",
        c if c < crate::capture::MAX_SPLICES => "11+",
        _ => "max",
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

/// Everything an event carries that is not the event itself: the identity, the
/// clock, and the environment. Passed in rather than read from globals so
/// `build_payload` stays pure and its assertions stay meaningful on any machine.
pub struct EventContext<'a> {
    pub device_id: &'a str,
    /// Milliseconds since the epoch — what the ingestion API expects.
    pub now_millis: u64,
    /// The API this instance is hosted through: "CLAP", "VST3" or
    /// "standalone". `None` before `initialize()` has run.
    ///
    /// AudioUnit reports as CLAP, and there is no fixing that here:
    /// clap-wrapper translates AU calls into calls on our own `clap_entry`, so
    /// nih-plug genuinely never sees an AU. The `daw` property is what
    /// separates them in practice — Logic and GarageBand load only the AU.
    pub plugin_format: Option<&'static str>,
    pub host: &'a HostInfo,
}

/// One Mixpanel event object.
pub fn build_payload(event: &AnalyticsEvent, ctx: &EventContext) -> serde_json::Value {
    let mut props = serde_json::Map::new();
    props.insert("token".into(), MIXPANEL_TOKEN.into());
    props.insert("distinct_id".into(), ctx.device_id.into());
    props.insert("time".into(), ctx.now_millis.into());
    props.insert("plugin_version".into(), env!("CARGO_PKG_VERSION").into());
    // The build target, which cannot move for a given install...
    props.insert("os".into(), std::env::consts::OS.into());
    // ...as opposed to the running OS, which can, and which is what tells a
    // crash cluster apart from a platform-wide regression.
    if let Some(version) = &ctx.host.os_version {
        props.insert("os_version".into(), version.as_str().into());
    }
    props.insert("daw".into(), ctx.host.daw.into());
    if let Some(version) = &ctx.host.daw_version {
        props.insert("daw_version".into(), version.as_str().into());
    }
    if let Some(format) = ctx.plugin_format {
        props.insert("plugin_format".into(), format.into());
    }

    let name = match event {
        AnalyticsEvent::PluginLoaded {
            sample_rate,
            upgraded_from,
        } => {
            props.insert("sample_rate".into(), (*sample_rate as u64).into());
            // Omitted rather than nulled, like every other optional property:
            // a null becomes a real value in Mixpanel's lexicon. Its absence
            // already means "not an upgrade, or not one we can prove".
            if let Some(previous) = upgraded_from {
                props.insert("upgraded_from".into(), previous.as_str().into());
            }
            "Plugin Loaded"
        }
        AnalyticsEvent::CaptureCompleted {
            confidence,
            offset_ms,
            capture_seconds,
            splice_count,
            polarity_inverted,
        } => {
            props.insert("confidence".into(), confidence_bucket(*confidence).into());
            props.insert("offset".into(), offset_bucket(*offset_ms).into());
            props.insert(
                "capture_length".into(),
                capture_seconds_bucket(*capture_seconds).into(),
            );
            props.insert("splices".into(), splice_count_bucket(*splice_count).into());
            props.insert("polarity_inverted".into(), (*polarity_inverted).into());
            "Capture Completed"
        }
        AnalyticsEvent::CaptureRejected {
            reason,
            capture_seconds,
            splice_count,
        } => {
            props.insert("reason".into(), reason_str(*reason).into());
            props.insert(
                "capture_length".into(),
                capture_seconds_bucket(*capture_seconds).into(),
            );
            props.insert("splices".into(), splice_count_bucket(*splice_count).into());
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
    /// Set once by `note_session`. A `OnceLock` rather than a plain field
    /// because hosts re-run `initialize()` freely, but never with a different
    /// plugin API for the same instance.
    plugin_format: OnceLock<&'static str>,
}

impl AnalyticsHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Called from `initialize()`. Idempotent across host re-initializations,
    /// so a state load or sample-rate change never doubles the session count.
    pub fn note_session(&self, sample_rate: f32, plugin_format: &'static str) {
        self.sample_rate.store(sample_rate, Ordering::Relaxed);
        let _ = self.plugin_format.set(plugin_format);
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
            // Behind the `enabled()` check above, so a declined install never
            // writes it. The CAS makes this once per *instance*; the write and
            // the non-`None` answer are once per *process*, because the config
            // is process-cached and the second caller sees the version already
            // stored. So a host with four instances still reports one upgrade.
            //
            // Like `set_analytics_consent`, this holds the config lock across
            // file I/O — fine on the main and background-task threads, which
            // are the only two that reach here, and already accounted for by
            // the `try_lock` in the panic-hook accessors.
            let upgraded_from = crate::config::note_running_version(env!("CARGO_PKG_VERSION"));
            self.send(AnalyticsEvent::PluginLoaded {
                sample_rate,
                upgraded_from,
            });
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
        // `host::info()` resolves on first use and is cached process-wide; it
        // runs here, on the main or background thread, never on the audio one.
        let ctx = EventContext {
            device_id: &device_id,
            now_millis,
            plugin_format: self.plugin_format.get().copied(),
            host: crate::host::info(),
        };
        let batch = serde_json::Value::Array(vec![build_payload(&event, &ctx)]);
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
    use crate::params::CAPTURE_MAX_SECS;

    /// A fixed environment, so payload assertions do not depend on the machine
    /// the tests happen to run on.
    fn test_host() -> HostInfo {
        HostInfo {
            daw: "Ableton Live",
            daw_version: Some("12.1.5".into()),
            os_version: Some("26.3.1".into()),
        }
    }

    fn ctx<'a>(device_id: &'a str, now_millis: u64, host: &'a HostInfo) -> EventContext<'a> {
        EventContext {
            device_id,
            now_millis,
            plugin_format: Some("VST3"),
            host,
        }
    }

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
            capture_seconds: 2.8125,
            splice_count: 7,
            polarity_inverted: true,
        };
        let host = test_host();
        let v = build_payload(&event, &ctx("abc123", 1_700_000_000_000, &host));
        assert_eq!(v["event"], "Capture Completed");
        let props = &v["properties"];
        assert_eq!(props["token"], MIXPANEL_TOKEN);
        assert_eq!(props["distinct_id"], "abc123");
        assert_eq!(props["time"], 1_700_000_000_000u64);
        assert_eq!(props["plugin_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(props["confidence"], "0.9+");
        assert_eq!(props["offset"], "1-10ms");
        assert_eq!(props["capture_length"], "2-4s");
        assert_eq!(props["splices"], "4-10");
        // The one raw value that ships: a single bit about the wiring, which
        // no bucket could coarsen further.
        assert_eq!(props["polarity_inverted"], true);

        // The raw measurements must not survive anywhere in the payload.
        let text = v.to_string();
        assert!(!text.contains("0.94"), "raw confidence leaked: {text}");
        assert!(!text.contains("3.75"), "raw offset leaked: {text}");
        assert!(
            !text.contains("2.8125"),
            "raw capture length leaked: {text}"
        );
        assert!(!text.contains(":7"), "raw splice count leaked: {text}");
    }

    #[test]
    fn capture_length_buckets_cover_the_range() {
        assert_eq!(capture_seconds_bucket(0.0), "<0.5s");
        assert_eq!(capture_seconds_bucket(0.49), "<0.5s");
        assert_eq!(capture_seconds_bucket(0.5), "0.5-1s");
        assert_eq!(capture_seconds_bucket(0.99), "0.5-1s");
        assert_eq!(capture_seconds_bucket(1.0), "1-2s");
        assert_eq!(capture_seconds_bucket(2.0), "2-4s");
        assert_eq!(capture_seconds_bucket(3.99), "2-4s");
        // The buffer cap: "the user filled it and we stopped for them".
        assert_eq!(capture_seconds_bucket(CAPTURE_MAX_SECS as f32), "4s+");

        // A non-finite reading must not read as the shortest capture.
        assert_eq!(capture_seconds_bucket(f32::NAN), "4s+");
        assert_eq!(capture_seconds_bucket(f32::INFINITY), "4s+");
    }

    #[test]
    fn splice_buckets_separate_the_tracking_limit() {
        assert_eq!(splice_count_bucket(0), "0");
        assert_eq!(splice_count_bucket(1), "1-3");
        assert_eq!(splice_count_bucket(3), "1-3");
        assert_eq!(splice_count_bucket(4), "4-10");
        assert_eq!(splice_count_bucket(10), "4-10");
        assert_eq!(splice_count_bucket(11), "11+");
        assert_eq!(splice_count_bucket(crate::capture::MAX_SPLICES - 1), "11+");
        // At capacity the seam list stops growing, so the count is a floor
        // rather than a total — a distinction the bucket has to preserve.
        assert_eq!(splice_count_bucket(crate::capture::MAX_SPLICES), "max");
        assert_eq!(
            splice_count_bucket(crate::capture::MAX_SPLICES + 100),
            "max"
        );
    }

    #[test]
    fn payload_carries_the_host_environment() {
        let host = test_host();
        let v = build_payload(
            &AnalyticsEvent::PluginLoaded {
                sample_rate: 48_000.0,
                upgraded_from: None,
            },
            &ctx("d", 1, &host),
        );
        let props = &v["properties"];
        assert_eq!(props["daw"], "Ableton Live");
        assert_eq!(props["daw_version"], "12.1.5");
        assert_eq!(props["os_version"], "26.3.1");
        assert_eq!(props["plugin_format"], "VST3");
        // The build target stays alongside the running version, not replaced
        // by it: they answer different questions.
        assert_eq!(props["os"], std::env::consts::OS);
    }

    /// The upgrade marker rides on `Plugin Loaded`, and only when there is an
    /// upgrade to mark. Its absence is what "first run, or not an upgrade we
    /// can prove" looks like on the wire.
    #[test]
    fn upgraded_from_is_present_only_on_an_upgrade() {
        let host = test_host();

        let v = build_payload(
            &AnalyticsEvent::PluginLoaded {
                sample_rate: 48_000.0,
                upgraded_from: Some("1.2.0".into()),
            },
            &ctx("d", 1, &host),
        );
        assert_eq!(v["properties"]["upgraded_from"], "1.2.0");
        // The running version is still reported alongside it: the pair is what
        // names the edge, and one half is useless without the other.
        assert_eq!(v["properties"]["plugin_version"], env!("CARGO_PKG_VERSION"));

        let v = build_payload(
            &AnalyticsEvent::PluginLoaded {
                sample_rate: 48_000.0,
                upgraded_from: None,
            },
            &ctx("d", 1, &host),
        );
        let props = v["properties"].as_object().unwrap();
        assert!(!props.contains_key("upgraded_from"), "{props:?}");
    }

    /// Absent values are left OUT of the payload rather than sent as null —
    /// a null lands in Mixpanel's lexicon as a real value and pollutes every
    /// breakdown on the property.
    #[test]
    fn unresolved_environment_values_are_omitted_not_nulled() {
        let host = HostInfo {
            daw: crate::host::UNKNOWN_DAW,
            daw_version: None,
            os_version: None,
        };
        let v = build_payload(
            &AnalyticsEvent::PluginLoaded {
                sample_rate: 48_000.0,
                upgraded_from: None,
            },
            &EventContext {
                device_id: "d",
                now_millis: 1,
                // Before `initialize()` has run there is no format to report.
                plugin_format: None,
                host: &host,
            },
        );
        let props = v["properties"].as_object().unwrap();
        assert_eq!(props["daw"], "other");
        assert!(!props.contains_key("daw_version"), "{props:?}");
        assert!(!props.contains_key("os_version"), "{props:?}");
        assert!(!props.contains_key("plugin_format"), "{props:?}");
    }

    /// What leaves names the DAW, never the path it was found at.
    #[test]
    fn payload_never_carries_a_filesystem_path() {
        let event = AnalyticsEvent::CaptureCompleted {
            confidence: 0.94,
            offset_ms: -3.75,
            capture_seconds: 2.8125,
            splice_count: 7,
            polarity_inverted: false,
        };
        let text = build_payload(&event, &ctx("abc123", 1, crate::host::info())).to_string();
        for fragment in [
            "/Users/",
            "/Applications",
            "C:\\",
            "\\Program Files",
            "/home/",
        ] {
            assert!(!text.contains(fragment), "path leaked ({fragment}): {text}");
        }
    }

    #[test]
    fn rejection_payload_names_the_reason() {
        let host = test_host();
        let v = build_payload(
            &AnalyticsEvent::CaptureRejected {
                reason: RejectReason::LowConfidence,
                capture_seconds: 0.2,
                splice_count: 0,
            },
            &ctx("d", 1, &host),
        );
        assert_eq!(v["event"], "Capture Rejected");
        assert_eq!(v["properties"]["reason"], "low_confidence");
        // A rejection carries the capture's shape too: "nothing got through
        // the gate" and "plenty did, but it did not correlate" are different
        // problems with the same `reason`.
        assert_eq!(v["properties"]["capture_length"], "<0.5s");
        assert_eq!(v["properties"]["splices"], "0");
        // No result, so no polarity to report.
        assert!(v["properties"]
            .as_object()
            .unwrap()
            .get("polarity_inverted")
            .is_none());
    }

    #[test]
    fn session_payload_carries_the_sample_rate() {
        let host = test_host();
        let v = build_payload(
            &AnalyticsEvent::PluginLoaded {
                sample_rate: 48_000.0,
                upgraded_from: None,
            },
            &ctx("d", 1, &host),
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
        // The live environment on purpose: the smoke test is also how a new
        // `daw` / `os_version` value gets seen once in the real project.
        let body = serde_json::Value::Array(vec![build_payload(
            &AnalyticsEvent::PluginLoaded {
                sample_rate: 48_000.0,
                upgraded_from: None,
            },
            &EventContext {
                device_id: "smoke-test",
                now_millis,
                plugin_format: Some("standalone"),
                host: crate::host::info(),
            },
        )])
        .to_string();

        let response = net::post(&endpoint, &body).expect("TLS handshake and POST should complete");
        println!("--- Mixpanel response ---\n{response}\n---");
        assert!(
            response.contains("\"status\": 1") || response.contains("\"status\":1"),
            "Mixpanel rejected the event — check MIXPANEL_TOKEN:\n{response}"
        );
    }
}
