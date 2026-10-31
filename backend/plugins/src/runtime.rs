//! The wasmtime side: the shared engine, the epoch ticker, compiled and linked
//! components, the memory budget and guarded guest calls (plan §7 *Engine &
//! instances*).
//!
//! # Memory budget
//!
//! `plugins.max_total_memory` is a [`tokio::sync::Semaphore`] whose permits are
//! MiB ([`MemoryBudget`]). [`Runtime::instantiate`] first takes a slot from the
//! plugin's own instance cap ([`InstanceCap`], shared by the clones of a
//! [`PreparedPlugin`]), then the instance's memory permits, both within one
//! `limits.exec` wait. A wait that runs out is a [`CallError::Budget`]; the
//! budget is never over-committed. The permits live in the [`LiveInstance`]
//! and return when it drops (the `Store` first).
//!
//! # Pooling allocator
//!
//! On 64-bit hosts the engine uses wasmtime's pooling allocator, sized from the
//! budget: every slot holds one instance of `max_memory` bytes, and the number
//! of slots is `clamp(2 * total / max_memory, 4, 64)`. Address space is only
//! reserved, not committed (about 6 GiB per slot). To make exhausting the pool
//! impossible, an instance is charged at least `ceil(total / slots)` MiB, so the
//! permits allow at most `slots` live instances. If the engine cannot be built
//! with the pool (address-space limits, an odd `ulimit -v`) a warning is logged
//! and the on-demand allocator is used instead, with a minimum charge of 1 MiB.
//! `target_pointer_width = "32"` (armv7: a 3 GiB address space) always uses the
//! on-demand allocator.
//!
//! # Deadlines
//!
//! The ticker advances the epoch every [`EPOCH_TICK`]. A guest never holds a
//! tokio worker for its whole deadline: the epoch callback yields to the
//! executor at every tick and interrupts the guest once `limits.exec` worth of
//! ticks is spent ([`CpuBudget`]; each [`LiveInstance::armed`] resets it). Ticks
//! count guest run slices, so time parked on a host call is not charged; the
//! wall-clock cap in [`call_with_deadline`] covers that.
//!
//! # Pulley and armv7
//!
//! No explicit `Config::target`. wasmtime's build script already points
//! `Config::new()` at the Pulley interpreter on architectures Cranelift cannot
//! host (armv7); the W0 spike on wasmtime 50.0.1 verified that the epoch trap
//! fires there (a 200 ms deadline traps at about 239 ms with a 10 ms tick).
//!
//! `Config::async_support` is a deprecated no-op in wasmtime 50: async comes
//! from the `async` cargo feature. Nothing here blocks on a future (plan §7,
//! Rule 0).

use crate::{
    manifest::Resources,
    rpc::RpcGate,
    state::{HostState, PluginEnv},
};
use log::warn;
use shared::error::TuliproxError;
use std::{
    collections::{HashMap, HashSet},
    fmt,
    future::Future,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex, PoisonError,
    },
    thread::JoinHandle,
    time::Duration,
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError},
    time::Instant,
};
use tuliprox_core::model::PluginsConfig;
use tuliprox_plugin_api::bindings::tuliprox::plugin::host;
use wasmtime::{
    component::{Component, HasSelf, Instance, InstancePre, Linker},
    Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig, Store, Trap, UpdateDeadline,
};

/// How often the epoch advances; the granularity of every deadline.
pub(crate) const EPOCH_TICK: Duration = Duration::from_millis(10);
/// Added to a call's CPU deadline for its wall-clock cap. The epoch cannot
/// interrupt a guest parked on a host call (an outbound request), so the future
/// is also dropped when the wall clock passes.
pub(crate) const WALL_CLOCK_SLACK: Duration = Duration::from_secs(1);

/// The `plugins:` clamps the runtime enforces.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RuntimeConfig {
    /// `plugins.max_total_memory`: the global budget of all live instances, in bytes.
    pub(crate) total_memory: u64,
    /// `plugins.max_memory`: the ceiling of one instance, in bytes.
    pub(crate) instance_memory: u64,
    /// `plugins.max_exec`: the ceiling of one hook call.
    pub(crate) exec: Duration,
    /// `plugins.max_task_exec`: the ceiling of one task run, and of one
    /// provider call ([`PROVIDER_EXEC_MULTIPLIER`]).
    pub(crate) task_exec: Duration,
    /// `plugins.max_instances`: the ceiling of one plugin's live instances.
    pub(crate) instances: u16,
}

impl RuntimeConfig {
    pub(crate) fn from_config(config: &PluginsConfig) -> Self {
        Self {
            total_memory: config.max_total_memory.get(),
            instance_memory: config.max_memory.get(),
            exec: Duration::from_millis(config.max_exec.get()),
            task_exec: Duration::from_millis(config.max_task_exec.get()),
            instances: config.max_instances,
        }
    }
}

/// One plugin's manifest `resources:`, clamped to the [`RuntimeConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PluginLimits {
    pub(crate) memory_bytes: usize,
    pub(crate) exec: Duration,
    pub(crate) max_instances: u16,
}

