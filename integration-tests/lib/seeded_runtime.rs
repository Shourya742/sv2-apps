use rand::{rngs::StdRng, Rng, SeedableRng};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as StdMutex, Weak,
    },
    task::{Context, Poll, Wake, Waker},
    time::Duration,
};
use stratum_apps::runtime::{BoxFuture, LocalFutureFactory, Runtime, RuntimeHandle, RuntimeTask};

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

struct SeededRuntimeState {
    now_ms: StdMutex<u128>,
    next_event_id: StdMutex<u64>,
    events: StdMutex<Vec<RuntimeEvent>>,
    sleepers: StdMutex<Vec<Sleeper>>,
    rng: StdMutex<StdRng>,
    sleep_mode: SleepMode,
}

#[derive(Clone)]
struct Sleeper {
    deadline_ms: u128,
    waker: Waker,
}

struct ExecutorState {
    ready: StdMutex<VecDeque<Arc<SeededTaskState>>>,
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
    executor: Arc<ExecutorState>,
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
                sleepers: StdMutex::new(Vec::new()),
                rng: StdMutex::new(StdRng::seed_from_u64(seed)),
                sleep_mode,
            }),
            executor: Arc::new(ExecutorState {
                ready: StdMutex::new(VecDeque::new()),
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
        let now_ms = {
            let mut now_ms = self.state.now_ms.lock().unwrap();
            *now_ms += duration.as_millis();
            *now_ms
        };
        wake_ready_sleepers(&self.state, now_ms);
    }

    pub fn block_on<F>(&self, future: F) -> F::Output
    where
        F: Future,
    {
        block_on_with_executor(&self.executor, future)
    }

    pub fn run_until_stalled(&self) {
        while poll_ready_task(&self.executor) {}
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
        Box::new(SeededTask::spawn(self.executor.clone(), fut))
    }

    fn spawn_local(&self, name: String, fut: LocalFutureFactory) -> Box<dyn RuntimeTask> {
        self.record(RuntimeEventKind::SpawnLocal { name: name.clone() });
        let executor = self.executor.clone();
        let handle = std::thread::Builder::new().name(name).spawn(move || {
            block_on_with_executor(&executor, fut());
        });

        Box::new(SeededThreadTask {
            handle: StdMutex::new(handle.ok()),
        })
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<()> {
        let state = self.state.clone();
        let deadline_ms = self.now_ms() + duration.as_millis();
        self.record(RuntimeEventKind::Sleep { deadline_ms });
        Box::pin(SeededSleep {
            state,
            deadline_ms,
            registered: false,
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

struct SeededSleep {
    state: Arc<SeededRuntimeState>,
    deadline_ms: u128,
    registered: bool,
}

impl Future for SeededSleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.state.sleep_mode == SleepMode::AutoAdvance {
            let now_ms = {
                let mut now_ms = self.state.now_ms.lock().unwrap();
                if *now_ms < self.deadline_ms {
                    *now_ms = self.deadline_ms;
                }
                *now_ms
            };
            wake_ready_sleepers(&self.state, now_ms);
            record_event(
                &self.state,
                RuntimeEventKind::Wake {
                    deadline_ms: self.deadline_ms,
                },
            );
            return Poll::Ready(());
        }

        let ready = *self.state.now_ms.lock().unwrap() >= self.deadline_ms;
        if ready {
            record_event(
                &self.state,
                RuntimeEventKind::Wake {
                    deadline_ms: self.deadline_ms,
                },
            );
            return Poll::Ready(());
        }

        if !self.registered {
            self.state.sleepers.lock().unwrap().push(Sleeper {
                deadline_ms: self.deadline_ms,
                waker: cx.waker().clone(),
            });
            self.registered = true;
        }
        Poll::Pending
    }
}

fn wake_ready_sleepers(state: &SeededRuntimeState, now_ms: u128) {
    let ready = {
        let mut sleepers = state.sleepers.lock().unwrap();
        let mut ready = Vec::new();
        let mut pending = Vec::new();
        for sleeper in sleepers.drain(..) {
            if sleeper.deadline_ms <= now_ms {
                ready.push(sleeper.waker);
            } else {
                pending.push(sleeper);
            }
        }
        *sleepers = pending;
        ready
    };

    for waker in ready {
        waker.wake();
    }
}

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn noop_waker() -> Waker {
    Waker::from(Arc::new(NoopWake))
}

fn block_on_with_executor<F>(executor: &Arc<ExecutorState>, future: F) -> F::Output
where
    F: Future,
{
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);

    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }

        if !poll_ready_task(executor) {
            std::thread::yield_now();
        }
    }
}

