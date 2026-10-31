//! Scheduled plugin tasks (plan §7 *Scheduled plugin tasks*): `run_task` with
//! the task deadline and concurrency policy, results, `kv_generation`.
//!
//! A task is a fire of the scheduler's cron entry or an operator's "run now".
//! Each fire runs the guest's `tasks.run` in its own fresh instance, under the
//! manifest's `max_exec_ms` clamped by `plugins.max_task_exec`. Per
//! (plugin, task) the runner keeps what the policy needs:
//!
//! * `concurrency: skip` drops a fire that arrives while the task runs.
//! * `concurrency: queue` keeps one pending fire. Its caller waits and gets the
//!   outcome of its own run, so a failing queued run is reported like any other.
//!   A second pending fire is dropped.
//! * A manual run never queues: its caller wants the answer of *this* run.
//!
//! A registry reload cancels a running task: the call is raced against
//! [`PluginSystem::changes`], and dropping the guest future ends it.

use crate::{
    dispatch::HookDispatcher,
    host_fns,
    logs::redact,
    manifest::{Concurrency, Task},
    registry::{LoadedPlugin, PluginRecord, RegistrySnapshot},
    runtime::{call_with_deadline, CallError},
    system::PluginSystem,
};
use shared::model::{PluginHook, PluginTaskRunDto};
use std::{
    collections::HashMap,
    future::{poll_fn, Future},
    pin::pin,
    sync::{atomic::Ordering, Arc, Mutex, MutexGuard, PoisonError},
    task::Poll,
    time::{Duration, Instant},
};
use tokio::sync::{oneshot, watch};
use tuliprox_core::{model::PluginsConfig, utils::Clock};
use tuliprox_plugin_api::bindings::exports::tuliprox::plugin::tasks::TaskContext;

/// What starts a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskTrigger {
    /// A cron fire; `at` is the fire time in unix seconds.
    Scheduled { at: i64 },
    /// "Run now". `scheduled-at` is the start time.
    Manual,
}

impl TaskTrigger {
    const fn is_manual(self) -> bool { matches!(self, Self::Manual) }
}

/// What a [`PluginSystem::run_task`] call came to.
#[derive(Debug, PartialEq, Eq)]
pub enum TaskOutcome {
    /// The task ran. A guest `err`, a trap or a timeout is a *run* with
    /// `error` set, so it can be reported and shown as the last result.
    Ran(PluginTaskRunDto),
    /// Nothing ran: the previous run is still going, or the plugin is not
    /// runnable (Disabled, Failed, ...). Carries why.
    Skipped(String),
    /// No such plugin, or the plugin declares no such task.
    Unknown(String),
}

/// The ceiling on the summary or error text of one run, so a guest cannot fill
/// the events and the API with an unbounded string.
const MAX_RESULT_BYTES: usize = 2048;

/// A fire that waits for the running one to end (`concurrency: queue`).
struct Pending {
    trigger: TaskTrigger,
    done: oneshot::Sender<TaskOutcome>,
}

#[derive(Default)]
struct TaskState {
    running: bool,
    pending: Option<Pending>,
    last_run: Option<PluginTaskRunDto>,
    /// Start time (unix seconds) of the last run that returned `ok`.
    last_success_at: Option<i64>,
}

/// What a fire may do, decided under the state lock.
enum Claim {
    /// Nothing runs: the caller owns the task and must call `next_pending` /
    /// `abandon` when done.
    Run,
    /// A run is going and this fire is the pending one.
    Wait(oneshot::Receiver<TaskOutcome>),
    Skip(String),
}

type Key = (String, String);

/// Running and pending runs, last results. Built once with the
/// [`crate::PluginSystem`].
pub(crate) struct TaskRunner {
    states: Mutex<HashMap<Key, TaskState>>,
}

impl TaskRunner {
    pub(crate) fn new(_config: &PluginsConfig) -> Self { Self { states: Mutex::new(HashMap::new()) } }

