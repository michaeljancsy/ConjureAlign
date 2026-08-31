//! "Did the last session end cleanly?" — the only way this plugin can see a
//! crash that is not a Rust panic.
//!
//! [`crate::crash`] is a panic hook and nothing else: an access violation, a
//! stack overflow, an `abort()`, or a fault inside linked C/C++ (clap-wrapper,
//! the GL driver, SChannel) kills the host with nothing sent. Release-health
//! sessions do not fill the gap either — sentry-core enqueues a healthy session
//! only from `Session::drop`, so a host that dies never reports one, and "no
//! data for version X" is ambiguous between *nobody runs it* and *everyone who
//! runs it dies*.
//!
//! So: stop trying to report *from* the dying process. Write down enough
//! beforehand that the **next** process can report on its behalf.
//!
//! One marker file per process id, holding the environment and a `stage` that
//! advances as the plugin gets further into its life. A clean teardown deletes
//! it. Anything left behind by a process that no longer exists is an abnormal
//! termination, and the stage says where it happened.
//!
//! Three rules:
//!
//! 1. **Consent gates the file, not just the report.** Nothing is written for a
//!    user who declined — writing notes about someone's DAW after they said no
//!    would be worse than the missing telemetry.
//! 2. **Never from the audio thread.** Every stage below is stamped from the
//!    host's main thread or the editor thread. `process()` must stay
//!    allocation-free and I/O-free, and no stage corresponds to it.
//! 3. **A live pid is never reported.** Pid reuse would otherwise turn an
//!    unrelated running program into a false crash report. Skipping a live pid
//!    can only *lose* a report, which is the safe direction to fail.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::config;

/// A marker whose process is gone but which is this old is almost certainly pid
/// reuse or a directory nobody cleaned up. Deleted without reporting.
const STALE_AFTER_SECS: u64 = 7 * 24 * 60 * 60;

/// Bounds on one sweep, so a directory that somehow filled up cannot turn into
/// a long scan on the network worker or a flood of Sentry events.
const MAX_FILES_PER_SWEEP: usize = 32;
const MAX_REPORTS_PER_SWEEP: usize = 8;

/// How far into its life the process had got. Ordered, but only the *last*
/// value written matters — this is a position, not a log.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Stage {
    /// `initialize()` entered: buffers are being sized, the delay line rebuilt.
    Initializing,
    /// `initialize()` returned true. Steady state for a plugin with no editor.
    Initialized,
    /// About to hand the parent window to baseview. On Windows this is where
    /// the OpenGL context gets created, which is native code the panic hook
    /// cannot see into.
    EditorCreating,
    /// The editor window exists and is drawing.
    EditorOpen,
    /// The editor was closed again; the plugin is still loaded.
    EditorClosed,
}

impl Stage {
    /// Wire value. These land in Sentry as a tag and in the issue title, so
    /// renaming one splits its history — add freely, rename never.
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Initializing => "initializing",
            Stage::Initialized => "initialized",
            Stage::EditorCreating => "editor_creating",
            Stage::EditorOpen => "editor_open",
            Stage::EditorClosed => "editor_closed",
        }
    }

    /// Whether this stage describes the editor rather than the plugin's own
    /// activation. `initialize()` will not overwrite one: a state load with a
    /// window open must not report as though there were no window.
    pub fn is_editor(self) -> bool {
        matches!(
            self,
            Stage::EditorCreating | Stage::EditorOpen | Stage::EditorClosed
        )
    }

    fn parse(s: &str) -> Option<Stage> {
        Some(match s {
            "initializing" => Stage::Initializing,
            "initialized" => Stage::Initialized,
            "editor_creating" => Stage::EditorCreating,
            "editor_open" => Stage::EditorOpen,
            "editor_closed" => Stage::EditorClosed,
            _ => return None,
        })
    }
}

/// `<config dir>/sessions/`. `None` on a platform with no config directory,
/// which is the same set of platforms where the reporters are inert.
pub fn sessions_dir() -> Option<PathBuf> {
    Some(config::config_dir()?.join("sessions"))
}

fn marker_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{pid}.json"))
}

fn fault_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{pid}.fault"))
}

// ---------------------------------------------------------------------------
// The live marker
// ---------------------------------------------------------------------------