impl PluginLimits {
    pub(crate) fn clamp(resources: &Resources, config: &RuntimeConfig) -> Self {
        let memory = resources
            .max_memory_mb
            .map_or(config.instance_memory, |mb| (u64::from(mb) * 1024 * 1024).min(config.instance_memory));
        let exec = resources.max_exec_ms.map_or(config.exec, |ms| Duration::from_millis(ms).min(config.exec));
        let max_instances = resources.max_instances.map_or(config.instances, |n| n.min(config.instances));
        Self { memory_bytes: usize::try_from(memory).unwrap_or(usize::MAX), exec, max_instances: max_instances.max(1) }
    }

    /// The wall-clock cap of one call.
    pub(crate) fn wall_clock(&self) -> Duration { self.exec + WALL_CLOCK_SLACK }

    /// The deadline of an input/output provider call: a full download or
    /// render, so the hook deadline times [`PROVIDER_EXEC_MULTIPLIER`], never
    /// more than `plugins.max_task_exec` (plan §7 *Input providers*).
    pub(crate) fn provider_exec(&self, config: &RuntimeConfig) -> Duration {
        (self.exec * PROVIDER_EXEC_MULTIPLIER).min(config.task_exec.max(self.exec))
    }
}

/// How many hook deadlines one provider call (`fetch-playlist`, `fetch-epg`,
/// one output render) may take.
pub(crate) const PROVIDER_EXEC_MULTIPLIER: u32 = 10;

/// Why a guest call or an instantiation did not produce a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallError {
    /// The epoch deadline (CPU) or the wall-clock cap passed.
    Deadline,
    /// The guest tried to grow memory past its limit.
    OutOfMemory,
    /// Any other trap.
    Trap(String),
    /// The memory budget or the plugin's instance cap stayed exhausted for a
    /// whole call deadline.
    Budget { waited: Duration },
    /// The component could not be instantiated.
    Instantiate(String),
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Deadline => f.write_str("deadline exceeded"),
            Self::OutOfMemory => f.write_str("out of memory"),
            Self::Trap(reason) => write!(f, "trapped: {reason}"),
            Self::Budget { waited } => write!(f, "no memory budget after {}ms", waited.as_millis()),
            Self::Instantiate(reason) => write!(f, "cannot instantiate: {reason}"),
        }
    }
}

/// The global memory budget, for stats.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct BudgetStats {
    pub(crate) total_mib: u32,
    pub(crate) in_use_mib: u32,
    /// Instantiations that had to wait for permits.
    pub(crate) waits: u64,
    pub(crate) wait_ms_total: u64,
    pub(crate) wait_ms_max: u64,
    /// Waits that ran out of time ([`CallError::Budget`]).
    pub(crate) timeouts: u64,
}

/// One plugin's instances, for stats.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct InstanceStats {
    pub(crate) live: u32,
    pub(crate) max: u16,
    pub(crate) instantiations: u64,
    /// Instantiations that queued behind the plugin's instance cap.
    pub(crate) waits: u64,
}

/// Advances the engine's epoch every [`EPOCH_TICK`] until dropped.
pub(crate) struct EpochTicker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl EpochTicker {
    pub(crate) fn start(engine: &Engine) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let engine = engine.clone();
            let stop = Arc::clone(&stop);
            std::thread::Builder::new().name("plugin-epoch".to_string()).spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    std::thread::park_timeout(EPOCH_TICK);
                    engine.increment_epoch();
                }
            })?
        };
        Ok(Self { stop, thread: Some(thread) })
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            // The loop checks the flag right after waking, so this is immediate.
            let _ = thread.join();
        }
    }
}

const MIB: u64 = 1024 * 1024;
/// wasm32 linear memory tops out at 4 GiB, which is also wasmtime's default
/// reservation per memory; a pool slot never needs more.
const POOL_MEMORY_CEILING: u64 = 4 * 1024 * MIB - 64 * 1024;
const POOL_MIN_SLOTS: u32 = 4;
const POOL_MAX_SLOTS: u32 = 64;
/// Core instances one component may hold (the main module plus the
/// `wit-component` shim and fixup modules).
const POOL_CORE_INSTANCES_PER_SLOT: u32 = 8;
/// Tables across those core instances (one funcref table per module).
const POOL_TABLES_PER_SLOT: u32 = 4;

/// `bytes` in whole MiB, rounded up.
fn mib_ceil(bytes: u64) -> u32 { u32::try_from(bytes.div_ceil(MIB)).unwrap_or(u32::MAX) }

/// The pooling allocator's size, derived from the memory budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PoolPlan {
    /// Concurrent instances the pool holds.
    slots: u32,
    /// Linear-memory size of one slot (`plugins.max_memory`, capped at 4 GiB).
    memory_bytes: usize,
}

impl PoolPlan {
    /// Two slots per `max_memory` that fit in the budget (small plugins use
    /// less than the ceiling), within `4..=64`.
    fn new(total_mib: u32, instance_memory: u64) -> Self {
        let per_instance = mib_ceil(instance_memory).max(1);
        let slots = (total_mib / per_instance).saturating_mul(2).clamp(POOL_MIN_SLOTS, POOL_MAX_SLOTS);
        let memory = instance_memory.min(POOL_MEMORY_CEILING);
        Self { slots, memory_bytes: usize::try_from(memory).unwrap_or(usize::MAX) }
    }

