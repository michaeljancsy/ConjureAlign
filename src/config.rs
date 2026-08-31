//! Install-wide preferences: the two consent answers, the analytics device id,
//! and the update checker's bookkeeping.
//!
//! This lives outside `#[persist]` state on purpose. Those fields are
//! per-DAW-session; these answers are per-install, and a user who says "no" in
//! one project has said no everywhere.
//!
//! The file is read once per process into a `OnceLock`, so a change made in one
//! running DAW reaches other processes at their next launch — which is the
//! right trade for a preference nobody edits twice.
//!
//! There are **two** independent tri-state answers, each `None` until asked:
//! [`Config::analytics`] (usage analytics *and* crash reporting — one question,
//! one identifier) and [`Config::updates`] (the update check). The first-run
//! prompt appears while either is unanswered and asks only for the ones that
//! are, which is what lets a new question be added without re-litigating a
//! settled one.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Both reporters and the update check only ship on the platforms that get
/// binaries. Elsewhere this module compiles but is inert — no TLS dependency,
/// no config file, no prompt, and (for updates) no release artifact to point a
/// build-from-source user at.
const SUPPORTED: bool = cfg!(any(target_os = "macos", target_os = "windows"));

/// A successful check is good for a day; a failed one is retried sooner,
/// because the likeliest cause is that the machine was simply offline.
pub const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
pub const CHECK_RETRY_SECS: u64 = 6 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    /// Usage analytics + crash reporting. `None` means the user has never been
    /// asked — one of the two states that shows the first-run prompt.
    ///
    /// Stored under the JSON key `"consent"` rather than `"analytics"`: that
    /// was the only question when the file was introduced, and renaming the key
    /// would read every existing install's answer as "never asked".
    pub analytics: Option<bool>,
    /// Present only once analytics consent has been granted.
    pub device_id: Option<String>,
    /// The update check. `None` is the other state that shows the prompt — so
    /// an install upgrading from a version that never asked gets this one
    /// question and keeps its stored analytics answer.
    pub updates: Option<bool>,
    /// Unix seconds of the last update check that reached a verdict, and
    /// whether that verdict was a successful fetch. Together these gate how
    /// soon the next automatic check may run (see [`should_check`]).
    pub update_last_check: Option<u64>,
    pub update_last_ok: bool,
    /// The newest version the last successful check saw, so the editor can
    /// show a known update immediately instead of waiting on the network.
    pub update_latest_seen: Option<String>,
    /// A version the user asked not to be reminded about. Anything newer than
    /// this still notifies.
    pub update_skipped: Option<String>,
    /// The plugin version that ran on this install last time, so a launch can
    /// tell that it is an *upgrade* rather than a first run. Bookkeeping, not
    /// an answer: see [`note_running_version`].
    ///
    /// Written only for an install that has granted analytics consent — a
    /// declined install has nothing to report it to, and storing state nobody
    /// will ever read is not what "declined" should mean.
    pub last_version: Option<String>,
}

/// `~/Library/Application Support/ConjureDSP/ConjureAlign/` on macOS,
/// `%APPDATA%\ConjureDSP\ConjureAlign\` on Windows, `None` anywhere else.
pub fn config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join("Library/Application Support/ConjureDSP/ConjureAlign"))
    }
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var_os("APPDATA")?;
        Some(
            PathBuf::from(appdata)
                .join("ConjureDSP")
                .join("ConjureAlign"),
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// The one preferences file. Still named `analytics.json` although it now holds
/// the update answer too: the name is documented in the README and, more to the
/// point, moving it would lose every existing install's consent.
pub fn config_path() -> Option<PathBuf> {
    Some(config_dir()?.join("analytics.json"))
}

fn tri_state(v: &serde_json::Value, key: &str) -> Option<bool> {
    match v.get(key).and_then(|c| c.as_str()) {
        Some("granted") => Some(true),
        Some("declined") => Some(false),
        _ => None,
    }
}

fn tri_state_str(answer: bool) -> &'static str {
    if answer {
        "granted"
    } else {
        "declined"
    }
}