/// The current process's marker file. Process-wide and refcounted through the
/// registry below, exactly like `net::Worker` and `crash::Reporter`: every
/// plugin instance holds a strong reference, and the last one to drop removes
/// the file.
pub struct Marker {
    path: PathBuf,
    fault_path: PathBuf,
    /// Everything about the environment, resolved once. Rewritten verbatim on
    /// each stage change so the file is always self-contained.
    env: Env,
    stage: Mutex<Stage>,
}

#[derive(Clone)]
struct Env {
    pid: u32,
    started_at: u64,
    plugin_version: &'static str,
    daw: &'static str,
    daw_version: Option<String>,
    os_version: Option<String>,
}

impl Marker {
    fn create(initial: Stage) -> Option<Arc<Marker>> {
        // Rule 1: nothing on disk for a user who has not said yes.
        if !config::analytics_enabled() {
            return None;
        }
        let dir = sessions_dir()?;
        let pid = std::process::id();
        let host = crate::host::info();
        let marker = Marker {
            path: marker_path(&dir, pid),
            fault_path: fault_path(&dir, pid),
            env: Env {
                pid,
                started_at: config::now_secs(),
                plugin_version: env!("CARGO_PKG_VERSION"),
                daw: host.daw,
                daw_version: host.daw_version.clone(),
                os_version: host.os_version.clone(),
            },
            stage: Mutex::new(initial),
        };
        marker.write(initial);
        Some(Arc::new(marker))
    }

    /// Records how far the process has got, if `allow` accepts the stage it is
    /// currently at. Silent on failure: a marker we could not write is a
    /// missing report, never an error the user should see.
    fn set_stage_if(&self, allow: impl FnOnce(Stage) -> bool, stage: Stage) {
        let mut current = self
            .stage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *current == stage || !allow(*current) {
            return;
        }
        *current = stage;
        self.write(stage);
    }

    fn write(&self, stage: Stage) {
        let mut obj = serde_json::Map::new();
        obj.insert("pid".into(), self.env.pid.into());
        obj.insert("started_at".into(), self.env.started_at.into());
        obj.insert("stage".into(), stage.as_str().into());
        obj.insert("plugin_version".into(), self.env.plugin_version.into());
        obj.insert("daw".into(), self.env.daw.into());
        if let Some(v) = &self.env.daw_version {
            obj.insert("daw_version".into(), v.clone().into());
        }
        if let Some(v) = &self.env.os_version {
            obj.insert("os_version".into(), v.clone().into());
        }
        let text = serde_json::Value::Object(obj).to_string();
        let _ = write_atomically(&self.path, &text);
    }

    /// Whether the exception handler or the panic hook left a fault record for
    /// this process. Checked at teardown: if something went wrong, the marker
    /// survives a clean exit so the next launch still reports it.
    ///
    /// **Non-empty, not merely present.** On Windows `veh::install` opens the
    /// fault file with `OPEN_ALWAYS`, which creates it — so every consenting
    /// session has one from the moment crash reporting arms. Testing for
    /// existence would keep every marker across every clean exit and report a
    /// false unclean shutdown on every launch.
    fn has_fault(&self) -> bool {
        std::fs::metadata(&self.fault_path).is_ok_and(|m| m.len() > 0)
    }
}

impl Drop for Marker {
    fn drop(&mut self) {
        if self.has_fault() {
            // Survived, but something in here faulted or panicked somewhere the
            // hook could not report from. Leave both files for the next sweep;
            // it reports and cleans up in one path.
            return;
        }
        let _ = std::fs::remove_file(&self.path);
        // The empty file the exception handler pre-opened, if it got that far.
        // Left behind it would accumulate one per pid forever, and the next
        // process to reuse this pid would inherit it.
        let _ = std::fs::remove_file(&self.fault_path);
    }
}

/// Write-then-rename, so a crash mid-write cannot leave a half file that the
/// next sweep would read as a corrupt marker. Same idiom as `Config::save_to`.
fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

fn registry() -> &'static Mutex<Weak<Marker>> {
    static REGISTRY: OnceLock<Mutex<Weak<Marker>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Weak::new()))
}