    /// What every instance is charged at least, so `slots` bounds the live
    /// instances and the pool cannot run dry.
    fn min_charge_mib(&self, total_mib: u32) -> u32 { total_mib.div_ceil(self.slots).max(1) }

    fn pooling(&self) -> PoolingAllocationConfig {
        let mut pool = PoolingAllocationConfig::new();
        pool.total_component_instances(self.slots)
            .total_core_instances(self.slots.saturating_mul(POOL_CORE_INSTANCES_PER_SLOT))
            .total_memories(self.slots)
            .total_tables(self.slots.saturating_mul(POOL_TABLES_PER_SLOT))
            .total_stacks(self.slots)
            .max_memories_per_component(1)
            .max_memory_size(self.memory_bytes);
        pool
    }
}

/// The global memory budget: a semaphore of MiB permits plus its counters.
/// Plain (no wasm), so the accounting is testable on its own.
#[derive(Debug)]
struct MemoryBudget {
    permits: Arc<Semaphore>,
    total_mib: u32,
    /// The least an instance is charged.
    min_charge_mib: u32,
    waits: AtomicU64,
    wait_ms_total: AtomicU64,
    wait_ms_max: AtomicU64,
    timeouts: AtomicU64,
}

impl MemoryBudget {
    fn new(total_mib: u32, min_charge_mib: u32) -> Self {
        let total_mib = total_mib.max(1);
        Self {
            permits: Arc::new(Semaphore::new(total_mib as usize)),
            total_mib,
            min_charge_mib: min_charge_mib.clamp(1, total_mib),
            waits: AtomicU64::new(0),
            wait_ms_total: AtomicU64::new(0),
            wait_ms_max: AtomicU64::new(0),
            timeouts: AtomicU64::new(0),
        }
    }

    /// The permits one instance of `memory_bytes` takes.
    fn charge(&self, memory_bytes: usize) -> u32 {
        mib_ceil(memory_bytes as u64).max(self.min_charge_mib).min(self.total_mib)
    }

    fn in_use_mib(&self) -> u32 {
        let free = u32::try_from(self.permits.available_permits()).unwrap_or(u32::MAX);
        self.total_mib.saturating_sub(free)
    }

    fn record_wait(&self, waited: Duration) {
        let ms = u64::try_from(waited.as_millis()).unwrap_or(u64::MAX);
        self.waits.fetch_add(1, Ordering::Relaxed);
        self.wait_ms_total.fetch_add(ms, Ordering::Relaxed);
        self.wait_ms_max.fetch_max(ms, Ordering::Relaxed);
    }

    fn stats(&self) -> BudgetStats {
        BudgetStats {
            total_mib: self.total_mib,
            in_use_mib: self.in_use_mib(),
            waits: self.waits.load(Ordering::Relaxed),
            wait_ms_total: self.wait_ms_total.load(Ordering::Relaxed),
            wait_ms_max: self.wait_ms_max.load(Ordering::Relaxed),
            timeouts: self.timeouts.load(Ordering::Relaxed),
        }
    }
}

/// One plugin's instance cap and its counters; shared by every clone of its
/// [`PreparedPlugin`].
#[derive(Debug)]
struct InstanceCap {
    slots: Arc<Semaphore>,
    max: u16,
    instantiations: AtomicU64,
    waits: AtomicU64,
}

impl InstanceCap {
    fn new(max: u16) -> Self {
        let max = max.max(1);
        Self {
            slots: Arc::new(Semaphore::new(usize::from(max))),
            max,
            instantiations: AtomicU64::new(0),
            waits: AtomicU64::new(0),
        }
    }

    fn stats(&self) -> InstanceStats {
        let free = u32::try_from(self.slots.available_permits()).unwrap_or(u32::MAX);
        InstanceStats {
            live: u32::from(self.max).saturating_sub(free),
            max: self.max,
            instantiations: self.instantiations.load(Ordering::Relaxed),
            waits: self.waits.load(Ordering::Relaxed),
        }
    }
}

/// The outcome of taking permits without ever waiting past a deadline.
enum Taken {
    Immediate(OwnedSemaphorePermit),
    Waited(OwnedSemaphorePermit),
    TimedOut,
}

async fn take(semaphore: &Arc<Semaphore>, permits: u32, deadline: Instant) -> Taken {
    match Arc::clone(semaphore).try_acquire_many_owned(permits) {
        Ok(permit) => Taken::Immediate(permit),
        Err(TryAcquireError::NoPermits) => {
            match tokio::time::timeout_at(deadline, Arc::clone(semaphore).acquire_many_owned(permits)).await {
                Ok(Ok(permit)) => Taken::Waited(permit),
                _ => Taken::TimedOut,
            }
        }
        Err(TryAcquireError::Closed) => Taken::TimedOut,
    }
}

/// An instance slot and its memory permits. Dropping returns both.
#[derive(Debug)]
struct Reservation {
    // Held for their `Drop`; the memory permits are released with the slot.
    _memory: OwnedSemaphorePermit,
    _slot: OwnedSemaphorePermit,
}

