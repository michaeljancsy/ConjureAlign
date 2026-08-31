//! The session marker is the only thing that can report a crash the panic hook
//! cannot see — an access violation, a stack overflow, the host being killed.
//! This exercises the whole path end to end: a marker left behind by a process
//! that no longer exists becomes exactly one Sentry event on the next launch,
//! carrying the stage it died in, and the file is cleaned up afterwards.
//!
//! Everything runs against a temporary `HOME`/`APPDATA` and a local TCP sink,
//! so it can neither read the developer's own preference file nor talk to
//! Sentry.
//!
//! **One `#[test]` per binary, deliberately.** The consent config is cached in
//! a `OnceLock` on first read and the sweep runs once per process behind a
//! `Once`, so a second test in this binary would race the environment writes
//! and find the sweep already spent.

mod sentry_sink;

use std::time::Duration;

use conjure_align::analytics;
use conjure_align::crash::CrashHandle;
use conjure_align::session_marker::{self, MarkerHandle, Stage};
use sentry_sink::{setup, wait_for_event};

/// A process id that is definitely gone: re-run this very test binary with a
/// filter that matches nothing, so it starts, runs zero tests and exits.
/// Portable in a way that a hardcoded pid or a shell builtin is not.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "__no_such_test__"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("could not re-exec the test binary");
    let pid = child.id();
    child.wait().expect("the child never exited");
    pid
}

#[test]
fn a_marker_left_by_a_dead_process_is_reported_once_and_cleaned_up() {
    // Temp HOME and the local sink, before anything touches the consent config
    // — see `sentry_sink::setup` for the ordering invariants.
    let (_home, rx) = setup("conjure-align-session-marker");

    let dir = session_marker::sessions_dir().expect("a supported platform has a sessions dir");

    // ---- before consent: no file is written at all ----
    // Not just "no report": a user who declined must not have notes about
    // their DAW accumulating on disk either.
    let markers = MarkerHandle::new();
    markers.set_stage(Stage::Initializing);
    assert!(
        !dir.join(format!("{}.json", std::process::id())).exists(),
        "a marker was written before consent was granted"
    );

    // ---- a previous session that died mid editor-creation ----
    let pid = dead_pid();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{pid}.json")),
        format!(
            r#"{{"pid":{pid},"started_at":{},"stage":"editor_creating",
                 "plugin_version":"9.9.9","daw":"LUNA","daw_version":"2.0.5",
                 "os_version":"10.0.26200"}}"#,
            conjure_align::config::now_secs()
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join(format!("{pid}.fault")),
        "exception\tcode=0xc0000005 offset=0x00001234 scope=0\n",
    )
    .unwrap();

    // ---- consent: arming is what dispatches the sweep ----
    analytics::set_consent(true);
    // Declared after `rx` so it drops first: the client shutdown it triggers
    // needs the sink still accepting.
    let handle = CrashHandle::new();
    handle.sync_consent();
    assert!(analytics::enabled());

    let event = wait_for_event(&rx, "Unclean shutdown", Duration::from_secs(15))
        .expect("the stale session never reached the sink");

    assert_eq!(event["tags"]["stage"], "editor_creating");
    assert_eq!(event["tags"]["prev_daw"], "LUNA");
    assert_eq!(event["tags"]["prev_plugin_version"], "9.9.9");
    assert_eq!(event["tags"]["prev_daw_version"], "2.0.5");
    assert_eq!(event["tags"]["has_fault_record"], "true");
    assert!(
        event["extra"]["faults"][0]
            .as_str()
            .is_some_and(|f| f.contains("0xc0000005")),
        "the exception handler's record did not ride along: {}",
        event["extra"]["faults"]
    );

    // The crash happened in 9.9.9, not in whatever is running now — reporting
    // it against the running release is what would leave a version whose every
    // session died invisible in Sentry, which is the hole this all exists to
    // close.
    assert_eq!(event["release"], "conjure_align@9.9.9");

    // `Warning`, not `Error`. `Session::update_from_event` marks a session
    // errored at `>= Error`, and the session this is captured into belongs to
    // the *healthy* process doing the reporting — using `Error` would corrupt
    // the crash-free rate.
    assert_eq!(event["level"], "warning");

    // Cleaned up, so the next launch does not report it again.
    assert!(
        !dir.join(format!("{pid}.json")).exists(),
        "the swept marker is still on disk and would be re-reported forever"
    );
    assert!(
        !dir.join(format!("{pid}.fault")).exists(),
        "the swept fault record is still on disk"
    );

    // ---- and now that consent exists, our own marker does get written ----
    markers.set_stage(Stage::Initialized);
    let ours = dir.join(format!("{}.json", std::process::id()));
    assert!(ours.exists(), "no marker for the running process");
    let text = std::fs::read_to_string(&ours).unwrap();
    assert!(text.contains("\"stage\":\"initialized\""), "{text}");

    // A clean teardown removes it — which is what makes a *surviving* marker
    // mean something.
    drop(markers);
    assert!(
        !ours.exists(),
        "the marker outlived its last handle, so a clean exit would look like a crash"
    );

    drop(handle);
}
