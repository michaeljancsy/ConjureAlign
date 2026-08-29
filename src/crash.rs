//! Opt-in crash reporting (Sentry).
//!
//! This rides the *same* consent answer as [`crate::analytics`] — one question,
//! one stored yes/no, both features — so it adds no permission surface of its
//! own. Everything here is inert until [`crate::analytics::enabled`] is true.
//!
//! Three rules shape it, two of them inherited from the analytics module and
//! one specific to panics:
//!
//! 1. **The audio thread never allocates or does I/O here.** `process()` does
//!    take a [`scope`] guard, which is a thread-local counter increment and
//!    nothing else — no allocation, no atomics, no consent read — so
//!    `assert_process_allocs` stays meaningful. Every *report* is raised from
//!    the main thread, the editor thread, or the background analysis task.
//! 2. **No thread outlives the dylib.** Sentry's transport owns a background
//!    thread tied to the client, so the client is owned by [`CrashHandle`]s
//!    through a `Weak` registry, exactly like `net::Worker`. The last
//!    plugin instance to drop closes the client and joins the thread.
//! 3. **Every panic the hook can see is ours, and every one is reported.** A
//!    panic hook lives in the panicking image's own statically-linked std, so
//!    the host's panics and other plugins' can never reach this one — there is
//!    no cross-image blanket to guard against, and a panic in the GUI event
//!    loop, a helper thread, or a dependency compiled into this dylib is
//!    still a ConjureAlign crash the user hit. The hook therefore gates on
//!    consent alone and stamps each report with an `in_scope` tag saying
//!    whether a [`Scope`] guard was held — attribution (a known callback vs
//!    shipped-but-unscoped code), not a filter. The one exception is Sentry's
//!    own `sentry-*` worker threads: those are must-not-panic (see
//!    `BoundedUreq` below), and reporting from one would capture into the
//!    very machinery that is failing.
//!
//! ## Ordering against nih-plug's own panic hook
//!
//! nih-plug installs a global hook of its own (`setup_logger()` ->
//! `log_panics()`) from the CLAP `clap_entry.init` / VST3 `bundleEntry` dylib
//! entry points, long before `Plugin::default()` runs. Ours is installed later,
//! on consent, which is what lets it chain: it takes the previous hook and
//! always calls it, so nih-plug's stderr panic logging survives. Installing
//! from a library constructor instead would run *before* nih-plug and get
//! silently replaced.

use std::cell::Cell;
use std::marker::PhantomData;

/// Sentry DSN for the "ConjureAlign" project. Like `analytics::MIXPANEL_TOKEN`,
/// a DSN is public by design — it is write-only ingestion, grants no read
/// access, and ships in every binary regardless of what we do here.
pub const SENTRY_DSN: &str =
    "https://d5c574afb565fb671e6ec70e673eedf0@o4511091371081728.ingest.us.sentry.io/4511972827136000";

/// Points the reporter at a local sink for tests and manual QA, mirroring the
/// endpoint overrides in `analytics` and `update`.
const DSN_ENV: &str = "CONJURE_ALIGN_SENTRY_DSN";

// ---------------------------------------------------------------------------
// Scope: which threads are "inside the plugin" right now
// ---------------------------------------------------------------------------
//
// Deliberately unconditional — a `Cell<u32>` costs nothing and keeping it off
// the cfg split means `lib.rs` can take a guard without any cfg of its own.

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// RAII marker saying "this thread is currently executing ConjureAlign code".
/// Not `Send`: the guard must be dropped on the thread that took it, or the
/// depth counter would drift.
#[must_use = "the scope ends as soon as the guard is dropped"]
pub struct Scope {
    _not_send: PhantomData<*const ()>,
}