impl Reservation {
    /// Take a slot of `cap`, then the memory permits for `memory_bytes`, both
    /// within `within`. Cancel-safe: a timed-out wait leaves nothing taken.
    async fn acquire(
        budget: &MemoryBudget,
        cap: &InstanceCap,
        memory_bytes: usize,
        within: Duration,
    ) -> Result<Self, CallError> {
        let started = Instant::now();
        let deadline = started + within;
        let timed_out = || {
            budget.timeouts.fetch_add(1, Ordering::Relaxed);
            CallError::Budget { waited: started.elapsed() }
        };

        let slot = match take(&cap.slots, 1, deadline).await {
            Taken::Immediate(permit) => permit,
            Taken::Waited(permit) => {
                cap.waits.fetch_add(1, Ordering::Relaxed);
                permit
            }
            Taken::TimedOut => {
                cap.waits.fetch_add(1, Ordering::Relaxed);
                return Err(timed_out());
            }
        };

        let mib = budget.charge(memory_bytes);
        let memory_started = Instant::now();
        let memory = match take(&budget.permits, mib, deadline).await {
            Taken::Immediate(permit) => permit,
            Taken::Waited(permit) => {
                budget.record_wait(memory_started.elapsed());
                permit
            }
            Taken::TimedOut => {
                budget.record_wait(memory_started.elapsed());
                return Err(timed_out());
            }
        };
        cap.instantiations.fetch_add(1, Ordering::Relaxed);
        Ok(Self { _memory: memory, _slot: slot })
    }
}

/// The CPU budget of the next guest call: how many epoch ticks it may still
/// run. The epoch callback spends it one tick at a time, yielding to the async
/// executor in between, and interrupts the guest when it is gone.
#[derive(Debug, Clone)]
struct CpuBudget {
    remaining: Arc<AtomicU32>,
    ticks: u32,
}

impl CpuBudget {
    /// Install the yielding deadline callback on `store` and arm it.
    fn install<T>(store: &mut Store<T>, limits: PluginLimits) -> Self {
        let ticks = limits.exec.as_millis().div_ceil(EPOCH_TICK.as_millis()).max(1);
        let budget = Self { remaining: Arc::new(AtomicU32::new(0)), ticks: u32::try_from(ticks).unwrap_or(u32::MAX) };
        let remaining = Arc::clone(&budget.remaining);
        store.epoch_deadline_callback(move |_| {
            let left = remaining.load(Ordering::Relaxed);
            if left <= 1 {
                remaining.store(0, Ordering::Relaxed);
                Ok(UpdateDeadline::Interrupt)
            } else {
                remaining.store(left - 1, Ordering::Relaxed);
                Ok(UpdateDeadline::Yield(1))
            }
        });
        budget.arm(store);
        budget
    }

    /// Reset the budget for the next call: the callback fires on the next tick
    /// and then once per tick.
    fn arm<T>(&self, store: &mut Store<T>) {
        self.remaining.store(self.ticks, Ordering::Relaxed);
        store.set_epoch_deadline(1);
    }
}

/// The engine, the linker and the budget. One per `PluginSystem`.
pub(crate) struct Runtime {
    engine: Engine,
    linker: Linker<HostState>,
    config: RuntimeConfig,
    budget: Arc<MemoryBudget>,
    /// Compiled and linked components by `wasm_sha256` ([`Runtime::prepare_cached`]).
    cache: Mutex<HashMap<String, CachedPlugin>>,
    /// The rpc route's token buckets and counters (W6b); never the breaker.
    rpc: RpcGate,
    #[cfg(test)]
    pooled: bool,
    // Declared last: dropped after everything that can still run a guest.
    _ticker: EpochTicker,
}

#[derive(Clone)]
struct CachedPlugin {
    component: Component,
    pre: InstancePre<HostState>,
}

fn base_config() -> Config {
    let mut wasm = Config::new();
    wasm.wasm_component_model(true);
    wasm.epoch_interruption(true);
    // No explicit target: wasmtime's build script already points `Config::new()`
    // at Pulley on architectures Cranelift cannot host (armv7).
    wasm
}

/// The engine with the pooling allocator when the host can take it, else with
/// the on-demand one. The plan is `Some` when the pool is in use.
fn build_engine(total_mib: u32, instance_memory: u64) -> Result<(Engine, Option<PoolPlan>), TuliproxError> {
    if !cfg!(target_pointer_width = "32") {
        let plan = PoolPlan::new(total_mib, instance_memory);
        let mut wasm = base_config();
        wasm.allocation_strategy(InstanceAllocationStrategy::Pooling(plan.pooling()));
        match Engine::new(&wasm) {
            Ok(engine) => return Ok((engine, Some(plan))),
            Err(err) => warn!("plugins: pooling allocator unavailable, using the on-demand allocator: {err:#}"),
        }
    }
    let engine = Engine::new(&base_config()).map_err(|err| plugin_error("cannot create the engine", &err))?;
    Ok((engine, None))
}