    fn states(&self) -> MutexGuard<'_, HashMap<Key, TaskState>> {
        self.states.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn claim(&self, key: &Key, policy: Concurrency, trigger: TaskTrigger) -> Claim {
        let mut states = self.states();
        let state = states.entry(key.clone()).or_default();
        if !state.running {
            state.running = true;
            return Claim::Run;
        }
        if policy == Concurrency::Skip || trigger.is_manual() {
            return Claim::Skip("the previous run is still going".to_string());
        }
        if state.pending.is_some() {
            return Claim::Skip("the previous run is still going and another run is already queued".to_string());
        }
        let (done, wait) = oneshot::channel();
        state.pending = Some(Pending { trigger, done });
        Claim::Wait(wait)
    }

    /// The fire queued behind the run that just ended, if any; otherwise the
    /// task is idle again. One lock, so a fire never slips in between.
    fn next_pending(&self, key: &Key) -> Option<Pending> {
        let mut states = self.states();
        let state = states.entry(key.clone()).or_default();
        let pending = state.pending.take();
        state.running = pending.is_some();
        pending
    }

    /// Release a task whose owner was dropped mid-run. Dropping the pending
    /// fire's sender tells its caller that nothing will run.
    fn abandon(&self, key: &Key) {
        let mut states = self.states();
        if let Some(state) = states.get_mut(key) {
            state.running = false;
            state.pending = None;
        }
    }

    fn record(&self, key: &Key, run: &PluginTaskRunDto) {
        let mut states = self.states();
        let state = states.entry(key.clone()).or_default();
        if run.error.is_none() {
            state.last_success_at = Some(run.started_at);
        }
        state.last_run = Some(run.clone());
    }

    fn last_success_at(&self, key: &Key) -> Option<i64> {
        self.states().get(key).and_then(|state| state.last_success_at)
    }

    /// The last run of a task, `None` before its first.
    pub(crate) fn last_run(&self, plugin: &str, task: &str) -> Option<PluginTaskRunDto> {
        self.states().get(&(plugin.to_string(), task.to_string())).and_then(|state| state.last_run.clone())
    }
}

/// Clears `running` if the owning future is dropped before it finished.
struct OwnerGuard<'a> {
    runner: &'a TaskRunner,
    key: &'a Key,
    finished: bool,
}

impl Drop for OwnerGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.runner.abandon(self.key);
        }
    }
}

/// The deadline of one run: the task's own `max_exec_ms`, never above
/// `plugins.max_task_exec`. A task that names none gets the ceiling, since
/// tasks exist for the work a hook deadline is too short for.
pub(crate) fn task_deadline(task_ms: Option<u64>, ceiling: Duration) -> Duration {
    task_ms.map_or(ceiling, |ms| Duration::from_millis(ms).min(ceiling))
}

/// Why a run that did not complete says so.
enum Ended {
    Done(Result<Result<String, String>, CallError>),
    /// A registry reload cancelled it.
    Reloaded,
}

/// A task resolved against one registry snapshot. `changes` was subscribed
/// before the snapshot was read, so no reload can slip between the two.
struct Resolved {
    snapshot: Arc<RegistrySnapshot>,
    index: usize,
    task: Task,
    changes: watch::Receiver<u64>,
}

impl Resolved {
    fn record(&self) -> &Arc<PluginRecord> { &self.snapshot.plugins[self.index] }
}

impl PluginSystem {
    /// Run task `task` of plugin `plugin` (plan §7 *Scheduled plugin tasks*).
    ///
    /// The call returns when the run has ended, so an `await` here can take as
    /// long as `plugins.max_task_exec`. Spawn it from the scheduler. Fail-open:
    /// nothing here can fail the server, a failed run is an outcome.
    ///
    /// Maintenance is the caller's gate (the plugin crate does not know
    /// inputs): see [`PluginSystem::task_respects_maintenance`].
    pub async fn run_task(&self, plugin: &str, task: &str, trigger: TaskTrigger) -> TaskOutcome {
        let resolved = match self.resolve_task(plugin, task) {
            Ok(resolved) => resolved,
            Err(outcome) => return outcome,
        };
        let key: Key = (plugin.to_string(), task.to_string());
        match self.tasks().claim(&key, resolved.task.concurrency, trigger) {
            Claim::Skip(reason) => TaskOutcome::Skipped(reason),
            Claim::Wait(wait) => {
                wait.await.unwrap_or_else(|_| TaskOutcome::Skipped("the running task was cancelled".to_string()))
            }
            Claim::Run => self.own_task(&key, resolved, trigger).await,
        }
    }

