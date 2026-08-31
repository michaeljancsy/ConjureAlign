//! The vectored exception handler, exercised against a real access violation.
//!
//! This is the **only** place `crash::veh` ever runs before a release: there is
//! no Windows toolchain on the development machine, and the editor cannot be
//! driven in CI. It runs on `windows-latest` as part of `cargo test --release`.
//!
//! Shape: the test re-executes its own binary with a trigger variable set. The
//! child grants consent (which is what registers the handler, through the same
//! `CrashHandle::sync_consent` path a plugin instance uses), then dereferences
//! null. The parent asserts the child died and that a fault record naming
//! `0xc0000005` was left behind at the path the sweep will look for.
//!
//! Two details make it safe to run unattended:
//!
//! - The child calls `SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX)`
//!   first, so Windows Error Reporting terminates it silently instead of
//!   putting up a dialog that would hang the job.
//! - The DSN points at a closed port. `sentry::init` still has to succeed —
//!   registering the handler happens right after it — but nothing may leave
//!   the runner.
#![cfg(target_os = "windows")]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Set in the child. Its presence is what selects the crashing half below.
const TRIGGER: &str = "CONJURE_ALIGN_VEH_CRASH_CHILD";
const TEST_NAME: &str = "an_access_violation_in_our_own_image_leaves_a_fault_record";

fn sessions_dir(appdata: &Path) -> PathBuf {
    appdata
        .join("ConjureDSP")
        .join("ConjureAlign")
        .join("sessions")
}

#[test]
fn an_access_violation_in_our_own_image_leaves_a_fault_record() {
    if std::env::var_os(TRIGGER).is_some() {
        crash_now();
    }

    let appdata = std::env::temp_dir().join("conjure-align-veh-windows");
    let _ = std::fs::remove_dir_all(&appdata);
    std::fs::create_dir_all(&appdata).unwrap();

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env(TRIGGER, "1")
        .env("APPDATA", &appdata)
        // Nothing may reach the real Sentry from CI. Port 1 is closed; the
        // client comes up regardless, which is all the handler needs.
        .env(
            "CONJURE_ALIGN_SENTRY_DSN",
            "http://00000000000000000000000000000000@127.0.0.1:1/1",
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("could not re-exec the test binary");
    let pid = child.id();
    let status = child.wait().expect("the child never exited");

    assert!(
        !status.success(),
        "the child was supposed to die of an access violation, but exited cleanly"
    );

    let fault = sessions_dir(&appdata).join(format!("{pid}.fault"));
    let text = std::fs::read_to_string(&fault).unwrap_or_else(|e| {
        panic!("no fault record at {}: {e}", fault.display());
    });

    assert!(
        text.contains("code=0xc0000005"),
        "the record does not name an access violation: {text}"
    );
    assert!(
        text.contains("offset=0x"),
        "the record carries no image offset, so it cannot be symbolicated: {text}"
    );
}

/// The child half: arm crash reporting the way a plugin instance does, then
/// fault inside this image.
fn crash_now() -> ! {
    use conjure_align::analytics;
    use conjure_align::crash::CrashHandle;
    use windows_sys::Win32::System::Diagnostics::Debug::{
        SetErrorMode, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX,
    };

    // Before anything can fault: no WER dialog, no "critical error" box. A
    // dialog here would hang the CI job rather than fail it.
    // SAFETY: no pointers; the call only changes this process's error mode.
    unsafe { SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX) };

    analytics::set_consent(true);
    let handle = CrashHandle::new();
    // Registers the panic hook *and* the vectored exception handler.
    handle.sync_consent();
    assert!(analytics::enabled());
    // Leaked on purpose: dropping it would run `veh::uninstall()`, which is
    // exactly what must NOT have happened when the fault below arrives.
    std::mem::forget(handle);

    // `black_box` plus a volatile store, so neither the optimizer nor the
    // release profile can fold this into an `ud2` trap — that would raise
    // ILLEGAL_INSTRUCTION and pass for the wrong reason.
    let target = std::hint::black_box(std::ptr::null_mut::<u8>());
    // SAFETY: deliberately none. Writing through a null pointer is the whole
    // point of this test.
    unsafe { std::ptr::write_volatile(target, 1) };

    unreachable!("the access violation did not terminate the process");
}