fn non_empty_str(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|d| d.as_str())
        .filter(|d| !d.is_empty())
        .map(str::to_owned)
}

impl Config {
    /// Anything unreadable or malformed reads as "never asked": the user gets
    /// the prompt again and the next answer overwrites the bad file. Losing a
    /// stored *yes* that way is the safe direction to fail.
    pub fn load_from(path: &std::path::Path) -> Config {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Config::default();
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Config::default();
        };
        Config {
            analytics: tri_state(&v, "consent"),
            device_id: non_empty_str(&v, "device_id"),
            updates: tri_state(&v, "updates"),
            update_last_check: v.get("update_last_check").and_then(|t| t.as_u64()),
            update_last_ok: v
                .get("update_last_ok")
                .and_then(|t| t.as_bool())
                .unwrap_or(false),
            update_latest_seen: non_empty_str(&v, "update_latest_seen"),
            update_skipped: non_empty_str(&v, "update_skipped"),
            last_version: non_empty_str(&v, "last_version"),
        }
    }

    /// The decision behind [`note_running_version`], split out so it can be
    /// tested without the process-wide config or a real `HOME`.
    ///
    /// `None` means the stored version already matches, so there is nothing to
    /// report and nothing to write. `Some(previous)` means it changed, with
    /// `previous` being what ran before — itself `None` on a first run.
    fn apply_running_version(&mut self, version: &str) -> Option<Option<String>> {
        if self.last_version.as_deref() == Some(version) {
            return None;
        }
        Some(self.last_version.replace(version.to_owned()))
    }

    pub fn save_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut obj = serde_json::Map::new();
        if let Some(c) = self.analytics {
            obj.insert("consent".into(), tri_state_str(c).into());
        }
        // Only ever written alongside a granted analytics consent; declining
        // leaves no identifier on disk at all.
        if let Some(id) = &self.device_id {
            obj.insert("device_id".into(), serde_json::Value::from(id.clone()));
        }
        if let Some(u) = self.updates {
            obj.insert("updates".into(), tri_state_str(u).into());
        }
        // Written only once a check has actually run, so a declined install
        // that never checks keeps a file with nothing in it but the two answers.
        if let Some(t) = self.update_last_check {
            obj.insert("update_last_check".into(), t.into());
            obj.insert("update_last_ok".into(), self.update_last_ok.into());
        }
        if let Some(v) = &self.update_latest_seen {
            obj.insert(
                "update_latest_seen".into(),
                serde_json::Value::from(v.clone()),
            );
        }
        if let Some(v) = &self.update_skipped {
            obj.insert("update_skipped".into(), serde_json::Value::from(v.clone()));
        }
        if let Some(v) = &self.last_version {
            obj.insert("last_version".into(), serde_json::Value::from(v.clone()));
        }
        let text = serde_json::Value::Object(obj).to_string();

        // Write-then-rename so a crash mid-write can't leave a half file that
        // would read back as "never asked".
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }
}

fn config() -> &'static Mutex<Config> {
    static CONFIG: OnceLock<Mutex<Config>> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let cfg = match config_path() {
            Some(p) => Config::load_from(&p),
            // Unsupported platform: report a settled "no" to both questions so
            // the first-run prompt never appears somewhere it could not be
            // answered durably.
            None => Config {
                analytics: Some(false),
                updates: Some(false),
                ..Config::default()
            },
        };
        Mutex::new(cfg)
    })
}

/// Reads the cached config. Poison-tolerant for the same reason the `_in_hook`
/// accessors are: a panic elsewhere must not turn every later read into one.
fn with_config<T>(f: impl FnOnce(&mut Config) -> T) -> T {
    let mut cfg = config()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut cfg)
}

fn save_now(cfg: &Config) {
    if let Some(path) = config_path() {
        if let Err(e) = cfg.save_to(&path) {
            nih_plug::nih_log!("ConjureAlign: could not save preferences: {e}");
        }
    }
}

