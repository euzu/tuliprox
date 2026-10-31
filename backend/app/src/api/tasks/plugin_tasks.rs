//! Scheduler fires and manual runs of plugin tasks (plan §7 *Scheduled plugin
//! tasks*): the app side of `tuliprox_plugins` task runs, maintenance gate and
//! the `scheduled_task.failed` publish.
//!
//! Works with and without the `plugins` feature: without it every run is
//! `Unknown`, so the callers need no `cfg` of their own.

use crate::api::model::AppState;
use shared::model::PluginTaskRunDto;
#[cfg(feature = "plugins")]
use shared::model::{EventMessage, ScheduledTaskFailure};
#[cfg(feature = "plugins")]
pub(crate) use tuliprox_plugins::TaskTrigger;

/// What starts a run. Mirrors `tuliprox_plugins::TaskTrigger`, which a build
/// without the `plugins` feature does not have.
#[cfg(not(feature = "plugins"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskTrigger {
    #[allow(dead_code, reason = "the scheduler builds it at integration")]
    Scheduled {
        at: i64,
    },
    Manual,
}

/// How [`run_plugin_task`] ended.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(feature = "plugins"), allow(dead_code, reason = "only a build with plugins runs a task"))]
pub(crate) enum PluginTaskResult {
    /// The task ran. A failed run is a run, with `error` set.
    Ran(PluginTaskRunDto),
    /// Nothing ran: the previous run is still going, the plugin is not
    /// runnable, or global maintenance is on.
    Skipped(String),
    /// No such plugin or task, or plugins are off.
    Unknown(String),
}

/// Whether every configured input is in maintenance, which is what the
/// maintenance wildcard (`PUT /maintenance/*`) sets. A server with no inputs
/// has nothing to put in maintenance, so it is never "in maintenance".
#[cfg(feature = "plugins")]
fn in_global_maintenance(app_state: &AppState) -> bool {
    let sources = app_state.app_config.sources.load();
    !sources.inputs.is_empty()
        && sources.inputs.iter().all(|input| app_state.maintenance.is_in_maintenance(&input.name))
}

/// Run task `task` of plugin `plugin`: the maintenance gate, the run, and, for
/// a run that failed, the `scheduled_task.failed` event (the same one the
/// built-in tasks publish, so operators' notification routing already covers
/// it). `schedule` is the cron expression of a scheduled fire.
///
/// A manual run ignores maintenance: it is the operator's own decision.
/// Returns when the run has ended, which can take as long as
/// `plugins.max_task_exec`; the scheduler spawns it.
#[cfg(feature = "plugins")]
pub(crate) async fn run_plugin_task(
    app_state: &AppState,
    plugin: &str,
    task: &str,
    trigger: TaskTrigger,
    schedule: Option<&str>,
) -> PluginTaskResult {
    use tuliprox_plugins::TaskOutcome;

    let Some(plugins) = app_state.plugins.as_ref() else {
        return PluginTaskResult::Unknown("plugins are not enabled".to_string());
    };
    if matches!(trigger, TaskTrigger::Scheduled { .. })
        && plugins.task_respects_maintenance(plugin, task).unwrap_or(true)
        && in_global_maintenance(app_state)
    {
        log::info!("[plugin:{plugin}] task `{task}` skipped: global maintenance");
        return PluginTaskResult::Skipped("global maintenance".to_string());
    }
    match plugins.run_task(plugin, task, trigger).await {
        TaskOutcome::Ran(run) => {
            if let Some(error) = &run.error {
                let mut failure = ScheduledTaskFailure::for_plugin_task(plugin, task, error.clone());
                if let Some(schedule) = schedule {
                    failure = failure.with_schedule(schedule.to_string());
                }
                let _ = app_state.event_manager.send_event(EventMessage::ScheduledTaskFailed(failure));
            }
            PluginTaskResult::Ran(run)
        }
        TaskOutcome::Skipped(reason) => {
            log::info!("[plugin:{plugin}] task `{task}` skipped: {reason}");
            PluginTaskResult::Skipped(reason)
        }
        TaskOutcome::Unknown(reason) => PluginTaskResult::Unknown(reason),
    }
}

/// Built without the `plugins` feature: there is nothing to run.
#[cfg(not(feature = "plugins"))]
#[allow(clippy::unused_async, reason = "Same signature as the `plugins` build.")]
pub(crate) async fn run_plugin_task(
    _app_state: &AppState,
    _plugin: &str,
    _task: &str,
    _trigger: TaskTrigger,
    _schedule: Option<&str>,
) -> PluginTaskResult {
    PluginTaskResult::Unknown("this build has no plugin support".to_string())
}