impl Runtime {
    pub(crate) fn new(config: RuntimeConfig) -> Result<Self, TuliproxError> {
        let total_mib = u32::try_from(config.total_memory / MIB).unwrap_or(u32::MAX).max(1);
        let (engine, plan) = build_engine(total_mib, config.instance_memory)?;
        let min_charge = plan.map_or(1, |plan| plan.min_charge_mib(total_mib));
        let linker = linker(&engine).map_err(|err| plugin_error("cannot build the linker", &err))?;
        let ticker = EpochTicker::start(&engine)
            .map_err(|err| TuliproxError::Plugin(format!("cannot start the epoch ticker: {err}")))?;
        Ok(Self {
            engine,
            linker,
            config,
            budget: Arc::new(MemoryBudget::new(total_mib, min_charge)),
            cache: Mutex::new(HashMap::new()),
            rpc: RpcGate::default(),
            #[cfg(test)]
            pooled: plan.is_some(),
            _ticker: ticker,
        })
    }

    pub(crate) fn engine(&self) -> &Engine { &self.engine }

    pub(crate) fn config(&self) -> &RuntimeConfig { &self.config }

    pub(crate) fn rpc(&self) -> &RpcGate { &self.rpc }

    /// Compile a component. CPU-bound and synchronous: call it off the async
    /// workers (the registry loads on `spawn_blocking`).
    pub(crate) fn compile(&self, wasm: &[u8]) -> Result<Component, TuliproxError> {
        Component::from_binary(&self.engine, wasm).map_err(|err| plugin_error("cannot compile plugin.wasm", &err))
    }

    /// Compile and link `wasm` once per `wasm_sha256`: a later call with the
    /// same hash reuses the cached component and link. Each call gets a fresh
    /// instance cap. Fails when the component imports what the host lacks, or
    /// when its memory limit exceeds the whole budget. CPU-bound like `compile`.
    pub(crate) fn prepare_cached(
        &self,
        wasm_sha256: &str,
        wasm: &[u8],
        limits: PluginLimits,
    ) -> Result<(Component, PreparedPlugin), TuliproxError> {
        self.check_memory(limits)?;
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(cached) = cache.get(wasm_sha256) {
            return Ok((cached.component.clone(), PreparedPlugin::new(cached.pre.clone(), limits)));
        }
        let component = self.compile(wasm)?;
        let pre = self.link(&component)?;
        cache.insert(wasm_sha256.to_string(), CachedPlugin { component: component.clone(), pre: pre.clone() });
        Ok((component, PreparedPlugin::new(pre, limits)))
    }

    /// Drop every cached component whose hash is not in `keep`, after a reload.
    pub(crate) fn retain_cached(&self, keep: &HashSet<String>) {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner).retain(|sha256, _| keep.contains(sha256));
    }

    fn link(&self, component: &Component) -> Result<InstancePre<HostState>, TuliproxError> {
        self.linker
            .instantiate_pre(component)
            .map_err(|err| plugin_error("plugin.wasm imports what the host does not provide", &err))
    }

    fn check_memory(&self, limits: PluginLimits) -> Result<(), TuliproxError> {
        let total = u64::from(self.budget.total_mib) * MIB;
        if limits.memory_bytes as u64 > total {
            return Err(TuliproxError::Plugin(format!(
                "plugin memory limit of {} MiB exceeds plugins.max_total_memory of {} MiB",
                mib_ceil(limits.memory_bytes as u64),
                self.budget.total_mib
            )));
        }
        Ok(())
    }

    /// A fresh instance of `plugin` in a fresh `Store`, after taking its memory
    /// permits and an instance slot (waiting up to the call deadline for them).
    /// Dropping the [`LiveInstance`] returns both.
    pub(crate) async fn instantiate(
        &self,
        plugin: &PreparedPlugin,
        env: &Arc<PluginEnv>,
    ) -> Result<LiveInstance, CallError> {
        let limits = plugin.limits;
        // Declared before the store, so it drops after it.
        let reservation = Reservation::acquire(&self.budget, &plugin.cap, limits.memory_bytes, limits.exec).await?;
        let mut store = Store::new(&self.engine, HostState::new(Arc::clone(env), limits.memory_bytes));
        store.limiter(|state| &mut state.limits);
        let cpu = CpuBudget::install(&mut store, limits);
        let instance =
            call_with_deadline(limits, plugin.pre.instantiate_async(&mut store)).await.map_err(|err| match err {
                CallError::Trap(reason) => CallError::Instantiate(reason),
                other => other,
            })?;
        Ok(LiveInstance { store, instance, limits, cpu, _reservation: reservation })
    }

    pub(crate) fn budget_stats(&self) -> BudgetStats { self.budget.stats() }
}

/// A plugin compiled and linked once, instantiated per run.
#[derive(Clone)]
pub(crate) struct PreparedPlugin {
    pre: InstancePre<HostState>,
    limits: PluginLimits,
    cap: Arc<InstanceCap>,
}

impl PreparedPlugin {
    fn new(pre: InstancePre<HostState>, limits: PluginLimits) -> Self {
        Self { pre, limits, cap: Arc::new(InstanceCap::new(limits.max_instances)) }
    }

    pub(crate) fn pre(&self) -> &InstancePre<HostState> { &self.pre }