/// Applies `f` and writes the result back to disk.
fn mutate(f: impl FnOnce(&mut Config)) {
    if !SUPPORTED {
        return;
    }
    with_config(|cfg| {
        f(cfg);
        save_now(cfg);
    });
}

/// A snapshot of the whole config, for callers that need several fields at
/// once without taking the lock repeatedly.
pub fn snapshot() -> Config {
    with_config(|cfg| cfg.clone())
}

/// `None` while the user has never been asked about analytics + crash
/// reporting — one of the two states that drive the first-run prompt.
pub fn analytics_consent() -> Option<bool> {
    with_config(|cfg| cfg.analytics)
}

pub fn analytics_enabled() -> bool {
    analytics_consent() == Some(true)
}

/// `None` while the user has never been asked about update checks.
pub fn update_consent() -> Option<bool> {
    with_config(|cfg| cfg.updates)
}

pub fn update_checks_enabled() -> bool {
    update_consent() == Some(true)
}

/// Whether anything is still unanswered — i.e. whether the first-run prompt
/// should be shown at all.
pub fn needs_prompt() -> bool {
    with_config(|cfg| cfg.analytics.is_none() || cfg.updates.is_none())
}

/// Hook-safe variant of [`analytics_enabled`], for the panic hook running on
/// the panicking thread. `try_lock`, because the panicking frame may hold the
/// config lock on this very thread (`set_analytics_consent` holds it across
/// file I/O) and a blocking re-lock there is a same-thread deadlock.
/// Poison-tolerant, because an `unwrap` here would be a panic inside the panic
/// hook — an immediate, unlogged abort. `WouldBlock` reads as "not enabled":
/// dropping one report beats hanging the host.
pub fn analytics_enabled_in_hook() -> bool {
    try_with_config(|cfg| cfg.analytics == Some(true)).unwrap_or(false)
}

/// Hook-safe variant of [`device_id`]; same rules as
/// [`analytics_enabled_in_hook`]. `None` on contention: the report goes out
/// unlabelled rather than not at all.
pub fn device_id_in_hook() -> Option<String> {
    try_with_config(|cfg| cfg.device_id.clone()).flatten()
}

