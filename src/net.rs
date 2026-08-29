//! The plugin's HTTP client and its one background thread.
//!
//! Both opt-in network features — usage analytics (`crate::analytics`) and the
//! update check (`crate::update`) — go out through here, because everything
//! that makes an HTTP request safe *inside a DAW* is fiddly and belongs in one
//! place:
//!
//! 1. **No thread outlives the dylib.** Hosts unload plugin bundles in-process.
//!    One worker thread is shared by every instance via a `Weak` registry, and
//!    the last handle to drop joins it. (One narrow exception: a timed-out DNS
//!    lookup abandons its helper thread — see [`resolve_bounded`].)
//! 2. **Every stage is bounded.** The drop-side join is only safe because no
//!    single request can run forever: DNS has a deadline, connect/read/write
//!    carry per-op timeouts, and the response read has a wall-clock deadline
//!    plus a size cap. The worst-case unload stall is one in-flight request.
//! 3. **Sends never block a caller.** The queue is small and `try_send`: if the
//!    network is wedged, work is dropped rather than backing up behind a GUI or
//!    `initialize()` thread.
//!
//! Nothing here decides *whether* to send anything. That is consent's job, and
//! it lives in `crate::config`.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, SyncSender};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
pub const IO_TIMEOUT: Duration = Duration::from_secs(3);
/// Wall-clock ceiling on one whole request, enforced by the response-read loop
/// in [`exchange`]: the socket's per-op read timeout cannot stop a server that
/// keeps trickling bytes, and an unbounded request rides through the worker
/// loop straight into `Worker::drop`'s join at plugin unload.
pub const DEADLINE: Duration = Duration::from_secs(10);
/// Default response cap. The analytics worker discards responses and the smoke
/// test reads a few hundred bytes of JSON; buffering more than this only serves
/// a misbehaving server. Callers that expect a larger document pass their own.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;
/// Small on purpose: if the network is wedged, work is dropped rather than
/// queued. A backlog must never grow without bound in a DAW.
const QUEUE_CAPACITY: usize = 32;

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// Minimal absolute-URL split — enough for the fixed endpoints the plugin ships
/// with and the `http://127.0.0.1:<port>/…` overrides used in tests.
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

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// `getaddrinfo` with a deadline. `ToSocketAddrs` exposes no timeout, and a
/// blackholed resolver (captive portal, dead VPN DNS) blocks it for the OS
/// resolver's own timeout — tens of seconds — which would ride through the
/// worker loop into `Worker::drop`'s join and stall plugin unload. So the
/// blocking call runs on a throwaway thread and we wait a bounded time for
/// its answer.
///
/// On timeout the helper is abandoned mid-`getaddrinfo` — a deliberate,
/// narrow exception to rule 1 ("no thread outlives the dylib"), the same
/// trade ureq's resolver makes. The lingering thread holds no plugin state
/// and ends with one channel send nobody hears; the theoretical hazard is
/// the dylib unmapping underneath it, but this cdylib cannot unload on
/// macOS — clap-wrapper's ObjC class registration marks the image
/// never-unload (verified in the built bundle), with the cdylib's
/// thread-locals as a second pin. The alternative — joining it — is the
/// certain unload hang this exists to remove.
fn resolve_bounded(host: &str, port: u16, timeout: Duration) -> std::io::Result<Vec<SocketAddr>> {
    // An IP literal never touches the resolver (tests, local sinks): no
    // thread needed.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    let (tx, rx) = channel();
    let host = host.to_owned();
    std::thread::Builder::new()
        .name("conjure-align-dns".into())
        .spawn(move || {
            // Fails harmlessly when the waiter has already given up.
            let _ = tx.send(
                (host.as_str(), port)
                    .to_socket_addrs()
                    .map(|i| i.collect::<Vec<_>>()),
            );
        })?;
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        // Timed out, or the helper died without sending; either way this
        // request fails, like any other network failure here.
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timed out resolving host",
        )),
    }
}