    /// Whether a scheduled fire of this task is skipped during maintenance
    /// (manifest `respect_maintenance`, default `true`). `None` for an unknown
    /// plugin or task.
    #[must_use]
    pub fn task_respects_maintenance(&self, plugin: &str, task: &str) -> Option<bool> {
        let snapshot = self.registry().snapshot();
        let manifest = snapshot.get(plugin)?.manifest.as_deref()?;
        manifest.tasks.iter().find(|candidate| candidate.id == task).map(|candidate| candidate.respect_maintenance)
    }

    /// Run the fire that owns the task and return its result. A fire queued
    /// behind it runs in the background, so this caller (a "run now" answer
    /// or a cron fire) never waits for the next run; the queued fire's caller
    /// gets its own result through its channel.
    async fn own_task(&self, key: &Key, first: Resolved, trigger: TaskTrigger) -> TaskOutcome {
        let mut guard = OwnerGuard { runner: self.tasks(), key, finished: false };
        let outcome = self.execute(key, &first, trigger).await;
        // No await from here on: ownership passes to the drain, or the task is idle.
        guard.finished = true;
        if let Some(pending) = self.tasks().next_pending(key) {
            let system = self.clone();
            let key = key.clone();
            tokio::spawn(async move { system.drain_pending(&key, pending).await });
        }
        outcome
    }

    /// Run queued fires until none is left; the task stays owned meanwhile.
    async fn drain_pending(&self, key: &Key, mut pending: Pending) {
        let mut guard = OwnerGuard { runner: self.tasks(), key, finished: false };
        loop {
            let outcome = match self.resolve_task(&key.0, &key.1) {
                Ok(resolved) => self.execute(key, &resolved, pending.trigger).await,
                Err(outcome) => outcome,
            };
            // The waiter may have gone away (its request was dropped).
            let _ = pending.done.send(outcome);
            match self.tasks().next_pending(key) {
                Some(next) => pending = next,
                None => break,
            }
        }
        guard.finished = true;
    }

    fn resolve_task(&self, plugin: &str, task: &str) -> Result<Resolved, TaskOutcome> {
        let changes = self.changes();
        let snapshot = self.registry().snapshot();
        let Some(index) = snapshot.plugins.iter().position(|record| &*record.id == plugin) else {
            return Err(TaskOutcome::Unknown(format!("no plugin `{plugin}`")));
        };
        let record = &snapshot.plugins[index];
        let Some(declared) = record
            .manifest
            .as_deref()
            .and_then(|manifest| manifest.tasks.iter().find(|candidate| candidate.id == task))
            .cloned()
        else {
            return Err(TaskOutcome::Unknown(format!("plugin `{plugin}` declares no task `{task}`")));
        };
        if !record.runs(PluginHook::RunTask) {
            return Err(TaskOutcome::Skipped(format!("plugin `{plugin}` is {}", record.state().as_str())));
        }
        Ok(Resolved { snapshot, index, task: declared, changes })
    }

    /// One run in its own instance: the deadline, the breaker, `last_run` and
    /// `kv_generation`.
    async fn execute(&self, key: &Key, resolved: &Resolved, trigger: TaskTrigger) -> TaskOutcome {
        let record = resolved.record();
        let Some(loaded) = record.loaded.as_ref() else {
            return TaskOutcome::Skipped(format!("plugin `{}` is not loaded", record.id));
        };
        let started = Instant::now();
        let started_at = unix_seconds(loaded);
        let context = TaskContext {
            task_id: resolved.task.id.clone(),
            scheduled_at: match trigger {
                TaskTrigger::Scheduled { at } => at,
                TaskTrigger::Manual => started_at,
            },
            last_success_at: self.tasks().last_success_at(key),
        };
        let exec = task_deadline(resolved.task.max_exec_ms, self.runtime().config().task_exec);
        let dispatcher = HookDispatcher::new(self.runtime(), Arc::clone(&resolved.snapshot));
        let mut changes = resolved.changes.clone();

        let ended = {
            let mut call = pin!(call_task(&dispatcher, loaded, exec, &context));
            let mut reloaded = pin!(changes.changed());
            poll_fn(|cx| {
                // The call first: a run that finished in the same instant as a
                // reload keeps its result.
                if let Poll::Ready(done) = call.as_mut().poll(cx) {
                    return Poll::Ready(Ended::Done(done));
                }
                if reloaded.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(Ended::Reloaded);
                }
                Poll::Pending
            })
            .await
        };