fn try_with_config<T>(f: impl FnOnce(&Config) -> T) -> Option<T> {
    use std::sync::TryLockError;
    match config().try_lock() {
        Ok(cfg) => Some(f(&cfg)),
        Err(TryLockError::Poisoned(p)) => Some(f(&p.into_inner())),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// False on platforms with no binary release, where an answer could not be
/// stored durably even if it were asked for.
pub fn is_supported() -> bool {
    SUPPORTED
}

/// Records the user's analytics answer and persists it. Granting mints the
/// device id if there isn't one; declining stores the decision and no
/// identifier.
pub fn set_analytics_consent(granted: bool) {
    mutate(|cfg| {
        cfg.analytics = Some(granted);
        if granted {
            if cfg.device_id.is_none() {
                cfg.device_id = Some(new_device_id());
            }
        } else {
            cfg.device_id = None;
        }
    });
}

/// Records the user's update-check answer. Deliberately mints nothing: the
/// check carries no identifier, so saying yes to it leaves nothing on disk but
/// the word "granted".
pub fn set_update_consent(granted: bool) {
    mutate(|cfg| cfg.updates = Some(granted));
}

/// Records the outcome of a completed check. `latest` is the newest version
/// the release feed reported, or `None` when the fetch failed.
pub fn record_update_check(now_secs: u64, latest: Option<String>) {
    mutate(|cfg| {
        cfg.update_last_check = Some(now_secs);
        cfg.update_last_ok = latest.is_some();
        if latest.is_some() {
            cfg.update_latest_seen = latest;
        }
    });
}

/// Just the skipped version, without cloning the whole config: the editor asks
/// once a frame.
pub fn update_skipped() -> Option<String> {
    with_config(|cfg| cfg.update_skipped.clone())
}

pub fn set_update_skipped(version: Option<String>) {
    mutate(|cfg| cfg.update_skipped = version);
}

/// Whether an *automatic* check may run now. Manual checks bypass this
/// entirely — the user clicked, so they get an answer.
///
/// A clock that has gone backwards (a corrected system time, a VM restored
/// from a snapshot) forces a check rather than locking one out until the
/// stored timestamp comes back around.
pub fn should_check(now_secs: u64, last_check: Option<u64>, last_ok: bool) -> bool {
    let Some(last) = last_check else {
        return true;
    };
    if now_secs < last {
        return true;
    }
    let interval = if last_ok {
        CHECK_INTERVAL_SECS
    } else {
        CHECK_RETRY_SECS
    };
    now_secs - last >= interval
}

/// The random per-install id minted on analytics consent. Public because
/// [`crate::crash`] labels its reports with the *same* id rather than minting a
/// second identifier — one opt-in, one identifier, one thing to explain.
pub fn device_id() -> Option<String> {
    with_config(|cfg| cfg.device_id.clone())
}

/// Records that `version` is running now, and returns the version that ran
/// before it — but only when the two differ.
///
/// This is what backs the `upgraded_from` property on `Plugin Loaded`. The
/// cohort-level question it replaces ("has any install ever been seen on two
/// versions?") is the wrong instrument: it cannot separate "nobody upgraded"
/// from "nobody was told there was one", and anything that resets the device
/// id — a clean reinstall, an uninstaller — silently degrades it. A per-device
/// before/after is unambiguous, and the first non-`None` answer is the first
/// proven in-place upgrade.
///
/// `None` on a first run, and also `None` for an install arriving from a
/// version that predates this bookkeeping: those two cannot be told apart, and
/// claiming an upgrade that cannot be proven is the wrong way to be wrong.
///
/// **Call only from a consent-gated path.** Nothing here checks the answer, and
/// a declined install must not accumulate state it will never report.
///
/// Writes only when the version actually changed, so every launch after the
/// first at a given version touches no disk. The config is process-cached, so
/// several instances in one host still produce at most one write and at most
/// one non-`None` answer.
pub fn note_running_version(version: &str) -> Option<String> {
    if !SUPPORTED {
        return None;
    }
    with_config(|cfg| {
        let previous = cfg.apply_running_version(version)?;
        save_now(cfg);
        previous
    })
}

/// Seconds since the Unix epoch, saturating at 0 on a clock before 1970.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn new_device_id() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // Randomness is unavailable often enough on locked-down systems to be
        // worth a fallback, and a clock-derived id is good enough for a
        // population counter.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        bytes[..16].copy_from_slice(&nanos.to_le_bytes());
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("conjure-align-config-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn config_roundtrips_all_three_consent_states() {
        let path = temp_dir("roundtrip").join("analytics.json");

        // Never asked: no file at all.
        assert_eq!(Config::load_from(&path), Config::default());
        assert_eq!(Config::load_from(&path).analytics, None);

        let granted = Config {
            analytics: Some(true),
            device_id: Some("deadbeef".into()),
            ..Config::default()
        };
        granted.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path), granted);

        let declined = Config {
            analytics: Some(false),
            ..Config::default()
        };
        declined.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path), declined);
        // A declined install must leave no identifier behind.
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("device_id"));
    }

    /// The upgrade path: a file written before update checks existed must read
    /// back with its analytics answer intact and the update question unasked,
    /// which is what shows the new prompt without re-asking the settled one.
    #[test]
    fn a_pre_update_config_keeps_its_answer_and_asks_the_new_question() {
        let path = temp_dir("upgrade").join("analytics.json");
        std::fs::write(&path, r#"{"consent":"granted","device_id":"abc123"}"#).unwrap();

        let cfg = Config::load_from(&path);
        assert_eq!(cfg.analytics, Some(true));
        assert_eq!(cfg.device_id.as_deref(), Some("abc123"));
        assert_eq!(cfg.updates, None);
        assert_eq!(cfg.update_last_check, None);
    }

    #[test]
    fn the_two_answers_are_independent() {
        let path = temp_dir("independent").join("analytics.json");
        let cfg = Config {
            analytics: Some(false),
            updates: Some(true),
            ..Config::default()
        };
        cfg.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path), cfg);
        // Saying yes to update checks must not mint an identifier.
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("device_id"));
    }

    #[test]
    fn update_bookkeeping_roundtrips() {
        let path = temp_dir("bookkeeping").join("analytics.json");
        let cfg = Config {
            analytics: Some(true),
            device_id: Some("d".into()),
            updates: Some(true),
            update_last_check: Some(1_756_400_000),
            update_last_ok: true,
            update_latest_seen: Some("1.2.0".into()),
            update_skipped: Some("1.2.0".into()),
            last_version: Some("1.1.0".into()),
        };
        cfg.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path), cfg);
    }

    /// The upgrade signal. A change is reported exactly once, and an unchanged
    /// version reports nothing — which is also what keeps an ordinary launch
    /// from touching the disk at all.
    #[test]
    fn running_version_reports_each_change_once() {
        let mut cfg = Config::default();

        // First run: the version is recorded, but there is no upgrade to
        // report. An install arriving from a build that predates this field
        // looks identical, and deliberately so — claiming an upgrade that
        // cannot be proven is the wrong way to be wrong.
        assert_eq!(cfg.apply_running_version("1.3.0"), Some(None));
        assert_eq!(cfg.last_version.as_deref(), Some("1.3.0"));

        // Same version again: nothing changed, so nothing to report or write.
        assert_eq!(cfg.apply_running_version("1.3.0"), None);

        // An upgrade — reported once, then quiet.
        assert_eq!(
            cfg.apply_running_version("1.4.0"),
            Some(Some("1.3.0".into()))
        );
        assert_eq!(cfg.apply_running_version("1.4.0"), None);
        assert_eq!(cfg.last_version.as_deref(), Some("1.4.0"));

        // A downgrade is a change too. Someone rolling back is exactly the
        // case worth being able to see.
        assert_eq!(
            cfg.apply_running_version("1.3.0"),
            Some(Some("1.4.0".into()))
        );
    }

    /// An older install's file has no `last_version`, which must read as
    /// "never recorded" rather than as an upgrade from nothing.
    #[test]
    fn a_config_without_last_version_reports_no_upgrade() {
        let path = temp_dir("no-last-version").join("analytics.json");
        std::fs::write(&path, r#"{"consent":"granted","device_id":"d"}"#).unwrap();

        let mut cfg = Config::load_from(&path);
        assert_eq!(cfg.last_version, None);
        assert_eq!(cfg.apply_running_version("1.3.0"), Some(None));
    }

    #[test]
    fn corrupt_config_reads_as_never_asked() {
        let path = temp_dir("corrupt").join("analytics.json");
        std::fs::write(&path, "{not json at all").unwrap();
        let cfg = Config::load_from(&path);
        assert_eq!(cfg.analytics, None);
        assert_eq!(cfg.updates, None);
    }

    #[test]
    fn device_id_is_random_and_hex() {
        let a = new_device_id();
        let b = new_device_id();
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn automatic_checks_respect_the_interval() {
        const DAY: u64 = CHECK_INTERVAL_SECS;
        // Never checked.
        assert!(should_check(DAY, None, false));
        // A success is good for a day.
        assert!(!should_check(DAY + 1, Some(DAY), true));
        assert!(!should_check(2 * DAY - 1, Some(DAY), true));
        assert!(should_check(2 * DAY, Some(DAY), true));
        // A failure is retried sooner.
        assert!(!should_check(DAY + CHECK_RETRY_SECS - 1, Some(DAY), false));
        assert!(should_check(DAY + CHECK_RETRY_SECS, Some(DAY), false));
        // A clock that went backwards must not lock the check out.
        assert!(should_check(1, Some(DAY), true));
    }
}