/// One fire-and-forget JSON POST. Returns the **raw** HTTP response, headers
/// included; the analytics worker discards it, but returning it unparsed is
/// what lets the smoke test tell "bytes moved" apart from "Mixpanel accepted
/// the event".
pub fn post(endpoint: &Endpoint, body: &str) -> std::io::Result<String> {
    let raw = send(
        "POST",
        endpoint,
        &[("Content-Type", "application/json")],
        Some(body),
        MAX_RESPONSE_BYTES,
    )?;
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

/// One GET, with the response framing actually decoded — status line stripped,
/// `Transfer-Encoding: chunked` de-framed. A caller that means to parse the
/// body as JSON needs that: chunk-size lines sit *inside* the byte stream and
/// would otherwise be spliced into the document.
///
/// `max_bytes` is per-call because the default cap is sized for a status reply:
/// a caller parsing a document has to be able to ask for enough of it that
/// truncation cannot silently turn into a parse failure.
pub fn get(
    endpoint: &Endpoint,
    headers: &[(&str, &str)],
    max_bytes: usize,
) -> std::io::Result<Response> {
    let raw = send("GET", endpoint, headers, None, max_bytes)?;
    parse_response(&raw).ok_or_else(|| std::io::Error::other("malformed HTTP response"))
}

fn send(
    method: &str,
    endpoint: &Endpoint,
    headers: &[(&str, &str)],
    body: Option<&str>,
    max_bytes: usize,
) -> std::io::Result<Vec<u8>> {
    let deadline = Instant::now() + DEADLINE;

    let mut request = format!(
        "{} {} HTTP/1.1\r\n\
         Host: {}\r\n\
         User-Agent: ConjureAlign/{}\r\n",
        method,
        endpoint.path,
        endpoint.host,
        env!("CARGO_PKG_VERSION"),
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    // Always sent, zero included: a GET without it is fine, and a POST without
    // it would be a framing error.
    request.push_str(&format!(
        "Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.map_or(0, str::len)
    ));
    if let Some(body) = body {
        request.push_str(body);
    }

    // One connection, opened here and handed to whichever writer applies —
    // connecting inside the TLS branch instead would leave this one dangling.
    // Every resolved address gets a try, not just the first: dual-stack hosts
    // routinely resolve IPv6-first, and a machine whose IPv6 route is broken
    // would otherwise never deliver a single request.
    let mut stream = Err(std::io::Error::other("no address for host"));
    for addr in resolve_bounded(&endpoint.host, endpoint.port, CONNECT_TIMEOUT)? {
        // The deadline must bound this loop too: dual-stack hosts resolve to
        // several addresses, and a firewall that blackholes SYNs would
        // otherwise stack a full CONNECT_TIMEOUT per address on top of the
        // budget the caller was promised.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            stream = Err(std::io::Error::other("request deadline exhausted"));
            break;
        }
        stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT.min(remaining));
        if stream.is_ok() {
            break;
        }
    }
    let stream = stream?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;

    if endpoint.tls {
        write_tls(
            &endpoint.host,
            stream,
            request.as_bytes(),
            deadline,
            max_bytes,
        )
    } else {
        exchange(stream, request.as_bytes(), deadline, max_bytes)
    }
}

