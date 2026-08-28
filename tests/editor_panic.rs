//! A panic raised while drawing the editor must not reach the host.
//!
//! This is not hypothetical hardening. Both editor closures are invoked from
//! an `extern "C"` frame — on macOS a CFRunLoop timer callback set up by
//! `baseview::macos::window::WindowState::setup_timer` — and a panic unwinding
//! out of one aborts the process. An arithmetic bug in `editor::view_math`
//! (`min > max, or either was NaN`, Sentry CONJUREALIGN-3) killed Ableton Live
//! 11 instantly on 2026-08-28, with no chance to save. The bug is fixed; the
//! amplification is what this test pins down.
//!
//! Two properties, both load-bearing:
//!
//! 1. The panic is *contained* — the frame is abandoned, the process lives on,
//!    and the editor latches into a state that draws a message instead of
//!    re-entering the drawing code at frame rate.
//! 2. It is still *attributed to us* — `crash`'s hook runs at the panic site,
//!    before the unwind that `guarded_frame` catches, so it still sees the
//!    `crash::scope()` guard the draw closure takes. Catching a panic must not
//!    quietly turn our crashes into ones we never hear about.
//!
//! The panics raised below are deliberate. The hook installed here chains to
//! the harness's, so their messages and backtraces print even when this test
//! passes; that output is expected.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use conjure_align::crash;
use conjure_align::editor::{guarded_frame, EditorState};

/// What `crash::in_plugin_code()` said at the moment the last panic was
/// raised — i.e. what the real hook would have decided.
static LOOKED_LIKE_OURS: AtomicBool = AtomicBool::new(false);
static PANICS_SEEN: AtomicU32 = AtomicU32::new(0);

/// One test function, not four: the hook below is process-global, so parallel
/// tests in this binary would race over what it recorded.
#[test]
fn an_editor_panic_is_contained_reported_and_latched() {
    let next = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Exactly the question `crash`'s own hook asks before reporting.
        LOOKED_LIKE_OURS.store(crash::in_plugin_code(), Ordering::SeqCst);
        PANICS_SEEN.fetch_add(1, Ordering::SeqCst);
        next(info);
    }));

    // ---- a frame that completes is left alone ----
    let mut state = EditorState::default();
    assert!(guarded_frame(&mut state, |_state| {}));
    assert!(!state.has_panicked());
    assert_eq!(PANICS_SEEN.load(Ordering::SeqCst), 0);

    // ---- a frame that panics does not take the process with it ----
    let completed = guarded_frame(&mut state, |state| {
        // Half-update the state first: the real crash panicked mid-draw, with
        // a snapshot already swapped in and its caches not yet rebuilt.
        assert!(!state.has_panicked());
        // The shape of the original: `f32::clamp` with a reversed range, from
        // a zoom window computed off a degenerate capture.
        let _ = std::hint::black_box(0.0f32).clamp(1.0, 0.0);
        unreachable!("clamp with min > max must panic");
    });
    // Reaching this line at all is the headline assertion: in the shipped
    // build the equivalent unwind reached an `extern "C"` frame and aborted.
    assert!(!completed, "a panicking frame reported completion");
    assert_eq!(PANICS_SEEN.load(Ordering::SeqCst), 1);

    // ---- and it latches, so the cost of reporting stays bounded ----
    assert!(
        state.has_panicked(),
        "a panicked frame did not latch; the next frame would re-enter the \
         drawing code, and a panic that recurs at 60 Hz is a Sentry report \
         plus a blocking flush per frame"
    );

    // ---- the hook still sees it as ours ----
    //
    // The guard belongs OUTSIDE the containment (that is how the draw closure
    // takes it): the hook runs at the panic site, before the unwind starts.
    // Were it taken inside `guarded_frame`'s body instead, this would still
    // pass — what must not happen is the guard moving to *after* the catch.
    LOOKED_LIKE_OURS.store(false, Ordering::SeqCst);
    let mut scoped = EditorState::default();
    {
        let _scope = crash::scope();
        assert!(!guarded_frame(&mut scoped, |_state| panic!(
            "conjure-align-editor-frame-marker"
        )));
    }
    assert!(
        LOOKED_LIKE_OURS.load(Ordering::SeqCst),
        "an editor panic no longer looks like ours to the crash hook, so it \
         would go unreported"
    );

    // ---- while a panic outside our code still belongs to whoever raised it ----
    assert!(!crash::in_plugin_code());
    let mut unscoped = EditorState::default();
    assert!(!guarded_frame(&mut unscoped, |_state| panic!(
        "conjure-align-editor-out-of-scope-marker"
    )));
    assert!(
        !LOOKED_LIKE_OURS.load(Ordering::SeqCst),
        "containment made a panic outside our scope look like ours"
    );

    // ---- a latched editor recovers on request ----
    assert!(scoped.has_panicked());
    scoped = EditorState::default();
    assert!(!scoped.has_panicked());
    assert_eq!(PANICS_SEEN.load(Ordering::SeqCst), 3);

    // The hook is deliberately left installed: it is the last statement's
    // worth of process state, and restoring it would need the original back
    // out of the closure that owns it.
}
