use rand::{rngs::StdRng, Rng, SeedableRng};
use std::cell::RefCell;
use std::collections::HashMap;
use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};
use stratum_apps::runtime::{BoxFuture, LocalFutureFactory, Runtime, RuntimeHandle, RuntimeTask};
use tokio::sync::Notify;

pub const SEED_ENV_VAR: &str = "SV2_SIM_SEED";
const DEFAULT_SEED: u64 = 0x5356_325f_5349_4d31;

static TEST_RUNTIMES: once_cell::sync::Lazy<StdMutex<HashMap<String, Arc<SeededRuntime>>>> =
    once_cell::sync::Lazy::new(|| StdMutex::new(HashMap::new()));

thread_local! {
    static CURRENT_TEST_ID: RefCell<Option<String>> = const { RefCell::new(None) };
}

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
    sleep_mode: SleepMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SleepMode {
    AutoAdvance,
    Manual,
}

pub struct SeededRuntime {
    test_id: String,
    base_seed: u64,
    seed: u64,
    state: Arc<SeededRuntimeState>,
}

impl fmt::Debug for SeededRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SeededRuntime")
            .field("test_id", &self.test_id)
            .field("base_seed", &self.base_seed)
            .field("seed", &self.seed)
            .field("now_ms", &self.now_ms())
            .finish_non_exhaustive()
    }
}

impl SeededRuntime {
    pub fn new(seed: u64) -> Arc<Self> {
        Self::manual("manual", seed, seed)
    }

    pub fn manual(test_id: impl Into<String>, base_seed: u64, seed: u64) -> Arc<Self> {
        Self::with_sleep_mode(test_id, base_seed, seed, SleepMode::Manual)
    }

    pub fn auto_advance(test_id: impl Into<String>, base_seed: u64, seed: u64) -> Arc<Self> {
        Self::with_sleep_mode(test_id, base_seed, seed, SleepMode::AutoAdvance)
    }

    fn with_sleep_mode(
        test_id: impl Into<String>,
        base_seed: u64,
        seed: u64,
        sleep_mode: SleepMode,
    ) -> Arc<Self> {
        Arc::new(Self {
            test_id: test_id.into(),
            base_seed,
            seed,
            state: Arc::new(SeededRuntimeState {
                now_ms: StdMutex::new(0),
                next_event_id: StdMutex::new(0),
                events: StdMutex::new(Vec::new()),
                notify: Notify::new(),
                rng: StdMutex::new(StdRng::seed_from_u64(seed)),
                sleep_mode,
            }),
        })
    }

    pub fn for_current_test() -> Arc<Self> {
        let test_id = current_test_id();
        let base_seed = base_seed_from_env();

        let mut runtimes = TEST_RUNTIMES.lock().unwrap();
        runtimes
            .entry(test_id.clone())
            .or_insert_with(|| {
                let seed = derive_seed(base_seed, &test_id);
                tracing::info!(
                    test = %test_id,
                    base_seed,
                    seed,
                    "starting seeded integration runtime"
                );
                Self::auto_advance(test_id, base_seed, seed)
            })
            .clone()
    }

    pub fn test_id(&self) -> &str {
        &self.test_id
    }

    pub fn base_seed(&self) -> u64 {
        self.base_seed
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
        record_event(&self.state, kind)
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

        if state.sleep_mode == SleepMode::AutoAdvance {
            return Box::pin(async move {
                {
                    let mut now_ms = state.now_ms.lock().unwrap();
                    if *now_ms < deadline_ms {
                        *now_ms = deadline_ms;
                    }
                }
                state.notify.notify_waiters();
                tokio::task::yield_now().await;
                record_event(&state, RuntimeEventKind::Wake { deadline_ms });
            });
        }

        Box::pin(async move {
            loop {
                let ready = {
                    let now_ms = *state.now_ms.lock().unwrap();
                    now_ms >= deadline_ms
                };
                if ready {
                    record_event(&state, RuntimeEventKind::Wake { deadline_ms });
                    return;
                }
                state.notify.notified().await;
            }
        })
    }
}

pub struct TestIdGuard {
    previous: Option<String>,
}

impl Drop for TestIdGuard {
    fn drop(&mut self) {
        CURRENT_TEST_ID.with(|test_id| {
            *test_id.borrow_mut() = self.previous.take();
        });
    }
}

pub fn enter_test(test_id: impl Into<String>) -> TestIdGuard {
    CURRENT_TEST_ID.with(|current| {
        let previous = current.borrow_mut().replace(test_id.into());
        TestIdGuard { previous }
    })
}

fn record_event(state: &SeededRuntimeState, kind: RuntimeEventKind) -> u64 {
    let mut next_event_id = state.next_event_id.lock().unwrap();
    let id = *next_event_id;
    *next_event_id += 1;

    state.events.lock().unwrap().push(RuntimeEvent { id, kind });
    id
}

fn current_test_id() -> String {
    if let Some(test_id) = CURRENT_TEST_ID.with(|test_id| test_id.borrow().clone()) {
        return test_id;
    }

    std::thread::current()
        .name()
        .filter(|name| !name.is_empty())
        .unwrap_or("integration-test-process")
        .to_string()
}

fn base_seed_from_env() -> u64 {
    match std::env::var(SEED_ENV_VAR) {
        Ok(value) if !value.trim().is_empty() => parse_seed(value.trim())
            .unwrap_or_else(|err| panic!("invalid {SEED_ENV_VAR} value: {err}")),
        _ => DEFAULT_SEED,
    }
}

fn parse_seed(value: &str) -> Result<u64, std::num::ParseIntError> {
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16)
    } else {
        value.parse()
    }
}

fn derive_seed(base_seed: u64, label: &str) -> u64 {
    let mut state = base_seed ^ fnv1a64(label.as_bytes());
    splitmix64(&mut state)
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut value = *state;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
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
    use crate::sim_test;
    use stratum_apps::task_manager::TaskManager;

    sim_test! {
    async fn virtual_sleep_waits_until_advanced() {
        let runtime = SeededRuntime::manual("manual-test", 42, 42);
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
    }

    sim_test! {
    async fn local_runtime_tasks_are_tracked() {
        let runtime = SeededRuntime::manual("local-test", 42, 42);
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
    }

    #[test]
    fn seeded_choices_repeat() {
        let first = SeededRuntime::manual("first", 42, 42);
        let second = SeededRuntime::manual("second", 42, 42);

        assert_eq!(
            first.choose_index("upstream", 4),
            second.choose_index("upstream", 4)
        );
        assert_eq!(first.events(), second.events());
    }

    sim_test! {
    async fn auto_advance_sleep_completes_without_manual_advance() {
        let runtime = SeededRuntime::auto_advance("auto-test", 42, 42);
        runtime.sleep(Duration::from_secs(5)).await;

        assert_eq!(runtime.now_ms(), 5000);
    }
    }

    #[test]
    fn derives_seed_from_base_and_test_name() {
        assert_eq!(derive_seed(42, "test-a"), derive_seed(42, "test-a"));
        assert_ne!(derive_seed(42, "test-a"), derive_seed(42, "test-b"));
    }
}