fn exchange<S: Read + Write>(
    mut stream: S,
    request: &[u8],
    deadline: Instant,
    max_bytes: usize,
) -> std::io::Result<Vec<u8>> {
    stream.write_all(request)?;
    stream.flush()?;
    // `Connection: close` means the far end hangs up once it has replied, so
    // this normally returns promptly — but the socket's read timeout only
    // bounds each *individual* read, so a peer trickling a byte per timeout
    // could pin the worker (and grow the buffer) forever. The deadline and
    // the size cap bound the whole read; the worst case is one blocked read
    // past the deadline. A truncated response still yields whatever arrived,
    // read errors included — which is all the analytics caller wants, and
    // matches the `read_to_end` this replaced, whose result was ignored too.
    let mut response = Vec::new();
    let mut chunk = [0u8; 4096];
    while response.len() < max_bytes && Instant::now() < deadline {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => response.extend_from_slice(&chunk[..n]),
        }
    }
    Ok(response)
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn write_tls(
    host: &str,
    stream: TcpStream,
    request: &[u8],
    deadline: Instant,
    max_bytes: usize,
) -> std::io::Result<Vec<u8>> {
    let connector = native_tls::TlsConnector::new().map_err(std::io::Error::other)?;
    let tls = connector
        .connect(host, stream)
        .map_err(std::io::Error::other)?;
    exchange(tls, request, deadline, max_bytes)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn write_tls(
    _host: &str,
    _stream: TcpStream,
    _request: &[u8],
    _deadline: Instant,
    _max_bytes: usize,
) -> std::io::Result<Vec<u8>> {
    Err(std::io::Error::other("TLS unavailable on this platform"))
}

// ---------------------------------------------------------------------------
// Response framing
// ---------------------------------------------------------------------------

/// A decoded HTTP response: the status code, and the body with its transfer
/// framing removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

/// Splits a raw response into status + body, de-framing a chunked one.
///
/// Done on bytes rather than on a `String`, deliberately: `Transfer-Encoding:
/// chunked` puts its size markers at arbitrary byte offsets, so a server is
/// free to split a multi-byte UTF-8 character across two chunks. Decoding to
/// text first and slicing by byte index there would corrupt exactly that case —
/// which for a GitHub release body means any emoji in the release notes. The
/// assembled body is converted once, at the end.
///
/// Returns `None` only when there is no header/body separator at all, i.e. the
/// peer hung up mid-headers. Anything past that is best-effort: a truncated
/// body is handed back as far as it got, because the size cap upstream can
/// truncate a legitimate response and the caller's parser is the right place to
/// notice.
pub fn parse_response(raw: &[u8]) -> Option<Response> {
    let split = find(raw, b"\r\n\r\n")
        .map(|i| (i, i + 4))
        .or_else(|| find(raw, b"\n\n").map(|i| (i, i + 2)))?;
    let (head, body) = (&raw[..split.0], &raw[split.1..]);
    let head = String::from_utf8_lossy(head);
    let mut lines = head.lines();

    // "HTTP/1.1 200 OK" — the middle token is the only part anyone needs.
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);

    let mut chunked = false;
    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            // Only the final encoding matters, and `chunked` is required to be
            // it whenever it appears at all.
            "transfer-encoding" => chunked |= value.to_ascii_lowercase().contains("chunked"),
            "content-length" => content_length = value.parse::<usize>().ok(),
            _ => {}
        }
    }

    let body = if chunked {
        dechunk(body)
    } else {
        // A cap-truncated response is shorter than Content-Length says; take
        // whatever actually arrived.
        body[..content_length.unwrap_or(body.len()).min(body.len())].to_vec()
    };

    Some(Response {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// Reassembles a chunked body. Stops at the terminating zero-length chunk, at
/// a malformed size line, or at the end of what arrived — all three of which
/// mean "this is everything we have".
fn dechunk(mut rest: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(eol) = find(rest, b"\r\n").or_else(|| find(rest, b"\n")) else {
            return out;
        };
        let line = String::from_utf8_lossy(&rest[..eol]);
        // A size line may carry `;chunk-extension` after the hex length.
        let size_text = line.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size_text, 16) else {
            return out;
        };
        if size == 0 {
            return out;
        }
        // Skip the CRLF (or bare LF) that ended the size line.
        let after_size = eol + if rest[eol] == b'\r' { 2 } else { 1 };
        let end = (after_size + size).min(rest.len());
        out.extend_from_slice(&rest[after_size..end]);
        if end == rest.len() {
            return out;
        }
        // ...and the CRLF that ends the chunk data.
        rest = &rest[end..];
        rest = rest.strip_prefix(b"\r\n").unwrap_or(rest);
        rest = rest.strip_prefix(b"\n").unwrap_or(rest);
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------
// Worker: one thread per process, shared by every plugin instance
// ---------------------------------------------------------------------------

/// A unit of background network work. Deliberately an opaque closure rather
/// than a message the worker interprets: the worker's only job is to run things
/// off the caller's thread and to stop running them at shutdown, and keeping it
/// ignorant of analytics and updates is what lets both share one thread.
type Job = Box<dyn FnOnce() + Send + 'static>;

pub struct Worker {
    /// `Option` so `Drop` can disconnect the channel *before* joining —
    /// dropping the sender is what wakes a worker parked in `recv()`.
    tx: Mutex<Option<SyncSender<Job>>>,
    shutdown: Arc<AtomicBool>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl Worker {
    fn spawn() -> Arc<Worker> {
        let (tx, rx) = sync_channel::<Job>(QUEUE_CAPACITY);
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = shutdown.clone();
        let join = std::thread::Builder::new()
            .name("conjure-align-net".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    // Shutting down: drain the queue without touching the
                    // network so the join in `Drop` stays quick.
                    if flag.load(Ordering::Acquire) {
                        continue;
                    }
                    job();
                }
            })
            .ok();

        Arc::new(Worker {
            // No thread means no reader: refusing work outright is what turns
            // "the OS would not give us a thread" into an immediate failure
            // the caller can see, rather than a job sitting in a channel
            // nobody will ever drain.
            tx: Mutex::new(join.is_some().then_some(tx)),
            shutdown,
            join: Mutex::new(join),
        })
    }

    /// Never blocks: a full queue means the network is not keeping up, and
    /// dropped background work is always preferable to a stalled caller.
    /// Returns whether the job was accepted — a caller that shows the user a
    /// "working on it" state needs to know when it will never run.
    pub fn try_send(&self, job: impl FnOnce() + Send + 'static) -> bool {
        self.tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|tx| tx.try_send(Box::new(job)).is_ok())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        drop(
            self.tx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
        );
        if let Some(handle) = self
            .join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = handle.join();
        }
    }
}

