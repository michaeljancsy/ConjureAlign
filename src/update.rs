//! Opt-in update checking: "is there a newer release than the one running?"
//!
//! It **notifies and nothing else**. No download, no install, no self-update.
//! The bundle is mapped into a running host, the install location is
//! `/Library/Audio/Plug-Ins` (admin rights), and the shipped installer is
//! signed, notarized and stapled — re-implementing any part of that inside a
//! plugin would be a security surface with no upside. The user runs the `.pkg`
//! themselves, exactly as they did the first time.
//!
//! Four rules, each load-bearing:
//!
//! 1. **Nothing happens before the user answers.** The update question is its
//!    own tri-state answer in [`crate::config`], separate from the
//!    analytics/crash one, and asked in the same first-run prompt. An
//!    *automatic* check requires a granted answer. A *manual* check (the "Check
//!    now" button) runs regardless, because the click is the consent for that
//!    one request — and it never writes an answer the user did not give.
//! 2. **Checks are triggered from the editor only, never from
//!    `initialize()`.** `auval`, `pluginval` and Logic's plugin scan
//!    instantiate and initialize the plugin headlessly, with no window; a
//!    network request during a plugin scan is bad manners and can slow it down.
//!    This is the same reasoning that forbids a native consent dialog.
//! 3. **The link is a compile-time constant, never a URL from the response.**
//!    Clicking it hands a URL to the OS browser, so a URL that arrived over the
//!    network — from a MITM, a hijacked endpoint, a poisoned cache — must never
//!    reach that call. The release feed is read for a *version string* and
//!    nothing else, and the version is parsed into three integers before it is
//!    used for anything.
//! 4. **Everything else is [`crate::net`]'s rules.** One shared worker, held
//!    alive per-instance by [`UpdateHandle`] and joined at unload; every stage
//!    of the request bounded; a failure is silent.
//!
//! Nothing about the user is sent. The request carries no identifier and no
//! payload — GitHub sees it the way it would see a browser opening the releases
//! page.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::config;
use crate::net::{self, Endpoint};

/// The release feed. `/releases/latest` already excludes drafts and
/// pre-releases, so un-advertising a bad release means marking it pre-release —
/// which is the right thing to do to it anyway.
const DEFAULT_ENDPOINT: &str = "https://api.github.com/repos/michaeljancsy/ConjureAlign/releases/latest";
/// Points the check at a local sink for tests and manual QA, mirroring
/// `analytics`'s and `crash`'s overrides.
const ENDPOINT_ENV: &str = "CONJURE_ALIGN_UPDATE_URL";

/// Where the user is sent to get the new version. A constant, on purpose — see
/// rule 3 in the module docs. It always resolves to the newest release, so it
/// stays correct even if a newer one lands between the check and the click.
pub const RELEASES_URL: &str = "https://github.com/michaeljancsy/ConjureAlign/releases/latest";

/// A GitHub release document is mostly release notes, which can run to several
/// kilobytes of Markdown. The default cap is sized for a status reply; using it
/// here would truncate the JSON mid-document and turn a perfectly good response
/// into a parse failure.
const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// What the editor shows. Deliberately has no "an update exists but I have not
/// looked" state beyond [`Status::Unknown`]: a check either has a verdict or
/// has not run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Status {
    /// No check has produced a verdict, and nothing newer is cached from a
    /// previous run. Renders as no status line at all.
    #[default]
    Unknown,
    Checking,
    UpToDate,
    /// A newer release exists. Carries only the version — the link is
    /// [`RELEASES_URL`], never anything from the response.
    Available { version: String },
    /// The last check did not produce an answer. Surfaced only for a manual
    /// check, where the user is waiting for one.
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The editor opened. Requires a granted answer and respects the interval.
    Auto,
    /// The user clicked "Check now". Bypasses both.
    Manual,
}

/// The version this binary was built as.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// ---------------------------------------------------------------------------
// Version comparison (pure)
// ---------------------------------------------------------------------------

