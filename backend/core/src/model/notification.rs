//! The notification event envelope.
//!
//! [`TemplateContext`](super::TemplateContext) carries one `Option` field
//! per message kind - `message`, `stats`, `watch`, `processing`, `disk`,
//! `recording`, `flat_stats` - which is why adding a kind used to touch the
//! renderer. [`NotificationEvent`] replaces that with a single shape every
//! event fits, so a channel can render any event without knowing its type.
//!
//! The typed payloads ([`ProcessingStats`], [`DiskAlert`],
//! [`RecordingLifecycleMessage`], [`WatchChanges`]) are unchanged; they now
//! travel serialized in [`NotificationEvent::fields`].

use super::messaging::{MessageContent, RecordingLifecycleMessage};
use chrono::Utc;
use shared::{
    model::{
        notification::{registry, EventId, Severity},
        DiskAlert, MsgKind,
    },
    utils::human_readable_byte_size,
};
use std::{fmt::Write, sync::Arc};

/// One thing that happened, in the shape every channel can render.
///
/// `title` and `body` are always populated, which is what lets Pushover, a
/// syslog channel or an email subject line render an event correctly
/// without a per-kind match - the gap that made Pushover emit raw JSON to
/// phones.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NotificationEvent {
    pub id: EventId,
    pub severity: Severity,
    /// Unix seconds. Matches the outbox's `enqueued_at` representation and
    /// avoids pulling chrono's serde feature into the workspace.
    pub timestamp: i64,
    /// Which tuliprox this came from. Populated from config for multi-host
    /// setups where one chat receives several instances.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<Arc<str>>,
    /// Suppression identity. Two events with the same key inside the
    /// suppression window are the same news told twice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedup_key: Option<String>,
    /// One-line summary. Always present.
    pub title: String,
    /// Plain-text fallback body. Always present.
    pub body: String,
    /// The typed payload, serialized. Templates reach it as `event.fields.*`.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub fields: serde_json::Value,
}

impl NotificationEvent {
    /// Build an event with the registry's default severity for `id`.
    pub fn new(id: EventId, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            id,
            severity: registry::default_severity(id),
            timestamp: Utc::now().timestamp(),
            instance: None,
            dedup_key: None,
            title: title.into(),
            body: body.into(),
            fields: serde_json::Value::Null,
        }
    }

    /// RFC 3339 rendering of [`NotificationEvent::timestamp`], for templates.
    #[must_use]
    pub fn timestamp_rfc3339(&self) -> String {
        chrono::DateTime::from_timestamp(self.timestamp, 0).unwrap_or_else(Utc::now).to_rfc3339()
    }

    #[must_use]
    pub fn with_severity(mut self, severity: Severity) -> Self {
        self.severity = severity;
        self
    }

    /// Set the suppression identity. See [`NotificationEvent::dedup_key`].
    #[must_use]
    pub fn with_dedup_key(mut self, key: impl Into<String>) -> Self {
        self.dedup_key = Some(key.into());
        self
    }

    #[must_use]
    pub fn with_instance(mut self, instance: Option<Arc<str>>) -> Self {
        self.instance = instance;
        self
    }

    /// Attach the typed payload. A payload that fails to serialize is
    /// dropped rather than failing the event - `title` and `body` already
    /// carry enough for every channel to render something useful.
    #[must_use]
    pub fn with_fields<T: serde::Serialize>(mut self, payload: &T) -> Self {
        self.fields = serde_json::to_value(payload).unwrap_or(serde_json::Value::Null);
        self
    }

    /// Lift a legacy [`MessageContent`] into the envelope.
    ///
    /// Every existing emitter constructs `MessageContent`, so this is the
    /// bridge that lets the new pipeline carry them unchanged.
    #[must_use]
    pub fn from_content(content: &MessageContent) -> Self {
        let id = event_id_for(content);
        let title = title_for(content);
        let body = body_for(content);
        let mut event = Self::new(id, title, body);
        event.fields = fields_for(content);
        event
    }
}

