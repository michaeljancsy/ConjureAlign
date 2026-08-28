//! A local stand-in for Sentry's ingestion endpoint, shared by the crash
//! tests (`crash_consent.rs`, `crash_regrant_threads.rs`). Each test binary
//! points `CONJURE_ALIGN_SENTRY_DSN` at one of these so nothing ever talks to
//! the real Sentry.
//!
//! Each binary compiles its own copy and uses a subset, hence the allow.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

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
