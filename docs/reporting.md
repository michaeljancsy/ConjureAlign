# Usage data, crash reports and update checks

Reference for src/analytics.rs, src/host.rs, src/net.rs, src/config.rs, src/crash.rs, src/crash/veh.rs, src/session_marker.rs and src/update.rs. (Moved from CLAUDE.md sections Analytics, Crash reporting, Crashes that are not panics, Update checks.)

## Analytics (opt-in, `src/analytics.rs`)

Anonymous Mixpanel events, OFF until the user consents. Three invariants, all load-bearing:

1. **The audio thread never touches analytics.** Events are raised from `initialize()` (main
   thread) and from the `Task::Analyze` arm of `task_executor()` — the capture event is built
   inside the `data` borrow but *sent* after it drops and after the phase returns to Idle, so
   neither the borrow nor `initialize()`'s spin is extended. `process()` has no analytics code,
   which is what keeps `assert_process_allocs` meaningful.
2. **Consent is tri-state and install-wide** — never asked / granted / declined — stored in
   `~/Library/Application Support/ConjureDSP/ConjureAlign/analytics.json` (`%APPDATA%\…` on
   Windows), NOT in `#[persist]` state, which is per-DAW-session. `None` (no file, or an
   unparseable one) is what shows the first-run prompt; **declining must write the file** or the
   prompt returns forever. Declining stores no device id. The file is read once per process into
   a `OnceLock`, so a change in one running DAW reaches other processes at their next launch.
   There are now **two** independent answers in that one file (`src/config.rs`): this one, under
   the JSON key `"consent"`, and the update check's, under `"updates"`. The key did not get
   renamed to match its Rust field (`Config::analytics`) on purpose — renaming it would read
   every existing install's answer as "never asked". The prompt appears while *either* is `None`
   and renders only the unanswered questions, which is what let the second question be added
   without re-asking the first.
3. **No thread outlives the dylib.** All of this now lives in `src/net.rs`, shared with the
   update check — one worker thread per process for both, via a `Mutex<Weak<Worker>>` registry;
   each `AnalyticsHandle`/`UpdateHandle` holds a `net::WorkerHandle`. `Worker::drop` sets a
   shutdown flag, drops the sender (which is what wakes a worker parked in `recv()` — joining
   before disconnecting would deadlock), then joins; while that flag is set queued jobs are
   drained *unrun*, which is what bounds the join to at most one in-flight request. Jobs are
   opaque `Box<dyn FnOnce()>` on a 32-slot bounded `try_send` channel: a wedged network drops
   work, never blocks a caller or grows a backlog. **Never let a job own the last strong ref to
   the worker** — dropping it on the worker thread makes `Worker::drop` join its own thread,
   which is precisely the nih-plug teardown bug below; `WorkerHandle` is the only route in, and
   it lives on a plugin instance. The drop-side join is bounded because every stage of a request
   is: DNS runs on a throwaway helper thread awaited with a timeout (`resolve_bounded` — the ONE
   deliberate exception to this rule, mirroring ureq's resolver: on timeout that thread is
   abandoned inside `getaddrinfo`, accepted because the alternative was a certain DAW-unload
   hang, and the image outlives it in practice — macOS pins ObjC-bearing images), every resolved
   address is tried, connect/read/write carry per-op timeouts, and the response read has a
   wall-clock deadline plus a size cap.

Transport (`src/net.rs`) is a hand-written HTTP/1.1 client over `native-tls` rather than an HTTP
client crate: the analytics request is one fire-and-forget JSON body to a fixed host, and
native-tls binds the OS stack (Security.framework / SChannel) so there is no `ring`/nasm
build-tooling risk on the Windows CI and no bundled root store to go stale. **`native-tls` is
target-gated to macOS+Windows** — on other targets it would drag in openssl, and Linux is a
build-from-source platform here, so `config_path()` returns `None`, both consent answers report
a settled `Some(false)`, and all three features are inert. `CONJURE_ALIGN_ANALYTICS_URL`
overrides the endpoint for a local sink.

`post()` returns the response *raw*, headers and all, because analytics discards it and the
smoke test wants to grep it. `get()` does not: it decodes the framing (status line stripped,
`Transfer-Encoding: chunked` de-framed) on **bytes**, not on a `String`. Both halves matter —
GitHub chunks, and chunk-size lines sit inside the byte stream, so an un-de-framed body is not
valid JSON; and chunk boundaries fall at arbitrary byte offsets, so decoding to text first and
slicing by byte index would corrupt any multi-byte character split across two chunks (an emoji
in a release note). The response size cap is per-call for the same reason: 64 KB is right for a
status reply and would silently truncate a release document into a parse failure.

