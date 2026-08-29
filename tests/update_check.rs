//! The properties the update check rests on: an automatic check happens only
//! once the user has said yes, a manual one happens whenever they ask and never
//! answers the question on their behalf, and neither runs more often than the
//! interval allows.
//!
//! Runs against a temporary `HOME`/`APPDATA` so it can never read or clobber
//! the developer's own preference file, and against a local TCP sink so it
//! never talks to GitHub.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use conjure_align::config;
use conjure_align::update::{self, Status, Trigger, UpdateHandle};

/// Accepts one request, returns its head, and answers with a GitHub-shaped
/// release document for `tag`. Chunked on purpose: it is what GitHub actually
/// does, and a body that is not de-framed would not parse.
fn serve_release(listener: &TcpListener, tag: &str) -> String {
    let (stream, _) = listener.accept().expect("sink accept");
    let mut reader = BufReader::new(stream);
    let mut head = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        head.push_str(&line);
        if line == "\r\n" {
            break;
        }
    }
    // `html_url` is deliberately hostile: nothing in the plugin may ever open
    // a URL that arrived over the network.
    let body = format!(
        r#"{{"tag_name":"{tag}","name":"ConjureAlign {tag}","html_url":"https://evil.example/pwned","body":"Notes 💚"}}"#
    );
    let mut stream = reader.into_inner();
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n")
        .unwrap();
    write!(stream, "{:x}\r\n{}\r\n0\r\n\r\n", body.len(), body).unwrap();
    stream.flush().unwrap();
    head
}

/// The check completes on the shared network worker, so the assertions have to
/// wait for it rather than assume it.
fn wait_for_verdict() -> Status {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = update::status();
        if status != Status::Checking {
            return status;
        }
        assert!(Instant::now() < deadline, "update check never finished");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn nothing_is_checked_automatically_until_the_user_says_yes() {
    // Set before anything touches the config or the endpoint: both are cached
    // in a OnceLock on first read. Safe here because this is the only test in
    // this binary, so no other thread is reading the environment.
    let home = std::env::temp_dir().join("conjure-align-update-e2e");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);
    std::env::set_var("APPDATA", &home);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::env::set_var(
        "CONJURE_ALIGN_UPDATE_URL",
        format!("http://127.0.0.1:{port}/releases/latest"),
    );

    let handle = UpdateHandle::new();

    // Never asked yet — that is what shows the first-run prompt, and it must
    // behave exactly like a "no" on the wire.
    assert_eq!(config::update_consent(), None);
    assert_eq!(update::status(), Status::Unknown);
    handle.check(Trigger::Auto);

    listener.set_nonblocking(true).unwrap();
    assert!(
        listener.accept().is_err(),
        "an unasked plugin checked for updates"
    );

    // Declining is likewise silent.
    config::set_update_consent(false);
    handle.check(Trigger::Auto);
    assert!(
        listener.accept().is_err(),
        "a declined plugin checked for updates"
    );
    assert_eq!(update::status(), Status::Unknown, "no check, no verdict");

    // A manual check runs regardless of the stored answer: the click is the
    // consent for this one request.
    listener.set_nonblocking(false).unwrap();
    handle.check(Trigger::Manual);
    let head = serve_release(&listener, "v9.9.9");
    assert!(
        head.starts_with("GET /releases/latest HTTP/1.1\r\n"),
        "{head}"
    );
    assert!(
        head.contains("Accept: application/vnd.github+json\r\n"),
        "{head}"
    );
    // The request must carry nothing that identifies the install.
    let device_id = config::device_id();
    assert!(device_id.is_none(), "the update path minted an identifier");
    assert!(!head.contains("Cookie"), "{head}");

    assert_eq!(
        wait_for_verdict(),
        Status::Available {
            version: "9.9.9".into()
        }
    );
    assert_eq!(update::pending_version().as_deref(), Some("9.9.9"));

    // ...and it must not have answered the question on the user's behalf.
    assert_eq!(
        config::update_consent(),
        Some(false),
        "a manual check changed the stored answer"
    );

    // Granting alone is not enough to check again: the manual check above just
    // recorded a timestamp, and the interval is what keeps a plugin that is
    // opened and closed all day down to one request.
    config::set_update_consent(true);
    listener.set_nonblocking(true).unwrap();
    handle.check(Trigger::Auto);
    assert!(
        listener.accept().is_err(),
        "an automatic check ignored the interval"
    );

    // With the last check aged out, the automatic path runs — and a release
    // that is not newer reads as up to date rather than as an update.
    listener.set_nonblocking(false).unwrap();
    config::record_update_check(config::now_secs() - 2 * config::CHECK_INTERVAL_SECS, None);
    handle.check(Trigger::Auto);
    serve_release(&listener, "v0.0.1");
    assert_eq!(wait_for_verdict(), Status::UpToDate);
    assert_eq!(update::pending_version(), None);

    // Skipping silences a version without touching the consent answer, and
    // anything newer still notifies.
    update::set_status_for_preview(Status::Available {
        version: "9.9.9".into(),
    });
    update::skip_current();
    assert_eq!(update::pending_version(), None, "skip did not take");
    update::set_status_for_preview(Status::Available {
        version: "9.9.10".into(),
    });
    assert_eq!(update::pending_version().as_deref(), Some("9.9.10"));

    // Everything landed in the temp preferences file, and nowhere else.
    let stored = config::config_path().expect("a config path under the temp HOME");
    assert!(stored.starts_with(&home), "wrote outside the temp HOME");
    let text = std::fs::read_to_string(&stored).unwrap();
    assert!(text.contains(r#""updates":"granted""#), "{text}");
    assert!(text.contains("update_last_check"), "{text}");
    assert!(text.contains(r#""update_skipped":"9.9.9""#), "{text}");
    // The analytics side must be untouched by any of this — one question's
    // answer is not the other's.
    assert!(!text.contains("device_id"), "{text}");
    assert!(!text.contains(r#""consent""#), "{text}");
}
