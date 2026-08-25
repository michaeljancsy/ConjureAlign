//! Opt-in usage analytics (Mixpanel).
//!
//! Three rules shape everything here:
//!
//! 1. **The audio thread never touches this module.** Events are raised from
//!    `initialize()` (main thread) and from the background analysis task, both
//!    of which may allocate. `process()` has no analytics code at all, so
//!    `assert_process_allocs` stays meaningful.
//! 2. **Nothing is sent, and nothing is written to disk, until the user says
//!    yes.** Consent is tri-state — never asked / granted / declined — and
//!    lives in a config file next to no other state, because `#[persist]`
//!    fields are per-DAW-session and consent is per-install.
//! 3. **No thread outlives the dylib.** Hosts unload plugin bundles in-process.
//!    One worker thread is shared by every instance in the process via a
//!    `Weak` registry, and the last instance to drop joins it.
//!
//! The payload is deliberately thin: a random device id, the plugin version,
//! the OS, the sample rate, and *bucketed* capture outcomes. No audio, no file
//! names, no host name, no raw measurements.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use atomic_float::AtomicF32;

use crate::analysis::RejectReason;

/// Mixpanel project token for the "ConjureAlign" project. Client-side tokens
/// are public by design — this one is write-only ingestion, it grants no read
/// access, and it ships in every binary regardless of what we do here.
pub const MIXPANEL_TOKEN: &str = "33c5c2d1578f3275ec2985bf4c92ad22";

const DEFAULT_ENDPOINT: &str = "https://api.mixpanel.com/track";
/// Points the sender at a local sink for tests and manual QA.
const ENDPOINT_ENV: &str = "CONJURE_ALIGN_ANALYTICS_URL";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const IO_TIMEOUT: Duration = Duration::from_secs(3);
/// Small on purpose: if the network is wedged, events are dropped rather than
/// queued. An analytics backlog must never grow without bound in a DAW.
const QUEUE_CAPACITY: usize = 32;

/// Analytics only ships on the platforms that get binaries. Elsewhere the
/// module compiles but is inert — no TLS dependency, no config file, no prompt.
const SUPPORTED: bool = cfg!(any(target_os = "macos", target_os = "windows"));

// ---------------------------------------------------------------------------
// Consent + device id (install-wide, not per session)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    /// `None` means the user has never been asked — the only state that shows
    /// the first-run prompt.
    pub consent: Option<bool>,
    /// Present only once consent has been granted.
    pub device_id: Option<String>,
}

/// `~/Library/Application Support/ConjureDSP/ConjureAlign/analytics.json` on
/// macOS, `%APPDATA%\ConjureDSP\ConjureAlign\analytics.json` on Windows,
/// `None` anywhere else.
pub fn config_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")?;
        Some(
            PathBuf::from(home)
                .join("Library/Application Support/ConjureDSP/ConjureAlign/analytics.json"),
        )
    }
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var_os("APPDATA")?;
        Some(
            PathBuf::from(appdata)
                .join("ConjureDSP")
                .join("ConjureAlign")
                .join("analytics.json"),
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        None
    }
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
        let consent = match v.get("consent").and_then(|c| c.as_str()) {
            Some("granted") => Some(true),
            Some("declined") => Some(false),
            _ => None,
        };
        let device_id = v
            .get("device_id")
            .and_then(|d| d.as_str())
            .filter(|d| !d.is_empty())
            .map(str::to_owned);
        Config { consent, device_id }
    }

    pub fn save_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut obj = serde_json::Map::new();
        if let Some(c) = self.consent {
            obj.insert(
                "consent".into(),
                serde_json::Value::from(if c { "granted" } else { "declined" }),
            );
        }
        // Only ever written alongside a granted consent; declining leaves no
        // identifier on disk at all.
        if let Some(id) = &self.device_id {
            obj.insert("device_id".into(), serde_json::Value::from(id.clone()));
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
            // Unsupported platform: report a settled "no" so the first-run
            // prompt never appears somewhere it could not be answered durably.
            None => Config {
                consent: Some(false),
                device_id: None,
            },
        };
        Mutex::new(cfg)
    })
}

/// `None` while the user has never been asked — this is what drives the
/// first-run prompt.
pub fn consent() -> Option<bool> {
    config().lock().unwrap().consent
}

pub fn enabled() -> bool {
    consent() == Some(true)
}

/// False on platforms with no binary release, where consent could not be
/// stored durably even if it were asked for.
pub fn is_supported() -> bool {
    SUPPORTED
}