/// `MAJOR.MINOR.PATCH`, with an optional `v` prefix. Anything else — a
/// pre-release suffix, build metadata, a four-part version, a non-number — is
/// `None`, and every caller treats `None` as "do not notify". Refusing to order
/// what we cannot parse is the safe direction: the cost is a missed
/// notification, and the alternative is announcing a "new version" that is
/// older, or does not exist.
pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim();
    let s = s.strip_prefix('v').or_else(|| s.strip_prefix('V')).unwrap_or(s);
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Whether `candidate` is a strictly newer release than `current`. Numeric
/// per component, so `1.10.0` beats `1.9.0` — the bug a string compare would
/// have.
pub fn is_newer(current: &str, candidate: &str) -> bool {
    match (parse_version(current), parse_version(candidate)) {
        (Some(c), Some(n)) => n > c,
        _ => false,
    }
}

/// Pulls the version out of a GitHub release document. Only `tag_name` is
/// read; `html_url` is deliberately ignored (rule 3).
pub fn parse_release(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let tag = v.get("tag_name")?.as_str()?;
    // Round-trips through the parser so nothing that failed to parse can reach
    // the UI or the config file as a "version".
    let (major, minor, patch) = parse_version(tag)?;
    Some(format!("{major}.{minor}.{patch}"))
}

// ---------------------------------------------------------------------------
// Process-wide status
// ---------------------------------------------------------------------------

fn status_slot() -> &'static Mutex<Status> {
    static STATUS: OnceLock<Mutex<Status>> = OnceLock::new();
    STATUS.get_or_init(|| {
        // Seed from the last successful check so a known update shows the
        // instant the window opens, instead of after a network round-trip.
        let cfg = config::snapshot();
        let seeded = match cfg.update_latest_seen {
            Some(v) if is_newer(current_version(), &v) => Status::Available { version: v },
            // Not `UpToDate`: a cached version that is not newer says nothing
            // about whether it is still the latest.
            _ => Status::Unknown,
        };
        Mutex::new(seeded)
    })
}

fn set_status(status: Status) {
    *status_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = status;
}

pub fn status() -> Status {
    status_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Whether an update is known *and* the user has not asked to skip it — i.e.
/// whether the ⚙ button should carry its label. Anything newer than a skipped
/// version notifies again.
pub fn notifies(status: &Status, skipped: Option<&str>) -> bool {
    let Status::Available { version } = status else {
        return false;
    };
    match skipped {
        Some(skipped) => match (parse_version(skipped), parse_version(version)) {
            (Some(s), Some(v)) => v > s,
            // Fails open. An `update_skipped` we cannot order is corrupt, or
            // was written by some future version of this plugin; silencing a
            // real update over it would be a bug with no way for the user to
            // see or clear it, whereas an unwanted notification is one click.
            _ => true,
        },
        None => true,
    }
}

/// Convenience for the editor: the version to shout about, if any. Called once
/// per frame from the control bar, so the overwhelmingly common answer — no
/// update — costs one lock and no allocation.
pub fn pending_version() -> Option<String> {
    let status = status();
    let Status::Available { version } = &status else {
        // The overwhelmingly common answer, and it must stay cheap: one lock,
        // no config read, no allocation.
        return None;
    };
    notifies(&status, config::update_skipped().as_deref()).then(|| version.clone())
}

/// Stop notifying about the version currently on offer. Anything newer still
/// will — which is what makes a persistent notification acceptable for someone
/// deliberately pinned to an old build.
pub fn skip_current() {
    if let Status::Available { version } = status() {
        config::set_update_skipped(Some(version));
    }
}

/// Preview-only: force a status so `examples/gui_preview.rs` can render the
/// update affordances without a network. The same escape hatch
/// `editor::open_settings_popup` is.
#[doc(hidden)]
pub fn set_status_for_preview(status: Status) {
    set_status(status);
}

// ---------------------------------------------------------------------------
// The check
// ---------------------------------------------------------------------------

fn endpoint() -> Option<&'static Endpoint> {
    static ENDPOINT: OnceLock<Option<Endpoint>> = OnceLock::new();
    ENDPOINT
        .get_or_init(|| {
            std::env::var(ENDPOINT_ENV)
                .ok()
                .as_deref()
                .and_then(net::parse_endpoint)
                .or_else(|| net::parse_endpoint(DEFAULT_ENDPOINT))
        })
        .as_ref()
}

