use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex as StdMutex},
    thread::JoinHandle as ThreadJoinHandle,
    time::Duration,
};

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
pub type LocalBoxFuture<T> = Pin<Box<dyn Future<Output = T> + 'static>>;
pub type LocalFutureFactory = Box<dyn FnOnce() -> LocalBoxFuture<()> + Send + 'static>;

pub trait RuntimeTask: Send {
    fn is_finished(&self) -> bool;
    fn abort(&self);
    fn join(self: Box<Self>) -> BoxFuture<()>;
}

pub trait Runtime: std::fmt::Debug + Send + Sync + 'static {
    fn spawn(&self, fut: BoxFuture<()>) -> Box<dyn RuntimeTask>;
    fn spawn_local(&self, name: String, fut: LocalFutureFactory) -> Box<dyn RuntimeTask>;
    fn sleep(&self, duration: Duration) -> BoxFuture<()>;
}

pub type RuntimeHandle = Arc<dyn Runtime>;

#[derive(Debug, Default)]
pub struct TokioRuntime;

impl TokioRuntime {
    pub fn new() -> Self {
        Self
    }

    pub fn handle() -> RuntimeHandle {
        Arc::new(Self::new())
    }
}

impl Runtime for TokioRuntime {
    fn spawn(&self, fut: BoxFuture<()>) -> Box<dyn RuntimeTask> {
        Box::new(TokioTask {
            handle: StdMutex::new(Some(tokio::spawn(fut))),
        })
    }

    fn spawn_local(&self, name: String, fut: LocalFutureFactory) -> Box<dyn RuntimeTask> {
        let thread_name = name.clone();
        let handle = std::thread::Builder::new()
            .name(name)
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!(
                            "failed to create Tokio runtime for local task `{thread_name}`: {e:?}"
                        );
                        return;
                    }
                };

                let local_set = tokio::task::LocalSet::new();
                local_set.block_on(&rt, fut());
            });

        Box::new(ThreadTask {
            handle: StdMutex::new(handle.ok()),
        })
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<()> {
        Box::pin(tokio::time::sleep(duration))
    }
}

struct TokioTask {
    handle: StdMutex<Option<tokio::task::JoinHandle<()>>>,
}

impl RuntimeTask for TokioTask {
    fn is_finished(&self) -> bool {
        self.handle
            .lock()
            .unwrap()
            .as_ref()
            .map(|handle| handle.is_finished())
            .unwrap_or(true)
    }

    fn abort(&self) {
        if let Some(handle) = self.handle.lock().unwrap().as_ref() {
            handle.abort();
        }
    }

    fn join(self: Box<Self>) -> BoxFuture<()> {
        Box::pin(async move {
            let handle = self.handle.lock().unwrap().take();
            if let Some(handle) = handle {
                let _ = handle.await;
            }
        })
    }
}

struct ThreadTask {
    handle: StdMutex<Option<ThreadJoinHandle<()>>>,
}

impl RuntimeTask for ThreadTask {
    fn is_finished(&self) -> bool {
        self.handle
            .lock()
            .unwrap()
            .as_ref()
            .map(|handle| handle.is_finished())
            .unwrap_or(true)
    }

    fn abort(&self) {
        // OS threads cannot be aborted. They must observe cancellation and exit.
    }

    fn join(self: Box<Self>) -> BoxFuture<()> {
        Box::pin(async move {
            let handle = self.handle.lock().unwrap().take();
            if let Some(handle) = handle {
                let _ = tokio::task::spawn_blocking(move || handle.join()).await;
            }
        })
    }
}