fn poll_ready_task(executor: &ExecutorState) -> bool {
    let Some(task) = executor.ready.lock().unwrap().pop_front() else {
        return false;
    };

    task.poll();
    true
}

struct SeededTaskState {
    future: StdMutex<Option<BoxFuture<()>>>,
    executor: Weak<ExecutorState>,
    finished: AtomicBool,
    aborted: AtomicBool,
    join_wakers: StdMutex<Vec<Waker>>,
}

impl SeededTaskState {
    fn schedule(self: &Arc<Self>) {
        if self.finished.load(Ordering::SeqCst) || self.aborted.load(Ordering::SeqCst) {
            return;
        }

        if let Some(executor) = self.executor.upgrade() {
            executor.ready.lock().unwrap().push_back(self.clone());
        }
    }

    fn poll(self: Arc<Self>) {
        if self.aborted.load(Ordering::SeqCst) {
            *self.future.lock().unwrap() = None;
            self.finish();
            return;
        }

        let Some(mut future) = self.future.lock().unwrap().take() else {
            self.finish();
            return;
        };

        let waker = Waker::from(self.clone());
        let mut cx = Context::from_waker(&waker);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(()) => self.finish(),
            Poll::Pending => {
                *self.future.lock().unwrap() = Some(future);
            }
        }
    }

    fn finish(&self) {
        if self.finished.swap(true, Ordering::SeqCst) {
            return;
        }

        for waker in self.join_wakers.lock().unwrap().drain(..) {
            waker.wake();
        }
    }
}

impl Wake for SeededTaskState {
    fn wake(self: Arc<Self>) {
        self.schedule();
    }
}

struct SeededJoin {
    task: Arc<SeededTaskState>,
}

impl Future for SeededJoin {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.task.finished.load(Ordering::SeqCst) {
            return Poll::Ready(());
        }

        self.task
            .join_wakers
            .lock()
            .unwrap()
            .push(cx.waker().clone());
        self.task.schedule();
        Poll::Pending
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
                let _ = handle.join();
            }
        })
    }
}

struct SeededTask {
    task: Arc<SeededTaskState>,
}

impl SeededTask {
    fn spawn(executor: Arc<ExecutorState>, future: BoxFuture<()>) -> Self {
        let task = Arc::new(SeededTaskState {
            future: StdMutex::new(Some(future)),
            executor: Arc::downgrade(&executor),
            finished: AtomicBool::new(false),
            aborted: AtomicBool::new(false),
            join_wakers: StdMutex::new(Vec::new()),
        });
        executor.ready.lock().unwrap().push_back(task.clone());
        Self { task }
    }
}

impl RuntimeTask for SeededTask {
    fn is_finished(&self) -> bool {
        self.task.finished.load(Ordering::SeqCst)
    }

    fn abort(&self) {
        self.task.aborted.store(true, Ordering::SeqCst);
        self.task.schedule();
    }

    fn join(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        Box::pin(SeededJoin { task: self.task })
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

        let slept = Arc::new(StdMutex::new(false));
        let slept_clone = slept.clone();
        let sleep_task_manager = task_manager.clone();
        task_manager.spawn(async move {
            let sleep = sleep_task_manager.sleep(Duration::from_secs(5));
            sleep.await;
            *slept_clone.lock().unwrap() = true;
        });

        runtime.run_until_stalled();
        assert!(!*slept.lock().unwrap());

        runtime.advance(Duration::from_secs(5));
        runtime.run_until_stalled();
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
