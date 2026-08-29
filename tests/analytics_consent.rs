//! The property the whole opt-in design rests on: nothing leaves the machine
//! until the user says yes, and once they do, the events actually arrive.
//!
//! Runs against a temporary `HOME`/`APPDATA` so it can never read or clobber
//! the developer's own preference file, and against a local TCP sink so it
//! never talks to Mixpanel.

use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;

use conjure_align::analytics::{self, AnalyticsEvent, AnalyticsHandle};

/// Reads one HTTP request off the socket and returns its body.
fn read_request(listener: &TcpListener) -> String {
    let (stream, _) = listener.accept().expect("sink accept");
    let mut reader = BufReader::new(stream);
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if let Some(v) = line.strip_prefix("Content-Length: ") {
            content_length = v.trim().parse().unwrap();
        }
        if line == "\r\n" {
            break;
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).unwrap();
    String::from_utf8(body).unwrap()
}

#[test]
fn nothing_is_sent_until_consent_is_granted() {
    // Set before anything touches the config: it is cached in a OnceLock on
    // first read. Safe here because this is the only test in this binary, so
    // no other thread is reading the environment.
    let home = std::env::temp_dir().join("conjure-align-consent-e2e");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);
    std::env::set_var("APPDATA", &home);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::env::set_var(
        "CONJURE_ALIGN_ANALYTICS_URL",
        format!("http://127.0.0.1:{port}/track"),
    );

    let handle = AnalyticsHandle::new();

    // Never asked yet — that is what shows the first-run prompt, and it must
    // behave exactly like a "no" on the wire.
    assert_eq!(analytics::consent(), None);
    assert!(!analytics::enabled());
    handle.note_session(48_000.0, "VST3");
    handle.track(AnalyticsEvent::CaptureCompleted {
        confidence: 0.95,
        offset_ms: 5.0,
        capture_seconds: 1.5,
        splice_count: 2,
        polarity_inverted: false,
    });

    listener.set_nonblocking(true).unwrap();
    assert!(
        listener.accept().is_err(),
        "an un-consented plugin opened a connection"
    );

    // Declining is likewise silent, and leaves no identifier behind.
    analytics::set_consent(false);
    handle.track(AnalyticsEvent::CaptureRejected {
        reason: conjure_align::analysis::RejectReason::Silence,
        capture_seconds: 0.0,
        splice_count: 0,
    });
    assert!(
        listener.accept().is_err(),
        "a declined plugin opened a connection"
    );

    // Granting mints a device id and starts the flow — including the session
    // event that was suppressed back when `note_session` ran.
    listener.set_nonblocking(false).unwrap();
    analytics::set_consent(true);
    assert_eq!(analytics::consent(), Some(true));

    handle.track(AnalyticsEvent::CaptureCompleted {
        confidence: 0.95,
        offset_ms: 5.0,
        capture_seconds: 1.5,
        splice_count: 2,
        polarity_inverted: false,
    });

    let session: serde_json::Value = serde_json::from_str(&read_request(&listener)).unwrap();
    assert_eq!(session[0]["event"], "Plugin Loaded");
    assert_eq!(session[0]["properties"]["sample_rate"], 48_000u64);
    // The environment properties survive the whole path to the wire, not just
    // `build_payload`: the format this instance was told about at
    // `note_session`, and the host resolved once per process.
    assert_eq!(session[0]["properties"]["plugin_format"], "VST3");
    let daw = session[0]["properties"]["daw"].as_str().unwrap();
    assert!(!daw.is_empty());
    // The test binary is not a DAW, so it must land in the "other" bucket and
    // carry no version — the invariant the consent copy promises.
    assert_eq!(daw, "other");
    assert!(
        session[0]["properties"].get("daw_version").is_none(),
        "an unrecognised host sent a version: {session}"
    );

    let capture: serde_json::Value = serde_json::from_str(&read_request(&listener)).unwrap();
    assert_eq!(capture[0]["event"], "Capture Completed");
    assert_eq!(capture[0]["properties"]["confidence"], "0.9+");
    assert_eq!(capture[0]["properties"]["offset"], "1-10ms");
    assert_eq!(capture[0]["properties"]["capture_length"], "1-2s");
    assert_eq!(capture[0]["properties"]["splices"], "1-3");
    assert_eq!(capture[0]["properties"]["polarity_inverted"], false);

    // Both events must carry the same freshly minted, non-empty device id.
    let device_id = session[0]["properties"]["distinct_id"].as_str().unwrap();
    assert_eq!(device_id.len(), 32);
    assert_eq!(capture[0]["properties"]["distinct_id"], device_id);

    // A second capture must not re-send the session event.
    handle.track(AnalyticsEvent::CaptureRejected {
        reason: conjure_align::analysis::RejectReason::TooShort,
        capture_seconds: 0.3,
        splice_count: 0,
    });
    let second: serde_json::Value = serde_json::from_str(&read_request(&listener)).unwrap();
    assert_eq!(second[0]["event"], "Capture Rejected");
    assert_eq!(second[0]["properties"]["reason"], "too_short");
    assert_eq!(second[0]["properties"]["capture_length"], "<0.5s");

    // The consent actually landed on disk, at the real config path.
    let stored = analytics::config_path().expect("a config path under the temp HOME");
    assert!(stored.starts_with(&home), "wrote outside the temp HOME");
    let text = std::fs::read_to_string(&stored).unwrap();
    assert!(text.contains("granted"), "{text}");
}
