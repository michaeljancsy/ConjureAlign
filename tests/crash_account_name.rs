//! A crash report must not say whose computer it came from. ConjureAlign can be
//! installed in the user's own folder, which is named after their account, so
//! this runs a copy of itself from under a made-up account's folder, has it send
//! every kind of crash report to a local sink, and looks for the account's name
//! in everything that arrives.
//!
//! The copy runs twice, because an unclean shutdown is reported by the next run.
//! One `#[test]` per binary, for the reasons in `session_marker.rs`.
//!
//! The panic is deliberate, so its backtrace on stderr is expected.

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
        Ok("die") => die_with_the_plugin_loaded(),
        Ok("report") => send_every_kind_of_report(),
        _ => run_the_copy_and_look_for_the_account_name(),
    }
}

fn die_with_the_plugin_loaded() {
    analytics::set_consent(true);
    let markers = MarkerHandle::new();
    markers.set_stage(Stage::Initialized);
    // Not a return, which would drop the handle and count as a clean shutdown.
    std::process::exit(0);
}

fn send_every_kind_of_report() {
    analytics::set_consent(true);
    let handle = CrashHandle::new();
    // Arming also reports the run that died.
    handle.sync_consent();
    handle.set_host_context("CLAP", 48_000.0);
    crash::report_issue(ISSUE);
    let panicked = catch_unwind(AssertUnwindSafe(|| {
        let _scope = crash::scope();
        panic!("{PANIC}");
    }));
    assert!(panicked.is_err());
    // Until the parent has seen them all: dropping the handle ends the session,
    // and the message that sends has to be last.
    let _ = std::io::stdin().read_line(&mut String::new());
    drop(handle);
}

fn run_the_copy_and_look_for_the_account_name() {
    // `setup` points HOME and APPDATA at this folder, and the copies inherit it.
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

    assert!(start("die").wait().unwrap().success());

    let mut reporter = start("report");
    let mut sent = Vec::new();
    for expected in ["Unclean shutdown", ISSUE, PANIC] {
        while !sent.iter().any(|body: &String| body.contains(expected)) {
            let body = rx
                .recv_timeout(Duration::from_secs(15))
                .unwrap_or_else(|_| panic!("no report containing {expected:?} arrived"));
            sent.push(body);
        }
    }
    drop(reporter.stdin.take());
    assert!(reporter.wait().unwrap().success());
    while let Ok(body) = rx.recv_timeout(Duration::from_secs(1)) {
        sent.push(body);
    }

    for body in &sent {
        assert!(
            !body.contains(ACCOUNT),
            "the account name was sent:\n{body}"
        );
    }
}
