//! Crash reporting rides the same opt-in as analytics, and the same property
//! has to hold: nothing leaves the machine until the user says yes. Beyond
//! that, two things specific to panics are checked here — that a panic raised
//! inside ConjureAlign code is reported, and that one raised outside it is not,
//! which is the whole difference between reporting our own crashes and
//! reporting the host's.
//!
//! Runs against a temporary `HOME`/`APPDATA` so it can never read or clobber
//! the developer's own preference file, and against a local TCP sink so it
//! never talks to Sentry.
//!
//! Two panics are deliberately raised below. The hook chains to whatever was
//! installed before it — here, the test harness's — so their backtraces print
//! to stderr even when this test passes. That output is expected.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use conjure_align::analytics;
use conjure_align::crash::{self, CrashHandle};

/// Reads one HTTP request off the socket, answers it, and returns the body.
fn read_request(stream: TcpStream) -> Option<String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().ok()?;
        }
        if line == "\r\n" {
            break;
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).ok()?;
    // Answering matters: `TransportThread::drop` joins its worker with no
    // timeout, so a request left unanswered would stall the client shutdown at
    // the end of this test for as long as its request timeout allows.
    let _ = reader
        .get_mut()
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    String::from_utf8_lossy(&body).into_owned().into()
}

/// Accepts for the lifetime of the test, including during the client shutdown
/// that runs when `CrashHandle` drops.
fn spawn_sink(listener: TcpListener) -> Receiver<String> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            if let Some(body) = read_request(stream) {
                if tx.send(body).is_err() {
                    break;
                }
            }
        }
    });
    rx
}

/// A Sentry envelope is newline-delimited: a header line, then alternating
/// item-header / item-payload lines. Returns the `event` items — the only type
/// this test cares about, since release-health sessions share the stream.
fn event_items(envelope: &str) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let mut lines = envelope.lines();
    let _envelope_header = lines.next();
    while let Some(item_header) = lines.next() {
        let Some(payload) = lines.next() else { break };
        let Ok(header) = serde_json::from_str::<serde_json::Value>(item_header) else {
            continue;
        };
        if header["type"] == "event" {
            if let Ok(v) = serde_json::from_str(payload) {
                out.push(v);
            }
        }
    }
    out
}

/// Every event reaching the sink within `budget`.
fn collect_events(rx: &Receiver<String>, budget: Duration) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + budget;
    let mut events = Vec::new();
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(remaining) {
            Ok(body) => events.extend(event_items(&body)),
            Err(_) => break,
        }
    }
    events
}

/// Like `collect_events`, but stops as soon as `marker` shows up.
fn wait_for_event(
    rx: &Receiver<String>,
    marker: &str,
    budget: Duration,
) -> Option<serde_json::Value> {
    let deadline = Instant::now() + budget;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        let Ok(body) = rx.recv_timeout(remaining) else {
            break;
        };
        if let Some(hit) = event_items(&body)
            .into_iter()
            .find(|e| e.to_string().contains(marker))
        {
            return Some(hit);
        }
    }
    None
}