    /// The same plugin under another call deadline: a task
    /// (`plugins.max_task_exec`), a provider call, or the 50 ms header clamp.
    /// It shares the instance cap, so the per-plugin instance limit still holds.
    pub(crate) fn with_exec(&self, exec: Duration) -> Self {
        Self { pre: self.pre.clone(), limits: PluginLimits { exec, ..self.limits }, cap: Arc::clone(&self.cap) }
    }
    pub(crate) fn limits(&self) -> PluginLimits { self.limits }

    pub(crate) fn stats(&self) -> InstanceStats { self.cap.stats() }
}

/// One live guest instance and the budget it holds.
pub(crate) struct LiveInstance {
    // Field order is drop order: the store goes before the permits.
    store: Store<HostState>,
    instance: Instance,
    limits: PluginLimits,
    cpu: CpuBudget,
    _reservation: Reservation,
}

impl LiveInstance {
    pub(crate) fn instance(&self) -> Instance { self.instance }

    pub(crate) fn limits(&self) -> PluginLimits { self.limits }

    pub(crate) fn store_mut(&mut self) -> &mut Store<HostState> { &mut self.store }

    /// The store with the CPU deadline re-armed, for the next guest call:
    /// `call_with_deadline(inst.limits(), hooks.call_x(inst.armed(), …))`.
    pub(crate) fn armed(&mut self) -> &mut Store<HostState> {
        self.cpu.arm(&mut self.store);
        &mut self.store
    }
}

/// What a trap or error from the guest means.
fn classify(err: &wasmtime::Error) -> CallError {
    if matches!(err.downcast_ref::<Trap>(), Some(Trap::Interrupt)) {
        return CallError::Deadline;
    }
    let reason = format!("{err:#}");
    // `StoreLimits` with `trap_on_grow_failure` raises these as plain errors.
    if reason.contains("forcing trap when growing memory")
        || reason.contains("forcing a memory growth failure")
        || reason.contains("exceeds memory limits")
    {
        CallError::OutOfMemory
    } else {
        CallError::Trap(reason)
    }
}

/// Run one guest call to completion under the wall-clock cap, classifying a
/// trap (epoch deadline, memory growth, other) or a timeout.
pub(crate) async fn call_with_deadline<T>(
    limits: PluginLimits,
    call: impl Future<Output = wasmtime::Result<T>>,
) -> Result<T, CallError> {
    match tokio::time::timeout(limits.wall_clock(), call).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(classify(&err)),
        Err(_) => Err(CallError::Deadline),
    }
}

/// A linker holding all of `wasi:*` (over an empty `WasiCtx`), the
/// `wasi:http` interfaces and the tuliprox `host` interface.
fn linker(engine: &Engine) -> wasmtime::Result<Linker<HostState>> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    host::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |state| state)?;
    Ok(linker)
}

