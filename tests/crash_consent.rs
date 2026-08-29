//! Crash reporting rides the same opt-in as analytics, and the same property
//! has to hold: nothing leaves the machine until the user says yes. Beyond
//! that, two things specific to panics are checked here — that a panic raised
//! anywhere in this dylib is reported once consent is granted (the hook is
//! per-image, so the host's panics can never reach it), and that each report
//! carries the `in_scope` tag saying whether a `crash::scope()` guard was
//! held, which is what separates a known callback's crash from one in the GUI
//! event loop or a helper thread.
//!
//! Runs against a temporary `HOME`/`APPDATA` so it can never read or clobber
//! the developer's own preference file, and against a local TCP sink so it
//! never talks to Sentry.
//!
//! Three panics are deliberately raised below. The hook chains to whatever was
//! installed before it — here, the test harness's — so their backtraces print
//! to stderr even when this test passes. That output is expected.

mod sentry_sink;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Duration;

use conjure_align::analytics;
use conjure_align::crash::{self, CrashHandle};
use sentry_sink::{collect_events, exactly_one_event, setup, wait_for_event};

#[test]
fn nothing_is_reported_until_consent_is_granted() {
    // Temp HOME and the local sink, before anything touches the consent
    // config — see `sentry_sink::setup` for the ordering invariants.
    let (home, rx) = setup("conjure-align-crash-e2e");
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
    // `attach_stacktrace`: a `report_issue` message is just a string, and the
    // stack is what turns "still borrowed" into a place in the code. Without
    // the option the AttachStacktrace integration is inert and the event
    // arrives with no stacktrace at all.
    let frames = issue["threads"]["values"][0]["stacktrace"]["frames"]
        .as_array()
        .expect("a reported issue carried no stacktrace");
    assert!(
        !frames.is_empty(),
        "a reported issue had an empty stacktrace"
    );

    // ---- a panic inside a scope is reported and tagged as a known callback ----
    //
    // `exactly_one_event`, not `wait_for_event`: exactly one report per panic
    // is what pins "Rule 3" — a second event for the same marker means a
    // blanket hook (a registered PanicIntegration) is reporting next to ours.
    let panicked = catch_unwind(AssertUnwindSafe(|| {
        let _scope = crash::scope();
        panic!("conjure-align-in-scope-marker");
    }));
    assert!(panicked.is_err());
    let reported = exactly_one_event(
        &rx,
        "conjure-align-in-scope-marker",
        Duration::from_secs(10),
    );
    assert_eq!(reported["level"], "fatal");
    assert_eq!(reported["tags"]["in_scope"], "true");

    // ---- a panic outside any scope is still ours — only this dylib's panics
    // ---- can reach the hook — but is tagged as unscoped ----
    assert!(!crash::in_plugin_code());
    let panicked = catch_unwind(AssertUnwindSafe(|| {
        panic!("conjure-align-out-of-scope-marker");
    }));
    assert!(panicked.is_err());
    let stray = exactly_one_event(
        &rx,
        "conjure-align-out-of-scope-marker",
        Duration::from_secs(10),
    );
    assert_eq!(stray["level"], "fatal");
    assert_eq!(stray["tags"]["in_scope"], "false");

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
