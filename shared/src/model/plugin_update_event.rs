use serde::{Deserialize, Serialize};

/// What happened to a marketplace update of an installed plugin. One payload,
/// three event kinds, like [`crate::model::UserLifecycleEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginUpdateOutcome {
    /// A newer release is staged and waits for an admin: it needs approval,
    /// or the plugin's policy keeps it from moving on its own.
    Available,
    /// The new release is active and its hash is pinned.
    Applied,
    /// The update was rejected or its activation rolled back; the previous
    /// package is still the one that runs.
    Failed,
}

/// `plugin.update.available` / `plugin.update.applied` / `plugin.update.failed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginUpdateEvent {
    pub outcome: PluginUpdateOutcome,
    /// The plugin id.
    pub plugin: String,
    /// The installed version before the update.
    pub from: String,
    /// The release the update moves to (or tried to).
    pub to: String,
    /// The marketplace source id the release comes from.
    pub source: String,
    /// Why it waits or failed; absent for `applied`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl PluginUpdateEvent {
    /// One plugin's update to one release is one piece of news per outcome.
    #[must_use]
    pub fn dedup_key(&self) -> String {
        let outcome = match self.outcome {
            PluginUpdateOutcome::Available => "available",
            PluginUpdateOutcome::Applied => "applied",
            PluginUpdateOutcome::Failed => "failed",
        };
        format!("plugin-update:{}:{}:{outcome}", self.plugin, self.to)
    }
}