        let secrets = &loaded.env.secrets;
        let (summary, error, guest_ran) = match ended {
            Ended::Done(Ok(Ok(summary))) => {
                record.report(Ok(()));
                (Some(summary), None, true)
            }
            // The guest answered `err`: a normal failed run, not a broken
            // plugin, so it does not feed the breaker (a trap or deadline does).
            Ended::Done(Ok(Err(message))) => {
                record.report(Ok(()));
                (None, Some(message), true)
            }
            Ended::Done(Err(err)) => {
                record.report(Err(&err));
                let ran = !matches!(err, CallError::Budget { .. } | CallError::Instantiate(_));
                (None, Some(err.to_string()), ran)
            }
            Ended::Reloaded => (None, Some("cancelled by a plugin reload".to_string()), true),
        };
        if guest_ran {
            // The run may have written KV: later hook results can differ.
            loaded.env.kv_generation.fetch_add(1, Ordering::Relaxed);
        }

        let run = PluginTaskRunDto {
            plugin: record.id.to_string(),
            task: resolved.task.id.clone(),
            started_at,
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            manual: trigger.is_manual(),
            summary: summary.map(|text| result_text(&text, secrets)),
            error: error.map(|text| result_text(&text, secrets)),
        };
        match &run.error {
            None => log::info!("[plugin:{}] task `{}` ran in {} ms", run.plugin, run.task, run.duration_ms),
            Some(error) => log::error!("[plugin:{}] task `{}` failed: {error}", run.plugin, run.task),
        }
        self.tasks().record(key, &run);
        TaskOutcome::Ran(run)
    }
}

/// The result text of a run: secrets masked, then cut to [`MAX_RESULT_BYTES`].
fn result_text(text: &str, secrets: &[String]) -> String {
    let redacted = redact(text, secrets);
    if redacted.len() <= MAX_RESULT_BYTES {
        return redacted.into_owned();
    }
    let mut end = MAX_RESULT_BYTES;
    while !redacted.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &redacted[..end])
}

fn unix_seconds(loaded: &LoadedPlugin) -> i64 {
    i64::try_from(loaded.env.clock.now_ms().get() / 1000).unwrap_or(i64::MAX)
}

