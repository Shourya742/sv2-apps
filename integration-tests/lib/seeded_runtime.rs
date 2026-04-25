use rand::{rngs::StdRng, Rng, SeedableRng};
use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};
use stratum_apps::runtime::{BoxFuture, LocalFutureFactory, Runtime, RuntimeHandle, RuntimeTask};
use tokio::sync::Notify;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEventKind {
    Spawn,
    SpawnLocal {
        name: String,
    },
    Sleep {
        deadline_ms: u128,
    },
    Wake {
        deadline_ms: u128,
    },
    Choice {
        label: String,
        choice: usize,
        len: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeEvent {
    pub id: u64,
    pub kind: RuntimeEventKind,
}

#[derive(Debug)]
struct SeededRuntimeState {
    now_ms: StdMutex<u128>,
    next_event_id: StdMutex<u64>,
    events: StdMutex<Vec<RuntimeEvent>>,
    notify: Notify,
    rng: StdMutex<StdRng>,
}

pub struct SeededRuntime {
    seed: u64,
    state: Arc<SeededRuntimeState>,
}

impl fmt::Debug for SeededRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SeededRuntime")
            .field("seed", &self.seed)
            .field("now_ms", &self.now_ms())
            .finish_non_exhaustive()
    }
}

impl SeededRuntime {
    pub fn new(seed: u64) -> Arc<Self> {
        Arc::new(Self {
            seed,
            state: Arc::new(SeededRuntimeState {
                now_ms: StdMutex::new(0),
                next_event_id: StdMutex::new(0),
                events: StdMutex::new(Vec::new()),
                notify: Notify::new(),
                rng: StdMutex::new(StdRng::seed_from_u64(seed)),
            }),
        })
    }

    pub fn runtime_handle(self: &Arc<Self>) -> RuntimeHandle {
        self.clone()
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    pub fn now_ms(&self) -> u128 {
        *self.state.now_ms.lock().unwrap()
    }

    pub fn events(&self) -> Vec<RuntimeEvent> {
        self.state.events.lock().unwrap().clone()
    }

    pub fn advance(&self, duration: Duration) {
        *self.state.now_ms.lock().unwrap() += duration.as_millis();
        self.state.notify.notify_waiters();
    }

    pub fn choose_index(&self, label: impl Into<String>, len: usize) -> Option<usize> {
        if len == 0 {
            return None;
        }

        let choice = self.state.rng.lock().unwrap().gen_range(0..len);
        self.record(RuntimeEventKind::Choice {
            label: label.into(),
            choice,
            len,
        });
        Some(choice)
    }

    fn record(&self, kind: RuntimeEventKind) -> u64 {
        let mut next_event_id = self.state.next_event_id.lock().unwrap();
        let id = *next_event_id;
        *next_event_id += 1;

        self.state
            .events
            .lock()
            .unwrap()
            .push(RuntimeEvent { id, kind });
        id
    }
}

impl Runtime for SeededRuntime {
    fn spawn(&self, fut: BoxFuture<()>) -> Box<dyn RuntimeTask> {
        self.record(RuntimeEventKind::Spawn);
        Box::new(SeededTask {
            handle: StdMutex::new(Some(tokio::spawn(fut))),
        })
    }

    fn spawn_local(&self, name: String, fut: LocalFutureFactory) -> Box<dyn RuntimeTask> {
        self.record(RuntimeEventKind::SpawnLocal { name: name.clone() });
        let handle = std::thread::Builder::new().name(name).spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to create seeded local runtime");
            let local_set = tokio::task::LocalSet::new();
            local_set.block_on(&rt, fut());
        });

        Box::new(SeededThreadTask {
            handle: StdMutex::new(handle.ok()),
        })
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<()> {
        let state = self.state.clone();
        let deadline_ms = self.now_ms() + duration.as_millis();
        self.record(RuntimeEventKind::Sleep { deadline_ms });

        Box::pin(async move {
            loop {
                let ready = {
                    let now_ms = *state.now_ms.lock().unwrap();
                    now_ms >= deadline_ms
                };
                if ready {
                    let runtime = SeededRuntime {
                        seed: 0,
                        state: state.clone(),
                    };
                    runtime.record(RuntimeEventKind::Wake { deadline_ms });
                    return;
                }
                state.notify.notified().await;
            }
        })
    }
}

struct SeededThreadTask {
    handle: StdMutex<Option<std::thread::JoinHandle<()>>>,
}

impl RuntimeTask for SeededThreadTask {
    fn is_finished(&self) -> bool {
        self.handle
            .lock()
            .unwrap()
            .as_ref()
            .map(|handle| handle.is_finished())
            .unwrap_or(true)
    }

    fn abort(&self) {}

    fn join(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        Box::pin(async move {
            let handle = self.handle.lock().unwrap().take();
            if let Some(handle) = handle {
                let _ = tokio::task::spawn_blocking(move || handle.join()).await;
            }
        })
    }
}

struct SeededTask {
    handle: StdMutex<Option<tokio::task::JoinHandle<()>>>,
}

impl RuntimeTask for SeededTask {
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

    fn join(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        Box::pin(async move {
            let handle = self.handle.lock().unwrap().take();
            if let Some(handle) = handle {
                let _ = handle.await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stratum_apps::task_manager::TaskManager;

    #[tokio::test]
    async fn virtual_sleep_waits_until_advanced() {
        let runtime = SeededRuntime::new(42);
        let task_manager = Arc::new(TaskManager::with_runtime(runtime.runtime_handle()));

        let (sleep_started_sender, sleep_started_receiver) = tokio::sync::oneshot::channel();
        let slept = Arc::new(StdMutex::new(false));
        let slept_clone = slept.clone();
        let sleep_task_manager = task_manager.clone();
        task_manager.spawn(async move {
            let sleep = sleep_task_manager.sleep(Duration::from_secs(5));
            let _ = sleep_started_sender.send(());
            sleep.await;
            *slept_clone.lock().unwrap() = true;
        });

        sleep_started_receiver.await.unwrap();
        tokio::task::yield_now().await;
        assert!(!*slept.lock().unwrap());

        runtime.advance(Duration::from_secs(5));
        tokio::task::yield_now().await;
        task_manager.join_all().await;

        assert!(*slept.lock().unwrap());
    }

    #[tokio::test]
    async fn local_runtime_tasks_are_tracked() {
        let runtime = SeededRuntime::new(42);
        let task_manager = TaskManager::with_runtime(runtime.runtime_handle());

        let completed = Arc::new(StdMutex::new(false));
        let completed_clone = completed.clone();
        task_manager.spawn_local("local-test", move || async move {
            *completed_clone.lock().unwrap() = true;
        });
        task_manager.join_all().await;

        assert!(*completed.lock().unwrap());
        assert!(runtime.events().iter().any(|event| {
            matches!(
                &event.kind,
                RuntimeEventKind::SpawnLocal { name } if name == "local-test"
            )
        }));
    }

    #[test]
    fn seeded_choices_repeat() {
        let first = SeededRuntime::new(42);
        let second = SeededRuntime::new(42);

        assert_eq!(
            first.choose_index("upstream", 4),
            second.choose_index("upstream", 4)
        );
        assert_eq!(first.events(), second.events());
    }
}