#[test]
fn nothing_is_reported_until_consent_is_granted() {
    // Set before anything touches the config: it is cached in a OnceLock on
    // first read. Safe here because this is the only test in this binary, so
    // no other thread is reading the environment.
    let home = std::env::temp_dir().join("conjure-align-crash-e2e");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);
    std::env::set_var("APPDATA", &home);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::env::set_var(
        "CONJURE_ALIGN_SENTRY_DSN",
        format!("http://00000000000000000000000000000000@127.0.0.1:{port}/1"),
    );
    // Belt and braces: the analytics sender is not exercised here, but if it
    // ever were, it must not reach the real Mixpanel either.
    std::env::set_var(
        "CONJURE_ALIGN_ANALYTICS_URL",
        format!("http://127.0.0.1:{port}/track"),
    );

    let rx = spawn_sink(listener);
    // Declared after `rx` so it drops first: the client shutdown it triggers
    // needs the sink still accepting.
    let handle = CrashHandle::new();

    // ---- never asked: identical to a "no" on the wire ----
    assert_eq!(analytics::consent(), None);
    handle.sync_consent();
    crash::report_issue("must not be sent while unasked");
    let panicked = catch_unwind(AssertUnwindSafe(|| {
        let _scope = crash::scope();
        panic!("must not be sent while unasked");
    }));
    assert!(panicked.is_err());
    assert!(
        collect_events(&rx, Duration::from_millis(500)).is_empty(),
        "an un-consented plugin sent something"
    );

    // ---- declined: still silent ----
    analytics::set_consent(false);
    handle.sync_consent();
    crash::report_issue("must not be sent after declining");
    assert!(
        collect_events(&rx, Duration::from_millis(500)).is_empty(),
        "a declined plugin sent something"
    );

    // ---- granted: the client comes up and reports start flowing ----
    analytics::set_consent(true);
    handle.sync_consent();
    assert!(analytics::enabled());
    handle.set_host_context("CLAP", 48_000.0);

    crash::report_issue("initialize(): capture buffers still borrowed after 500 ms");
    let issue = wait_for_event(&rx, "still borrowed", Duration::from_secs(10))
        .expect("the reported issue never reached the sink");

    // The consent copy promises no machine name and no identity beyond the
    // random install id — these are the assertions behind that promise.
    assert!(
        issue["server_name"].is_null(),
        "a machine name leaked: {}",
        issue["server_name"]
    );
    let device_id = analytics::device_id().expect("consent minted a device id");
    assert_eq!(issue["user"]["id"], device_id);
    assert!(
        issue["user"]["ip_address"].is_null(),
        "an IP address leaked: {}",
        issue["user"]
    );
    // Every image left standing must be ours; a DAW would otherwise contribute
    // one per plugin the user has loaded.
    if let Some(images) = issue["debug_meta"]["images"].as_array() {
        for image in images {
            let name = image["code_file"]
                .as_str()
                .or_else(|| image["name"].as_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            assert!(
                name.contains("conjure"),
                "a foreign debug image survived: {name}"
            );
        }
    }
    assert_eq!(
        issue["release"],
        concat!("conjure_align@", env!("CARGO_PKG_VERSION"))
    );
    // Applied in `scrub`, not through a thread-local scope — a panic captured
    // on the audio thread must carry these too.
    assert_eq!(issue["tags"]["plugin_api"], "CLAP");
    assert_eq!(issue["tags"]["sample_rate"], "48000");

    // ---- a panic inside our code is ours to report ----
    let panicked = catch_unwind(AssertUnwindSafe(|| {
        let _scope = crash::scope();
        panic!("conjure-align-in-scope-marker");
    }));
    assert!(panicked.is_err());
    let reported = wait_for_event(
        &rx,
        "conjure-align-in-scope-marker",
        Duration::from_secs(10),
    )
    .expect("a panic inside a scope was not reported");
    assert_eq!(reported["level"], "fatal");

    // ---- a panic outside it belongs to whoever raised it ----
    assert!(!crash::in_plugin_code());
    let panicked = catch_unwind(AssertUnwindSafe(|| {
        panic!("conjure-align-out-of-scope-marker");
    }));
    assert!(panicked.is_err());
    assert!(
        !collect_events(&rx, Duration::from_secs(2))
            .iter()
            .any(|e| e.to_string().contains("conjure-align-out-of-scope-marker")),
        "a panic raised outside ConjureAlign code was reported as ours"
    );

    // ---- withdrawn: the client is torn down, and reports stop ----
    analytics::set_consent(false);
    handle.sync_consent();
    crash::report_issue("must not be sent after withdrawing");
    assert!(
        collect_events(&rx, Duration::from_millis(500)).is_empty(),
        "a withdrawn plugin sent something"
    );

    // ---- re-granted: the registry's Weak is dead by now, so this exercises
    // ---- the bring-it-back-up path, not just the first init ----
    analytics::set_consent(true);
    handle.sync_consent();
    crash::report_issue("conjure-align-regrant-marker");
    wait_for_event(&rx, "conjure-align-regrant-marker", Duration::from_secs(10))
        .expect("re-granting consent did not bring reporting back");

    // The consent actually landed on disk, at the real config path.
    let stored = analytics::config_path().expect("a config path under the temp HOME");
    assert!(stored.starts_with(&home), "wrote outside the temp HOME");
}