fn plugin_error(what: &str, err: &wasmtime::Error) -> TuliproxError {
    TuliproxError::Plugin(format!("{what}: {err:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::Module;

    const MIB_BYTES: usize = 1024 * 1024;

    fn config() -> RuntimeConfig {
        RuntimeConfig {
            total_memory: 512 * MIB,
            instance_memory: 64 * MIB,
            exec: Duration::from_millis(500),
            instances: 4,
            task_exec: Duration::from_secs(600),
        }
    }

    /// `(module (func (export "spin") (loop (br 0))))`
    const SPIN: &[u8] = &[
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // header
        0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type: () -> ()
        0x03, 0x02, 0x01, 0x00, // function 0 has type 0
        0x07, 0x08, 0x01, 0x04, b's', b'p', b'i', b'n', 0x00, 0x00, // export "spin"
        0x0a, 0x09, 0x01, 0x07, 0x00, 0x03, 0x40, 0x0c, 0x00, 0x0b, 0x0b, // loop { br 0 }
    ];

    fn engine_with_epochs() -> Engine {
        let mut wasm = Config::new();
        wasm.epoch_interruption(true);
        Engine::new(&wasm).expect("engine")
    }

    #[test]
    fn clamp_takes_the_smaller_of_manifest_and_config() {
        let config = config();
        let asked =
            Resources { max_memory_mb: Some(16), max_exec_ms: Some(100), max_instances: Some(2), ..Default::default() };
        let limits = PluginLimits::clamp(&asked, &config);
        assert_eq!(limits.memory_bytes, 16 * 1024 * 1024);
        assert_eq!(limits.exec, Duration::from_millis(100));
        assert_eq!(limits.max_instances, 2);

        let greedy = Resources {
            max_memory_mb: Some(4096),
            max_exec_ms: Some(60_000),
            max_instances: Some(64),
            ..Default::default()
        };
        let limits = PluginLimits::clamp(&greedy, &config);
        assert_eq!(limits.memory_bytes, 64 * 1024 * 1024);
        assert_eq!(limits.exec, Duration::from_millis(500));
        assert_eq!(limits.max_instances, 4);
    }

    #[test]
    fn clamp_defaults_to_the_config_and_keeps_one_instance() {
        let limits = PluginLimits::clamp(&Resources::default(), &config());
        assert_eq!(limits.memory_bytes, 64 * 1024 * 1024);
        assert_eq!(limits.exec, Duration::from_millis(500));
        assert_eq!(limits.max_instances, 4);
        assert_eq!(limits.wall_clock(), Duration::from_millis(500) + WALL_CLOCK_SLACK);

        let none = RuntimeConfig { instances: 0, ..config() };
        assert_eq!(PluginLimits::clamp(&Resources::default(), &none).max_instances, 1);
    }

    #[test]
    fn pool_plan_follows_the_budget() {
        // 512 MiB / 64 MiB = 8 instances, two slots each.
        let plan = PoolPlan::new(512, 64 * MIB);
        assert_eq!(plan.slots, 16);
        assert_eq!(plan.memory_bytes, 64 * 1024 * 1024);
        assert_eq!(plan.min_charge_mib(512), 32);
        // Never below 4 or above 64 slots; the memory size stops short of 4 GiB.
        assert_eq!(PoolPlan::new(64, 64 * MIB).slots, POOL_MIN_SLOTS);
        assert_eq!(PoolPlan::new(65_536, 16 * MIB).slots, POOL_MAX_SLOTS);
        assert!(PoolPlan::new(8192, 8 * 1024 * MIB).memory_bytes < 4 * 1024 * 1024 * 1024);
        // The minimum charge keeps the live instances within the slots.
        let plan = PoolPlan::new(100, 64 * MIB);
        assert!(100 / plan.min_charge_mib(100) <= plan.slots);
    }

    #[test]
    fn charge_rounds_up_and_respects_the_minimum() {
        let budget = MemoryBudget::new(512, 4);
        assert_eq!(budget.charge(1), 4);
        assert_eq!(budget.charge(10 * 1024 * 1024), 10);
        assert_eq!(budget.charge(10 * 1024 * 1024 + 1), 11);
        assert_eq!(budget.charge(usize::MAX), 512);
    }

    #[tokio::test]
    async fn budget_reserves_and_releases() {
        let budget = MemoryBudget::new(100, 1);
        let cap = InstanceCap::new(4);
        let within = Duration::from_millis(200);

        let first = Reservation::acquire(&budget, &cap, 60 * 1024 * 1024, within).await.expect("first");
        assert_eq!(budget.stats().in_use_mib, 60);
        assert_eq!(cap.stats().live, 1);
        let second = Reservation::acquire(&budget, &cap, 40 * 1024 * 1024, within).await.expect("second");
        assert_eq!(budget.stats().in_use_mib, 100);
        assert_eq!(cap.stats(), InstanceStats { live: 2, max: 4, instantiations: 2, waits: 0 });
        assert_eq!(budget.stats().waits, 0);

        drop(first);
        assert_eq!(budget.stats().in_use_mib, 40);
        drop(second);
        let stats = budget.stats();
        assert_eq!((stats.total_mib, stats.in_use_mib, stats.timeouts), (100, 0, 0));
        assert_eq!(cap.stats().live, 0);
    }

    #[tokio::test]
    async fn budget_never_over_commits_and_times_out() {
        let budget = MemoryBudget::new(100, 1);
        let cap = InstanceCap::new(4);
        let held =
            Reservation::acquire(&budget, &cap, 80 * 1024 * 1024, Duration::from_millis(50)).await.expect("held");

        let err = Reservation::acquire(&budget, &cap, 40 * 1024 * 1024, Duration::from_millis(40))
            .await
            .expect_err("only 20 MiB are free");
        let CallError::Budget { waited } = err else { panic!("expected Budget, got {err:?}") };
        assert!(waited >= Duration::from_millis(30), "waited {waited:?}");

        let stats = budget.stats();
        assert_eq!(stats.in_use_mib, 80, "a failed wait takes nothing");
        assert_eq!((stats.waits, stats.timeouts), (1, 1));
        assert!(stats.wait_ms_max >= 30 && stats.wait_ms_total >= stats.wait_ms_max);
        // The slot taken before the memory wait went back.
        assert_eq!(cap.stats().live, 1);
        assert_eq!(cap.stats().instantiations, 1);
        drop(held);
    }

    #[tokio::test]
    async fn a_waiter_gets_the_permits_when_they_return() {
        let budget = Arc::new(MemoryBudget::new(100, 1));
        let cap = Arc::new(InstanceCap::new(4));
        let held =
            Reservation::acquire(&budget, &cap, 80 * 1024 * 1024, Duration::from_millis(50)).await.expect("held");

        let waiter = {
            let (budget, cap) = (Arc::clone(&budget), Arc::clone(&cap));
            tokio::spawn(async move {
                Reservation::acquire(&budget, &cap, 40 * 1024 * 1024, Duration::from_secs(5)).await.map(drop)
            })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(held);
        waiter.await.expect("join").expect("the waiter gets its permits");

        let stats = budget.stats();
        assert_eq!((stats.waits, stats.timeouts, stats.in_use_mib), (1, 0, 0));
        assert!(stats.wait_ms_max >= 20, "waited {}ms", stats.wait_ms_max);
    }

    #[tokio::test]
    async fn the_instance_cap_queues_and_times_out() {
        let budget = MemoryBudget::new(100, 1);
        let cap = InstanceCap::new(1);
        let held = Reservation::acquire(&budget, &cap, MIB_BYTES, Duration::from_millis(50)).await.expect("held");

        let err =
            Reservation::acquire(&budget, &cap, MIB_BYTES, Duration::from_millis(30)).await.expect_err("the cap is 1");
        assert!(matches!(err, CallError::Budget { .. }));
        assert_eq!(cap.stats(), InstanceStats { live: 1, max: 1, instantiations: 1, waits: 1 });
        assert_eq!(budget.stats().timeouts, 1);
        assert_eq!(budget.stats().in_use_mib, 1, "no memory taken past a full cap");
        drop(held);
        assert_eq!(cap.stats().live, 0);
    }

    #[test]
    fn classify_maps_traps() {
        assert_eq!(classify(&wasmtime::Error::from(Trap::Interrupt)), CallError::Deadline);
        assert_eq!(
            classify(&wasmtime::Error::msg("forcing trap when growing memory to 70000000 bytes")),
            CallError::OutOfMemory
        );
        assert_eq!(
            classify(&wasmtime::Error::msg("memory minimum size of 2000 pages exceeds memory limits")),
            CallError::OutOfMemory
        );
        assert!(matches!(classify(&wasmtime::Error::from(Trap::UnreachableCodeReached)), CallError::Trap(_)));
    }

    #[test]
    fn the_ticker_advances_the_epoch() {
        let engine = engine_with_epochs();
        let _ticker = EpochTicker::start(&engine).expect("ticker");
        let module = Module::new(&engine, SPIN).expect("module");
        let mut store = Store::new(&engine, ());
        store.epoch_deadline_trap();
        store.set_epoch_deadline(5);
        let instance = wasmtime::Instance::new(&mut store, &module, &[]).expect("instance");
        let spin = instance.get_typed_func::<(), ()>(&mut store, "spin").expect("spin");
        let started = std::time::Instant::now();
        let err = spin.call(&mut store, ()).expect_err("the spin loop is interrupted");
        assert_eq!(err.downcast_ref::<Trap>(), Some(&Trap::Interrupt));
        let elapsed = started.elapsed();
        assert!(elapsed >= EPOCH_TICK * 3 && elapsed < Duration::from_secs(2), "trapped after {elapsed:?}");
    }

    #[test]
    fn dropping_the_ticker_stops_its_thread() {
        let engine = engine_with_epochs();
        let ticker = EpochTicker::start(&engine).expect("ticker");
        // `Drop` joins the thread; a hang here would be the bug.
        drop(ticker);
    }

    #[tokio::test]
    async fn a_spinning_guest_yields_and_then_traps_at_its_deadline() {
        let engine = engine_with_epochs();
        let _ticker = EpochTicker::start(&engine).expect("ticker");
        let module = Module::new(&engine, SPIN).expect("module");
        let mut store = Store::new(&engine, ());
        let limits = PluginLimits { memory_bytes: 1 << 20, exec: Duration::from_millis(200), max_instances: 1 };
        let cpu = CpuBudget::install(&mut store, limits);
        let instance = wasmtime::Instance::new_async(&mut store, &module, &[]).await.expect("instance");
        let spin = instance.get_typed_func::<(), ()>(&mut store, "spin").expect("spin");

        let started = std::time::Instant::now();
        // One thread: the timer below only fires if the guest yields.
        let (result, timer_fired_at) = tokio::join!(
            async {
                cpu.arm(&mut store);
                call_with_deadline(limits, spin.call_async(&mut store, ())).await
            },
            async {
                tokio::time::sleep(Duration::from_millis(40)).await;
                started.elapsed()
            }
        );
        assert_eq!(result, Err(CallError::Deadline));
        assert!(timer_fired_at < Duration::from_millis(150), "the guest held the worker for {timer_fired_at:?}");
        let total = started.elapsed();
        assert!(total >= Duration::from_millis(150) && total < Duration::from_millis(1000), "trapped after {total:?}");

        // Armed again, the next call gets a whole new budget.
        cpu.arm(&mut store);
        let again = call_with_deadline(limits, spin.call_async(&mut store, ())).await;
        assert_eq!(again, Err(CallError::Deadline));
    }

    #[test]
    fn the_runtime_builds_with_the_pool_on_64_bit_hosts() {
        let runtime = Runtime::new(config()).expect("runtime");
        assert_eq!(runtime.pooled, !cfg!(target_pointer_width = "32"), "the pooling allocator did not build");
        let stats = runtime.budget_stats();
        assert_eq!((stats.total_mib, stats.in_use_mib), (512, 0));
    }

    #[test]
    fn prepare_rejects_a_plugin_larger_than_the_budget() {
        let runtime = Runtime::new(RuntimeConfig { total_memory: 32 * MIB, ..config() }).expect("runtime");
        let limits =
            PluginLimits { memory_bytes: 64 * 1024 * 1024, exec: Duration::from_millis(100), max_instances: 1 };
        let err = runtime.check_memory(limits).expect_err("64 MiB > 32 MiB");
        assert!(err.to_string().contains("max_total_memory"), "{err}");
        let fits = PluginLimits { memory_bytes: 32 * 1024 * 1024, ..limits };
        runtime.check_memory(fits).expect("32 MiB fits 32 MiB");
    }
}
