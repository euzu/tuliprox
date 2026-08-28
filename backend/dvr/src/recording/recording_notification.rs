//! Recording lifecycle notification payload + routing.
//!
//! This module owns the payload shape, template-field resolution, and routing
//! decision. Shared, administrator-owned private, and legacy administrator
//! events may route to global channels; regular users' private recordings do
//! not.

use shared::model::recording::{RecordingMetadata, RecordingOwner, RecordingVisibility};

/// The lifecycle event the adapter consumes. One per transition
/// (start / completion / failure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleEvent {
    Started,
    Completed,
    Failed,
}

impl LifecycleEvent {
    #[cfg(test)]
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Started => "recording_started",
            Self::Completed => "recording_completed",
            Self::Failed => "recording_failed",
        }
    }
}

/// Template fields the messaging layer can render. Optional fields are `None`
/// when the caller did not supply them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LifecyclePayload {
    pub programme_title: Option<String>,
    pub channel: Option<String>,
    pub effective_start: Option<i64>,
    pub effective_end: Option<i64>,
    pub visibility: Option<RecordingVisibility>,
    pub output_filename: Option<String>,
    /// `Some` for `Failed` events; `None` for `Started` / `Completed`.
    pub failure_reason: Option<String>,
    /// The opaque task id. Used for log correlation; never
    /// serialized in the user-facing template.
    pub task_id: Option<String>,
}

impl LifecyclePayload {
    /// Build the payload from the recording metadata and an optional failure
    /// reason. The `output_filename` exposes only the file-name component
    /// of `meta.relative_path` (when present), so a user-owned
    /// notification template never embeds the owning user's identifier
    /// — paths under a `users/<web:id>/...` layout would otherwise leak
    /// the owner's `UserId` into global notifications.
    pub fn from_metadata(meta: &RecordingMetadata, failure_reason: Option<String>) -> Self {
        let output_filename = meta
            .relative_path
            .as_deref()
            .and_then(|p| std::path::Path::new(p).file_name().and_then(|s| s.to_str()))
            .map(str::to_string);
        Self {
            programme_title: meta.program_title.clone(),
            channel: meta.channel_name.clone().or_else(|| meta.channel_id.clone()),
            effective_start: meta.scheduled_start,
            effective_end: meta.scheduled_end,
            visibility: Some(meta.visibility),
            output_filename,
            failure_reason,
            task_id: None,
        }
    }

    /// Render the placeholder dictionary used by unit tests.
    #[cfg(test)]
    pub fn template_fields(&self) -> Vec<(String, String)> {
        let mut fields: Vec<(String, String)> = Vec::new();
        if let Some(title) = &self.programme_title {
            fields.push(("programme_title".into(), title.clone()));
        }
        if let Some(channel) = &self.channel {
            fields.push(("channel".into(), channel.clone()));
        }
        if let Some(start) = self.effective_start {
            fields.push(("effective_start".into(), start.to_string()));
        }
        if let Some(end) = self.effective_end {
            fields.push(("effective_end".into(), end.to_string()));
        }
        if let Some(visibility) = self.visibility {
            fields.push(("visibility".into(), visibility_wire(visibility).to_string()));
        }
        if let Some(filename) = &self.output_filename {
            fields.push(("output_filename".into(), filename.clone()));
        }
        if let Some(reason) = &self.failure_reason {
            fields.push(("failure_reason".into(), reason.clone()));
        }
        fields
    }
}

#[cfg(test)]
fn visibility_wire(v: RecordingVisibility) -> &'static str {
    match v {
        RecordingVisibility::Private => "private",
        RecordingVisibility::Shared => "shared",
    }
}

/// Routing decision for global notification channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingDecision {
    Deliver,
    Suppress,
}

/// Decide whether the event reaches the global channels.
pub fn route(owner: &RecordingOwner, visibility: RecordingVisibility, is_admin_role: bool) -> RoutingDecision {
    if matches!(visibility, RecordingVisibility::Shared) {
        return RoutingDecision::Deliver;
    }
    match owner {
        RecordingOwner::LegacyAdmin => RoutingDecision::Deliver,
        RecordingOwner::User(_) => {
            if is_admin_role {
                // Administrator's own private recording.
                RoutingDecision::Deliver
            } else {
                // Another regular user's private recording.
                RoutingDecision::Suppress
            }
        }
    }
}