The payload is bucketed on purpose (`confidence_bucket`, `offset_bucket`,
`capture_seconds_bucket`, `splice_count_bucket`): raw figures would describe the user's
material. Both capture events also carry `capture_length` and `splices` — read once from
`data.filled`/`data.sample_rate` and `data.splices.len()` inside the borrow that
`analyze_and_publish` already holds, so they cost nothing and touch no new state — and
`Capture Completed` adds `polarity_inverted`, the one un-bucketed value, because a single
bit about mic wiring cannot be coarsened further. They are on the *rejected* event too on
purpose: `reason` alone cannot separate "the user never played anything" from "they played
and it did not correlate", and a high `splices` bucket is the readable symptom of a
`gate_threshold` default that is chopping takes up. `splice_count_bucket` keeps `"max"`
distinct because at `MAX_SPLICES` the seam list stops growing and the rest of the capture
records continuously — past that the count is a floor, not a total. `capture_seconds_bucket`
routes non-finite input to the TOP bucket, not the bottom: a NaN fails every `<` guard, and
silently reporting a broken reading as the shortest capture would be worse than as the
longest. `MIXPANEL_TOKEN` holds the ConjureAlign project's token;
client-side tokens are public by design (write-only ingestion, no read access).

Every event also carries the environment, assembled in `EventContext`: `plugin_version`, `os`
(the *build target*, which cannot move), `os_version` and `daw`/`daw_version` from
`src/host.rs`, and `plugin_format` from `context.plugin_api()` — CLAP, VST3 or standalone,
with **AU folded into CLAP** because clap-wrapper translates AU calls onto our own
`clap_entry` and nih-plug genuinely never sees an AU (`daw` is what separates them again:
`"Apple AU host"`, below, is an AU load by construction). `build_payload` takes that
context as a parameter rather than reading globals, which is what keeps its assertions valid
on any machine. An unresolved value is OMITTED, never sent as null — a null lands in
Mixpanel's lexicon as a real value and pollutes every breakdown on the property.

`src/host.rs` resolves the environment once per process into a `OnceLock`, and three rules
hold it together. **Nothing there may panic**: it runs inside `initialize()`, inside the
host's `extern "C"` activation frame, where an unwind aborts the DAW — which is why the
macOS bundle lookup uses the raw `core-foundation-sys` externs with hand-written null checks
instead of the safe `core-foundation` wrappers, whose `wrap_under_get_rule` asserts on the
NULL that `CFBundleGetMainBundle()` legitimately returns in an unbundled host (`auval`,
`clap-validator`). **The DAW is an allowlisted label, never a raw name or path** — the
`current_exe()` path can contain the user's home directory, and even a bare unrecognized
stem is a fingerprint; anything off the list is `"other"` and carries NO version, since an
unknown program plus its version identifies far more than either half. **The labels are wire
values**: renaming one splits its history in Mixpanel, so add freely and rename never.

Why `current_exe()` and not the host name the plugin API offers: nih-plug exposes neither
CLAP's `clap_host.name` nor VST3's `IHostApplication::getName`, so reaching them would mean a
fourth patch on the vendored tree — and it would be *wrong* for AU, where clap-wrapper is the
CLAP host and would name the wrapper rather than Logic. Platform sources: macOS reads
`CFBundleShortVersionString` from the main bundle and `kern.osproductversion` via `sysctl`
(note macOS serves a capped "10.16" to processes linked against a pre-Big-Sur SDK, so a very
old DAW under-reports); Windows reads the executable's `VS_FIXEDFILEINFO` block via
`version.dll` and the OS version from `os_info`. All four deps were already compiled into
their targets before being declared — `os_info` is Windows-ONLY in this graph, because
`sentry-contexts` uses `uname` elsewhere, which is why macOS goes through sysctl instead.
The `daw` labels include `auval`, `pluginval` and `clap-validator`: they load the plugin on
a machine that may well have consented, and labelling them is what lets them be filtered out
of the real usage figures rather than silently inflating the "other" bucket.