/// Canonical id for a legacy message.
#[must_use]
pub fn event_id_for(content: &MessageContent) -> EventId {
    match content {
        MessageContent::Info(_) => registry::SYSTEM_INFO,
        MessageContent::Error(_) => registry::SYSTEM_ERROR,
        MessageContent::Watch(_) => registry::PLAYLIST_WATCH_CHANGED,
        MessageContent::ProcessingStats(stats) => {
            // Matches the legacy `MessageContent::kind` split: errors with no
            // stats are a failed update, anything else is a completed one.
            if stats.errors.is_some() && stats.stats.is_none() {
                registry::PLAYLIST_UPDATE_FAILED
            } else {
                registry::PLAYLIST_UPDATE_COMPLETED
            }
        }
        MessageContent::DiskAlert(_) => registry::SYSTEM_DISK_ALERT,
        MessageContent::RecordingLifecycle(recording) => match recording.event {
            MsgKind::RecordingStarted => registry::RECORDING_STARTED,
            MsgKind::RecordingCompleted => registry::RECORDING_COMPLETED,
            MsgKind::RecordingFailed => registry::RECORDING_FAILED,
            _ => registry::SYSTEM_INFO,
        },
    }
}

/// Human-readable one-liner for a disk alert.
#[must_use]
pub fn disk_alert_text(alert: &DiskAlert) -> String {
    format!(
        "Disk usage {}: {:.1}% used ({} of {}), {} free.",
        alert.level,
        alert.percent,
        human_readable_byte_size(alert.used_bytes),
        human_readable_byte_size(alert.total_bytes),
        human_readable_byte_size(alert.free_bytes),
    )
}

/// Label for a recording lifecycle event.
#[must_use]
pub fn recording_label(event: MsgKind) -> &'static str {
    match event {
        MsgKind::RecordingStarted => "Recording started",
        MsgKind::RecordingCompleted => "Recording completed",
        MsgKind::RecordingFailed => "Recording failed",
        _ => "Recording lifecycle event",
    }
}

/// Human-readable one-liner for a recording lifecycle event.
#[must_use]
pub fn recording_lifecycle_text(recording: &RecordingLifecycleMessage) -> String {
    let label = recording_label(recording.event);
    let title = recording.programme_title.as_deref().unwrap_or("Untitled");
    let channel = recording.channel.as_deref().unwrap_or("unknown channel");
    match recording.failure_reason.as_deref() {
        Some(reason) => format!("{label}: {title} on {channel} ({reason})"),
        None => format!("{label}: {title} on {channel}"),
    }
}

/// Short summary line - the subject line of the event.
fn title_for(content: &MessageContent) -> String {
    match content {
        MessageContent::Info(s) | MessageContent::Error(s) => first_line(s),
        MessageContent::Watch(w) => {
            format!("{} channel(s) added, {} removed in {}/{}", w.added.len(), w.removed.len(), w.target, w.group)
        }
        MessageContent::ProcessingStats(stats) => match (&stats.stats, &stats.errors) {
            (None, Some(_)) => "Playlist update failed".to_string(),
            (Some(sources), _) => {
                let inputs: usize = sources.iter().map(|s| s.inputs.len()).sum();
                let targets: usize = sources.iter().map(|s| s.targets.len()).sum();
                format!("Playlist update finished: {inputs} input(s), {targets} target(s)")
            }
            (None, None) => "Playlist update finished".to_string(),
        },
        MessageContent::DiskAlert(alert) => {
            format!("Disk usage {}: {:.1}% used", alert.level, alert.percent)
        }
        MessageContent::RecordingLifecycle(recording) => {
            let label = recording_label(recording.event);
            match recording.programme_title.as_deref() {
                Some(title) => format!("{label}: {title}"),
                None => label.to_string(),
            }
        }
    }
}

