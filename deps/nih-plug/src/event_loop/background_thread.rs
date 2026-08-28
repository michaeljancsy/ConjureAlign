//! Used by the other [`EventLoop`][super::EventLoop] implementations to spawn threads for running
//! tasks in the background without blocking the GUI thread.
//!
//! This is essentially a slimmed down version of the `LinuxEventLoop`.

use anymap3::Entry;
use crossbeam::channel;
use parking_lot::Mutex;
use std::sync::{Arc, LazyLock, Weak};
use std::thread::{self, JoinHandle};

use super::MainThreadExecutor;
use crate::util::permit_alloc;

/// See the module's documentation. This is a background thread that can be used to run tasks on.
/// The implementation shares a single thread between all of a plugin's instances hosted in the same
/// process.
pub(crate) struct BackgroundThread<T, E> {
    /// The object that actually executes the task `T`. We'll send a weak reference to this to the
    /// worker thread whenever a task needs to be executed. This allows multiple plugin instances to
    /// share the same worker thread.
    executor: Weak<E>,
    /// A thread that act as our worker thread. When [`schedule()`][Self::schedule()] is called,
    /// this thread will be woken up to execute the task on the executor. When the last worker
    /// thread handle gets dropped the thread is shut down.
    worker_thread: Arc<WorkerThread<T, E>>,
}

/// A handle for the singleton worker thread. This lets multiple instances of the same plugin share
/// a worker thread, and when the last instance gets dropped the worker thread gets terminated.
struct WorkerThread<T, E> {
    tasks_sender: channel::Sender<Message<T, E>>,
    /// The thread's join handle. Joined when the WorkerThread is dropped.
    join_handle: Option<JoinHandle<()>>,
}

/// A message for communicating with the worker thread.
enum Message<T, E> {
    /// A new task for the event loop to execute along with the executor that should execute the
    /// task. A reference to the executor is sent alongside because multiple plugin instances may
    /// share the same background thread.
    Task((T, Weak<E>)),
    /// Shut down the worker thread. Send when the last reference to the thread is dropped.
    Shutdown,
}

impl<T, E> BackgroundThread<T, E>
where
    T: Send + 'static,
    E: MainThreadExecutor<T> + 'static,
{
    pub fn get_or_create(executor: Weak<E>) -> Self {
        Self {
            executor,
            // The same worker thread can be shared by multiple instances. Lifecycle management
            // happens through reference counting.
            worker_thread: get_or_create_worker_thread(),
        }
    }

    pub fn schedule(&self, task: T) -> bool {
        // NOTE: This may check the current thread ID, which involves an allocation whenever this
        //       first happens on a new thread because of the way thread local storage works
        permit_alloc(|| {
            self.worker_thread
                .tasks_sender
                .try_send(Message::Task((task, self.executor.clone())))
                .is_ok()
        })
    }
}

// Rust does not allow us to use the `T` and `E` type variable in statics, so this is a
// workaround to have a singleton that also works if for whatever reason there are multiple `T`
// and `E`s in a single process (won't happen with normal plugin usage, but who knows).
static HANDLE_MAP: LazyLock<Mutex<anymap3::Map<dyn std::any::Any + Send>>> =
    LazyLock::new(|| Mutex::new(anymap3::Map::new()));

impl<T: Send + 'static, E: MainThreadExecutor<T> + 'static> WorkerThread<T, E> {
    fn spawn() -> Self {
        let (tasks_sender, tasks_receiver) = channel::bounded(super::TASK_QUEUE_CAPACITY);
        let join_handle = thread::Builder::new()
            .name(String::from("bg-worker"))
            .spawn(move || worker_thread(tasks_receiver))
            .expect("Could not spawn background worker thread");

        Self {
            join_handle: Some(join_handle),
            tasks_sender,
        }
    }
}

impl<T, E> Drop for WorkerThread<T, E> {
    fn drop(&mut self) {
        // The thread is shut down and joined when the handle is dropped.
        //
        // LOCAL PATCH (ConjureAlign): teardown has to tolerate every state the worker can be
        // in, because this destructor runs while the host is destroying a plugin instance:
        //
        // - Nothing here may `.expect()`. The worker may already be gone (it exits if its
        //   channel disconnects, and before this patch a single dead executor killed it for
        //   the whole process), and a panic here unwinds into the host's `extern "C"`
        //   plugin-destruction call, which aborts the process.
        // - This destructor can run *on the worker thread itself*. The worker holds the
        //   upgraded `Arc<E>` (the plugin wrapper) while executing a task, so when the host
        //   destroys the instance mid-task, the last strong reference — which owns the
        //   `BackgroundThread`, and through it the last handle to this struct — dies on the
        //   worker. Joining would then be a self-join: `pthread_join` returns EDEADLK and std
        //   panics on macOS, and `WaitForSingleObject(INFINITE)` hangs forever on Windows.
        //   Detach instead; the loop winds down on its own right after the current task,
        //   through the `Shutdown` message when it fits in the queue and through the channel
        //   disconnecting (this struct owns the only `Sender`) when it does not.
        let join_handle = match self.join_handle.take() {
            Some(join_handle) => join_handle,
            // Only possible if the WorkerThread got dropped twice, somehow?
            None => return,
        };

        if join_handle.thread().id() == thread::current().id() {
            // `try_send`, not `send`: a blocking send on a full queue whose only receiver is
            // this very thread could never complete.
            let _ = self.tasks_sender.try_send(Message::Shutdown);
            nih_trace!(
                "The last instance's reference to the shared worker thread was dropped from \
                 the worker thread itself, detaching the thread instead of self-joining"
            );
        } else {
            let _ = self.tasks_sender.send(Message::Shutdown);
            if join_handle.join().is_err() {
                nih_trace!("The worker thread panicked before it could be shut down");
            }
        }
    }
}