**Logic Pro never reports as `"Logic Pro"` on current macOS — it reports `"Apple AU host"`.**
Logic does not load an AU into its own process: it asks macOS to, and macOS puts it in
Apple's helper, `/System/Library/Frameworks/AudioToolbox.framework/XPCServices/
AUHostingServiceXPC_arrow.xpc/Contents/MacOS/AUHostingServiceXPC_arrow` (verified with
Logic Pro 12.3.1 on 2026-11-10). So `current_exe()` names Apple's helper, and up to 1.4.1
every Logic user was counted as `"other"`, mixed in with unrecognized VST3 hosts; "Logic Pro"
never appeared in the data at all. `daw_label` now maps three exact stems to that one label:
`AUHostingServiceXPC_arrow` (Apple silicon) and `AUHostingServiceXPC` (Intel, used under
Rosetta), both present on macOS 26.3, and `AUHostingCompatibilityService`, which is not on
macOS 26.3 but which users and Ableton's support pages name as the helper that hosted Intel
AUs on earlier macOS. Read the label as "some Apple-hosted AU load", NOT as "Logic": the
helper cannot see which app asked for it, and GarageBand, MainStage and Final Cut Pro go
through the same one. It carries **no `daw_version`**, like `"other"` (`host::reports_version`):
the main bundle in that process is the helper's own, whose version is a constant "1.0", and
sending it would read as a Logic version. The `"Logic Pro"` entry stays on the list for any
setup where Logic does load the AU into itself. Data from before this change cannot be
re-labelled: in it, Logic is part of macOS `"other"` with `plugin_format = "CLAP"` (AU folds
into CLAP, above).

Apple's helper does not sandbox the plug-in in practice: from inside `AUHostingServiceXPC_arrow`
the plug-in reads the normal home folder, writes its settings, and reaches Mixpanel, Sentry and
GitHub, so consent, analytics, update checks and crash reports all work for Logic users.

The bundle lookup cannot be reached by a unit test — a test binary has no `.app` around it.
`host::tests::print_resolved_host_info` (`#[ignore]`d) exists for that: copy the test binary
into `Foo.app/Contents/MacOS/<an allowlisted name>` with an Info.plist beside it and run it
from there, which is the same shape as a DAW loading the plugin.

Three events: **`Plugin Loaded`** (once per instance, from `initialize()`), **`Capture
Completed`**, **`Capture Rejected`**. The first is deliberately NOT called "Session Start" —
Mixpanel ships a built-in virtual event, `$session_start`, whose *display name* is exactly
"Session Start", so a custom event by that name is indistinguishable from it in the event
picker. Check `Get-Events` before naming anything new. Verify ingestion with
`cargo test --release -- --ignored --nocapture smoke_test`, which asks Mixpanel for
`verbose=1` and asserts `"status": 1` — a bad token otherwise reads as a silent bare `0`.
**It writes one real event to the live project**, tagged `smoke-test` — and deliberately
with the REAL host context, so a newly added `daw` or `os_version` value gets seen once in
the project before it arrives from a user.

`Plugin Loaded` also carries **`upgraded_from`** — the version that ran on this install
before this one — backed by `config::pending_upgrade_from` and the `last_version` key in
`analytics.json`. It is present only on the launch that first observes a version change, so
one upgrade produces one marked event however many instances the host loads; absent means
"first run, or an upgrade we cannot prove", and the two are deliberately indistinguishable
(an install arriving from a build that predates the field looks like a first run). The
write happens behind the `enabled()` check in `flush_session`, so a declined install stores
nothing, and only when the version actually changed, so an ordinary launch touches no disk.

It exists because **the cohort-level version breakdown is the wrong instrument.** Asking
Mixpanel "has any device been seen on two versions?" cannot separate "nobody upgraded" from
"nobody was told there was an upgrade" — and on 2026-08-31 the answer was the latter: the
update check itself first shipped in 1.2.0 (`src/update.rs` landed in `c14c06e`, after the
v1.1.0 tag), so the 24 installs then on 1.1.0 had no in-plugin way to learn 1.2.0 existed.
That breakdown is also silently degraded by anything resetting the device id, including the
`/reinstall` skill and the uninstallers. A per-device before/after has none of those
problems. Do not re-derive upgrade rates from disjoint version buckets; read
`upgraded_from`. (Disclosure: a previously-running version is strictly less revealing than
`plugin_version`, already on every event, so this needed no change to the prompt or README —
which is exactly why it has to be written down here.)

