use crate::model::ScheduleTaskType;
use serde::{Deserialize, Serialize};

/// A scheduled task that could not complete.
///
/// The playlist update and the library scan both report their own outcomes,
/// so this exists for the tasks that have no terminal event of their own -
/// today that is the `GeoIP` database refresh, which logged one line and moved
/// on. An operator running on a stale `GeoIP` database had no way to find out.
///
/// The task type is the whole payload's discriminant rather than a free
/// string, so a task added to [`ScheduleTaskType`] cannot be reported under a
/// name nothing else recognises.
///
/// A plugin task has no `ScheduleTaskType` of its own (the enum is closed and
/// `Copy`), so it reports `task: PluginTask` plus [`Self::plugin_task`] naming
/// which one. Built-in payloads never carry the field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledTaskFailure {
    pub task: ScheduleTaskType,
    /// The cron expression that triggered it, when the emitter knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    /// `plugin:<id>/<task>` for a [`ScheduleTaskType::PluginTask`], else `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_task: Option<String>,
    pub error: String,
}

impl ScheduledTaskFailure {
    #[must_use]
    pub fn new(task: ScheduleTaskType, error: String) -> Self {
        Self { task, schedule: None, plugin_task: None, error }
    }

    /// A failed run of task `task` of plugin `plugin`.
    #[must_use]
    pub fn for_plugin_task(plugin: &str, task: &str, error: String) -> Self {
        Self {
            task: ScheduleTaskType::PluginTask,
            schedule: None,
            plugin_task: Some(format!("plugin:{plugin}/{task}")),
            error,
        }
    }

    #[must_use]
    pub fn with_schedule(mut self, schedule: String) -> Self {
        self.schedule = Some(schedule);
        self
    }

    /// Per task type, and per plugin task: a task that fails on a schedule
    /// fails the same way every time it runs, but one plugin's failing task
    /// must not hide another's.
    #[must_use]
    pub fn dedup_key(&self) -> String {
        match &self.plugin_task {
            Some(plugin_task) => format!("scheduled-task:{:?}:{plugin_task}", self.task),
            None => format!("scheduled-task:{:?}", self.task),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_built_in_payload_is_unchanged() {
        let failure = ScheduledTaskFailure::new(ScheduleTaskType::GeoIpUpdate, "boom".to_string())
            .with_schedule("0 0 4 * * *".to_string());
        let value = serde_json::to_value(&failure).unwrap();
        assert!(value.get("plugin_task").is_none());
        assert_eq!(value.as_object().unwrap().len(), 3, "task, schedule and error only: {value}");
        assert_eq!(failure.dedup_key(), "scheduled-task:GeoIpUpdate");
        let back: ScheduledTaskFailure = serde_json::from_value(value).unwrap();
        assert_eq!(back, failure);
    }

    #[test]
    fn a_plugin_task_names_itself_and_round_trips() {
        let failure = ScheduledTaskFailure::for_plugin_task("sports", "refresh", "boom".to_string());
        assert_eq!(failure.task, ScheduleTaskType::PluginTask);
        let value = serde_json::to_value(&failure).unwrap();
        assert_eq!(value["plugin_task"], "plugin:sports/refresh");
        assert!(value.get("schedule").is_none());
        let back: ScheduledTaskFailure = serde_json::from_value(value).unwrap();
        assert_eq!(back, failure);
    }

    #[test]
    fn a_payload_without_plugin_task_still_deserializes() {
        let failure: ScheduledTaskFailure = serde_json::from_str(r#"{"task":"GeoIpUpdate","error":"stale"}"#).unwrap();
        assert_eq!(failure.plugin_task, None);
    }

    #[test]
    fn the_dedup_key_separates_plugin_tasks() {
        let a = ScheduledTaskFailure::for_plugin_task("sports", "refresh", "x".to_string());
        let b = ScheduledTaskFailure::for_plugin_task("sports", "prune", "x".to_string());
        let c = ScheduledTaskFailure::new(ScheduleTaskType::PluginTask, "x".to_string());
        assert_eq!(a.dedup_key(), "scheduled-task:PluginTask:plugin:sports/refresh");
        assert_ne!(a.dedup_key(), b.dedup_key());
        assert_ne!(a.dedup_key(), c.dedup_key());
        assert_eq!(a.dedup_key(), a.clone().with_schedule("* * * * * *".to_string()).dedup_key());
    }
}