/// The kind to lifecycle-event mapping.
#[cfg(test)]
pub fn kind_for_event(event: LifecycleEvent) -> &'static str {
    event.wire_name()
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::UserId;

    fn user(name: &str) -> UserId {
        UserId::from(name)
    }

    fn make_meta(visibility: RecordingVisibility, owner: RecordingOwner) -> RecordingMetadata {
        RecordingMetadata {
            owner,
            visibility,
            source: None,
            program_start: Some(1_700_000_000),
            program_end: Some(1_700_003_600),
            scheduled_start: Some(1_700_000_000),
            scheduled_end: Some(1_700_003_600),
            pre_roll_secs: 0,
            post_roll_secs: 0,
            channel_id: Some("ch-1".into()),
            channel_name: Some("Channel 1".into()),
            program_title: Some("Programme".into()),
            epg: None,
            provenance: shared::model::recording::RecordingProvenance::default(),
            relative_path: Some("users/web:alice/Programme_2023-11-14_20-00.ts".into()),
            partial_relative_path: None,
            reserved_bytes: 0,
            measured_bytes: 0,
            completed_at: None,
            notification_markers: vec![],
            deleting_previous_state: None,
        }
    }

    #[test]
    fn wire_names_are_stable() {
        assert_eq!(LifecycleEvent::Started.wire_name(), "recording_started");
        assert_eq!(LifecycleEvent::Completed.wire_name(), "recording_completed");
        assert_eq!(LifecycleEvent::Failed.wire_name(), "recording_failed");
    }

    #[test]
    fn template_fields_carry_metadata() {
        let meta = make_meta(RecordingVisibility::Shared, RecordingOwner::User(user("web:alice")));
        let payload = LifecyclePayload::from_metadata(&meta, None);
        let fields = payload.template_fields();
        let get = |k: &str| fields.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("programme_title").as_deref(), Some("Programme"));
        assert_eq!(get("channel").as_deref(), Some("Channel 1"));
        assert_eq!(get("visibility").as_deref(), Some("shared"));
        assert_eq!(get("output_filename").as_deref(), Some("Programme_2023-11-14_20-00.ts"));
        assert!(get("failure_reason").is_none());
    }

    #[test]
    fn failure_reason_only_for_failed_event() {
        let meta = make_meta(RecordingVisibility::Shared, RecordingOwner::User(user("web:alice")));
        let payload = LifecyclePayload::from_metadata(&meta, Some("encoder died".into()));
        let fields = payload.template_fields();
        let get = |k: &str| fields.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("failure_reason").as_deref(), Some("encoder died"));
    }

    #[test]
    fn legacy_admin_owner_always_delivers() {
        let owner = RecordingOwner::LegacyAdmin;
        assert_eq!(route(&owner, RecordingVisibility::Private, false), RoutingDecision::Deliver);
        assert_eq!(route(&owner, RecordingVisibility::Private, true), RoutingDecision::Deliver);
    }

    #[test]
    fn user_owner_administrator_delivers() {
        let owner = RecordingOwner::User(user("builtin:admin"));
        assert_eq!(route(&owner, RecordingVisibility::Private, true), RoutingDecision::Deliver);
    }

    #[test]
    fn user_owner_non_administrator_private_suppresses() {
        let owner = RecordingOwner::User(user("web:alice"));
        assert_eq!(route(&owner, RecordingVisibility::Private, false), RoutingDecision::Suppress);
    }

    #[test]
    fn shared_visibility_always_delivers() {
        let owner = RecordingOwner::User(user("web:alice"));
        assert_eq!(route(&owner, RecordingVisibility::Shared, false), RoutingDecision::Deliver);
    }

    #[test]
    fn kind_for_event_is_stable() {
        assert_eq!(kind_for_event(LifecycleEvent::Started), "recording_started");
        assert_eq!(kind_for_event(LifecycleEvent::Completed), "recording_completed");
        assert_eq!(kind_for_event(LifecycleEvent::Failed), "recording_failed");
    }
}