/// The process-wide marker, created on first use. `None` when consent has not
/// been granted or the platform has no config directory.
fn marker(initial: Stage) -> Option<Arc<Marker>> {
    let mut slot = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = slot.upgrade() {
        return Some(existing);
    }
    let fresh = Marker::create(initial)?;
    *slot = Arc::downgrade(&fresh);
    Some(fresh)
}

/// The fault file for the *current* process, for callers that must write one
/// without holding any of our locks. `None` before consent.
pub fn current_fault_path() -> Option<PathBuf> {
    Some(fault_path(&sessions_dir()?, std::process::id()))
}

/// One per plugin instance. Holds the process-wide marker alive for the
/// instance's lifetime; constructing one does no I/O, so plugin scanners and
/// opted-out users pay nothing until a stage is actually stamped.
#[derive(Default)]
pub struct MarkerHandle {
    marker: Mutex<Option<Arc<Marker>>>,
}

impl MarkerHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stamps `stage`, attaching to (or creating) the process-wide marker on
    /// first use. A no-op without consent.
    pub fn set_stage(&self, stage: Stage) {
        self.set_stage_if(|_| true, stage);
    }

    /// Stamps `stage` only if `allow` accepts the stage the marker is at now.
    ///
    /// `initialize()` uses this for both of its transitions, and both
    /// conditions matter. On the way in it refuses to overwrite an editor
    /// stage, so a host re-initializing while a window is open (every state
    /// load does) neither loses the more specific stage nor writes a file. On
    /// the way out it fires only if it was this call that set `Initializing`,
    /// which is what keeps the pair self-contained.
    pub fn set_stage_if(&self, allow: impl FnOnce(Stage) -> bool, stage: Stage) {
        let mut slot = self
            .marker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_none() {
            *slot = marker(stage);
            // Creating it *is* the stamp — but only if the caller would have
            // allowed one, and a fresh marker has no prior stage to judge.
            return;
        }
        if let Some(m) = slot.as_ref() {
            m.set_stage_if(allow, stage);
        }
    }

    /// Drops this instance's reference — the marker file goes away with the
    /// last one. Called when consent is withdrawn, so a decline stops leaving
    /// files behind immediately rather than at unload.
    pub fn detach(&self) {
        let stale = self
            .marker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(stale);
    }
}

// ---------------------------------------------------------------------------
// Faults recorded by the panic hook
// ---------------------------------------------------------------------------

/// Appends a fault record from a context where none of our locks may be taken.
///
/// Used by the panic hook for panics on Sentry's own worker threads, which it
/// deliberately does not capture (a report there would go into the very
/// machinery that is failing) and which would otherwise be entirely invisible —
/// including the one that upstream turns into a host abort at unload, via
/// `TransportThread::drop`'s `join().unwrap()`.
///
/// Plain `OpenOptions` + `write`: it takes no lock of ours, allocates only a
/// short string, and a failure here is simply a lost record.
pub fn record_fault(kind: &str, detail: &str) {
    use std::io::Write;

    let Some(path) = current_fault_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // One record per line; the sweep reads the first few and ignores the rest.
    let line = format!(
        "{}\t{}\n",
        kind,
        detail.replace(['\t', '\n', '\r'], " ").trim()
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    }
}

// ---------------------------------------------------------------------------
// The sweep
// ---------------------------------------------------------------------------

/// A previous process that never cleaned up after itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleSession {
    pub pid: u32,
    pub started_at: u64,
    pub stage: Stage,
    pub plugin_version: String,
    pub daw: String,
    pub daw_version: Option<String>,
    pub os_version: Option<String>,
    /// Records written by the exception handler or the panic hook, if any.
    pub faults: Vec<String>,
}

/// Collects every marker whose process is gone, deleting each one as it goes,
/// and returns what the caller should report. Deleting before reporting is
/// deliberate: a marker that somehow makes the reporter crash must not be
/// re-read on every launch forever.
///
/// The caller is `crash`, which turns these into Sentry events — keeping the
/// `sentry` dependency out of this module is what lets the whole sweep be
/// tested without a client.
pub fn take_stale() -> Vec<StaleSession> {
    let Some(dir) = sessions_dir() else {
        return Vec::new();
    };
    take_stale_in(
        &dir,
        config::now_secs(),
        std::process::id(),
        process_is_alive,
    )
}

