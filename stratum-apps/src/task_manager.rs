use std::{future::Future, sync::Mutex as StdMutex, time::Duration};

use crate::runtime::{BoxFuture, RuntimeHandle, RuntimeTask, TokioRuntime};

/// Manages a collection of spawned runtime tasks.
///
/// This struct provides a centralized way to spawn, track, and manage the lifecycle
/// of async tasks in the apps. It maintains a list of task handles that can
/// be used to wait for all tasks to complete or abort them during shutdown.
pub struct TaskManager {
    runtime: RuntimeHandle,
    tasks: StdMutex<Vec<Box<dyn RuntimeTask>>>,
}

impl Default for TaskManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskManager {
    /// Creates a new TaskManager instance.
    ///
    /// Initializes an empty task manager backed by Tokio.
    pub fn new() -> Self {
        Self::with_runtime(TokioRuntime::handle())
    }

    /// Creates a new TaskManager backed by an injected runtime.
    pub fn with_runtime(runtime: RuntimeHandle) -> Self {
        Self {
            runtime,
            tasks: StdMutex::new(Vec::new()),
        }
    }

    /// Returns the runtime used by this task manager.
    pub fn runtime(&self) -> RuntimeHandle {
        self.runtime.clone()
    }

    /// Spawns a new async task and adds it to the managed collection.
    ///
    /// The task will be tracked by this manager and can be waited for or aborted
    /// using the other methods.
    ///
    /// # Arguments
    /// * `fut` - The future to spawn as a task
    #[track_caller]
    pub fn spawn<F>(&self, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        use tracing::Instrument;
        let location = std::panic::Location::caller();
        let span = tracing::trace_span!(
            "task",
            file = location.file(),
            line = location.line(),
            column = location.column(),
        );

        let handle = self.runtime.spawn(Box::pin(fut.instrument(span)));
        let mut tasks = self.tasks.lock().unwrap();
        tasks.retain(|task| !task.is_finished());
        tasks.push(handle);
    }

    /// Sleeps using the runtime attached to this task manager.
    pub fn sleep(&self, duration: Duration) -> BoxFuture<()> {
        self.runtime.sleep(duration)
    }

    /// Waits for all managed tasks to complete.
    ///
    /// This method will block until all tasks that were spawned through this
    /// manager have finished executing. Tasks are joined in reverse order
    /// (most recently spawned first).
    pub async fn join_all(&self) {
        let handles = {
            let mut tasks = self.tasks.lock().unwrap();
            std::mem::take(&mut *tasks)
        };

        for handle in handles {
            handle.join().await;
        }
    }

    /// Aborts all managed tasks.
    ///
    /// This method immediately cancels all tasks that were spawned through this
    /// manager. The tasks will be terminated without waiting for them to complete.
    pub async fn abort_all(&self) {
        let mut tasks = self.tasks.lock().unwrap();
        for task in tasks.drain(..) {
            task.abort();
        }
    }
}