`Plugin Loaded` also carries **`update_checks`** and **`update_notice`** (issue #58), and
unlike `upgraded_from` both are ALWAYS present: every install has a value for each, so
there is nothing to omit, and a missing key would read as "unknown" instead of "no".
`update_checks` is the install's answer to the update-check question, `"on"` / `"off"` /
`"unasked"` (from `config::Config::updates`: `Some(true)` / `Some(false)` / `None`), kept
as three states because an install that was never asked cannot have ignored anything.
`update_notice` is a bool: whether the ⚙ button says "Update" for this install right now,
i.e. `update::notifies` over the cached `update_latest_seen` and `update_skipped`, the same
two inputs `update::pending_version` uses. They are wire values; never rename them. The
event is built by `analytics::plugin_loaded(cfg, status)` from a settings snapshot, so the
tests drive every consent state and notice on / off / skipped without this machine's file.
Both are read wherever `Plugin Loaded` is built: in `initialize()`, or, when consent arrives
after it, in the first `track()` call, which runs on the background analysis task. Neither is
the audio thread, and neither touches the network or writes a file.

They exist because "installs stay on old versions" has two causes the data could not tell
apart: the user said no to update checks, or the user saw "⚙ Update" and ignored it.
Read them with `upgraded_from` per install: of the installs shown a notice, how many
upgraded? Data arrives only from installs running the release that ships this, so an
empty breakdown on an older version means nothing. Disclosure check (done): a yes or no to
update checks, and whether an update is pending, reveal less than `plugin_version`, which
every event already carries, so the prompt and README needed no change.

UI: the first-run prompt (`editor::consent_modal`) and the ⚙ popover (`settings_menu`) are both
drawn OUTSIDE `draw_ui` / from the control bar respectively, and both are `pub` so
`examples/gui_preview.rs` can render them headless (`_consent.png`, `_settings.png`,
`_settings_update.png`) — a consent dialog has no business in the panel screenshots. The prompt
is each question's heading plus its two buttons and nothing else, with one closing line saying
the answers are changeable under ⚙; what detail survives is in the README, plus the popover for
the update check. It renders only its unanswered questions, so the preview points `HOME` at a
scratch directory to get a virgin install; without that it would render whatever this machine
has already answered. Two layout constraints learned the
hard way: the status strip has **zero slack at the 600×460 minimum** (its labels already reach
the Capture button; every one goes through the truncating `status_label` and the row clips at
the button edge — full text on hover — so parking anything else there costs status text),
which is why the gear rides the centered control-bar row's spare width instead; and
egui never flips a popup, so the gear's popover is explicitly `AboveOrBelow::Above` or it opens
out through the window bottom.

Never use a native OS dialog (`NSAlert`/`MessageBox`) for consent: headless scanners (`auval`,
`pluginval`, Logic's scan) instantiate the plugin with no GUI, and a modal there hangs the scan.

## Crash reporting (opt-in, `src/crash.rs`)

Sentry, gated on the **same** consent answer as analytics — `analytics::enabled()`, the same
`analytics.json`, one question in the UI. There is no second toggle and no second identifier:
reports are labelled with `analytics::device_id()`, the same random install id.

The two analytics invariants above hold here too (nothing before consent; no thread outliving
the dylib — `Reporter` is held through a `Mutex<Weak<Reporter>>` registry exactly like
`net::Worker`, and dropping the last strong ref closes the client, which ends the release-health
session and joins Sentry's transport thread). Three things are specific to panics:

1. **The hook is per-image, gated on consent only, and captures via `Hub::main()`.** A panic
   hook lives in the panicking image's own statically-linked std, so the host's panics and
   other Rust plugins' can physically never reach ours — every panic our hook sees was raised
   inside this dylib, and all of those are reported (a panic in egui/baseview/nih-plug code we
   ship is still our crash). The `crash::scope()` guards in `initialize()`, `process()`,
   `reset()`, the `task_executor` closure and the editor's build and draw closures feed the
   `in_scope` tag — attribution (known callback vs GUI event loop / helper thread / dependency
   internals), NOT a gate. The one skip: panics on Sentry's own `sentry-*` threads, which are
   must-not-panic and where a capture+flush would wait on the failing machinery itself.
   `PanicIntegration` is only ever *constructed* (for `event_from_panic_info`), never
   registered — registering would install a second, ungated hook next to ours
   (`default_integrations` is `false`, the other four listed by hand; `attach_stacktrace` is
   on so `report_issue` messages carry a stack). Captures and the hook's bounded flush go
   through `Hub::main()`, and `reporter()` re-binds the fresh client on that hub after every
   `sentry::init` — init alone binds only the *calling* thread's hub while every other
   thread's hub is a never-re-synced snapshot, so a consent decline→re-grant from the editor
   would otherwise leave audio/main/bg-worker panics captured into the closed client for the
   rest of the process (pinned by `tests/crash_regrant_threads.rs`). Everything the hook
   (and `crash::scrub`) touches on the panicking thread must use the `_in_hook` analytics
   accessors / `try_lock` — the panicking frame may hold those very locks, so a blocking
   `lock().unwrap()` there is a same-thread deadlock or a panic-inside-the-hook abort.
2. **The hook must chain, and must be installed late.** nih-plug installs a global hook of its
   own from the CLAP `clap_entry.init` / VST3 `bundleEntry` dylib entry points (`setup_logger()`
   → `log_panics()`), long before `Plugin::default()`. Ours is installed on consent, so
   `take_hook()` picks that one up and always calls it — nih-plug's stderr/`NIH_LOG` panic
   logging keeps working. Installing from a library constructor would run *before* nih-plug and
   be silently replaced.
3. **The hook body is wrapped in `permit_alloc`.** A panic can originate on the audio thread —
   the `AtomicRefCell` borrows in `process()` are the likeliest failure in this codebase — where
   `assert_process_allocs` would otherwise turn the report into a second panic inside the first.
   nih-plug's own hook does the same. The `scope()` guard itself is just a thread-local
   increment, so `process()` still allocates nothing.
4. **An editor panic is reported AND swallowed.** Both editor closures run their body inside
   `editor::guarded_frame` (`catch_unwind` + `AssertUnwindSafe`), because they are called from
   an `extern "C"` frame — a CFRunLoop timer on macOS, a window proc on Windows — and unwinding
   out of one aborts the host: an arithmetic bug in `view_math` killed Ableton Live instantly
   (CONJUREALIGN-3). The `crash::scope()` guard stays OUTSIDE the catch, which is what preserves
   attribution: the hook runs at the panic site, before the unwind. On a catch the whole
   `EditorState` is replaced by `EditorState::after_panic()` — it may be half-updated, and its
   snapshot is the likeliest input to the panic — which latches `panic_screen` (a message plus a
   "Reload the editor" button, previewed as `gui_preview_panic.png`) in place of the panels.
   The latch is not cosmetic: without it a panic that recurs every frame costs a Sentry report
   and a blocking 2 s flush at 60 Hz. Nothing here touches the audio thread, so a dead editor
   still leaves the correction running and every parameter reachable from the host's generic UI.

Crash reports carry the same environment as the analytics events — `plugin_api`,
`sample_rate`, `daw`, `daw_version`, `os_version` as tags. `set_host_context` resolves ALL of
it on the main thread and stores plain data, so `scrub` (which runs on the *panicking* thread)
only clones out of the mutex; calling `host::info()` from there instead would run
`current_exe()` and a CoreFoundation lookup inside a panic hook. `os_version` is not
redundant with sentry's own OS context: `sentry-contexts` fills that from `uname`, which on
macOS reports the Darwin version ("25.3.0"), not the one users and release notes talk about
("26.3.1").

`before_send` (`crash::scrub`) is the last gate before anything leaves: `server_name` is
nulled (`sentry-contexts` fills it from the `hostname` crate), `user` is reduced to the device
id, and `debug_meta.images` is trimmed to our own dylib, by file name — `debug-images` otherwise
enumerates every shared library in the process, i.e. every other plugin the user owns, by full
path. All of `scrub` is now an unstated guarantee.

**The analytics prompt no longer enumerates what is collected.** `analytics_question` is a
bare question and its `privacy_section` checkbox a bare checkbox, by an explicit product
decision on 2026-08-28 — so nothing in the UI describes the payload, and the README's
Sent/Never-sent table that used to is gone. Adding a property therefore changes no
user-facing text, which is exactly why it is worth noticing: this file and the
`analytics`/`host` module docs are the only remaining record of what leaves the machine.
Re-check the disclosure question before shipping a property more revealing than the bucketed
outcomes already sent.

That decision covers question ONE only, and it is about *disclosure*. `updates_question` is
bare in the first-run prompt as well, but for an unrelated reason — the prompt was cut to
headings and buttons on length grounds — and its half of `privacy_section` and the README
still carry its copy. That copy is still a promise: "sends nothing about you, never installs
anything" has to stay true of `update.rs`.

`RejectReason` is NOT an error: it is an expected user outcome, already logged and already a
Mixpanel event. Routing it here would bury real crashes. Only panics and genuine
should-not-happen conditions (`crash::report_issue`) go to Sentry.

The `sentry` dep is target-gated to macOS+Windows for the same reason `native-tls` is, and uses
`default-features = false`: the default `transport` feature is `["reqwest", "native-tls"]` and
`reqwest` pulls `tokio`, i.e. an async runtime inside every DAW. The `ureq` transport is
declared `default-features = false` upstream, so no `rustls`, no `ring`, no nasm on the Windows
CI. `CONJURE_ALIGN_SENTRY_DSN` overrides the DSN for a local sink (see
`tests/crash_consent.rs`).

**`crash::BoundedUreq` exists because sentry's stock ureq agent is wrong for a plugin in two
ways.** Do not simplify it back to the default transport:

- **No timeouts.** Sentry's transport configures TLS and proxying but sets none, and every
  `ureq` timeout defaults to `None`. `TransportThread::drop` then joins its worker with
  `handle.join().unwrap()` — unbounded — so one request that never completes (captive portal, a
  firewall that blackholes the connection) blocks the drop forever, on whichever host thread is
  unloading the plugin. A DAW would hang instead of quitting. `CONNECT_TIMEOUT`/`REQUEST_TIMEOUT`
  bound the worst-case unload stall to `SHUTDOWN_TIMEOUT` plus one in-flight request.
- **`RootCerts` defaults to `WebPki`.** That makes ureq call `disable_built_in_roots(true)` and
  trust a *bundled Mozilla store* — the exact thing choosing native-tls was meant to avoid.
  `BoundedUreq` sets `RootCerts::PlatformVerifier` so TLS rides the OS trust store.

**`ureq` must be taken on the `native-tls` feature, never `native-tls-no-default`.** The latter
looks equivalent — it pulls the native-tls crate, and the dependency graph reads correctly — but
ureq's `src/tls/native_tls.rs` is gated on `#[cfg(feature = "native-tls")]`, so the backend is
simply not compiled. `TlsProvider::NativeTls` then panics on the first https request. Raised on
sentry's transport thread, that panic becomes a **host abort**: `TransportThread::drop` joins
with `handle.join().unwrap()`, so the `Err` from the dead worker unwraps on the host's main
thread and unwinds out of the VST3 `extern "C"` teardown. It crashed Ableton Live on plugin
removal. `crash::tls_backend_is_compiled_in_not_just_the_crate` guards it offline, by pointing
the real agent at a local listener that hangs up: a compiled backend errors, a missing one
panics. The cost is `webpki-root-certs` back in the tree — dead weight only, since
`PlatformVerifier` means those roots are never consulted.

That last mechanism generalizes: **any** panic on sentry's transport thread — TLS, envelope
serialization, anything — becomes an abort of the host at unload, because of that unguarded
`join().unwrap()` upstream. There is no way to catch it from our side, so the only defence is
that the transport must not panic. Treat changes to `BoundedUreq` or the `ureq` features as
crash-risk changes and exercise a real TLS connection, not just `cargo tree`.

Nor is the transport thread the only upstream landmine at that seam: sentry-core's session
flusher spawns its thread with `.unwrap()` (a panic on the consenting editor frame under
thread exhaustion) and its Drop unwraps its own mutexes after swallowing a worker panic — so
`sentry::init` is wrapped in `catch_unwind` inside `reporter()` (a failed init degrades to
no-reporting and retries later), `sync_consent` decides under the per-instance mutex but runs
init/teardown OUTSIDE it (a decline's client drain would otherwise freeze the editor frame
while `initialize()` convoys on the lock), and BOTH sentry threads are treated as
must-not-panic.

**Release builds symbolicate to nothing without a debug-file upload.** `[profile.release]` keeps
`strip = "symbols"`; `debug = "limited"` + `split-debuginfo = "packed"` leave a `.dSYM`/`.pdb`
beside the binary that the shipped bundle does not contain, and the Mach-O UUID / PE build id is
what matches the two up. `scripts/release.sh` and `.github/workflows/windows.yml` upload them
via `sentry-cli`, and both **skip with a warning** rather than failing when the credentials are
absent — a release that ships beats a symbolicated crash report, and the upload can be repeated
later from the same build tree.

The two gate on that differently, on purpose. CI reads `secrets.SENTRY_AUTH_TOKEN` plus the
`SENTRY_ORG`/`SENTRY_PROJECT` repository *variables* (slugs and ids are not credentials) and
checks the env vars directly, because on a runner that is the only route they can arrive by.
`release.sh` instead asks `sentry-cli info`, because locally the token usually comes from
`~/.sentryclirc` — which is the only place `sentry-cli login` writes — and an env-var check
there reports a perfectly authenticated machine as unconfigured and silently drops the symbols.
Note that `login` sets the token but NOT the org/project defaults, so those still need to be in
the environment or in `~/.sentryclirc`'s `[defaults]`; that is the likeliest way an
authenticated machine still fails the upload. The second likeliest is scope: a read-only token passes `sentry-cli info`, then the upload
403s and the script warns and continues, so a release built that way can never be
symbolicated. An org auth token (sentry.io → Settings → Auth Tokens) works; it lists only the
scope `org:ci`, which is enough. Do not reject a token for lacking `project:releases`. The
reliable pre-flight before a macOS release is a real one-file upload of any existing dSYM
(`sentry-cli debug-files upload <path>`, without `--include-sources`): it takes seconds, and
Sentry de-duplicates by debug id. The token on the main Mac can upload but cannot read
issues. v1.0.0 and v1.1.0 shipped without symbols and cannot be symbolicated.

Both call sites name the plugin's own binary and dSYM/PDB explicitly rather than pointing at a
release directory. That is not tidiness: `sentry-cli` searches paths recursively, so a whole
release tree uploads every dependency's `build_script_build`, the proc-macro dylibs, the test
binaries and `xtask` — 334 files on the run that caught this, of which 3 were ours — and
`--include-sources` bundles their source too.

The Sentry `release` is `conjure_align@<CARGO_PKG_VERSION>` and must stay in step with what the
upload tags. **macOS has no CI**, so a macOS release only ever gets symbols if this machine is
configured — and debug files match by build id, so a release shipped without them cannot be
symbolicated afterwards without reproducing a byte-identical binary. Adding the
dep cost ~1.8 MB per architecture slice (4.996 → 6.803 MB), i.e. ~10.8 MB on the macOS pkg (two
architectures × three bundles) and ~3.6 MB on the Windows zip.

## Crashes that are not panics (`src/session_marker.rs`, `src/crash/veh.rs`)

`crash.rs` above is a **panic hook and nothing else** — `std::panic::set_hook` is the only
handler in the tree. An access violation, a stack overflow, an `abort()`, or a fault inside
linked C/C++ (clap-wrapper, the GL driver, SChannel) kills the host with nothing sent. On
Windows that is most of what "crash" means, and it is how a 1.2.0 install could crash a DAW
repeatedly while the Sentry project stayed completely empty (2026-08-30, LUNA/Windows).

Release health does not fill the gap either, and the reason is not obvious: sentry-core
enqueues a *healthy* session only from `Session::drop` (`Drop` → `close(Exited)` →
`enqueue_session`; there is no periodic send). A host that dies never reports one. So **"no
Sentry data for version X" is ambiguous** between "nobody runs X" and "everyone who runs X
dies" — do not read an empty release as an unused release.

The fix is to stop reporting *from* the dying process. `session_marker` writes one file per
pid under `<config dir>/sessions/`, holding the environment and a `stage`; a clean teardown
deletes it. Anything left behind by a process that no longer exists was an abnormal
termination, and the next launch reports it. Four rules:

1. **Consent gates the file, not just the report.** A declined user gets nothing on disk.
   A fault record counts as evidence only when it is **non-empty**: `veh::install` opens
   that file with `OPEN_ALWAYS`, so on Windows every consenting session has an empty one
   from the moment reporting arms, and testing for existence kept every marker across every
   clean exit — a false unclean-shutdown report on every launch (caught by CI, 2026-08-31).
2. **Never from the audio thread, and never on the `initialize()` fast path.** The stages
   are `initializing` / `initialized` (from `initialize()`), `editor_creating` /
   `editor_open` (around `Editor::spawn`, via the delegating `StageStamped` wrapper in
   `editor/mod.rs`) and `editor_closed` (that handle's `Drop`). None corresponds to
   `process()`, and none may. Both `initialize()` stamps go through `set_stage_if`: the
   entry one only on the slow path and only when the current stage is not an editor one
   (a state load with a window open must neither write a file on the path that exists to
   avoid dropouts, nor claim there was no window), and the exit one only if that entry
   stamp actually landed.
3. **A live pid is never reported** — that would turn pid reuse into an invented crash.
   Skipping one can only *lose* a report, which is the safe direction.
4. **The report is `Level::Warning`, and carries the dead session's own release.** Warning
   because `Session::update_from_event` marks a session errored at `>= Error`, and the
   session being captured into belongs to the *healthy* process doing the reporting — `Error`
   would corrupt the very crash-free rate this exists to make trustworthy. The release
   override (`conjure_align@<the version that died>`) is what makes a version whose every
   session crashed visible at all. Its environment tags are `prev_`-prefixed because `scrub`
   stamps `daw`/`daw_version`/`os_version` from the *current* process.

The sweep runs once per process, dispatched onto the shared `net::Worker` (so `initialize()`
never scans a directory) from the arming branch of `CrashHandle::sync_consent`. It deletes
each marker as it reads it — a marker that somehow crashed the reporter must not be re-read
forever — and caps itself at 32 files and 8 reports.

Crash reporting also arms in **`Plugin::editor()`**, not just `initialize()`: hosts may build
the view before activating, and opening a window is the heaviest native work this plugin
does. It deliberately does NOT arm in `Plugin::default()` — that would start Sentry and make
a network request during a plugin scan, for an instance nobody uses.

`src/crash/veh.rs` (Windows only) adds a vectored exception handler for the hardware faults
themselves. It cannot report — a crashing process is no place for an HTTPS request — so it
writes a fixed-width record into `sessions/<pid>.fault` and the sweep folds it into the next
launch's event. **It is process-global**, so it is written to be invisible to everyone else:
`AddVectoredExceptionHandler` (chainable) never `SetUnhandledExceptionFilter` (a single
global slot the host may own); a fatal-code allowlist that excludes `0xE06D7363` (C++
`throw`) and the other codes hosts raise routinely; an "is the faulting address inside our
own image" test; a re-entrancy guard; no allocation, no locks, no CRT; and it always returns
`EXCEPTION_CONTINUE_SEARCH`. **`uninstall()` runs from `Reporter::drop`** — a handler left
registered across a DLL unload means the OS calls into freed memory at the next exception
anywhere in the process, which would be far worse than the bug it exists to find.

Two things it deliberately cannot do: a fault *inside the GPU driver* called from our code
has its address in the driver, so the ownership filter rejects it; and
`EXCEPTION_STACK_OVERFLOW` leaves so little stack that the write may not complete. The
marker's `stage` is the coarse answer in both cases.

`tests/veh_windows.rs` is the only place the handler ever runs before a release (no Windows
toolchain locally): it re-execs the test binary, arms through the real `sync_consent` path,
and dereferences null. The child calls `SetErrorMode(SEM_NOGPFAULTERRORBOX)` first, or a WER
dialog would hang CI instead of failing it.

## Update checks (opt-in, `src/update.rs`)

Asks GitHub's `/repos/michaeljancsy/ConjureAlign/releases/latest` for `tag_name` and compares
it to `CARGO_PKG_VERSION`. **It notifies and nothing else** — no download, no install, no
self-update. The bundle is mapped into a running host, `/Library/Audio/Plug-Ins` needs admin
rights, and the shipped pkg is signed/notarized/stapled; re-implementing any part of that
inside a plugin buys nothing. The REST API rather than a manifest committed here, deliberately:
a manifest is one more thing to bump at release time, and forgetting it fails silently in the
"nobody hears about the new version" direction, i.e. the exact failure the feature exists to
prevent. `/releases/latest` already skips drafts and pre-releases, so un-advertising a bad
release means marking it pre-release — which is the right thing to do to it anyway. No release
script changes; the notice appears as soon as the Release is published.

Its own consent answer (`"updates"` in `analytics.json`), asked as a **second question in the
same first-run prompt**, deliberately not folded into the analytics one: this check shares no
data and mints no identifier, so tying it to the analytics answer would cost update notices to
users who declined for reasons that have nothing to do with it.

Four invariants:

1. **Checks run from the editor only, NEVER from `initialize()`.** `auval`, `pluginval` and
   Logic's scan all instantiate and initialize the plugin headlessly; a network request during
   a plugin scan is bad manners and can slow it. The one automatic check is in `editor::create`'s
   *build* closure (a window opened, so a human is present), gated on a granted answer and on
   `config::should_check` (24 h after a success, 6 h after a failure, and a clock that went
   backwards forces one rather than locking it out). This is the same reasoning that forbids a
   native consent dialog.
2. **A manual check ("Check now" in the ⚙ popover) runs whatever the stored answer is** — the
   click is the consent for that one request — and must never write an answer the user did not
   give. `tests/update_check.rs` pins that.
3. **The link is a compile-time constant, never a URL from the response.** Clicking it hands a
   URL to the OS browser via `open::that_detached`, so `html_url` is read from the JSON by
   nobody: `parse_release` takes `tag_name`, parses it to three integers, and renders them back
   out, so nothing attacker-shaped can reach the UI, the config file or the browser.
4. **A queued check that never runs must not wedge anything.** `net`'s worker drops whatever is
   still queued at shutdown, and refuses work when its queue is full or its thread never
   spawned. Left alone that would strand `IN_FLIGHT` (no further check for the life of the
   process) and the status on `Checking` — which the editor treats as "animate", i.e. a
   permanent 60 Hz repaint in a DAW. A `CheckGuard` moved into the closure releases both from
   `Drop`, so dropping the job unrun is self-correcting.

UI: the whole notification is the ⚙ button's label — plain `⚙`, or `⚙ Update` in
`ACCENT_DETECTED` when there is something to see. It rides the centred control row's spare
width (~140 px at the 600 px minimum) for the same reason the gear itself does; the status
strip has none. Nothing louder: an update notice must never interrupt a session, and a banner
would cost a row out of the graphs' budget. "Skip this version" stores `update_skipped`, and
anything newer notifies again — an unparseable stored value fails *open* (notify), because a
silenced-forever update the user cannot see or clear is the worse bug. `Status::Failed` is
surfaced only inside the popover, which the user opened on purpose; a failed automatic check
produces no label, no banner and no dialog.

`CONJURE_ALIGN_UPDATE_URL` overrides the endpoint for a local sink.

## Rules the code keeps (short form)

The short rules that used to sit in CLAUDE.md, word for word. The sections above give the
reasons.

- Nothing is sent and nothing is written before the user answers. Declining writes the file.
- Analytics and crash reports share one answer. The update check has its own second question.
- The JSON key `"consent"` in `analytics.json` keeps its name; renaming it re-asks every install.
- Event names, property names and `daw` labels are wire values. Add freely, never rename.
- Leave out a value you cannot resolve. Never send null.
- No thread may outlive the library. A job must never hold the last reference to the worker.
- Update checks run only from the editor, never from `initialize()`. The download link is a fixed
  constant, never a URL from the response.
- Do not simplify `crash::BoundedUreq` back to sentry's default transport. Take `ureq` on its
  `native-tls` feature, never `native-tls-no-default`. Treat any change there as a crash risk and
  test a real TLS connection.
- A user-visible "rejected capture" is not an error. Only panics and should-not-happen conditions
  go to Sentry.