/// The sweep with its three environmental inputs injected, so the age cutoff,
/// the caps and the liveness rule are all unit-testable.
pub fn take_stale_in(
    dir: &Path,
    now_secs: u64,
    current_pid: u32,
    is_alive: fn(u32) -> bool,
) -> Vec<StaleSession> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut found = Vec::new();
    for entry in entries.flatten().take(MAX_FILES_PER_SWEEP) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(pid) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        // Our own marker, and any other process still running: rule 3.
        if pid == current_pid || is_alive(pid) {
            continue;
        }

        let faults_path = fault_path(dir, pid);
        let session = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| parse_marker(&text, pid))
            .filter(|s| now_secs.saturating_sub(s.started_at) < STALE_AFTER_SECS);

        let _ = std::fs::remove_file(&path);
        let faults = read_faults(&faults_path);
        let _ = std::fs::remove_file(&faults_path);

        if let Some(mut session) = session {
            if found.len() < MAX_REPORTS_PER_SWEEP {
                session.faults = faults;
                found.push(session);
            }
        }
    }
    found
}

fn read_faults(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(4)
        .map(str::to_owned)
        .collect()
}

/// Anything unparseable reads as "no session": the file is still deleted, but
/// nothing is reported. A malformed marker cannot describe a crash usefully.
fn parse_marker(text: &str, pid: u32) -> Option<StaleSession> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let string = |key: &str| {
        v.get(key)
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    Some(StaleSession {
        pid,
        started_at: v.get("started_at").and_then(|x| x.as_u64()).unwrap_or(0),
        stage: Stage::parse(v.get("stage")?.as_str()?)?,
        plugin_version: string("plugin_version").unwrap_or_else(|| "unknown".into()),
        daw: string("daw").unwrap_or_else(|| crate::host::UNKNOWN_DAW.into()),
        daw_version: string("daw_version"),
        os_version: string("os_version"),
        faults: Vec::new(),
    })
}

/// Whether a process id is still running.
///
/// A false "alive" only loses a report; a false "dead" invents a crash. Both
/// implementations therefore answer "alive" whenever they cannot tell.
#[cfg(target_os = "windows")]
fn process_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, STILL_ACTIVE,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: `OpenProcess` takes no pointers; `GetExitCodeProcess` gets a
    // valid out-parameter and a handle that is open for the whole call; and the
    // handle is closed on every path where it was obtained.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            // Only "no such process id" means gone. Anything else — most
            // plausibly ERROR_ACCESS_DENIED for a process belonging to another
            // user — means it exists, and answering "alive" loses a report
            // rather than inventing one. Mirrors the ESRCH/EPERM split below.
            return GetLastError() != ERROR_INVALID_PARAMETER;
        }

        // An open handle is NOT proof of life. A process object outlives the
        // process itself for as long as anyone holds a handle to it — a parent,
        // a debugger, Windows Error Reporting — so `OpenProcess` succeeds on a
        // process that has already terminated. Without this second check a
        // crashed DAW whose handle something still holds would read as running,
        // and its marker would go unreported until the staleness cutoff deleted
        // it unread, which is precisely the report most worth having.
        let mut exit_code = 0u32;
        let queried = GetExitCodeProcess(handle, &mut exit_code);
        CloseHandle(handle);

        // A failed query tells us nothing, so it fails toward "alive". The one
        // ambiguity in the other arm is a process that genuinely exited with
        // 259, which reads as running — the same safe direction.
        queried == 0 || exit_code == STILL_ACTIVE as u32
    }
}