/// One in-flight check per process. Two editor windows opening together would
/// otherwise fire two identical requests at a rate-limited endpoint.
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Released when the queued check is finished with — *including* when it is
/// dropped without ever running, which is what the shared worker does to
/// anything still queued at shutdown, and to anything offered while its queue
/// is full.
///
/// Without this, either case would leave `IN_FLIGHT` set for the life of the
/// process (no further check, ever) and the status stuck on `Checking` — which
/// the editor treats as "animate", so the window would repaint at full rate
/// forever. Tying both to a guard makes the unhappy paths self-correcting:
/// dropping the closure drops the guard.
struct CheckGuard;

impl Drop for CheckGuard {
    fn drop(&mut self) {
        {
            let mut slot = status_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Only fires when the job never reached its own `set_status`.
            if *slot == Status::Checking {
                *slot = Status::Failed;
            }
        }
        IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// One per plugin instance. Its only job is to hold the shared network worker
/// alive for the instance's lifetime — a check queued by an instance that is
/// being torn down still completes, and the worker is joined when the last
/// instance goes.
#[derive(Default)]
pub struct UpdateHandle {
    worker: net::WorkerHandle,
}

impl UpdateHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs a check if this trigger is allowed to. Returns immediately; the
    /// result lands in [`status`].
    pub fn check(&self, trigger: Trigger) {
        if !config::is_supported() {
            return;
        }
        if trigger == Trigger::Auto {
            let cfg = config::snapshot();
            if cfg.updates != Some(true) {
                return;
            }
            if !config::should_check(
                config::now_secs(),
                cfg.update_last_check,
                cfg.update_last_ok,
            ) {
                return;
            }
        }
        let Some(endpoint) = endpoint() else {
            return;
        };
        if IN_FLIGHT
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        set_status(Status::Checking);

        let guard = CheckGuard;
        self.worker.spawn_job(move || {
            // Moved in so that a job dropped unrun still releases it.
            let _guard = guard;
            let latest = fetch(endpoint);
            config::record_update_check(config::now_secs(), latest.clone());
            set_status(match latest {
                Some(version) if is_newer(current_version(), &version) => {
                    Status::Available { version }
                }
                Some(_) => Status::UpToDate,
                None => Status::Failed,
            });
        });
    }
}

/// The whole network side, in one place: everything that can go wrong here —
/// no route, a rate-limit 403, a body that is not the document we expected —
/// collapses to `None`, i.e. "no answer this time".
fn fetch(endpoint: &Endpoint) -> Option<String> {
    let response = net::get(
        endpoint,
        &[
            ("Accept", "application/vnd.github+json"),
            ("X-GitHub-Api-Version", "2022-11-28"),
        ],
        MAX_RESPONSE_BYTES,
    )
    .ok()?;
    if response.status != 200 {
        nih_plug::nih_log!(
            "ConjureAlign: update check returned HTTP {}",
            response.status
        );
        return None;
    }
    parse_release(&response.body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_with_or_without_the_tag_prefix() {
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("  v1.2.3  "), Some((1, 2, 3)));
        assert_eq!(parse_version("0.0.0"), Some((0, 0, 0)));
    }

    #[test]
    fn unparseable_versions_are_refused_rather_than_guessed() {
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("1.2.3-beta"), None);
        assert_eq!(parse_version("1.2.x"), None);
        assert_eq!(parse_version("latest"), None);
        assert_eq!(parse_version("-1.0.0"), None);
    }

    #[test]
    fn newer_is_compared_numerically_not_lexically() {
        assert!(is_newer("1.9.0", "1.10.0"), "the classic string-compare bug");
        assert!(is_newer("1.1.0", "2.0.0"));
        assert!(is_newer("1.1.0", "1.2.0"));
        assert!(is_newer("1.1.0", "1.1.1"));
        assert!(!is_newer("1.1.0", "1.1.0"), "equal is not newer");
        assert!(!is_newer("1.2.0", "1.1.9"));
        assert!(!is_newer("2.0.0", "1.99.99"));
    }

    #[test]
    fn an_unorderable_version_never_notifies() {
        assert!(!is_newer("1.1.0", "banana"));
        assert!(!is_newer("banana", "9.9.9"));
        assert!(!is_newer("1.1.0", "2.0.0-rc1"));
    }

    #[test]
    fn the_shipped_version_is_orderable() {
        // If this ever fails, every comparison above silently returns false and
        // the feature is dead in the field.
        assert!(
            parse_version(current_version()).is_some(),
            "CARGO_PKG_VERSION {} does not parse",
            current_version()
        );
    }