/// Take a scope guard. Cheap enough for the audio thread: one thread-local
/// read and one write, no allocation and no synchronization.
pub fn scope() -> Scope {
    DEPTH.with(|d| d.set(d.get().saturating_add(1)));
    Scope {
        _not_send: PhantomData,
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Whether the calling thread is inside a [`scope`]. The panic hook stamps
/// this on every report as the `in_scope` tag: `true` means a known plugin
/// callback, `false` means somewhere else in this dylib — the GUI event loop,
/// a helper thread, a dependency's internals.
pub fn in_plugin_code() -> bool {
    DEPTH.with(|d| d.get()) > 0
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod imp {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::{Arc, Mutex, Once, OnceLock, Weak};
    use std::time::Duration;

    use sentry::protocol::{DebugImage, Event, User};
    use sentry::types::Scheme;
    use sentry::{
        ClientInitGuard, ClientOptions, Level, Transport, TransportFactory, TransportOptions,
    };

    use super::{in_plugin_code, DSN_ENV, SENTRY_DSN};
    use crate::analytics;

    /// How long a panic report may hold the panicking thread while it goes over
    /// the wire. A plugin panic usually precedes the host aborting, so a
    /// fire-and-forget send would simply be lost — but the thread is already on
    /// its way to a crash, so the wait must still be bounded and short.
    const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

    /// Matches the flush budget: `ClientInitGuard::drop` closes the client with
    /// this, which is what joins the transport thread on unload.
    const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

    /// Bounds on a single request to Sentry. These exist to bound something much
    /// less obvious than a slow report — see [`BoundedUreq`]. Between them the
    /// worst case unload stall is SHUTDOWN_TIMEOUT plus one in-flight request.
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

    /// The stock `ureq` transport with timeouts, which it otherwise has none of:
    /// sentry configures TLS and proxying on its agent but no timeouts, and every
    /// `ureq` timeout defaults to `None`.
    ///
    /// That would be a slow-report problem in an application and is a hang in a
    /// plugin. `TransportThread::drop` joins its worker with
    /// `handle.join().unwrap()` — no timeout — so an in-flight request that never
    /// completes blocks the drop forever, and the thread doing that drop is
    /// whichever host thread is unloading us. A DAW quitting against a captive
    /// portal would hang instead of quitting.
    ///
    /// Proxy handling mirrors `UreqHttpTransport`'s own agent setup. Root certs
    /// deliberately do NOT: `RootCerts` defaults to `WebPki`, which makes ureq
    /// call `disable_built_in_roots(true)` and trust a bundled Mozilla store
    /// instead of the OS one. This project picked native-tls precisely so that
    /// TLS rides the OS trust store (Security.framework / SChannel) and no
    /// bundled store can go stale — see the analytics note in Cargo.toml — so
    /// `PlatformVerifier` is set explicitly.
    struct BoundedUreq;

    /// Split out from [`BoundedUreq`] so the regression test below builds the *same*
    /// agent; a second copy of this construction would defeat the point of it.
    fn build_agent(accept_invalid_certs: bool, proxy: Option<ureq::Proxy>) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(Some(REQUEST_TIMEOUT))
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .provider(ureq::tls::TlsProvider::NativeTls)
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .disable_verification(accept_invalid_certs)
                    .build(),
            )
            .proxy(proxy)
            .build()
            .new_agent()
    }

    impl TransportFactory for BoundedUreq {
        fn create_transport_with_options(&self, options: TransportOptions) -> Arc<dyn Transport> {
            let proxy = match (
                options.dsn.scheme(),
                &options.http_proxy,
                &options.https_proxy,
            ) {
                (Scheme::Https, _, Some(proxy)) => ureq::Proxy::new(proxy).ok(),
                (_, Some(proxy), _) => ureq::Proxy::new(proxy).ok(),
                _ => None,
            };
            let agent = build_agent(options.accept_invalid_certs, proxy);
            Arc::new(
                sentry::transports::UreqHttpTransportOptions::from(options)
                    .with_agent(agent)
                    .build(),
            )
        }
    }

    /// Owns the Sentry client for as long as any plugin instance is alive.
    /// Dropping it ends the release-health session, flushes, and joins the
    /// transport thread — see rule 2 in the module docs.
    struct Reporter {
        _guard: ClientInitGuard,
    }

    impl Drop for Reporter {
        fn drop(&mut self) {
            // The session lives on the process hub (see `reporter()`), but
            // the guard's own drop ends sessions on the *dropping* thread's
            // hub — for a decline, the editor thread, which does not hold
            // it. Close it where it actually lives; this runs before the
            // guard field drops, so the final update still rides the
            // client's shutdown drain.
            sentry::Hub::main().end_session();
        }
    }

    fn options() -> ClientOptions {
        let dsn = std::env::var(DSN_ENV)
            .ok()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| SENTRY_DSN.to_owned());

        // `ClientOptions` is #[non_exhaustive], so this is field-by-field
        // rather than a struct literal.
        let mut opts = ClientOptions::default();
        opts.dsn = dsn.parse().ok();
        // Must match what `sentry-cli debug-files upload` associates the
        // dSYM/PDB with, or release builds symbolicate to nothing.
        opts.release = Some(concat!("conjure_align@", env!("CARGO_PKG_VERSION")).into());
        // Rule 3: no `PanicIntegration`. Registering it would install a
        // second, ungated hook next to ours — no consent check, no `in_scope`
        // tag, no bounded flush, and a double report per panic. The other
        // four are exactly the defaults, listed explicitly so that adding one
        // is a deliberate act.
        opts.default_integrations = false;
        opts.integrations = vec![
            Arc::new(sentry::integrations::backtrace::AttachStacktraceIntegration),
            Arc::new(sentry::integrations::debug_images::DebugImagesIntegration::default()),
            Arc::new(sentry::integrations::contexts::ContextIntegration::default()),
            Arc::new(sentry::integrations::backtrace::ProcessStacktraceIntegration),
        ];
        // What arms `AttachStacktraceIntegration` above: a `report_issue`
        // message is just a string, and without this it ships with no stack at
        // all — the integration is gated on the option. Panic events are
        // unaffected either way; theirs comes from `event_from_panic_info`,
        // and the integration skips any event that already has one.
        opts.attach_stacktrace = true;
        // The README promises no machine name and no identity beyond the random
        // install id. `send_default_pii` off is what stops Sentry backfilling
        // the client IP; `server_name` is nulled here and again in
        // `before_send`, because `sentry-contexts` fills it from the `hostname`
        // crate.
        opts.send_default_pii = false;
        opts.server_name = None;
        // nih-plug owns the global `log` logger, so there is no breadcrumb
        // source wired up at all. Pinned to zero so that stays true by
        // construction rather than by accident.
        opts.max_breadcrumbs = 0;
        // One session per process: started explicitly on the process hub in
        // `reporter()` (i.e. on consent, so only opted-in users ever have
        // one) and ended in `Reporter::drop` when the last instance unloads.
        // NOT the automatic variant: `sentry::init` would start the session
        // on the *init-calling* thread's hub, and after a consent decline →
        // re-grant that is the editor thread — the `Hub::main()` captures
        // could then never mark the session crashed. A meaningful crash-free
        // rate is the point of having sessions at all — see the note in
        // Cargo.toml. `Application` mode is what attaches the session update
        // to the same envelope as the event that changed it.
        opts.auto_session_tracking = false;
        opts.session_mode = sentry::SessionMode::Application;
        opts.shutdown_timeout = SHUTDOWN_TIMEOUT;
        opts.transport = Some(Arc::new(BoundedUreq));
        opts.before_send = Some(Arc::new(scrub));
        opts
    }

    /// Where the plugin is running, as `initialize()` saw it. Applied in
    /// `scrub` rather than through `configure_scope`, because a scope belongs
    /// to the thread that set it: `initialize()` runs on the host's main
    /// thread, while panics are captured from the audio thread and from
    /// nih-plug's `bg-worker`, which would not see it.
    fn host_context() -> &'static Mutex<Option<(String, f32)>> {
        static HOST: OnceLock<Mutex<Option<(String, f32)>>> = OnceLock::new();
        HOST.get_or_init(|| Mutex::new(None))
    }

    /// Last gate before anything leaves the machine. Everything dropped here is
    /// something the consent copy in the editor promises not to send.
    fn scrub(mut event: Event<'static>) -> Option<Event<'static>> {
        // `sentry-contexts` fills this from the `hostname` crate.
        event.server_name = None;

        // The random install id and nothing else — no username, no IP, no email.
        // The hook-safe accessor: `scrub` runs synchronously on the panicking
        // thread inside `capture_event`, under the same lock hazards as the
        // hook itself.
        event.user = analytics::device_id_in_hook().map(|id| User {
            id: Some(id),
            ..Default::default()
        });

        // `debug-images` enumerates every shared library in the process, which
        // in a DAW is every other plugin the user owns. Ours is the only one we
        // have any business knowing about, and the only one we upload symbols
        // for.
        let images = &mut event.debug_meta.to_mut().images;
        images.retain(|image| image_name(image).is_some_and(is_ours));

        // Same hazards as above: never block and never unwrap on the
        // panicking thread. On contention the report just goes out without
        // the host tags.
        let host = match host_context().try_lock() {
            Ok(guard) => guard.clone(),
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner().clone(),
            Err(std::sync::TryLockError::WouldBlock) => None,
        };
        if let Some((plugin_api, sample_rate)) = host {
            event.tags.insert("plugin_api".into(), plugin_api);
            event
                .tags
                .insert("sample_rate".into(), (sample_rate as u32).to_string());
        }

        Some(event)
    }

    fn image_name(image: &DebugImage) -> Option<&str> {
        match image {
            DebugImage::Symbolic(i) => Some(i.name.as_str()),
            DebugImage::Apple(i) => Some(i.name.as_str()),
            _ => None,
        }
    }

    /// Covers every name the one cdylib is loaded under: `ConjureAlign` inside
    /// the three bundles, `libconjure_align.dylib` / `conjure_align.dll` for
    /// tests, examples and the standalone binary.
    fn is_ours(name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        name.contains("conjure_align") || name.contains("conjurealign")
    }

    // -----------------------------------------------------------------------
    // Panic hook
    // -----------------------------------------------------------------------

    static HOOK: Once = Once::new();

    /// Whether the calling thread is one of Sentry's own workers
    /// ("sentry-transport", "sentry-session-flusher"). Those are
    /// must-not-panic (an escaped panic there is a host abort at unload —
    /// see `BoundedUreq`), and the hook must not report from them: the
    /// capture would go into the very machinery that is failing, and `flush`
    /// on the transport thread queues a barrier behind itself and waits out
    /// the full timeout. The prefix is the dependency's naming convention,
    /// not ours — pinned by `sentry_transport_thread_matches_the_hook_skip_prefix`
    /// below so a sentry upgrade that renames its workers fails a test
    /// instead of silently re-opening that path.
    fn is_sentry_internal_thread() -> bool {
        std::thread::current()
            .name()
            .is_some_and(|name| name.starts_with("sentry-"))
    }

    /// Installed once per process, and only after consent — an opted-out user's
    /// host never gets our hook at all.
    fn install_hook() {
        HOOK.call_once(|| {
            let next = std::panic::take_hook();
            // Constructed once, out here, and never registered: registering
            // it would run `Integration::setup`, which installs the second
            // ungated hook the options() comment warns about. All we want is
            // its panic-info-to-event conversion, which carries the
            // backtrace and the panic location.
            let integration = sentry::integrations::panic::PanicIntegration::default();
            std::panic::set_hook(Box::new(move |info| {
                // A panic can originate on the audio thread: `process()` holds
                // the `AtomicRefCell` borrows whose collision is the loudest
                // failure this codebase has. There, `assert_process_allocs`
                // would turn our own reporting into a second panic inside the
                // first. nih-plug's hook wraps itself for the same reason.
                nih_plug::util::permit_alloc(|| {
                    // `enabled_in_hook`, not `enabled`: the panicking frame
                    // may hold the config lock on this thread, and a blocking
                    // or poisoned `lock().unwrap()` here is a deadlock or a
                    // panic-inside-the-hook abort.
                    if !is_sentry_internal_thread() && analytics::enabled_in_hook() {
                        let mut event = integration.event_from_panic_info(info);
                        // Attribution, not a gate (rule 3): every panic
                        // reaching this hook was raised inside our dylib.
                        event
                            .tags
                            .insert("in_scope".into(), in_plugin_code().to_string());
                        // `Hub::main()`, not `Hub::current()`: a thread's own
                        // hub is a snapshot taken the first time that thread
                        // touched Sentry, and after a consent decline →
                        // re-grant it can still point at the closed client.
                        // The process hub is re-bound on every init (see
                        // `reporter()`), so it is the one place a capture is
                        // never stale.
                        let hub = sentry::Hub::main();
                        hub.capture_event(event);
                        if let Some(client) = hub.client() {
                            client.flush(Some(FLUSH_TIMEOUT));
                        }
                    }
                });
                // Always — nih-plug's hook logs the panic to stderr and to
                // NIH_LOG, and that must keep working whether or not we
                // reported anything.
                next(info);
            }));
        });
    }

    // -----------------------------------------------------------------------
    // Client lifetime
    // -----------------------------------------------------------------------

    fn registry() -> &'static Mutex<Weak<Reporter>> {
        static REGISTRY: OnceLock<Mutex<Weak<Reporter>>> = OnceLock::new();
        REGISTRY.get_or_init(|| Mutex::new(Weak::new()))
    }

    /// The process-wide client, shared through the registry. The registry
    /// mutex is held across `sentry::init` on purpose: it is what serializes
    /// concurrent grants, so the second caller upgrades the `Weak` instead of
    /// initializing a second client.
    ///
    /// `None` when the client could not be brought up. The slot is left empty
    /// so the next `sync_consent` retries, rather than pinning reporting off
    /// for the rest of the process.
    fn reporter() -> Option<Arc<Reporter>> {
        let registry = registry();
        let mut slot = registry.lock().unwrap();
        if let Some(existing) = slot.upgrade() {
            return Some(existing);
        }
        // A failed init retries, but not at the editor's frame rate: every
        // attempt builds the TLS agent, spawns and joins a transport thread,
        // and panics through nih-plug's logging hook (a backtrace per try) —
        // a 60 Hz churn loop on exactly the thread-starved machine that made
        // init fail. One attempt per backoff interval keeps the eventual
        // recovery that not latching failures off for the process buys.
        const RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);
        static LAST_FAILURE: Mutex<Option<std::time::Instant>> = Mutex::new(None);
        {
            let last = LAST_FAILURE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if last.is_some_and(|t| t.elapsed() < RETRY_BACKOFF) {
                return None;
            }
        }
        // `sentry::init` spawns two threads — the transport worker and the
        // session flusher — and both spawns `.unwrap()`, so on a machine out
        // of threads this is a *panic*, raised on whichever host thread is
        // syncing consent, unwinding out through the FFI boundary. Contain
        // it: a starved host gets no crash reporting, not an abort. Sound to
        // catch, because both spawns run inside `Client::from(opts)`, before
        // anything is bound to the global hub.
        let guard = match catch_unwind(AssertUnwindSafe(|| sentry::init(options()))) {
            Ok(guard) => guard,
            Err(_) => {
                *LAST_FAILURE
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(std::time::Instant::now());
                // Once per process: the editor re-syncs every frame, and a
                // persistent failure must not flood the log at frame rate.
                static WARNED: Once = Once::new();
                WARNED.call_once(|| {
                    nih_plug::nih_log!(
                        "ConjureAlign: starting the crash reporter panicked \
                         (could not spawn its threads?); crash reporting stays off"
                    );
                });
                return None;
            }
        };
        // Only after a successful init — a failed attempt must leave the
        // process exactly as it found it, panic hook included.
        install_hook();
        // `sentry::init` binds the client to the *calling* thread's hub only,
        // and every other thread's hub snapshots the process hub the first
        // time that thread touches Sentry — and never re-syncs. On the first
        // grant those are the same hub (this thread is the first Sentry
        // toucher in the process), but a decline → re-grant re-inits from the
        // editor thread while the process hub still holds the closed client,
        // which would silently drop every capture from the audio thread, the
        // host's main thread, and the bg-worker for the rest of the process.
        // Re-binding the process hub is what keeps `Hub::main()` — where the
        // hook and `report_issue` capture — always fresh.
        sentry::Hub::main().bind_client(sentry::Hub::current().client());
        // Sessions have the same per-hub shape (`start_session` writes the
        // *calling* thread's hub scope), so the release-health session is
        // started on the process hub too: a capture can only mark a session
        // crashed if the session lives on the capturing hub's scope.
        // `auto_session_tracking` is off in `options()` for exactly this
        // reason; the matching `end_session` is in `Reporter::drop`.
        sentry::Hub::main().start_session();
        let fresh = Arc::new(Reporter { _guard: guard });
        *slot = Arc::downgrade(&fresh);
        Some(fresh)
    }

    /// One per plugin instance. Constructing it does no I/O, starts no thread
    /// and reads no consent, so plugin scanners and opted-out users pay
    /// nothing.
    #[derive(Default)]
    pub struct CrashHandle {
        /// Holds the process-wide client alive for this instance's lifetime.
        reporter: Mutex<Option<Arc<Reporter>>>,
    }

    impl CrashHandle {
        pub fn new() -> Self {
            Self::default()
        }

        /// Bring this instance in line with the stored consent answer: arm on
        /// yes, tear down on no.
        ///
        /// Called from `initialize()` and from the editor's draw closure. The
        /// editor is where consent is actually given or withdrawn (the
        /// first-run modal and the settings popover), so syncing there is what
        /// makes a click take effect on the next frame rather than the next
        /// session — and it cannot be forgotten the way a call bolted onto
        /// each button would be.
        pub fn sync_consent(&self) {
            // Decide under the instance lock, act outside it: `initialize()`
            // (a host thread) and the editor's draw closure both funnel
            // through this mutex, and both arms below are heavyweight —
            // arming spawns Sentry's threads, disarming can run the full
            // client teardown. Neither belongs under a lock another host
            // thread convoys on. A stale decision costs one extra frame: the
            // editor re-syncs every frame, and consent only changes through
            // the editor, so the next sync converges.
            let armed = self.reporter.lock().unwrap().is_some();
            match (analytics::enabled(), armed) {
                // Built outside the instance lock, stored under a brief
                // re-lock. Racing grants (this instance's `initialize()`
                // against its editor, or another instance's) are fine: the
                // registry inside `reporter()` hands every concurrent caller
                // the same client, so the loser overwrites the slot with the
                // Arc it already holds.
                (true, false) => {
                    if let Some(fresh) = reporter() {
                        // Re-checked under the re-lock: a decline can land
                        // while `reporter()` was building (two thread spawns
                        // plus TLS agent construction), and an editor-less
                        // instance would otherwise stay armed — client,
                        // session and all — with consent declined on disk.
                        // The discarded `fresh` drops after the guard, so any
                        // teardown it triggers runs outside the lock.
                        let mut slot = self.reporter.lock().unwrap();
                        if analytics::enabled() {
                            *slot = Some(fresh);
                        } else {
                            drop(slot);
                            drop(fresh);
                        }
                    }
                }
                // Dropping the last strong reference is what closes the client,
                // ending the session and joining the transport thread. The
                // guard is a temporary that dies at the end of the `let`, so
                // that teardown runs *after* the lock is released — but it
                // still blocks this thread, bounded at SHUTDOWN_TIMEOUT plus
                // one in-flight request (~7–10 s against a blackholed
                // network). Accepted: no thread may outlive the dylib (rule 2
                // in the module docs), so handing the drop to a detached
                // thread is not an option.
                (false, true) => {
                    let stale = self.reporter.lock().unwrap().take();
                    drop(stale);
                }
                _ => {}
            }
        }

        /// Records what the report should say about *where* it happened. Both
        /// values are disclosed: sample rate already rides the analytics
        /// payload, and the plugin format (VST3/CLAP) is named in the README
        /// privacy table and the consent/settings copy. Stored
        /// unconditionally — consent may arrive later, and a report raised
        /// then should still say where it came from.
        pub fn set_host_context(&self, plugin_api: &str, sample_rate: f32) {
            // Poison-tolerant: the value is plain data, and one earlier
            // panic must not disable host tagging for the process's lifetime.
            *host_context()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some((plugin_api.to_owned(), sample_rate));
        }
    }

    /// A condition that should not happen and that we cannot see any other way.
    /// Deliberately NOT used for `RejectReason`: a rejected capture is an
    /// ordinary user outcome, already logged and already an analytics event,
    /// and routing it here would bury real crashes.
    pub fn report_issue(message: &str) {
        if !analytics::enabled() {
            return;
        }
        // Through `Hub::main()` for the same reason as the panic hook: the
        // free function captures on the calling thread's hub, whose client
        // snapshot can be stale after a consent decline → re-grant.
        sentry::Hub::main().capture_message(message, Level::Error);
    }

    #[cfg(test)]
    mod dsn_tests {
        use super::*;

        /// ureq's native-tls backend lives behind `#[cfg(feature = "native-tls")]`.
        /// Enabling only `native-tls-no-default` pulls in the native-tls *crate* but
        /// not that module, so `TlsProvider::NativeTls` has no backend and ureq
        /// panics on the first https request. Raised on sentry's transport thread,
        /// that panic becomes a host abort, because `TransportThread::drop` joins
        /// with `handle.join().unwrap()` — it crashed Ableton Live on plugin
        /// removal, and the dependency graph looked correct throughout.
        ///
        /// Deliberately offline and deterministic: point the real agent at a local
        /// listener that accepts and hangs up. A working backend gives a TLS error;
        /// a missing one panics, failing this test.
        #[test]
        fn tls_backend_is_compiled_in_not_just_the_crate() {
            use std::net::TcpListener;

            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                // Accept then hang up, so the handshake fails immediately rather
                // than waiting out REQUEST_TIMEOUT.
                for stream in listener.incoming().take(1) {
                    drop(stream);
                }
            });

            let agent = build_agent(false, None);
            let result = agent.post(&format!("https://127.0.0.1:{port}/")).send(b"");
            assert!(
                result.is_err(),
                "a bare TCP listener somehow completed a TLS handshake"
            );
        }

        /// A DSN that does not parse leaves `opts.dsn` as `None`, which makes
        /// `sentry::init` return a *disabled* client — no error, no log, nothing
        /// on the wire. Everything downstream still behaves as though reporting
        /// were on, so the only symptom is silence.
        #[test]
        fn the_shipped_dsn_parses_and_reaches_the_options() {
            assert!(
                SENTRY_DSN.parse::<sentry::types::Dsn>().is_ok(),
                "SENTRY_DSN does not parse: {SENTRY_DSN}"
            );
            let opts = options();
            let dsn = opts.dsn.expect("options() produced no DSN");
            assert_eq!(dsn.project_id().value(), "4511972827136000");
        }

        /// The panic hook skips Sentry's own worker threads by name prefix
        /// (`is_sentry_internal_thread`) — a convention owned by the sentry
        /// crate, not by us. Pin it the way the test above pins ureq's
        /// feature wiring: run a real client against a local listener, with a
        /// middleware on the transport's agent that records — from inside the
        /// request path, i.e. ON sentry's transport thread — whether the
        /// hook's predicate would skip that thread. A sentry bump that
        /// renames its workers fails here instead of silently re-opening the
        /// capture-into-failing-transport path.
        #[test]
        fn sentry_transport_thread_matches_the_hook_skip_prefix() {
            use std::net::TcpListener;

            struct Probe(Arc<Mutex<Option<(String, bool)>>>);
            impl ureq::middleware::Middleware for Probe {
                fn handle(
                    &self,
                    request: ureq::http::Request<ureq::SendBody>,
                    next: ureq::middleware::MiddlewareNext,
                ) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
                    *self.0.lock().unwrap() = Some((
                        std::thread::current().name().unwrap_or("<unnamed>").to_owned(),
                        is_sentry_internal_thread(),
                    ));
                    next.handle(request)
                }
            }

            struct ProbeFactory(Arc<Mutex<Option<(String, bool)>>>);
            impl TransportFactory for ProbeFactory {
                fn create_transport_with_options(
                    &self,
                    options: TransportOptions,
                ) -> Arc<dyn Transport> {
                    let agent = ureq::Agent::config_builder()
                        .timeout_global(Some(Duration::from_millis(500)))
                        .middleware(Probe(self.0.clone()))
                        .build()
                        .new_agent();
                    Arc::new(
                        sentry::transports::UreqHttpTransportOptions::from(options)
                            .with_agent(agent)
                            .build(),
                    )
                }
            }

            // Accept-and-hang-up, like the TLS test above: the middleware
            // records on the way IN, so the request never needs to succeed.
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                for stream in listener.incoming().take(1) {
                    drop(stream);
                }
            });

            let seen = Arc::new(Mutex::new(None));
            let mut opts = ClientOptions::default();
            opts.dsn = format!("http://00000000000000000000000000000000@127.0.0.1:{port}/1")
                .parse()
                .ok();
            opts.default_integrations = false;
            opts.transport = Some(Arc::new(ProbeFactory(seen.clone())));
            let client = sentry::Client::from(opts);
            client.capture_event(Event::default(), None);
            assert!(
                client.flush(Some(Duration::from_secs(5))),
                "the transport never processed the probe event"
            );
            let (name, skipped) = seen
                .lock()
                .unwrap()
                .clone()
                .expect("the transport thread never entered the request path");
            assert!(
                skipped,
                "sentry's transport thread is now named {name:?}, which \
                 `is_sentry_internal_thread` no longer matches — a panic on it \
                 would be captured into the failing transport itself"
            );
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod imp {
    //! No binaries ship for this platform, so there is no consent to collect
    //! and nothing to report. The API is kept so `lib.rs` and the editor need
    //! no `cfg` of their own; `Scope` above is real everywhere and simply has
    //! nothing reading it here.

    #[derive(Default)]
    pub struct CrashHandle;

    impl CrashHandle {
        pub fn new() -> Self {
            Self
        }
        pub fn sync_consent(&self) {}
        pub fn set_host_context(&self, _plugin_api: &str, _sample_rate: f32) {}
    }

    pub fn report_issue(_message: &str) {}
}

pub use imp::{report_issue, CrashHandle};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_is_off_by_default_and_nests() {
        // The panic hook stamps this on every report as the `in_scope` tag.
        // Getting it wrong in the "off" direction mislabels a known callback's
        // crash as stray; in the "on" direction it dresses a GUI-event-loop or
        // helper-thread panic up as one of the audited code paths.
        assert!(!in_plugin_code());
        {
            let _outer = scope();
            assert!(in_plugin_code());
            {
                // `process()` inside `initialize()` is not a real call graph,
                // but the editor draw closure calling into scoped helpers is,
                // and an inner guard must not end the outer one.
                let _inner = scope();
                assert!(in_plugin_code());
            }
            assert!(in_plugin_code());
        }
        assert!(!in_plugin_code());
    }

    #[test]
    fn scope_is_per_thread() {
        let _guard = scope();
        assert!(in_plugin_code());
        // A scope on the audio thread must not tag the analysis thread's
        // panics as in-scope, and vice versa.
        assert!(!std::thread::spawn(in_plugin_code).join().unwrap());
    }
}