/// Open an instance under `exec` and call `tasks.run` once.
async fn call_task(
    dispatcher: &HookDispatcher<'_>,
    loaded: &LoadedPlugin,
    exec: Duration,
    context: &TaskContext,
) -> Result<Result<String, String>, CallError> {
    let indices = loaded.tasks.as_ref().ok_or_else(|| CallError::Instantiate("does not export tasks".to_string()))?;
    let mut live = dispatcher.open_with(loaded, exec).await?;
    let instance = live.instance();
    let guest = indices.load(live.store_mut(), &instance).map_err(|err| CallError::Trap(format!("{err:#}")))?;
    let limits = live.limits();
    let result = call_with_deadline(limits, guest.call_run(live.armed(), context)).await;
    host_fns::end_call(live.store_mut().data_mut()).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::REDACTED_SECRET;

    fn runner() -> TaskRunner { TaskRunner::new(&PluginsConfig::default()) }

    fn key() -> Key { ("p".to_string(), "t".to_string()) }

    const SCHEDULED: TaskTrigger = TaskTrigger::Scheduled { at: 10 };

    #[test]
    fn skip_drops_a_fire_while_running() {
        let runner = runner();
        assert!(matches!(runner.claim(&key(), Concurrency::Skip, SCHEDULED), Claim::Run));
        assert!(matches!(runner.claim(&key(), Concurrency::Skip, SCHEDULED), Claim::Skip(_)));
        // Skip never leaves anything pending.
        assert!(runner.next_pending(&key()).is_none());
        assert!(matches!(runner.claim(&key(), Concurrency::Skip, SCHEDULED), Claim::Run));
    }

    #[test]
    fn queue_keeps_one_pending_fire_and_never_more() {
        let runner = runner();
        assert!(matches!(runner.claim(&key(), Concurrency::Queue, SCHEDULED), Claim::Run));
        let Claim::Wait(mut waiting) = runner.claim(&key(), Concurrency::Queue, TaskTrigger::Scheduled { at: 20 })
        else {
            panic!("the second fire must wait");
        };
        assert!(matches!(runner.claim(&key(), Concurrency::Queue, TaskTrigger::Scheduled { at: 30 }), Claim::Skip(_)));

        // The owner ends: the queued fire runs next, and the task stays busy.
        let pending = runner.next_pending(&key()).expect("one pending fire");
        assert_eq!(pending.trigger, TaskTrigger::Scheduled { at: 20 });
        assert!(matches!(runner.claim(&key(), Concurrency::Queue, SCHEDULED), Claim::Wait(_)));
        let outcome = TaskOutcome::Skipped("done".to_string());
        pending.done.send(outcome).unwrap();
        assert_eq!(waiting.try_recv().unwrap(), TaskOutcome::Skipped("done".to_string()));

        // The claim above filled the pending slot again; drain it, then idle.
        assert!(runner.next_pending(&key()).is_some());
        assert!(runner.next_pending(&key()).is_none());
        assert!(matches!(runner.claim(&key(), Concurrency::Queue, SCHEDULED), Claim::Run));
    }

    #[test]
    fn a_manual_run_never_queues() {
        let runner = runner();
        assert!(matches!(runner.claim(&key(), Concurrency::Queue, SCHEDULED), Claim::Run));
        assert!(matches!(runner.claim(&key(), Concurrency::Queue, TaskTrigger::Manual), Claim::Skip(_)));
    }

    #[test]
    fn an_abandoned_owner_releases_the_task_and_its_waiter() {
        let runner = runner();
        assert!(matches!(runner.claim(&key(), Concurrency::Queue, SCHEDULED), Claim::Run));
        let Claim::Wait(mut waiting) = runner.claim(&key(), Concurrency::Queue, SCHEDULED) else {
            panic!("the second fire must wait");
        };
        runner.abandon(&key());
        assert!(waiting.try_recv().is_err(), "the sender was dropped without an outcome");
        assert!(matches!(runner.claim(&key(), Concurrency::Queue, SCHEDULED), Claim::Run));
    }

    #[test]
    fn tasks_are_independent() {
        let runner = runner();
        assert!(matches!(runner.claim(&key(), Concurrency::Skip, SCHEDULED), Claim::Run));
        let other = ("p".to_string(), "other".to_string());
        assert!(matches!(runner.claim(&other, Concurrency::Skip, SCHEDULED), Claim::Run));
    }

    #[test]
    fn the_task_deadline_is_clamped_by_the_ceiling() {
        let ceiling = Duration::from_secs(60);
        assert_eq!(task_deadline(Some(5_000), ceiling), Duration::from_secs(5));
        assert_eq!(task_deadline(Some(600_000), ceiling), ceiling);
        assert_eq!(task_deadline(None, ceiling), ceiling);
    }

    #[test]
    fn last_run_and_last_success_are_tracked_apart() {
        let runner = runner();
        let run = |started_at, error: Option<&str>| PluginTaskRunDto {
            plugin: "p".to_string(),
            task: "t".to_string(),
            started_at,
            duration_ms: 1,
            manual: false,
            summary: None,
            error: error.map(ToString::to_string),
        };
        assert_eq!(runner.last_run("p", "t"), None);
        assert_eq!(runner.last_success_at(&key()), None);
        runner.record(&key(), &run(100, None));
        runner.record(&key(), &run(200, Some("boom")));
        assert_eq!(runner.last_run("p", "t").unwrap().started_at, 200);
        assert_eq!(runner.last_success_at(&key()), Some(100), "a failed run is not a success");
    }

    #[test]
    fn result_text_masks_secrets_and_cuts_on_a_char_boundary() {
        assert_eq!(
            result_text("token hunter2 failed", &["hunter2".to_string()]),
            format!("token {REDACTED_SECRET} failed")
        );
        let long = "€".repeat(MAX_RESULT_BYTES);
        let cut = result_text(&long, &[]);
        assert!(cut.len() <= MAX_RESULT_BYTES + '…'.len_utf8());
        assert!(cut.ends_with('…'));
        assert_eq!(result_text("short", &[]), "short");
    }
}
