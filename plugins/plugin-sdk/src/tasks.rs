//! Task dispatch for the `tasks` hook: one `run` export, many manifest
//! `tasks:` entries, told apart by `ctx.task-id`.
//!
//! ```ignore
//! impl Tasks for Plugin {
//!     fn run(ctx: TaskContext) -> Result<String, String> {
//!         tasks::Router::new()
//!             .task("refresh-teams", refresh_teams)
//!             .task("prune-cache", prune_cache)
//!             .dispatch(&ctx)
//!     }
//! }
//! fn refresh_teams(ctx: &TaskContext) -> Result<String, String> { /* ... */ }
//! ```

use crate::wit::tasks::TaskContext;

/// A task body. `Ok(summary)` is shown in the UI and the task stats; `Err`
/// publishes `scheduled_task.failed`.
pub type TaskFn = fn(&TaskContext) -> Result<String, String>;

/// Routes a [`TaskContext`] to the task registered under its id.
#[derive(Default)]
pub struct Router {
    tasks: Vec<(&'static str, TaskFn)>,
}

impl Router {
    #[must_use]
    pub fn new() -> Self { Self::default() }

    /// Registers `run` for the manifest task `id`.
    #[must_use]
    pub fn task(mut self, id: &'static str, run: TaskFn) -> Self {
        self.tasks.push((id, run));
        self
    }

    /// Runs the task named by `ctx.task_id`; an unknown id is an error naming
    /// the ids that exist.
    pub fn dispatch(&self, ctx: &TaskContext) -> Result<String, String> {
        match self.tasks.iter().find(|(id, _)| *id == ctx.task_id) {
            Some((_, run)) => run(ctx),
            None => Err(format!(
                "unknown task `{}`; this plugin runs: {}",
                ctx.task_id,
                self.tasks.iter().map(|(id, _)| *id).collect::<Vec<_>>().join(", ")
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(id: &str) -> TaskContext {
        TaskContext { task_id: id.to_string(), scheduled_at: 100, last_success_at: None }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn refresh(ctx: &TaskContext) -> Result<String, String> { Ok(format!("refreshed at {}", ctx.scheduled_at)) }
    fn prune(_: &TaskContext) -> Result<String, String> { Err("disk full".into()) }

    #[test]
    fn tasks_dispatch_by_id() {
        let router = Router::new().task("refresh", refresh).task("prune", prune);
        assert_eq!(router.dispatch(&ctx("refresh")), Ok("refreshed at 100".into()));
        assert_eq!(router.dispatch(&ctx("prune")), Err("disk full".into()));
        let unknown = router.dispatch(&ctx("nope")).unwrap_err();
        assert!(unknown.contains("`nope`") && unknown.contains("refresh, prune"));
    }
}