/// Either acquire a handle for an existing worker thread or create one if it does not yet exists.
/// This allows multiple plugin instances to share a worker thread. Reference counting happens
/// automatically as part of this function and `WorkerThreadHandle`'s lifecycle.
fn get_or_create_worker_thread<T, E>() -> Arc<WorkerThread<T, E>>
where
    T: Send + 'static,
    E: MainThreadExecutor<T> + 'static,
{
    let mut handle_map = HANDLE_MAP.lock();

    match handle_map.entry::<Weak<WorkerThread<T, E>>>() {
        Entry::Occupied(mut entry) => {
            let weak = entry.get_mut();
            if let Some(arc) = weak.upgrade() {
                arc
            } else {
                let arc = Arc::new(WorkerThread::spawn());
                *weak = Arc::downgrade(&arc);
                arc
            }
        }
        Entry::Vacant(entry) => {
            let arc = Arc::new(WorkerThread::spawn());
            entry.insert(Arc::downgrade(&arc));
            arc
        }
    }
}

/// The worker thread used in [`EventLoop`] that executes incoming tasks on the event loop's
/// executor.
fn worker_thread<T, E>(tasks_receiver: channel::Receiver<Message<T, E>>)
where
    T: Send,
    E: MainThreadExecutor<T> + 'static,
{
    loop {
        match tasks_receiver.recv() {
            Ok(Message::Task((task, executor))) => match executor.upgrade() {
                Some(e) => e.execute(task, true),
                None => {
                    // LOCAL PATCH (ConjureAlign): this used to `return`. But this thread is
                    // shared by every instance of the plugin in the process, and a dead
                    // executor is an ordinary occurrence — it just means the host destroyed
                    // that one instance while its task was still queued. Shutting down here
                    // silently stopped task execution for every *other* instance, and made
                    // the eventual last teardown panic on the disconnected channel. Drop the
                    // orphaned task and keep serving the rest.
                    nih_trace!(
                        "Received a task for a plugin instance that no longer exists, \
                         dropping the task"
                    );
                }
            },
            Ok(Message::Shutdown) => return,
            Err(err) => {
                nih_trace!(
                    "Worker thread got disconnected unexpectedly, shutting down: {}",
                    err
                );
                return;
            }
        }
    }
}

// LOCAL PATCH (ConjureAlign): regression tests for the two teardown fixes above. Run with
//
//     cargo test --manifest-path deps/nih-plug/Cargo.toml -p nih_plug --lib background_thread
//
// Both tests deadlock, panic, or abort against the unpatched code, so they double as a
// demonstration of the failure modes. See deps/PATCHES.md in the plugin repository.
#[cfg(test)]
mod tests {
    use super::*;

    use std::panic;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Once;
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_secs(10);

    /// The number of panics that have occurred on a thread named `bg-worker`. The pre-patch
    /// self-join failure on macOS is a panic on the *worker* thread (`pthread_join` returns
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

    /// A task is sent along with a `Weak` to its own executor, and instances share one worker
    /// thread. An instance destroyed while its task is still queued must cost only that task,
    /// not the worker every other instance relies on. Pre-patch the worker `return`ed: the
    /// survivor's task never ran, and the eventual teardown panicked on the dead channel.
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

        // "Destroy" one instance, then let the worker find its queued task. `schedule()` only
        // holds a `Weak`, so this mirrors a queued task racing the host's `destroy()`.
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

        // Tearing the worker down must survive whatever state the loop is in. Pre-patch this
        // was the `.expect()` panic on the disconnected channel.
        drop(doomed_thread);
        drop(survivor_thread);
        assert_eq!(BG_WORKER_PANICS.load(Ordering::SeqCst), 0);
    }

    /// The worker holds the upgraded `Arc<E>` while executing a task. When the host destroys
    /// the instance in that window, the last strong reference — which owns the
    /// `BackgroundThread`, and with it the last `WorkerThread` handle — dies on the worker
    /// thread. Pre-patch `WorkerThread::drop` then joined the very thread it ran on: a panic
    /// on macOS (`pthread_join` returns EDEADLK), an infinite hang on Windows.
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
                // Hold the task — and with it the worker's upgraded `Arc<Self>` — open until
                // the test has dropped the reference that plays the host's.
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

        // The executor — and with it the entire worker-thread teardown — must now finish *on*
        // the worker thread, without self-joining.
        dropped_receiver
            .recv_timeout(TIMEOUT)
            .expect("teardown on the worker thread deadlocked");

        // The registry still holds the dead `Weak`; the next instance of the same type must
        // come up with a fresh, working worker thread.
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

        // This ordinary main-thread teardown (and the detached worker's own exit) must not
        // have panicked anywhere; on macOS the pre-patch self-join above shows up here.
        drop(second_thread);
        drop(second);
        assert_eq!(BG_WORKER_PANICS.load(Ordering::SeqCst), 0);
    }
}