/// Records the user's answer and persists it. Granting mints the device id if
/// there isn't one; declining stores the decision and no identifier.
pub fn set_consent(granted: bool) {
    if !SUPPORTED {
        return;
    }
    let mut cfg = config().lock().unwrap();
    cfg.consent = Some(granted);
    if granted {
        if cfg.device_id.is_none() {
            cfg.device_id = Some(new_device_id());
        }
    } else {
        cfg.device_id = None;
    }
    if let Some(path) = config_path() {
        if let Err(e) = cfg.save_to(&path) {
            nih_plug::nih_log!("ConjureAlign: could not save analytics preference: {e}");
        }
    }
}

fn device_id() -> Option<String> {
    config().lock().unwrap().device_id.clone()
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

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AnalyticsEvent {
    /// Once per plugin instance. Goes over the wire as "Plugin Loaded", NOT
    /// "Session Start" — Mixpanel ships a built-in virtual event,
    /// `$session_start`, whose display name is exactly "Session Start", and
    /// two identically labelled entries in the event picker is a trap.
    PluginLoaded { sample_rate: f32 },
    CaptureCompleted { confidence: f32, offset_ms: f32 },
    CaptureRejected { reason: RejectReason },
}

/// Bucketed, never raw: a precise confidence figure would say more about the
/// user's material than we have any reason to know.
pub fn confidence_bucket(confidence: f32) -> &'static str {
    match confidence {
        c if c < 0.5 => "<0.5",
        c if c < 0.7 => "0.5-0.7",
        c if c < 0.9 => "0.7-0.9",
        _ => "0.9+",
    }
}

/// Magnitude only — the sign says which mic was closer, which is the user's
/// business, and the bucket is all we need to know if the shift window is
/// sized sensibly.
pub fn offset_bucket(offset_ms: f32) -> &'static str {
    match offset_ms.abs() {
        m if m < 1.0 => "<1ms",
        m if m < 10.0 => "1-10ms",
        m if m < 50.0 => "10-50ms",
        _ => "50ms+",
    }
}

pub fn reason_str(reason: RejectReason) -> &'static str {
    match reason {
        RejectReason::TooShort => "too_short",
        RejectReason::Silence => "silence",
        RejectReason::LowConfidence => "low_confidence",
    }
}

/// One Mixpanel event object. `time` is milliseconds since the epoch, which is
/// what the ingestion API expects.
pub fn build_payload(
    event: &AnalyticsEvent,
    device_id: &str,
    now_millis: u64,
) -> serde_json::Value {
    let mut props = serde_json::Map::new();
    props.insert("token".into(), MIXPANEL_TOKEN.into());
    props.insert("distinct_id".into(), device_id.into());
    props.insert("time".into(), now_millis.into());
    props.insert("plugin_version".into(), env!("CARGO_PKG_VERSION").into());
    props.insert("os".into(), std::env::consts::OS.into());

    let name = match event {
        AnalyticsEvent::PluginLoaded { sample_rate } => {
            props.insert("sample_rate".into(), (*sample_rate as u64).into());
            "Plugin Loaded"
        }
        AnalyticsEvent::CaptureCompleted {
            confidence,
            offset_ms,
        } => {
            props.insert("confidence".into(), confidence_bucket(*confidence).into());
            props.insert("offset".into(), offset_bucket(*offset_ms).into());
            "Capture Completed"
        }
        AnalyticsEvent::CaptureRejected { reason } => {
            props.insert("reason".into(), reason_str(*reason).into());
            "Capture Rejected"
        }
    };

    serde_json::json!({ "event": name, "properties": serde_json::Value::Object(props) })
}

// ---------------------------------------------------------------------------
// Endpoint + transport
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// Minimal absolute-URL split — enough for the one constant above and the
/// `http://127.0.0.1:<port>/…` overrides used in tests.
pub fn parse_endpoint(url: &str) -> Option<Endpoint> {
    let (scheme, rest) = url.split_once("://")?;
    let tls = match scheme {
        "https" => true,
        "http" => false,
        _ => return None,
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().ok()?),
        None => (authority, if tls { 443 } else { 80 }),
    };
    if host.is_empty() {
        return None;
    }
    Some(Endpoint {
        tls,
        host: host.to_owned(),
        port,
        path: path.to_owned(),
    })
}

