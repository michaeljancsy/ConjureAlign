//! A local stand-in for Sentry's ingestion endpoint, shared by the crash
//! tests (`crash_consent.rs`, `crash_regrant_threads.rs`). Each test binary
//! points `CONJURE_ALIGN_SENTRY_DSN` at one of these so nothing ever talks to
//! the real Sentry.
//!
//! Each binary compiles its own copy and uses a subset, hence the allow.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

/// Everything a crash test binary needs before it touches any plugin code: a
/// temporary `HOME`/`APPDATA` (the consent config is cached in a `OnceLock`
/// on first read, so this must run first — safe because each caller is the
/// only test in its binary, so nothing races the environment writes), and
/// both endpoint overrides pointed at one local sink. The DSN override is
/// the load-bearing line: without it `options()` falls back to the real
/// production DSN, and a test would pass while shipping its deliberately
/// raised panics to Sentry. The analytics override is belt and braces — the
/// sender isn't exercised by these tests, but if it ever were, it must not
/// reach the real Mixpanel either.
///
/// Returns the temp home and the sink's receiver. Create the `CrashHandle`
/// AFTER binding the receiver, so the handle drops first and the client
/// shutdown it triggers still finds the sink accepting.
pub fn setup(dir_name: &str) -> (PathBuf, Receiver<String>) {
    let home = std::env::temp_dir().join(dir_name);
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
    std::env::set_var(
        "CONJURE_ALIGN_ANALYTICS_URL",
        format!("http://127.0.0.1:{port}/track"),
    );
    (home, spawn_sink(listener))
}

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
    // the end of a test for as long as its request timeout allows.
    let _ = reader
        .get_mut()
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    String::from_utf8_lossy(&body).into_owned().into()
}

/// Accepts for the lifetime of the test, including during the client shutdown
/// that runs when `CrashHandle` drops.
pub fn spawn_sink(listener: TcpListener) -> Receiver<String> {
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
/// these tests care about, since release-health sessions share the stream.
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
pub fn collect_events(rx: &Receiver<String>, budget: Duration) -> Vec<serde_json::Value> {
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
pub fn wait_for_event(
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

/// Like `wait_for_event`, but returns the whole raw envelope body containing
/// `marker` — for asserting on non-event items, e.g. the release-health
/// session update that rides the same envelope as the crash event that
/// changed it.
pub fn wait_for_body(rx: &Receiver<String>, marker: &str, budget: Duration) -> Option<String> {
    let deadline = Instant::now() + budget;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        let Ok(body) = rx.recv_timeout(remaining) else {
            break;
        };
        if body.contains(marker) {
            return Some(body);
        }
    }
    None
}

/// The single event containing `marker`, asserting no duplicate follows. The
/// duplicate check is load-bearing, not pedantry: a second report for the
/// same panic is the signature of a blanket hook reporting next to the crash
/// module's own — e.g. sentry's `PanicIntegration` accidentally *registered*
/// instead of merely constructed ("Rule 3" in `src/crash.rs`'s `options()`).
/// Both hooks run before the panic's unwind continues, so a duplicate is
/// already in flight when the first event is read back; a short drain window
/// is enough to catch it deterministically.
pub fn exactly_one_event(
    rx: &Receiver<String>,
    marker: &str,
    budget: Duration,
) -> serde_json::Value {
    let first = wait_for_event(rx, marker, budget)
        .unwrap_or_else(|| panic!("no event containing {marker:?} reached the sink"));
    let duplicates = collect_events(rx, Duration::from_secs(2))
        .into_iter()
        .filter(|e| e.to_string().contains(marker))
        .count();
    assert_eq!(
        duplicates, 0,
        "a second event for {marker:?} arrived — a blanket panic hook \
         (a registered PanicIntegration?) is double-reporting"
    );
    first
}
