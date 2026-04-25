use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

pub trait RuntimeTask: Send {
    fn is_finished(&self) -> bool;
    fn abort(&self);
    fn join(self: Box<Self>) -> BoxFuture<()>;
}

pub trait Runtime: std::fmt::Debug + Send + Sync + 'static {
    fn spawn(&self, fut: BoxFuture<()>) -> Box<dyn RuntimeTask>;
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