/// Returns the raw HTTP response. The worker discards it — a dropped event is
/// not worth reacting to — but returning it is what lets the smoke test below
/// tell "bytes moved" apart from "Mixpanel accepted the event".
fn post(endpoint: &Endpoint, body: &str) -> std::io::Result<String> {
    let request = format!(
        "POST {} HTTP/1.1\r\n\
         Host: {}\r\n\
         User-Agent: ConjureAlign/{}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        endpoint.path,
        endpoint.host,
        env!("CARGO_PKG_VERSION"),
        body.len(),
        body
    );

    // One connection, opened here and handed to whichever writer applies —
    // connecting inside the TLS branch instead would leave this one dangling.
    let addr = (endpoint.host.as_str(), endpoint.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::other("no address for analytics host"))?;
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;

    if endpoint.tls {
        write_tls(&endpoint.host, stream, request.as_bytes())
    } else {
        write_plain(stream, request.as_bytes())
    }
}

fn exchange<S: Read + Write>(mut stream: S, request: &[u8]) -> std::io::Result<String> {
    stream.write_all(request)?;
    stream.flush()?;
    // `Connection: close` means the far end hangs up once it has replied, so
    // this returns promptly; the socket's read timeout bounds it either way.
    // A truncated read still yields whatever arrived, which is all the caller
    // wants.
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    Ok(String::from_utf8_lossy(&response).into_owned())
}

fn write_plain(stream: TcpStream, request: &[u8]) -> std::io::Result<String> {
    exchange(stream, request)
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn write_tls(host: &str, stream: TcpStream, request: &[u8]) -> std::io::Result<String> {
    let connector = native_tls::TlsConnector::new().map_err(std::io::Error::other)?;
    let tls = connector
        .connect(host, stream)
        .map_err(std::io::Error::other)?;
    exchange(tls, request)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn write_tls(_host: &str, _stream: TcpStream, _request: &[u8]) -> std::io::Result<String> {
    Err(std::io::Error::other("TLS unavailable on this platform"))
}

// ---------------------------------------------------------------------------
// Worker: one thread per process, shared by every plugin instance
// ---------------------------------------------------------------------------

struct Worker {
    /// `Option` so `Drop` can disconnect the channel *before* joining —
    /// dropping the sender is what wakes a worker parked in `recv()`.
    tx: Mutex<Option<SyncSender<String>>>,
    shutdown: Arc<AtomicBool>,
    join: Mutex<Option<JoinHandle<()>>>,
}

/// The default endpoint, unless the environment points somewhere else (a
/// local sink for tests and manual QA).
fn resolve_endpoint() -> Option<Endpoint> {
    std::env::var(ENDPOINT_ENV)
        .ok()
        .as_deref()
        .and_then(parse_endpoint)
        .or_else(|| parse_endpoint(DEFAULT_ENDPOINT))
}

impl Worker {
    fn spawn() -> Arc<Worker> {
        Worker::spawn_to(resolve_endpoint())
    }

    fn spawn_to(endpoint: Option<Endpoint>) -> Arc<Worker> {
        let (tx, rx) = sync_channel::<String>(QUEUE_CAPACITY);
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = shutdown.clone();
        let join = std::thread::Builder::new()
            .name("conjure-align-analytics".into())
            .spawn(move || {
                let Some(endpoint) = endpoint else {
                    return;
                };
                while let Ok(body) = rx.recv() {
                    // Shutting down: drain the queue without touching the
                    // network so the join in `Drop` stays quick.
                    if flag.load(Ordering::Acquire) {
                        continue;
                    }
                    let _ = post(&endpoint, &body);
                }
            })
            .ok();

        Arc::new(Worker {
            tx: Mutex::new(Some(tx)),
            shutdown,
            join: Mutex::new(join),
        })
    }

    /// Never blocks: a full queue means the network is not keeping up, and a
    /// dropped analytics event is always preferable to a stalled caller.
    fn try_send(&self, body: String) {
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.try_send(body);
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        drop(self.tx.lock().unwrap().take());
        if let Some(handle) = self.join.lock().unwrap().take() {
            let _ = handle.join();
        }
    }
}

fn registry() -> &'static Mutex<Weak<Worker>> {
    static REGISTRY: OnceLock<Mutex<Weak<Worker>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Weak::new()))
}

fn worker() -> Arc<Worker> {
    let registry = registry();
    let mut slot = registry.lock().unwrap();
    if let Some(existing) = slot.upgrade() {
        return existing;
    }
    let fresh = Worker::spawn();
    *slot = Arc::downgrade(&fresh);
    fresh
}

// ---------------------------------------------------------------------------
// Per-instance handle
// ---------------------------------------------------------------------------

/// One per plugin instance. Constructing it does no I/O and starts no thread,
/// so plugin scanners and opted-out users pay nothing.
#[derive(Default)]
pub struct AnalyticsHandle {
    /// Holds the process-wide worker alive for this instance's lifetime.
    worker: Mutex<Option<Arc<Worker>>>,
    session_sent: AtomicBool,
    /// Zero until `note_session` has run; also the Plugin Loaded payload.
    sample_rate: AtomicF32,
}