#[cfg(target_os = "macos")]
fn process_is_alive(pid: u32) -> bool {
    // SAFETY: signal 0 performs the permission and existence checks without
    // delivering anything. No pointers involved.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    // ESRCH is the only answer that means "gone". EPERM means it exists and
    // belongs to someone else.
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// No binaries ship here, so `config_dir()` is `None`, no marker is ever
/// written and this is unreachable. It answers "alive" anyway — the direction
/// that reports nothing.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("conjure-align-sessions-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn plant(dir: &Path, pid: u32, stage: &str, started_at: u64) {
        let text = format!(
            r#"{{"pid":{pid},"started_at":{started_at},"stage":"{stage}",
               "plugin_version":"9.9.9","daw":"LUNA","os_version":"10.0.26200"}}"#
        );
        std::fs::write(marker_path(dir, pid), text).unwrap();
    }

    const DEAD: fn(u32) -> bool = |_| false;
    const ALIVE: fn(u32) -> bool = |_| true;

    #[test]
    fn every_stage_round_trips_through_its_wire_value() {
        for stage in [
            Stage::Initializing,
            Stage::Initialized,
            Stage::EditorCreating,
            Stage::EditorOpen,
            Stage::EditorClosed,
        ] {
            assert_eq!(Stage::parse(stage.as_str()), Some(stage));
        }
        assert_eq!(Stage::parse("processing"), None);
    }

    #[test]
    fn a_dead_pids_marker_is_reported_and_removed() {
        let dir = temp_dir("dead");
        plant(&dir, 4242, "editor_creating", 1_000);

        let stale = take_stale_in(&dir, 1_100, 1, DEAD);

        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].pid, 4242);
        assert_eq!(stale[0].stage, Stage::EditorCreating);
        assert_eq!(stale[0].daw, "LUNA");
        assert_eq!(stale[0].plugin_version, "9.9.9");
        assert!(!marker_path(&dir, 4242).exists());
    }

    /// Rule 3. A live pid is another running DAW or pid reuse; either way,
    /// reporting it would be inventing a crash that did not happen.
    #[test]
    fn a_live_pid_is_left_alone() {
        let dir = temp_dir("live");
        plant(&dir, 4242, "editor_open", 1_000);

        assert!(take_stale_in(&dir, 1_100, 1, ALIVE).is_empty());
        assert!(marker_path(&dir, 4242).exists());
    }

    #[test]
    fn our_own_marker_is_never_swept() {
        let dir = temp_dir("self");
        plant(&dir, 77, "initialized", 1_000);

        assert!(take_stale_in(&dir, 1_100, 77, DEAD).is_empty());
        assert!(marker_path(&dir, 77).exists());
    }

    /// Old enough to be pid reuse or an abandoned directory: cleaned up, but
    /// not reported as a crash that nobody can act on.
    #[test]
    fn an_ancient_marker_is_deleted_without_reporting() {
        let dir = temp_dir("ancient");
        plant(&dir, 4242, "initialized", 1_000);

        assert!(take_stale_in(&dir, 1_000 + STALE_AFTER_SECS + 1, 1, DEAD).is_empty());
        assert!(!marker_path(&dir, 4242).exists());
    }

    #[test]
    fn fault_records_ride_along_and_are_cleaned_up() {
        let dir = temp_dir("faults");
        plant(&dir, 4242, "editor_open", 1_000);
        std::fs::write(
            fault_path(&dir, 4242),
            "exception\tcode=0xc0000005 offset=0x1234\n",
        )
        .unwrap();

        let stale = take_stale_in(&dir, 1_100, 1, DEAD);

        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].faults.len(), 1);
        assert!(stale[0].faults[0].contains("0xc0000005"));
        assert!(!fault_path(&dir, 4242).exists());
    }

    /// A half-written or corrupt marker still gets cleaned up — otherwise it
    /// would be re-read on every launch forever — but describes no crash.
    #[test]
    fn a_corrupt_marker_is_removed_without_reporting() {
        let dir = temp_dir("corrupt");
        std::fs::write(marker_path(&dir, 4242), "{not json").unwrap();

        assert!(take_stale_in(&dir, 1_100, 1, DEAD).is_empty());
        assert!(!marker_path(&dir, 4242).exists());
    }

    #[test]
    fn reports_are_capped_per_sweep() {
        let dir = temp_dir("cap");
        for pid in 100..100 + (MAX_REPORTS_PER_SWEEP as u32 + 5) {
            plant(&dir, pid, "initialized", 1_000);
        }

        let stale = take_stale_in(&dir, 1_100, 1, DEAD);

        assert_eq!(stale.len(), MAX_REPORTS_PER_SWEEP);
        // Everything examined is cleaned up, reported or not.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[test]
    fn only_editor_stages_report_as_editor_stages() {
        assert!(Stage::EditorCreating.is_editor());
        assert!(Stage::EditorOpen.is_editor());
        assert!(Stage::EditorClosed.is_editor());
        assert!(!Stage::Initializing.is_editor());
        assert!(!Stage::Initialized.is_editor());
    }

    /// The guard `initialize()` relies on. A state load with the editor open
    /// must not walk the stage backwards — and, just as importantly, must not
    /// write a file on a path the codebase keeps free of I/O.
    #[test]
    fn a_refused_stage_change_neither_writes_nor_moves() {
        let dir = temp_dir("refused");
        let marker = Marker {
            path: marker_path(&dir, 1),
            fault_path: fault_path(&dir, 1),
            env: Env {
                pid: 1,
                started_at: 0,
                plugin_version: "0.0.0",
                daw: "LUNA",
                daw_version: None,
                os_version: None,
            },
            stage: Mutex::new(Stage::EditorOpen),
        };

        marker.set_stage_if(|s| !s.is_editor(), Stage::Initializing);
        assert!(
            !marker.path.exists(),
            "a refused change still wrote the marker file"
        );
        assert_eq!(*marker.stage.lock().unwrap(), Stage::EditorOpen);

        // ...and an accepted one does both.
        marker.set_stage_if(|_| true, Stage::EditorClosed);
        assert!(marker.path.exists());
        assert!(std::fs::read_to_string(&marker.path)
            .unwrap()
            .contains("editor_closed"));
    }

    fn marker_for(dir: &Path, pid: u32) -> Marker {
        Marker {
            path: marker_path(dir, pid),
            fault_path: fault_path(dir, pid),
            env: Env {
                pid,
                started_at: 0,
                plugin_version: "0.0.0",
                daw: "LUNA",
                daw_version: None,
                os_version: None,
            },
            stage: Mutex::new(Stage::Initialized),
        }
    }

    /// The Windows shape: `veh::install` opens the fault file with
    /// `OPEN_ALWAYS`, so an *empty* one exists from the moment crash reporting
    /// arms. Treating that as evidence would keep every marker across every
    /// clean exit and report a false unclean shutdown on every launch.
    #[test]
    fn an_empty_fault_file_is_not_evidence_of_a_fault() {
        let dir = temp_dir("empty-fault");
        let marker = marker_for(&dir, 1);
        marker.write(Stage::Initialized);
        std::fs::write(fault_path(&dir, 1), "").unwrap();

        assert!(!marker.has_fault());
        drop(marker);

        assert!(
            !marker_path(&dir, 1).exists(),
            "a clean exit left its marker behind and would be reported as a crash"
        );
        assert!(
            !fault_path(&dir, 1).exists(),
            "the empty fault file would accumulate one per pid forever"
        );
    }

    /// The converse: a record with content keeps both files alive for the
    /// sweep, even though this process is exiting cleanly.
    #[test]
    fn a_recorded_fault_keeps_the_marker_past_a_clean_exit() {
        let dir = temp_dir("real-fault");
        let marker = marker_for(&dir, 1);
        marker.write(Stage::Initialized);
        std::fs::write(
            fault_path(&dir, 1),
            "exception\tcode=0xc0000005 offset=0x1234 scope=1\n",
        )
        .unwrap();

        assert!(marker.has_fault());
        drop(marker);

        assert!(marker_path(&dir, 1).exists());
        assert!(fault_path(&dir, 1).exists());
    }

    /// The process running this test is, definitionally, alive.
    #[test]
    fn liveness_recognizes_the_current_process() {
        assert!(process_is_alive(std::process::id()));
    }

    /// The converse — and on Windows specifically the case an
    /// `OpenProcess`-only check gets wrong: `child` is deliberately still in
    /// scope, so Rust still holds a handle to the terminated process and its
    /// process object has not gone away. A handle is not a heartbeat.
    #[test]
    fn liveness_sees_through_a_reaped_child_whose_handle_is_still_open() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "__no_such_test__"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("could not re-exec the test binary");
        let pid = child.id();
        child.wait().expect("the child never exited");

        assert!(
            !process_is_alive(pid),
            "a terminated process read as alive, so its marker would never be reported"
        );
        drop(child);
    }
}
