//! Withdrawing and re-granting consent must bring reporting back for EVERY
//! thread, not just the one the checkbox was clicked on.
//!
//! The hazard is in sentry's hub model: `sentry::init` binds the client to
//! the *calling* thread's hub only, and every other thread's hub is a
//! snapshot of the process hub taken the first time that thread touched
//! Sentry — never re-synced. The first grant of a process is safe (the init
//! thread is the first Sentry toucher, so it IS the process hub). But when
//! consent was already on disk, the first init runs in `initialize()` on a
//! host thread, and a later decline → re-grant runs on the editor thread —
//! whose hub is such a snapshot. Without `reporter()` re-binding the process
//! hub (and the hook capturing through `Hub::main()`), the re-granted client
//! would be visible only to the editor thread, and panics on the audio
//! thread, the host's main thread, and the bg-worker would be captured into
//! the closed first client and silently dropped for the rest of the process.
//!
//! Three threads below play the roles: a spawned one stands in for the host
//! thread that runs `initialize()` (first init), the test's main thread for
//! the editor (decline + re-grant), and a long-lived worker for the audio
//! thread (panics before AND after the re-grant; both must reach the sink).
//! Release-health sessions have the same per-hub shape (`start_session`
//! writes the calling thread's hub), so the test also asserts the
//! post-re-grant crash envelope carries a crashed session update — the
//! crash-free rate depends on it.
//!
//! Runs against a temporary `HOME`/`APPDATA` and a local TCP sink, like
//! `crash_consent.rs`. Two panics are deliberately raised; their backtraces
//! printing to stderr is expected.

mod sentry_sink;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::channel;
use std::time::Duration;

use conjure_align::analytics;
use conjure_align::crash::{self, CrashHandle};
use sentry_sink::{setup, wait_for_body, wait_for_event};

#[test]
fn a_regrant_from_another_thread_restores_reporting_process_wide() {
    // Temp HOME and the local sink, before anything touches the consent
    // config — see `sentry_sink::setup` for the ordering invariants.
    let (_home, rx) = setup("conjure-align-crash-regrant");
    // After `rx`, so its drop-time client shutdown finds the sink accepting.
    let handle = CrashHandle::new();

    // Consent already on disk — the returning-user shape that makes the first
    // init run on a host thread rather than the editor's.
    analytics::set_consent(true);

    // ---- "initialize()": first init, NOT on this thread ----
    // This spawned thread becomes the first Sentry toucher in the process,
    // i.e. the owner of the process hub. Everything after here happens on
    // other threads, which is the whole point.
    std::thread::scope(|s| {
        s.spawn(|| handle.sync_consent()).join().unwrap();
    });

    // ---- the "audio thread": alive across the whole consent cycle ----
    let (to_audio, audio_gate) = channel::<()>();
    let (to_main, main_gate) = channel::<()>();
    let audio = std::thread::spawn(move || {
        // Under the first client: materializes this thread's view of Sentry
        // exactly the way a real pre-regrant panic would.
        let panicked = catch_unwind(AssertUnwindSafe(|| {
            let _scope = crash::scope();
            panic!("conjure-align-regrant-before-marker");
        }));
        assert!(panicked.is_err());
        to_main.send(()).unwrap();
        // Parked while the "editor" withdraws and re-grants.
        audio_gate.recv().unwrap();
        let panicked = catch_unwind(AssertUnwindSafe(|| {
            let _scope = crash::scope();
            panic!("conjure-align-regrant-after-marker");
        }));
        assert!(panicked.is_err());
    });

    main_gate.recv().unwrap();
    wait_for_event(
        &rx,
        "conjure-align-regrant-before-marker",
        Duration::from_secs(10),
    )
    .expect("a panic before the consent cycle was not reported at all");

    // ---- the "editor": withdraw, then re-grant, on this thread ----
    // The decline tears the first client down; the re-grant re-inits from
    // here — a thread whose hub is a snapshot, not the process hub.
    analytics::set_consent(false);
    handle.sync_consent();
    analytics::set_consent(true);
    handle.sync_consent();

    // ---- the audio thread must be reportable again, not just this one ----
    to_audio.send(()).unwrap();
    audio.join().unwrap();
    let body = wait_for_body(
        &rx,
        "conjure-align-regrant-after-marker",
        Duration::from_secs(10),
    )
    .expect(
        "a panic on another thread after a consent re-grant was silently \
         dropped — the re-granted client is bound only to the editor thread's \
         hub (Hub::main() re-bind missing?)",
    );
    // The crash must also mark the re-granted session: the session update
    // rides the same envelope as the event that changed it, and a capture
    // can only flip a session that lives on the capturing hub's scope —
    // which is why `reporter()` starts sessions on `Hub::main()` instead of
    // letting `sentry::init` start one on whichever thread re-granted.
    assert!(
        body.contains("\"status\":\"crashed\""),
        "the crash event arrived but its envelope carries no crashed session \
         update — the re-granted session is not on Hub::main(): {body}"
    );
}
