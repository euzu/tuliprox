use crate::model::ScheduleTaskType;
use serde::{Deserialize, Serialize};

/// A scheduled task that could not complete.
///
/// The playlist update and the library scan both report their own outcomes,
/// so this exists for the tasks that have no terminal event of their own -
/// today that is the GeoIP database refresh, which logged one line and moved
/// on. An operator running on a stale GeoIP database had no way to find out.
///
/// The task type is the whole payload's discriminant rather than a free
/// string, so a task added to [`ScheduleTaskType`] cannot be reported under a
/// name nothing else recognises.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledTaskFailure {
    pub task: ScheduleTaskType,
    /// The cron expression that triggered it, when the emitter knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    pub error: String,
}

impl ScheduledTaskFailure {
    #[must_use]
    pub fn new(task: ScheduleTaskType, error: String) -> Self { Self { task, schedule: None, error } }

    #[must_use]
    pub fn with_schedule(mut self, schedule: String) -> Self {
        self.schedule = Some(schedule);
        self
    }

    /// Per task type: a task that fails on a schedule fails the same way every
    /// time it runs.
    #[must_use]
    pub fn dedup_key(&self) -> String { format!("scheduled-task:{:?}", self.task) }
}