/// The plain-text rendering used when a channel has no template.
///
/// Preserves the legacy `default_text_for` output for the string and disk
/// variants, so a channel that was already readable stays byte-identical.
fn body_for(content: &MessageContent) -> String {
    match content {
        MessageContent::Info(s) | MessageContent::Error(s) => s.clone(),
        MessageContent::Watch(w) => {
            let mut out = format!("Watched group {}/{} changed.", w.target, w.group);
            if !w.added.is_empty() {
                out.push_str("\n\nAdded:\n");
                out.push_str(&w.added.join("\n"));
            }
            if !w.removed.is_empty() {
                out.push_str("\n\nRemoved:\n");
                out.push_str(&w.removed.join("\n"));
            }
            out
        }
        MessageContent::ProcessingStats(stats) => {
            let mut out = String::new();
            if let Some(sources) = &stats.stats {
                for source in sources {
                    for input in &source.inputs {
                        let _ = writeln!(
                            out,
                            "{} ({:?}): {} group(s), {} channel(s), {} error(s)",
                            input.name,
                            input.input_type,
                            input.processed_stats.group_count,
                            input.processed_stats.channel_count,
                            input.error_count,
                        );
                    }
                }
            }
            if let Some(errors) = &stats.errors {
                out.push_str("\nErrors:\n");
                out.push_str(errors);
            }
            if out.trim().is_empty() {
                "Playlist update finished.".to_string()
            } else {
                out.trim_end().to_string()
            }
        }
        MessageContent::DiskAlert(alert) => disk_alert_text(alert),
        MessageContent::RecordingLifecycle(recording) => recording_lifecycle_text(recording),
    }
}

/// The typed payload, serialized for template access.
fn fields_for(content: &MessageContent) -> serde_json::Value {
    let value = match content {
        MessageContent::Info(s) | MessageContent::Error(s) => serde_json::to_value(s),
        MessageContent::Watch(w) => serde_json::to_value(w),
        MessageContent::ProcessingStats(ps) => serde_json::to_value(ps),
        MessageContent::DiskAlert(alert) => serde_json::to_value(alert),
        MessageContent::RecordingLifecycle(recording) => serde_json::to_value(recording),
    };
    value.unwrap_or(serde_json::Value::Null)
}

/// Telegram and Pushover both truncate hard; a title longer than this is
/// never useful and pushes the body out of the preview.
const MAX_TITLE_CHARS: usize = 120;

fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or(s).trim();
    if line.chars().count() > MAX_TITLE_CHARS {
        let truncated: String = line.chars().take(MAX_TITLE_CHARS - 1).collect();
        format!("{truncated}\u{2026}")
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{super::messaging::ProcessingStats, *};
    use shared::model::{DiskAlertLevel, InputStats, InputType, PlaylistStats, SourceStats, TargetStats};

    fn stats_content() -> MessageContent {
        let input = InputStats {
            name: "Provider A".to_string(),
            input_type: InputType::M3u,
            error_count: 2,
            raw_stats: PlaylistStats { group_count: 100, channel_count: 1000 },
            processed_stats: PlaylistStats { group_count: 50, channel_count: 500 },
            secs_took: 12,
        };
        MessageContent::ProcessingStats(ProcessingStats::new_stats(vec![SourceStats {
            inputs: vec![input],
            targets: vec![TargetStats::success("Target 1")],
        }]))
    }

    #[test]
    fn legacy_kinds_map_onto_the_canonical_ids() {
        assert_eq!(event_id_for(&MessageContent::Info(String::new())), registry::SYSTEM_INFO);
        assert_eq!(event_id_for(&MessageContent::Error(String::new())), registry::SYSTEM_ERROR);
        assert_eq!(event_id_for(&stats_content()), registry::PLAYLIST_UPDATE_COMPLETED);
    }

    #[test]
    fn processing_stats_with_only_errors_is_a_failed_update() {
        // Mirrors the legacy `MessageContent::kind` split, which reported
        // `MsgKind::Error` for a stats message carrying only errors.
        let content = MessageContent::ProcessingStats(ProcessingStats::new_error("boom".to_string()));
        assert_eq!(event_id_for(&content), registry::PLAYLIST_UPDATE_FAILED);
    }

    #[test]
    fn every_event_has_a_non_empty_title_and_body() {
        // The whole point of the envelope: a channel with no template can
        // always render something a human can read.
        let contents = vec![
            MessageContent::Info("hello".to_string()),
            MessageContent::Error("bang".to_string()),
            stats_content(),
            MessageContent::ProcessingStats(ProcessingStats::new_error("boom".to_string())),
            MessageContent::DiskAlert(DiskAlert {
                level: DiskAlertLevel::Critical,
                total_bytes: 1_000_000,
                free_bytes: 50_000,
                used_bytes: 950_000,
                percent: 95.0,
            }),
            MessageContent::RecordingLifecycle(RecordingLifecycleMessage {
                event: MsgKind::RecordingCompleted,
                programme_title: Some("The Programme".to_string()),
                channel: Some("Channel 1".to_string()),
                effective_start: None,
                effective_end: None,
                visibility: None,
                output_filename: None,
                failure_reason: None,
            }),
        ];
        for content in contents {
            let event = NotificationEvent::from_content(&content);
            assert!(!event.title.trim().is_empty(), "empty title for {:?}", event.id);
            assert!(!event.body.trim().is_empty(), "empty body for {:?}", event.id);
        }
    }

    #[test]
    fn watch_body_lists_added_and_removed_instead_of_dumping_json() {
        // Regression: the legacy default text for a watch change was a raw
        // `serde_json` dump, which is what Pushover received.
        let content = MessageContent::Watch(super::super::messaging::WatchChanges {
            target: "T".to_string(),
            group: "G".to_string(),
            added: vec!["Channel A".to_string()],
            removed: vec!["Channel B".to_string()],
        });
        let event = NotificationEvent::from_content(&content);
        assert!(event.body.contains("Channel A"), "body missing added channel: {}", event.body);
        assert!(event.body.contains("Channel B"), "body missing removed channel: {}", event.body);
        assert!(!event.body.starts_with('{'), "body is still a json dump: {}", event.body);
    }

    #[test]
    fn severity_defaults_come_from_the_registry() {
        let failed = NotificationEvent::from_content(&MessageContent::ProcessingStats(ProcessingStats::new_error(
            "boom".to_string(),
        )));
        assert_eq!(failed.severity, Severity::Error);
        let ok = NotificationEvent::from_content(&stats_content());
        assert_eq!(ok.severity, Severity::Info);
    }

    #[test]
    fn fields_carry_the_typed_payload_for_templates() {
        let event = NotificationEvent::from_content(&stats_content());
        let rendered = event.fields.to_string();
        assert!(rendered.contains("Provider A"), "typed payload lost: {rendered}");
    }

    #[test]
    fn a_long_info_title_is_truncated_to_one_line() {
        let long = format!("{}\nsecond line", "x".repeat(500));
        let event = NotificationEvent::from_content(&MessageContent::Info(long));
        assert!(event.title.chars().count() <= 120, "title not truncated: {}", event.title.len());
        assert!(!event.title.contains('\n'), "title spans lines");
        // The body keeps everything.
        assert!(event.body.contains("second line"));
    }

    #[test]
    fn builders_override_severity_and_dedup_key() {
        let event = NotificationEvent::new(registry::SYSTEM_INFO, "t", "b")
            .with_severity(Severity::Critical)
            .with_dedup_key("disk:critical");
        assert_eq!(event.severity, Severity::Critical);
        assert_eq!(event.dedup_key.as_deref(), Some("disk:critical"));
    }

    #[test]
    fn envelope_round_trips_through_json() {
        let event = NotificationEvent::from_content(&stats_content()).with_dedup_key("k");
        let bytes = serde_json::to_vec(&event).expect("serialize");
        let restored: NotificationEvent = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(restored.id, event.id);
        assert_eq!(restored.title, event.title);
        assert_eq!(restored.dedup_key, event.dedup_key);
    }
}