    #[test]
    fn a_release_document_yields_its_tag_and_nothing_else() {
        let body = r#"{
            "tag_name": "v1.2.0",
            "name": "ConjureAlign 1.2.0",
            "html_url": "https://evil.example/pwned",
            "body": "Release notes with an emoji 💚"
        }"#;
        assert_eq!(parse_release(body).as_deref(), Some("1.2.0"));
    }

    /// The URL in the response is never used, so a hostile one cannot reach
    /// the OS browser. This pins that: the only thing `parse_release` returns
    /// is three integers rendered back out.
    #[test]
    fn a_hostile_tag_cannot_become_a_version() {
        for tag in [
            r#"{"tag_name": "https://evil.example"}"#,
            r#"{"tag_name": "v1.2.0; rm -rf /"}"#,
            r#"{"tag_name": "../../etc/passwd"}"#,
            r#"{"tag_name": ""}"#,
            r#"{"tag_name": 12}"#,
            r#"{"name": "no tag at all"}"#,
            "not json",
        ] {
            assert_eq!(parse_release(tag), None, "{tag}");
        }
    }

    #[test]
    fn the_release_link_is_a_constant_https_github_url() {
        assert!(RELEASES_URL.starts_with("https://github.com/"));
    }

    #[test]
    fn skipping_silences_that_version_but_not_a_newer_one() {
        let available = Status::Available {
            version: "1.2.0".into(),
        };
        assert!(notifies(&available, None));
        assert!(!notifies(&available, Some("1.2.0")), "skipped");
        assert!(!notifies(&available, Some("1.3.0")), "skipped past");
        assert!(notifies(&available, Some("1.1.0")), "newer than the skip");
        // A skip stored by some future version we cannot order must not
        // silence a real update.
        assert!(notifies(&available, Some("garbage")));
    }

    #[test]
    fn only_an_available_status_notifies() {
        for status in [
            Status::Unknown,
            Status::Checking,
            Status::UpToDate,
            Status::Failed,
        ] {
            assert!(!notifies(&status, None), "{status:?}");
        }
    }

    /// The worker drops anything still queued at shutdown, and anything
    /// offered while its queue is full. Either would otherwise leave the
    /// status on `Checking` — which the editor animates against — and
    /// `IN_FLIGHT` set, disabling every later check for the life of the
    /// process.
    #[test]
    fn a_check_that_never_runs_leaves_nothing_wedged() {
        set_status(Status::Checking);
        IN_FLIGHT.store(true, Ordering::Release);

        drop(CheckGuard);

        assert_eq!(status(), Status::Failed, "status stayed on Checking");
        assert!(!IN_FLIGHT.load(Ordering::Acquire), "IN_FLIGHT stayed set");

        // A guard released after the job set its own verdict must leave that
        // verdict alone.
        set_status(Status::UpToDate);
        drop(CheckGuard);
        assert_eq!(status(), Status::UpToDate);

        set_status(Status::Unknown);
    }

    #[test]
    fn the_default_endpoint_is_a_valid_https_url() {
        let endpoint = net::parse_endpoint(DEFAULT_ENDPOINT).expect("must parse");
        assert!(endpoint.tls);
        assert_eq!(endpoint.host, "api.github.com");
        assert_eq!(endpoint.port, 443);
    }

    /// The only test here that leaves the machine, so it is opt-in:
    /// `cargo test --release -- --ignored --nocapture update_smoke`.
    ///
    /// Read-only — a GET against a public endpoint — unlike the analytics
    /// smoke test, which writes a real event. Worth having anyway: every real
    /// user takes the TLS path and the chunked path, and no local sink
    /// exercises GitHub's actual response shape, header requirements or
    /// certificate chain.
    #[test]
    #[ignore = "talks to the live GitHub API"]
    fn update_smoke_test_against_live_github() {
        let endpoint = net::parse_endpoint(DEFAULT_ENDPOINT).unwrap();
        let latest = fetch(&endpoint)
            .expect("TLS handshake, GET, response framing and JSON parse should all complete");
        println!("--- latest release: {latest} (running {}) ---", current_version());
        assert!(parse_version(&latest).is_some(), "unusable version: {latest}");
    }
}