fn registry() -> &'static Mutex<Weak<Worker>> {
    static REGISTRY: OnceLock<Mutex<Weak<Worker>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Weak::new()))
}

/// The process-wide worker, spawned on first use.
///
/// **The returned handle must be held by a plugin instance, never by a job
/// running on the worker itself.** A job that owned the last strong reference
/// would drop it on the worker thread, and `Worker::drop` would then join its
/// own thread — the EDEADLK/hang failure documented for nih-plug's shared
/// background worker in CLAUDE.md.
pub fn worker() -> Arc<Worker> {
    let registry = registry();
    let mut slot = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = slot.upgrade() {
        return existing;
    }
    let fresh = Worker::spawn();
    *slot = Arc::downgrade(&fresh);
    fresh
}

/// One per plugin instance, per feature. Holds the shared worker alive for the
/// instance's lifetime and is the only way to reach it, which is what keeps the
/// "never dropped on the worker thread" rule above enforceable by construction.
///
/// Lazily attached: constructing one does no I/O and starts no thread, so
/// plugin scanners and opted-out users pay nothing.
#[derive(Default)]
pub struct WorkerHandle {
    worker: Mutex<Option<Arc<Worker>>>,
}

impl WorkerHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues `job` on the shared worker, spawning the thread if this is the
    /// first use anywhere in the process. Returns whether it was accepted.
    pub fn spawn_job(&self, job: impl FnOnce() + Send + 'static) -> bool {
        let mut slot = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.get_or_insert_with(worker).try_send(job)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel as std_channel;

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

    /// The load-bearing shutdown property: a worker mid-queue must disconnect
    /// and join, never outliving the plugin. Also covers the POST wire format.
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
        let endpoint = parse_endpoint(&format!("http://127.0.0.1:{port}/track")).unwrap();
        let handle = WorkerHandle::new();
        assert!(handle.spawn_job(move || {
            let _ = post(&endpoint, r#"[{"event":"Plugin Loaded"}]"#);
        }));

        let (request, body) = sink.join().unwrap();
        assert!(request.starts_with("POST /track HTTP/1.1\r\n"), "{request}");
        assert!(request.contains("Host: 127.0.0.1\r\n"), "{request}");
        assert!(
            request.contains("Content-Type: application/json\r\n"),
            "{request}"
        );
        assert!(
            request.contains(concat!(
                "User-Agent: ConjureAlign/",
                env!("CARGO_PKG_VERSION")
            )),
            "{request}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed[0]["event"], "Plugin Loaded");

        // Dropping the last reference must disconnect and join the thread.
        drop(handle);
    }

    /// A shutting-down worker drains without running anything: that is what
    /// bounds `Worker::drop`'s join to at most one in-flight job.
    #[test]
    fn a_shutting_down_worker_drops_queued_jobs_unrun() {
        let (tx, rx) = std_channel::<()>();
        let worker = Worker::spawn();
        worker.shutdown.store(true, Ordering::Release);
        assert!(worker.try_send(move || {
            let _ = tx.send(());
        }));
        drop(worker);
        assert!(
            rx.try_recv().is_err(),
            "a job ran after shutdown was signalled"
        );
    }

    #[test]
    fn get_sends_its_headers_and_no_body() {
        use std::io::BufRead;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let sink = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut stream = reader.into_inner();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"tag_name\":\"v9.9.9\"}")
                .unwrap();
            request
        });

        let endpoint = parse_endpoint(&format!("http://127.0.0.1:{port}/latest")).unwrap();
        let response = get(
            &endpoint,
            &[("Accept", "application/vnd.github+json")],
            256 * 1024,
        )
        .unwrap();

        let request = sink.join().unwrap();
        assert!(request.starts_with("GET /latest HTTP/1.1\r\n"), "{request}");
        assert!(
            request.contains("Accept: application/vnd.github+json\r\n"),
            "{request}"
        );
        assert!(request.contains("Content-Length: 0\r\n"), "{request}");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, r#"{"tag_name":"v9.9.9"}"#);
    }

    #[test]
    fn a_plain_response_is_split_at_the_header_break() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 9\r\n\r\n{\"a\":1}\r\n";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "{\"a\":1}\r\n");
    }

    #[test]
    fn a_short_response_yields_what_arrived_not_what_was_promised() {
        // The size cap upstream can cut a body off mid-document; Content-Length
        // must not be trusted to index past the end of the buffer.
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\n\r\n{\"a\":1}";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.body, "{\"a\":1}");
    }

    #[test]
    fn status_codes_are_read_off_the_status_line() {
        let r = parse_response(b"HTTP/1.1 403 rate limit exceeded\r\n\r\n{}").unwrap();
        assert_eq!(r.status, 403);
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nno body separator").is_none());
    }

    /// The reason this parsing exists at all: chunk-size lines live *inside*
    /// the byte stream, so a JSON document arriving chunked is not valid JSON
    /// until they are stripped.
    #[test]
    fn a_chunked_body_is_reassembled() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                    f\r\n{\"tag_name\":\"v1\r\n\
                    8\r\n.2.0\"}  \r\n\
                    0\r\n\r\n";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.body, r#"{"tag_name":"v1.2.0"}  "#);
        assert!(serde_json::from_str::<serde_json::Value>(r.body.trim()).is_ok());
    }

    /// A chunk boundary is a byte offset, so a server may split a multi-byte
    /// character across two chunks. Decoding to text before reassembling would
    /// turn that into replacement characters — which for a GitHub release body
    /// means any emoji in the release notes.
    #[test]
    fn a_chunk_boundary_inside_a_utf8_character_survives() {
        let heart = "💚".as_bytes(); // four bytes
        let mut raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        raw.extend_from_slice(b"2\r\n");
        raw.extend_from_slice(&heart[..2]);
        raw.extend_from_slice(b"\r\n2\r\n");
        raw.extend_from_slice(&heart[2..]);
        raw.extend_from_slice(b"\r\n0\r\n\r\n");
        let r = parse_response(&raw).unwrap();
        assert_eq!(r.body, "💚");
    }

    #[test]
    fn a_truncated_chunked_body_yields_what_arrived() {
        // No terminating zero-chunk: the cap cut it off. Everything decoded so
        // far must still come back.
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n5\r\nwor";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.body, "hellowor");
    }

    #[test]
    fn chunk_extensions_are_ignored() {
        let raw =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5;foo=bar\r\nhello\r\n0\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap().body, "hello");
    }

    /// A peer that talks forever: every read yields more bytes, after an
    /// optional pause — the shape of a server trickling a response that never
    /// ends. `exchange` being generic is what lets this run without a socket.
    struct EndlessPeer {
        pause: Duration,
    }

    impl Read for EndlessPeer {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            std::thread::sleep(self.pause);
            let n = buf.len().min(1024);
            buf[..n].fill(b'x');
            Ok(n)
        }
    }

    impl Write for EndlessPeer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn exchange_caps_how_much_response_it_buffers() {
        let peer = EndlessPeer {
            pause: Duration::ZERO,
        };
        let far_off = Instant::now() + Duration::from_secs(30);
        let response = exchange(peer, b"x", far_off, MAX_RESPONSE_BYTES).unwrap();
        assert!(response.len() >= MAX_RESPONSE_BYTES);
        // The cap is checked between reads, so one chunk of overshoot at most.
        assert!(
            response.len() < MAX_RESPONSE_BYTES + 4096,
            "{}",
            response.len()
        );
    }

    /// The per-call cap is what keeps a large JSON document from being
    /// truncated into a parse failure by a limit sized for a status reply.
    #[test]
    fn exchange_honours_a_larger_per_call_cap() {
        let peer = EndlessPeer {
            pause: Duration::ZERO,
        };
        let far_off = Instant::now() + Duration::from_secs(30);
        let response = exchange(peer, b"x", far_off, 256 * 1024).unwrap();
        assert!(response.len() >= 256 * 1024, "{}", response.len());
    }

    #[test]
    fn exchange_deadline_beats_a_trickling_peer() {
        let peer = EndlessPeer {
            pause: Duration::from_millis(5),
        };
        let start = Instant::now();
        let response = exchange(
            peer,
            b"x",
            start + Duration::from_millis(100),
            MAX_RESPONSE_BYTES,
        )
        .unwrap();
        let elapsed = start.elapsed();
        assert!(!response.is_empty());
        assert!(
            elapsed < Duration::from_secs(2),
            "trickle held exchange for {elapsed:?}"
        );
    }

    /// The transport half of the unload-hang fix: a server that accepts and
    /// then goes silent must not pin the worker — and, transitively,
    /// `Worker::drop`'s join — beyond the deadline. (Takes ~`IO_TIMEOUT` to
    /// run: the client has to actually give up on the read.)
    #[test]
    fn post_survives_a_server_that_never_responds() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (done_tx, done_rx) = std_channel::<()>();
        let sink = std::thread::spawn(move || {
            // Hold the accepted connection open without writing a byte until
            // the client has given up — dropping it early would read as a
            // clean EOF rather than a hung server.
            let (_stream, _) = listener.accept().unwrap();
            let _ = done_rx.recv_timeout(Duration::from_secs(30));
        });

        let endpoint = parse_endpoint(&format!("http://127.0.0.1:{port}/track")).unwrap();
        let start = Instant::now();
        let response = post(&endpoint, "[]").unwrap();
        let elapsed = start.elapsed();
        assert_eq!(response, "");
        assert!(
            elapsed < DEADLINE,
            "silent server held post for {elapsed:?}"
        );

        let _ = done_tx.send(());
        sink.join().unwrap();
    }

    /// `localhost` resolves to `::1` before `127.0.0.1` on plenty of systems
    /// (Windows notably), and only `127.0.0.1` has a listener here — so this
    /// passes only because `send` walks the whole address list instead of
    /// trusting the first entry. Also the one test that exercises
    /// `resolve_bounded`'s helper thread, since every other sink is an IP
    /// literal.
    #[test]
    fn post_tries_every_resolved_address() {
        use std::io::BufRead;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let sink = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line.strip_prefix("Content-Length: ") {
                    content_length = v.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).unwrap();
            let mut stream = reader.into_inner();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n1")
                .unwrap();
        });

        let endpoint = parse_endpoint(&format!("http://localhost:{port}/track")).unwrap();
        let response = post(&endpoint, "[]").unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.ends_with('1'), "{response}");
        sink.join().unwrap();
    }
}