impl AnalyticsHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Called from `initialize()`. Idempotent across host re-initializations,
    /// so a state load or sample-rate change never doubles the session count.
    pub fn note_session(&self, sample_rate: f32) {
        self.sample_rate.store(sample_rate, Ordering::Relaxed);
        self.flush_session();
    }

    pub fn track(&self, event: AnalyticsEvent) {
        if !enabled() {
            return;
        }
        // Consent usually arrives *after* `initialize()` — the prompt is shown
        // when the editor first opens — so the session event is emitted here
        // rather than being lost.
        self.flush_session();
        self.send(event);
    }

    fn flush_session(&self) {
        if !enabled() {
            return;
        }
        let sample_rate = self.sample_rate.load(Ordering::Relaxed);
        if sample_rate <= 0.0 {
            return;
        }
        if self
            .session_sent
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.send(AnalyticsEvent::PluginLoaded { sample_rate });
        }
    }

    fn send(&self, event: AnalyticsEvent) {
        let Some(device_id) = device_id() else {
            return;
        };
        let now_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let batch = serde_json::Value::Array(vec![build_payload(&event, &device_id, now_millis)]);

        let mut slot = self.worker.lock().unwrap();
        let worker = slot.get_or_insert_with(worker);
        worker.try_send(batch.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_buckets_cover_the_range() {
        assert_eq!(confidence_bucket(0.0), "<0.5");
        assert_eq!(confidence_bucket(0.49), "<0.5");
        assert_eq!(confidence_bucket(0.5), "0.5-0.7");
        assert_eq!(confidence_bucket(0.69), "0.5-0.7");
        assert_eq!(confidence_bucket(0.7), "0.7-0.9");
        assert_eq!(confidence_bucket(0.89), "0.7-0.9");
        assert_eq!(confidence_bucket(0.9), "0.9+");
        assert_eq!(confidence_bucket(1.0), "0.9+");
    }

    #[test]
    fn offset_buckets_use_magnitude() {
        assert_eq!(offset_bucket(0.4), "<1ms");
        assert_eq!(offset_bucket(-0.4), "<1ms");
        assert_eq!(offset_bucket(-5.0), "1-10ms");
        assert_eq!(offset_bucket(5.0), "1-10ms");
        assert_eq!(offset_bucket(-49.0), "10-50ms");
        assert_eq!(offset_bucket(120.0), "50ms+");
        assert_eq!(offset_bucket(-120.0), "50ms+");
    }

    #[test]
    fn payload_carries_only_bucketed_values() {
        let event = AnalyticsEvent::CaptureCompleted {
            confidence: 0.94,
            offset_ms: -3.75,
        };
        let v = build_payload(&event, "abc123", 1_700_000_000_000);
        assert_eq!(v["event"], "Capture Completed");
        let props = &v["properties"];
        assert_eq!(props["token"], MIXPANEL_TOKEN);
        assert_eq!(props["distinct_id"], "abc123");
        assert_eq!(props["time"], 1_700_000_000_000u64);
        assert_eq!(props["plugin_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(props["confidence"], "0.9+");
        assert_eq!(props["offset"], "1-10ms");

        // The raw measurements must not survive anywhere in the payload.
        let text = v.to_string();
        assert!(!text.contains("0.94"), "raw confidence leaked: {text}");
        assert!(!text.contains("3.75"), "raw offset leaked: {text}");
    }

    #[test]
    fn rejection_payload_names_the_reason() {
        let v = build_payload(
            &AnalyticsEvent::CaptureRejected {
                reason: RejectReason::LowConfidence,
            },
            "d",
            1,
        );
        assert_eq!(v["event"], "Capture Rejected");
        assert_eq!(v["properties"]["reason"], "low_confidence");
    }

    #[test]
    fn session_payload_carries_the_sample_rate() {
        let v = build_payload(
            &AnalyticsEvent::PluginLoaded {
                sample_rate: 48_000.0,
            },
            "d",
            1,
        );
        assert_eq!(v["event"], "Plugin Loaded");
        assert_eq!(v["properties"]["sample_rate"], 48_000u64);
    }

    #[test]
    fn endpoints_parse() {
        assert_eq!(
            parse_endpoint("https://api.mixpanel.com/track"),
            Some(Endpoint {
                tls: true,
                host: "api.mixpanel.com".into(),
                port: 443,
                path: "/track".into(),
            })
        );
        assert_eq!(
            parse_endpoint("http://127.0.0.1:9000"),
            Some(Endpoint {
                tls: false,
                host: "127.0.0.1".into(),
                port: 9000,
                path: "/".into(),
            })
        );
        assert_eq!(parse_endpoint("ftp://x/y"), None);
        assert_eq!(parse_endpoint("api.mixpanel.com/track"), None);
        assert_eq!(parse_endpoint("http:///track"), None);
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("conjure-align-analytics-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn config_roundtrips_all_three_consent_states() {
        let path = temp_dir("roundtrip").join("analytics.json");

        // Never asked: no file at all.
        assert_eq!(Config::load_from(&path), Config::default());
        assert_eq!(Config::load_from(&path).consent, None);

        let granted = Config {
            consent: Some(true),
            device_id: Some("deadbeef".into()),
        };
        granted.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path), granted);

        let declined = Config {
            consent: Some(false),
            device_id: None,
        };
        declined.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path), declined);
        // A declined install must leave no identifier behind.
        assert!(!std::fs::read_to_string(&path).unwrap().contains("device_id"));
    }

    #[test]
    fn corrupt_config_reads_as_never_asked() {
        let path = temp_dir("corrupt").join("analytics.json");
        std::fs::write(&path, "{not json at all").unwrap();
        assert_eq!(Config::load_from(&path).consent, None);
    }

    #[test]
    fn device_id_is_random_and_hex() {
        let a = new_device_id();
        let b = new_device_id();
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// The load-bearing shutdown property: a worker mid-queue must disconnect
    /// and join, never outliving the plugin. Also covers the wire format.
    #[test]
    fn worker_posts_then_shuts_down_cleanly() {
        use std::io::BufRead;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let sink = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let mut request = String::new();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line.strip_prefix("Content-Length: ") {
                    content_length = v.trim().parse().unwrap();
                }
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).unwrap();
            (request, String::from_utf8(body).unwrap())
        });

        // Injected rather than set through the environment: cargo runs tests
        // in threads, and mutating the process environment underneath them is
        // a data race.
        let worker = Worker::spawn_to(parse_endpoint(&format!("http://127.0.0.1:{port}/track")));
        let body = build_payload(
            &AnalyticsEvent::PluginLoaded {
                sample_rate: 44_100.0,
            },
            "test-device",
            42,
        );
        worker.try_send(serde_json::Value::Array(vec![body]).to_string());

        let (request, body) = sink.join().unwrap();
        assert!(request.starts_with("POST /track HTTP/1.1\r\n"), "{request}");
        assert!(request.contains("Host: 127.0.0.1\r\n"), "{request}");
        assert!(
            request.contains("Content-Type: application/json\r\n"),
            "{request}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed[0]["event"], "Plugin Loaded");
        assert_eq!(parsed[0]["properties"]["distinct_id"], "test-device");

        // Dropping the last reference must disconnect and join the thread.
        drop(worker);
    }

    /// The only test that leaves the machine, so it is opt-in:
    /// `cargo test --release -- --ignored --nocapture smoke_test`.
    ///
    /// Worth having because every real user takes the TLS path and no local
    /// sink can reach it — the plain-HTTP tests above would not have caught,
    /// for instance, the branch that used to open a second TCP connection.
    /// Asking for `verbose=1` turns a silent rejection (a wrong token reads
    /// as a bare `0`) into a message that says which field Mixpanel disliked.
    ///
    /// It writes ONE event to the live project, tagged `smoke-test` so it can
    /// be filtered out; `MIXPANEL_TOKEN` must be real for it to pass.
    #[test]
    #[ignore = "sends one real event to the live Mixpanel project"]
    fn smoke_test_against_live_mixpanel() {
        let mut endpoint = parse_endpoint(DEFAULT_ENDPOINT).unwrap();
        endpoint.path = "/track?verbose=1".into();

        let now_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let body = serde_json::Value::Array(vec![build_payload(
            &AnalyticsEvent::PluginLoaded {
                sample_rate: 48_000.0,
            },
            "smoke-test",
            now_millis,
        )])
        .to_string();

        let response = post(&endpoint, &body).expect("TLS handshake and POST should complete");
        println!("--- Mixpanel response ---\n{response}\n---");
        assert!(
            response.contains("\"status\": 1") || response.contains("\"status\":1"),
            "Mixpanel rejected the event — check MIXPANEL_TOKEN:\n{response}"
        );
    }

    #[test]
    fn default_endpoint_is_a_valid_https_url() {
        let endpoint = parse_endpoint(DEFAULT_ENDPOINT).expect("default endpoint must parse");
        assert!(endpoint.tls);
        assert_eq!(endpoint.port, 443);
    }
}
