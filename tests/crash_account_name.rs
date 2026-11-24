//! A crash report must not say whose computer it came from. A per-user install
//! of ConjureAlign is inside the user's home folder, which is named after their
//! account, so this runs a copy of itself from under a made-up account's home,
//! has it send crash reports to a local sink, and looks for the account's name
//! in the body of everything that arrives.
//!
//! What it sends: an error report, a panic, the report of an unclean shutdown
//! (which the next run makes), and the session messages of a run that crashes
//! and one that ends normally. It does not exercise fault records, counts of
//! dropped reports, request headers, or usage analytics.
//!
//! One `#[test]` per binary, for the reasons in `session_marker.rs`. The panic
//! is deliberate, so its backtrace on stderr is expected.

mod sentry_sink;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::process::{Command, Stdio};
use std::time::Duration;

use conjure_align::analytics;
use conjure_align::crash::{self, CrashHandle};
use conjure_align::session_marker::{MarkerHandle, Stage};
use sentry_sink::setup;

const ACCOUNT: &str = "somebodys-account";
const ROLE: &str = "CONJURE_ALIGN_TEST_ROLE";
const ISSUE: &str = "conjure-align-account-name-issue";
const PANIC: &str = "conjure-align-account-name-panic";

#[test]
fn no_crash_report_contains_the_account_name() {
    match std::env::var(ROLE).as_deref() {
        Ok("end normally") => end_normally(),
        Ok("die") => die_with_the_plugin_loaded(),
        Ok("report") => send_the_reports(),
        _ => run_the_copy_and_look_for_the_account_name(),
    }
}

fn end_normally() {
    analytics::set_consent(true);
    let handle = CrashHandle::new();
    handle.sync_consent();
    drop(handle);
}

fn die_with_the_plugin_loaded() {
    analytics::set_consent(true);
    let markers = MarkerHandle::new();
    markers.set_stage(Stage::Initialized);
    // Not a return, which would drop the handle and count as a clean shutdown.
    std::process::exit(0);
}

fn send_the_reports() {
    analytics::set_consent(true);
    let handle = CrashHandle::new();
    // Arming also reports the run that died, from a background thread.
    handle.sync_consent();
    handle.set_host_context("CLAP", 48_000.0);
    crash::report_issue(ISSUE);
    let panicked = catch_unwind(AssertUnwindSafe(|| {
        let _scope = crash::scope();
        panic!("{PANIC}");
    }));
    assert!(panicked.is_err());
    // Until the parent has every report. Dropping the handle closes the client,
    // and the report of the run that died may not have been made yet.
    let _ = std::io::stdin().read_line(&mut String::new());
    drop(handle);
}

fn run_the_copy_and_look_for_the_account_name() {
    // Points HOME and APPDATA at this folder and both reporting addresses at the
    // local sink. The copies inherit all of it, which is what keeps them off the
    // real home folder and the real Sentry.
    let (home, rx) = setup(&format!("conjure-align-account-name/{ACCOUNT}"));
    let program = home
        .join("Library/Audio/Plug-Ins/CLAP/ConjureAlign.clap/Contents/MacOS")
        .join(format!("ConjureAlign{}", std::env::consts::EXE_SUFFIX));
    std::fs::create_dir_all(program.parent().unwrap()).unwrap();
    std::fs::copy(std::env::current_exe().unwrap(), &program).unwrap();

    let start = |role: &str| {
        Command::new(&program)
            .args(["--exact", "no_crash_report_contains_the_account_name"])
            .env(ROLE, role)
            .env("USER", ACCOUNT)
            .env("LOGNAME", ACCOUNT)
            .env("USERNAME", ACCOUNT)
            .env("USERPROFILE", &home)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("could not start the copy")
    };

    assert!(start("end normally").wait().unwrap().success());
    assert!(start("die").wait().unwrap().success());

    let mut reporter = start("report");
    let mut sent: Vec<String> = Vec::new();
    for expected in [
        r#""status":"exited""#,
        "Unclean shutdown",
        ISSUE,
        PANIC,
        r#""status":"crashed""#,
        // The library list, which is what carried the folder.
        r#""name":"ConjureAlign"#,
    ] {
        while !sent.iter().any(|body| body.contains(expected)) {
            let body = rx
                .recv_timeout(Duration::from_secs(15))
                .unwrap_or_else(|_| panic!("nothing containing {expected} arrived"));
            assert!(
                !body.contains(ACCOUNT),
                "the account name was sent:\n{body}"
            );
            sent.push(body);
        }
    }
    drop(reporter.stdin.take());
    assert!(reporter.wait().unwrap().success());
    while let Ok(body) = rx.recv_timeout(Duration::from_secs(1)) {
        assert!(
            !body.contains(ACCOUNT),
            "the account name was sent:\n{body}"
        );
    }

    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}
