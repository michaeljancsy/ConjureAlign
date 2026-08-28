# nih-plug upstream issue draft: `BackgroundThread` teardown

**Status: DRAFT — not filed.** For Michael to review and file by hand. Nothing in this
repository posts it anywhere. The local mitigation we ship in the meantime is the vendored
`deps/nih-plug` (see `deps/PATCHES.md`); the diff proposed below is that patch minus the
vendoring scaffolding, so if upstream takes it (or an equivalent), the vendor can be
dropped at the next rev bump.

**Where to file (checked 2026-08-28).** robbert-vdh/nih-plug is officially in maintenance
mode (its README says so and redirects framework users to the community successor), so a
report there — <https://github.com/robbert-vdh/nih-plug/issues/new> — is mostly for the
record. The active successor is **nice-plug**
(<https://codeberg.org/RustAudio/nice-plug>), and the report matters MORE there: the same
two bugs exist verbatim in its `crates/nice-plug/src/event_loop/background_thread.rs`, and
nice-plug has already fixed the wrapper reference cycle (its closed issues #3/#4 are the
equivalent of nih-plug PR #225), which means the masking described under "Related reports"
is *gone* there — teardown really runs, so the self-join and the worker-suicide are live,
not latent, and its tracker has no report of either. When filing at nice-plug, adapt the
body: their file path above, `nice_trace!` for `nih_trace!`, and invert the Related-reports
framing ("the #3/#4 leak fix armed these two" instead of "merging #225 will"). The diff and
both regression tests port essentially unchanged.

Suggested title:

> BackgroundThread: destroying an instance mid-task makes the shared worker join itself
> (deadlock/panic), and a dead instance's queued task shuts the worker down for all other
> instances (silent task loss, then a host abort at teardown)

Everything below the rule is the proposed issue body.

---

## Summary

`src/event_loop/background_thread.rs` shares one `bg-worker` thread between every instance
of a plugin in the process. Two related lifecycle bugs, both reachable from ordinary host
behavior whenever an `execute_background()` task is in flight around instance destruction
(all links pin master as of
[`f36931f`](https://github.com/robbert-vdh/nih-plug/blob/f36931f7af4646065488a9845d8f8c2f95252c23/src/event_loop/background_thread.rs)):

1. **Destroy during a running task → the worker joins itself.** The worker holds the
   upgraded `Arc<E>` (the wrapper) while executing a task. If the host destroys that
   instance concurrently, the *last* strong reference dies on the worker thread, so
   `WorkerThread::drop` runs there and `join()`s its own thread: `pthread_join` returns
   `EDEADLK` and std panics on macOS/Linux; `WaitForSingleObject(INFINITE)` never returns on
   Windows, parking the worker (and the half-finished wrapper teardown) forever.
2. **Destroy while a task is queued → the worker dies for everyone.** When the worker
   dequeues a task whose `Weak<E>` no longer upgrades, the loop `return`s — shutting down
   the thread that every *other* live instance shares. From then on scheduled tasks are
   dropped silently in release builds, and the eventual last-instance teardown panics on the
   now-disconnected channel inside the host's `extern "C"` destroy call — a host abort at
   project close.

Both were found from a plugin that schedules a background analysis task per user action, so
tasks are routinely in flight when users remove an instance or close a project.

## The relevant code

- Tasks travel as `Message::Task((T, Weak<E>))`; the worker upgrades per task and holds the
  `Arc` for the duration of `e.execute(task, true)`
  ([background_thread.rs#L145-L156](https://github.com/robbert-vdh/nih-plug/blob/f36931f7af4646065488a9845d8f8c2f95252c23/src/event_loop/background_thread.rs#L145-L156)),
  with `None => return` as the upgrade-failure arm.
- `WorkerThread::drop` does `send(Message::Shutdown).expect(…)` followed by
  `join().expect(…)`, unconditionally
  ([background_thread.rs#L94-L107](https://github.com/robbert-vdh/nih-plug/blob/f36931f7af4646065488a9845d8f8c2f95252c23/src/event_loop/background_thread.rs#L94-L107)).
- Lifecycle is reference-counted through `HANDLE_MAP`'s `Weak<WorkerThread<T, E>>`; the last
  `BackgroundThread` to drop tears the thread down.

## Bug 1: destroy during a running task → self-join

Sequence, written for the CLAP wrapper (VST3 is symmetric):

1. Instance A calls `execute_background()`; the worker dequeues the task, upgrades the
   `Weak<Wrapper<P>>`, and enters `execute()` holding a strong `Arc<Wrapper<P>>`.
2. The host calls `clap_plugin::destroy` concurrently — legal, since the background thread
   is invisible to it. `destroy` reconstructs and drops the wrapper's `Arc`
   ([wrapper/clap/wrapper.rs#L1862-L1868](https://github.com/robbert-vdh/nih-plug/blob/f36931f7af4646065488a9845d8f8c2f95252c23/src/wrapper/clap/wrapper.rs#L1862-L1868)).
   Its `nih_debug_assert_eq!(Arc::strong_count(&this), 1)` already fires here in debug
   builds — the worker holds the second reference. (The same assert also fires for the
   unrelated reference-cycle leak of #222, so seeing it fire does not by itself
   distinguish the two.) The VST3 wrapper documents the same
   last-reference assumption in its `Drop`
   ([wrapper/vst3/wrapper.rs#L60-L64](https://github.com/robbert-vdh/nih-plug/blob/f36931f7af4646065488a9845d8f8c2f95252c23/src/wrapper/vst3/wrapper.rs#L60-L64)).
3. `execute()` returns and the worker drops its upgraded `Arc` — now the last strong
   reference. `Wrapper::drop` therefore runs **on the worker thread**, and dropping the
   wrapper's event loop drops the last `Arc<WorkerThread>`.
4. `WorkerThread::drop` sends `Shutdown` into its own queue (which succeeds) and then
   `join()`s **the thread it is running on**:
   - macOS/Linux: `pthread_join` returns `EDEADLK`, and std's join panics (verified against
     std 1.93.1). The panic unwinds out of a `Drop` on the worker; the thread dies
     mid-teardown and leaks whatever it still owned. Nothing reaches the host, so the
     failure is silent unless a panic hook is watching — background tasks just stop working.
   - Windows: `WaitForSingleObject(handle, INFINITE)` on the thread's own handle never
     returns. The worker is parked forever inside plugin code: a leak while the module stays
     loaded, and a latent access violation if the host later unloads the DLL.

## Bug 2: a dead instance's queued task kills the worker every other instance shares

1. Instances A and B share the worker. A's task sits queued — for example behind a
   long-running task of B's.
2. The host destroys A. Nothing purges A's queued message; its `Weak` is now dead.
3. The worker reaches A's message, `upgrade()` returns `None`, and the loop **`return`s**:
   the shared thread exits and its `Receiver` is dropped.
4. Every later `schedule()` — from B or any other instance, for GUI-notification or plugin
   tasks alike — fails its `try_send` on the disconnected channel. The only report is
   `nih_debug_assert!(task_posted, …)` at the call sites (e.g.
   [wrapper/clap/wrapper.rs#L708-L709](https://github.com/robbert-vdh/nih-plug/blob/f36931f7af4646065488a9845d8f8c2f95252c23/src/wrapper/clap/wrapper.rs#L708-L709)),
   so release builds drop tasks silently. On Linux this includes **GUI** tasks scheduled off
   the main thread: `LinuxEventLoop::schedule_gui` delegates them to this same
   `BackgroundThread`
   ([event_loop/linux.rs#L41-L53](https://github.com/robbert-vdh/nih-plug/blob/f36931f7af4646065488a9845d8f8c2f95252c23/src/event_loop/linux.rs#L41-L53)).
5. When the last instance is destroyed, `WorkerThread::drop` runs
   `send(Shutdown).expect(…)` on the disconnected channel and panics inside the host's
   destroy path. Unwinding out of the `extern "C"` boundary into a C++ host is an abort in
   practice — and if teardown happens while anything else is unwinding, it is a
   panic-in-destructor-during-cleanup abort immediately (that variant is what the
   reproduction below shows).

## Reproduction

Two unit tests against `BackgroundThread` directly, no host involved — they are written as
regression tests for the fix below, so against current master they fail by demonstrating
each bug. Apply the appendix diff to `src/event_loop/background_thread.rs` and run:

```
cargo test -p nih_plug --lib background_thread
```

Observed against master (macOS, rustc 1.93.1):

- `dead_executor_does_not_kill_the_shared_worker` fails its "survivor's task still runs"
  assertion, and while that failure unwinds, dropping the two handles hits the `.expect` in
  `WorkerThread::drop`: **"thread caused non-unwinding panic. aborting."** — the entire test
  process aborts, which is bug 2's host-abort mechanism reproduced in miniature.
- `destroying_the_last_instance_mid_task_does_not_self_join` fails through a panic-hook
  counter that watches the `bg-worker` thread: the self-join's EDEADLK panic lands there,
  where the harness would otherwise never see it. On Windows this test hangs instead
  (self-`WaitForSingleObject(INFINITE)`).

With the fix below, both pass.

## Suggested fix

Behavior-preserving outside the failure paths; happy to turn this plus the two tests into a
PR if the direction is welcome.

```diff
--- a/src/event_loop/background_thread.rs
+++ b/src/event_loop/background_thread.rs
@@ impl<T, E> Drop for WorkerThread<T, E> {
     fn drop(&mut self) {
-        // The thread is shut down and joined when the handle is dropped
-        self.tasks_sender
-            .send(Message::Shutdown)
-            .expect("Failed while sending worker thread shutdown request");
-        self.join_handle
-            .take()
-            // Only possible if the WorkerThread got dropped twice, somehow?
-            .expect("Missing Worker thread JoinHandle")
-            .join()
-            .expect("Worker thread panicked");
+        // The thread is shut down and joined when the handle is dropped. This has to
+        // tolerate every state the worker can be in, because it runs while the host is
+        // destroying a plugin instance:
+        //
+        // - Nothing here may panic: an unwind escapes into the host's plugin-destruction
+        //   call, and the worker may legitimately be gone already (it exits if its channel
+        //   disconnects, e.g. because it panicked while executing a plugin task).
+        // - This destructor can run *on the worker thread itself*: the worker holds the
+        //   upgraded `Arc<E>` while executing a task, so if the host destroys the instance
+        //   mid-task the last strong reference dies here. Joining would then be a
+        //   self-join (EDEADLK panic on macOS/Linux, infinite hang on Windows). Detach
+        //   instead; the loop winds down right after the current task, through the
+        //   `Shutdown` message when it fits in the queue and through the channel
+        //   disconnecting (this struct owns the only `Sender`) when it does not.
+        let join_handle = match self.join_handle.take() {
+            Some(join_handle) => join_handle,
+            // Only possible if the WorkerThread got dropped twice, somehow?
+            None => return,
+        };
+
+        if join_handle.thread().id() == thread::current().id() {
+            // `try_send`, not `send`: a blocking send on a full queue whose only receiver
+            // is this very thread could never complete
+            let _ = self.tasks_sender.try_send(Message::Shutdown);
+        } else {
+            let _ = self.tasks_sender.send(Message::Shutdown);
+            let _ = join_handle.join();
+        }
     }
 }
@@ fn worker_thread(...)
             Ok(Message::Task((task, executor))) => match executor.upgrade() {
                 Some(e) => e.execute(task, true),
                 None => {
                     nih_trace!(
-                        "Received a new task but the executor is no longer alive, shutting down \
-                         worker"
+                        "Received a task for a plugin instance that no longer exists, \
+                         dropping the task"
                     );
-                    return;
                 }
             },
```

Notes on the shape:

- On upgrade failure the orphaned task is dropped and the worker keeps serving every other
  instance. The worker still exits normally, through `Shutdown` or channel disconnect.
- In `Drop`, join errors are swallowed rather than re-panicked: if the worker panicked
  earlier, that panic already went through the panic hook, and re-raising it inside a
  destructor on a host thread only converts a logged failure into an abort.
- The detach path uses `try_send` because a blocking send on a full queue whose only
  receiver is the current thread cannot complete; if `Shutdown` doesn't fit, dropping this
  struct's `Sender` (the only one) disconnects the channel, and the worker's `Err` arm ends
  the loop the same way.

One residual issue the detach cannot solve: in the destroy-during-task scenario the worker
still executes a few final instructions of plugin code after `destroy()` has returned, so a
host that unloads the module immediately afterwards can still fault. That window exists on
master too — the worker is inside plugin code when `destroy` returns, by definition of the
scenario. Closing it completely probably means having destroy/terminate synchronize with an
in-flight task before the wrapper's `Arc` is released, which is a bigger design change than
this patch; the patch at least makes teardown non-fatal and keeps the worker alive for
surviving instances.

## Related reports

Checked 2026-08-28; none of the existing issues cover these two bugs, but one is close and
interacts:

- [#222](https://github.com/robbert-vdh/nih-plug/issues/222) (open) with its fix
  [#225](https://github.com/robbert-vdh/nih-plug/pull/225) (open): the wrappers'
  `execute_background`/`execute_gui` closures capture strong `Arc`s to their own wrapper — a
  reference cycle, so on destroy the drop glue never runs at all. That is the *opposite*
  failure — this issue is about the drop glue running at the wrong moment or on the wrong
  thread — and the two are not duplicates. Worth knowing when triaging: **that cycle only
  closes for plugins that *retain* the `AsyncExecutor`.** It is built at the call site and
  moved into `Plugin::editor()`, so a plugin whose `editor()` ignores the parameter — with a
  GUI library that does not store one either, as `nih_plug_egui` does not — drops those
  strong clones when `editor()` returns. Such plugins tear down normally today, which makes
  both bugs below **live rather than latent** for them; that is how they were found. For
  plugins that do keep the executor, #222's leak keeps every task's `Weak` upgradeable and
  never releases the worker handle, masking these two until #225 lands. Either way, #225
  makes them universal: destroy then always releases the last reference (bug 1's mid-task
  race), and its "ignore tasks posted after the wrapper is dropped" model makes dead `Weak`s
  on queued tasks routine (bug 2). #225 does not touch `background_thread.rs`, so the fix
  below composes cleanly with it and is arguably its missing second half.
- [#250](https://github.com/robbert-vdh/nih-plug/issues/250) (open) is cross-referenced
  from #222 but its crash log points into the NVIDIA GL driver during normal use, not this
  teardown path.

## Environment

- nih-plug master @ `f36931f7af4646065488a9845d8f8c2f95252c23`; the module is shared by all
  wrappers and platforms, and `src/event_loop/background_thread.rs` has no commits after
  this rev as of 2026-08-28, so the permalinks and the diff below apply to current master
  unchanged
- Verified on macOS 26 (Darwin 25.3), rustc/std 1.93.1; the Windows analysis follows from
  `WaitForSingleObject(INFINITE)` on the thread's own handle and from crossbeam's
  disconnect semantics
- Found via a CLAP/VST3/AU plugin that schedules one background analysis task per user
  capture

## Appendix: the two regression tests

<details>
<summary>Diff adding the tests to <code>src/event_loop/background_thread.rs</code></summary>

```rust
#[cfg(test)]
mod tests {
    use super::*;

    use std::panic;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Once;
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_secs(10);

    /// The number of panics that have occurred on a thread named `bg-worker`. The self-join
    /// failure on macOS/Linux is a panic on the *worker* thread (`pthread_join` returns
    /// EDEADLK and std asserts), which the test harness on its own would never notice.
    static BG_WORKER_PANICS: AtomicUsize = AtomicUsize::new(0);
    static INSTALL_PANIC_HOOK: Once = Once::new();

    fn install_panic_hook() {
        INSTALL_PANIC_HOOK.call_once(|| {
            let previous = panic::take_hook();
            panic::set_hook(Box::new(move |panic_info| {
                if thread::current().name() == Some("bg-worker") {
                    BG_WORKER_PANICS.fetch_add(1, Ordering::SeqCst);
                }
                previous(panic_info);
            }));
        });
    }

    /// Forwards every executed task to a channel so the test can observe execution.
    struct ForwardingExecutor {
        executed: channel::Sender<u8>,
    }

    impl MainThreadExecutor<u8> for ForwardingExecutor {
        fn execute(&self, task: u8, _is_gui_thread: bool) {
            let _ = self.executed.send(task);
        }
    }

    /// A task is sent along with a `Weak` to its own executor, and instances share one
    /// worker thread. An instance destroyed while its task is still queued must cost only
    /// that task, not the worker every other instance relies on.
    #[test]
    fn dead_executor_does_not_kill_the_shared_worker() {
        install_panic_hook();

        let (executed_sender, executed_receiver) = channel::unbounded();
        let doomed = Arc::new(ForwardingExecutor {
            executed: executed_sender.clone(),
        });
        let survivor = Arc::new(ForwardingExecutor {
            executed: executed_sender,
        });

        let doomed_thread = BackgroundThread::get_or_create(Arc::downgrade(&doomed));
        let survivor_thread = BackgroundThread::get_or_create(Arc::downgrade(&survivor));

        // Sanity check while both instances are alive.
        assert!(survivor_thread.schedule(1));
        assert_eq!(executed_receiver.recv_timeout(TIMEOUT), Ok(1));

        // "Destroy" one instance, then let the worker find its queued task. `schedule()`
        // only holds a `Weak`, so this mirrors a queued task racing the host's `destroy()`.
        drop(doomed);
        assert!(doomed_thread.schedule(2));
        assert!(
            survivor_thread.schedule(3),
            "the shared worker thread died after one dead executor"
        );

        // The orphaned task is dropped; the survivor's task must still execute.
        assert_eq!(
            executed_receiver.recv_timeout(TIMEOUT),
            Ok(3),
            "the shared worker thread died after one dead executor"
        );

        // Tearing the worker down must survive whatever state the loop is in.
        drop(doomed_thread);
        drop(survivor_thread);
        assert_eq!(BG_WORKER_PANICS.load(Ordering::SeqCst), 0);
    }

    /// The worker holds the upgraded `Arc<E>` while executing a task. When the host
    /// destroys the instance in that window, the last strong reference — which owns the
    /// `BackgroundThread`, and with it the last `WorkerThread` handle — dies on the worker
    /// thread, and `WorkerThread::drop` must not join the thread it is running on.
    #[test]
    fn destroying_the_last_instance_mid_task_does_not_self_join() {
        install_panic_hook();

        struct DropSentinel(channel::Sender<()>);
        impl Drop for DropSentinel {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }

        /// Owns its own `BackgroundThread` the way the plugin wrappers do. Fields drop in
        /// declaration order, so the sentinel fires only after the worker-thread teardown
        /// (triggered by dropping `background_thread`) has completed.
        struct SelfOwningExecutor {
            in_task: channel::Sender<()>,
            resume: channel::Receiver<()>,
            background_thread: Mutex<Option<BackgroundThread<(), SelfOwningExecutor>>>,
            _dropped: DropSentinel,
        }

        impl MainThreadExecutor<()> for SelfOwningExecutor {
            fn execute(&self, _task: (), _is_gui_thread: bool) {
                let _ = self.in_task.send(());
                // Hold the task — and with it the worker's upgraded `Arc<Self>` — open
                // until the test has dropped the reference that plays the host's.
                let _ = self.resume.recv_timeout(TIMEOUT);
            }
        }

        let (in_task_sender, in_task_receiver) = channel::bounded(1);
        let (resume_sender, resume_receiver) = channel::bounded(1);
        let (dropped_sender, dropped_receiver) = channel::bounded(1);

        let executor = Arc::new(SelfOwningExecutor {
            in_task: in_task_sender,
            resume: resume_receiver,
            background_thread: Mutex::new(None),
            _dropped: DropSentinel(dropped_sender),
        });
        let background_thread = BackgroundThread::get_or_create(Arc::downgrade(&executor));
        *executor.background_thread.lock() = Some(background_thread);
        assert!(executor.background_thread.lock().as_ref().unwrap().schedule(()));

        // The worker is now inside `execute()`, holding an upgraded `Arc` to the executor.
        in_task_receiver
            .recv_timeout(TIMEOUT)
            .expect("the task never started");
        // The host's `destroy()`: after this the worker holds the last strong reference.
        drop(executor);
        resume_sender
            .send(())
            .expect("the worker thread disappeared mid-task");

        // The executor — and with it the entire worker-thread teardown — must now finish
        // *on* the worker thread, without self-joining.
        dropped_receiver
            .recv_timeout(TIMEOUT)
            .expect("teardown on the worker thread deadlocked");

        // The registry still holds the dead `Weak`; the next instance of the same type
        // must come up with a fresh, working worker thread.
        let (in_task_sender, in_task_receiver) = channel::bounded(1);
        let (resume_sender, resume_receiver) = channel::bounded(1);
        let (dropped_sender, _dropped_receiver) = channel::bounded(1);
        let second = Arc::new(SelfOwningExecutor {
            in_task: in_task_sender,
            resume: resume_receiver,
            background_thread: Mutex::new(None),
            _dropped: DropSentinel(dropped_sender),
        });
        let second_thread = BackgroundThread::get_or_create(Arc::downgrade(&second));
        assert!(second_thread.schedule(()));
        in_task_receiver
            .recv_timeout(TIMEOUT)
            .expect("no fresh worker thread was spawned after the detached teardown");
        resume_sender.send(()).unwrap();

        // Neither the detached worker's own exit nor this ordinary main-thread teardown
        // may have panicked anywhere; on macOS/Linux the pre-fix self-join shows up here.
        drop(second_thread);
        drop(second);
        assert_eq!(BG_WORKER_PANICS.load(Ordering::SeqCst), 0);
    }
}
```

</details>
