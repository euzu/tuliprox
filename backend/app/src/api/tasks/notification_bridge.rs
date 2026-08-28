//! Bridges the in-process event bus onto the notification pipeline.
//!
//! `EventMessage` already carries fourteen variants from thirteen emitters -
//! playlist updates, config changes, library scans, user connections,
//! metadata updates, recording changes - and every one of them reached the
//! Web UI over the websocket and nowhere else. Meanwhile the notification
//! side had six emitters of its own.
//!
//! One subscriber closes that gap: every bus event becomes notifiable, and
//! every future `EventMessage` variant comes along for free.
//!
//! Three things keep it from being a firehose:
//!
//! * high-frequency variants (progress ticks, download deltas, periodic
//!   system info) map to terminal events only, or to nothing;
//! * everything defaults to unsubscribed, so an upgrade does not silently
//!   start messaging people;
//! * the broadcast channel has capacity 10, so a slow subscriber must
//!   survive `Lagged` rather than dying and taking the bridge with it.

use crate::api::model::AppState;
use log::{debug, warn};
use shared::model::{EventKind, EventKindMask, EventMessage};
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use tuliprox_core::model::{MessageContent, NotificationEvent};

/// Subscribe to the event bus and forward what the config asks for.
pub fn spawn_notification_bridge(app_state: &Arc<AppState>, cancel_token: &CancellationToken) {
    let mut receiver = app_state.event_manager.subscribe_filtered(NOTIFIABLE_KINDS);
    let app_state = Arc::clone(app_state);
    let cancel_token = cancel_token.clone();
    tokio::spawn(async move {
        loop {
            let message = tokio::select! {
                () = cancel_token.cancelled() => break,
                message = receiver.recv() => message,
            };
            match message {
                Ok(message) => {
                    if let Some(event) = to_notification(&message) {
                        let client = app_state.http_client.load();
                        tuliprox_messaging::send_event(&app_state.app_config, &client, event).await;
                    }
                }
                // Capacity is 10. Falling behind must not kill the bridge -
                // report the gap and keep going.
                Err(RecvError::Lagged(skipped)) => {
                    app_state.event_manager.stats().record_lag(skipped);
                    warn!("Notification bridge fell behind and skipped {skipped} event(s)");
                }
                Err(RecvError::Closed) => break,
            }
        }
        debug!("Notification bridge stopped");
    });
}

/// The kinds this bridge can turn into a notification.
///
/// The four high-frequency kinds are deliberately absent rather than
/// filtered inside `to_notification`: excluding them here means the bus
/// never wakes this task for a progress tick at all, which under a playlist
/// refresh is most of the traffic on the channel.
///
/// Keep in step with the `None` arms below - the test at the bottom of this
/// file asserts they agree.
const NOTIFIABLE_KINDS: EventKindMask = EventKindMask::new()
    .with(EventKind::DiskAlert)
    .with(EventKind::ConfigReloadFailed)
    .with(EventKind::PlaylistWatchChanged)
    .with(EventKind::ProviderAccountStatus)
    .with(EventKind::ProviderAccountExpiring)
    .with(EventKind::ProviderAccountExpired)
    .with(EventKind::ServerError)
    .with(EventKind::PlaylistUpdate)
    .with(EventKind::ConfigChange)
    .with(EventKind::LibraryScanProgress)
    .with(EventKind::InputMetadataUpdatesStarted)
    .with(EventKind::InputMetadataUpdatesCompleted)
    .with(EventKind::ActiveUser)
    .with(EventKind::ActiveProvider)
    .with(EventKind::RecordingChanged)
    .with(EventKind::RecordingRulesChanged);

/// Map a bus event onto a notification, or `None` to ignore it.
///
/// `None` is the right answer for the high-frequency variants: progress
/// ticks and download deltas fire many times per operation and carry
/// nothing an operator wants pushed to a phone. Their terminal counterparts
/// are what get through.
// Two arms return `None` for different reasons - one because delivery is
// owned by the durable recording path, one because the event declared itself
// non-notifiable. Merging them would collapse that distinction into a list of
// variants with no way to tell which is which.
#[allow(clippy::match_same_arms)]
#[must_use]
pub fn to_notification(message: &EventMessage) -> Option<NotificationEvent> {
    // Which notification an event *is* belongs to the event; this function
    // only decides how to word it and what to attach. `None` here means the
    // event declared itself non-notifiable, so there is no second table to
    // keep in step.
    let id = message.notification_id()?;
    match message {
        EventMessage::ServerError(error) => {
            Some(NotificationEvent::new(id, first_line(error), error.clone()).with_severity(message.severity()))
        }

        EventMessage::PlaylistUpdate(state) => {
            let title = format!("Playlist update finished: {state:?}");
            Some(NotificationEvent::new(id, title.clone(), title).with_severity(message.severity()).with_fields(state))
        }

        EventMessage::ConfigChange(config_type) => {
            let title = format!("Configuration reloaded: {config_type}");
            Some(
                NotificationEvent::new(id, title.clone(), title)
                    // One reload of the same file is one piece of news, however
                    // many times the watcher fires for it.
                    .with_dedup_key(format!("config:{config_type}"))
                    .with_fields(&ConfigTypeField { config_type: config_type.to_string() }),
            )
        }

        EventMessage::LibraryScanProgress(event) => {
            // Progress ticks are not news; the finished scan is.
            let summary = &event.summary;
            let title = "Library scan finished".to_string();
            Some(NotificationEvent::new(id, title.clone(), title).with_fields(summary))
        }

        EventMessage::InputMetadataUpdatesStarted(input) => {
            let title = format!("Metadata update started for {input}");
            Some(NotificationEvent::new(id, title.clone(), title))
        }
        EventMessage::InputMetadataUpdatesCompleted(input) => {
            let title = format!("Metadata update finished for {input}");
            Some(NotificationEvent::new(id, title.clone(), title))
        }

        EventMessage::ActiveUser(change) => {
            let title = "User connection changed".to_string();
            Some(NotificationEvent::new(id, title.clone(), title).with_fields(change))
        }
        EventMessage::ActiveProvider(name, count) => {
            let title = format!("Provider {name} now has {count} active connection(s)");
            Some(NotificationEvent::new(id, title.clone(), title))
        }

        EventMessage::RecordingChanged => {
            let title = "Recording queue changed".to_string();
            Some(NotificationEvent::new(id, title.clone(), title))
        }
        EventMessage::RecordingRulesChanged => {
            let title = "Recording rules changed".to_string();
            Some(NotificationEvent::new(id, title.clone(), title))
        }

        // The three that already have a `MessageContent` shape reuse it:
        // `from_content` is what built their title, body and template fields
        // before, and rebuilding that here by hand would be a second copy
        // free to drift from the templates that render it.
        EventMessage::DiskAlert(alert) => {
            Some(NotificationEvent::from_content(&MessageContent::DiskAlert(alert.clone())))
        }
        EventMessage::PlaylistWatchChanged(changes) => {
            Some(NotificationEvent::from_content(&MessageContent::Watch(changes.clone())))
        }
        // Deliberately not notified from here. Recording notifications are
        // at-most-once: a durable marker is persisted inside the
        // queue-mutation boundary *before* delivery, and the outbox retries
        // per channel. The bus is a lossy broadcast - a lagging subscriber
        // misses events - so routing that path through it would let a
        // recording be marked delivered and then never sent. The event is on
        // the bus for plugins and subscribers; `download_api` still owns
        // operator delivery.
        EventMessage::RecordingLifecycle(_) => None,

        EventMessage::ConfigReloadFailed(failure) => {
            let title = format!("Configuration reload failed: {}", failure.paths);
            Some(
                NotificationEvent::new(id, title, failure.error.clone())
                    .with_severity(message.severity())
                    .with_fields(failure),
            )
        }

        EventMessage::ProviderAccount(event) => Some(
            NotificationEvent::new(id, event.message.clone(), event.message.clone())
                .with_severity(message.severity())
                // Re-evaluated on every playlist refresh; without this an
                // expiring account notifies on each one for three days.
                .with_dedup_key(event.dedup_key())
                .with_fields(event),
        ),

        // Unreachable: `notification_id` already returned `None` for these,
        // which is the single place that decision is made.
        EventMessage::PlaylistUpdateProgress(_)
        | EventMessage::SystemInfoUpdate(_)
        | EventMessage::DownloadsUpdate(_)
        | EventMessage::DownloadsDeltaUpdate(_) => None,
    }
}

/// `ConfigType` is not `Serialize`, so the template payload carries its
/// display form.
#[derive(serde::Serialize)]
struct ConfigTypeField {
    config_type: String,
}

fn first_line(s: &str) -> String { s.lines().next().unwrap_or(s).trim().to_string() }

#[cfg(test)]
mod tests {
    use super::{to_notification, EventMessage, NOTIFIABLE_KINDS};
    use shared::model::{
        notification::{registry, Severity},
        ActiveUserConnectionChange, ConfigReloadFailure, ConfigType, DiskAlert, DiskAlertLevel, DownloadsResponse,
        EventKind, LibraryScanProgressEvent, LibraryScanSummary, LibraryScanSummaryStatus, MsgKind,
        PlaylistUpdateProgressEvent, PlaylistUpdateState, ProviderAccountEvent, ProviderAccountState,
        RecordingLifecycleMessage, SystemInfo, WatchChanges,
    };
    use std::sync::Arc;

    /// The subscription mask and the mapping must agree. If they drift, the
    /// bridge either wakes for events it will drop, or - worse - silently
    /// stops delivering a kind that `to_notification` still handles.
    #[test]
    fn notifiable_mask_matches_the_mapped_variants() {
        for (message, kind) in sample_of_every_kind() {
            let mapped = to_notification(&message).is_some();
            assert_eq!(
                NOTIFIABLE_KINDS.contains(kind),
                mapped,
                "{kind:?}: mask says {}, to_notification says {mapped}",
                NOTIFIABLE_KINDS.contains(kind),
            );
        }
    }

    /// One `EventMessage` per `EventKind`, so the test above cannot miss a
    /// variant added later.
    fn sample_of_every_kind() -> Vec<(EventMessage, EventKind)> {
        let empty_downloads = DownloadsResponse { queue: Vec::new(), finished: Vec::new(), active: Vec::new() };
        let samples = vec![
            EventMessage::ServerError("x".to_string()),
            EventMessage::ActiveUser(ActiveUserConnectionChange::Connections(0, 0)),
            EventMessage::ActiveProvider("p".into(), 1),
            EventMessage::ConfigChange(ConfigType::Config),
            EventMessage::PlaylistUpdate(PlaylistUpdateState::Success),
            EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent {
                target: String::new(),
                message: String::new(),
            }),
            EventMessage::SystemInfoUpdate(Arc::new(SystemInfo {
                cpu_usage: 0.0,
                memory_usage: 0,
                memory_total: 0,
                net_rx_bytes_per_sec: 0.0,
                net_tx_bytes_per_sec: 0.0,
                net_rx_bytes_total: 0,
                net_tx_bytes_total: 0,
                disk_total_bytes: 0,
                disk_free_bytes: 0,
            })),
            EventMessage::LibraryScanProgress(LibraryScanProgressEvent {
                summary: LibraryScanSummary {
                    status: LibraryScanSummaryStatus::Success,
                    message: String::new(),
                    result: None,
                },
            }),
            EventMessage::DownloadsUpdate(Arc::new(empty_downloads)),
            EventMessage::DownloadsDeltaUpdate(shared::model::TransfersDelta::ActiveCleared),
            EventMessage::RecordingChanged,
            EventMessage::RecordingRulesChanged,
            EventMessage::InputMetadataUpdatesStarted("a".into()),
            EventMessage::InputMetadataUpdatesCompleted("a".into()),
            EventMessage::DiskAlert(DiskAlert {
                level: DiskAlertLevel::Warn,
                total_bytes: 100,
                free_bytes: 5,
                used_bytes: 95,
                percent: 95.0,
            }),
            EventMessage::ConfigReloadFailed(ConfigReloadFailure {
                paths: "config.yml".to_string(),
                error: "boom".to_string(),
            }),
            EventMessage::PlaylistWatchChanged(WatchChanges {
                target: "t".to_string(),
                group: "g".to_string(),
                added: Vec::new(),
                removed: Vec::new(),
            }),
            recording_lifecycle(MsgKind::RecordingStarted),
            recording_lifecycle(MsgKind::RecordingCompleted),
            recording_lifecycle(MsgKind::RecordingFailed),
            provider_account(ProviderAccountState::StatusChanged),
            provider_account(ProviderAccountState::Expiring),
            provider_account(ProviderAccountState::Expired),
        ];
        assert_eq!(samples.len(), EventKind::ALL.len(), "add the new variant to this list");
        samples
            .into_iter()
            .map(|message| {
                let kind = message.kind();
                (message, kind)
            })
            .collect()
    }

    #[test]
    fn high_frequency_variants_are_not_notifiable() {
        // Progress ticks and download deltas fire many times per operation;
        // their terminal counterparts are what reaches a channel. Guarded by
        // the exhaustive match in `to_notification`, so a new variant is a
        // compile error rather than a silent firehose.
        assert!(to_notification(&EventMessage::DownloadsDeltaUpdate(shared::model::TransfersDelta::ActiveCleared))
            .is_none());
    }

    #[test]
    fn a_failed_playlist_update_maps_to_the_failure_event_at_error_severity() {
        let event = to_notification(&EventMessage::PlaylistUpdate(PlaylistUpdateState::Failure)).expect("mapped");
        assert_eq!(event.id, registry::PLAYLIST_UPDATE_FAILED);
        assert_eq!(event.severity, Severity::Error);
    }

    #[test]
    fn a_partial_playlist_update_is_a_warning_not_a_success() {
        let event = to_notification(&EventMessage::PlaylistUpdate(PlaylistUpdateState::Partial)).expect("mapped");
        assert_eq!(event.id, registry::PLAYLIST_UPDATE_COMPLETED);
        assert_eq!(event.severity, Severity::Warn, "a partial update must not read as a clean success");
    }

    #[test]
    fn a_successful_playlist_update_is_informational() {
        let event = to_notification(&EventMessage::PlaylistUpdate(PlaylistUpdateState::Success)).expect("mapped");
        assert_eq!(event.id, registry::PLAYLIST_UPDATE_COMPLETED);
        assert_eq!(event.severity, Severity::Info);
    }

    #[test]
    fn config_changes_dedup_per_file_so_a_watcher_burst_is_one_notification() {
        let event = to_notification(&EventMessage::ConfigChange(ConfigType::Mapping)).expect("mapped");
        assert_eq!(event.id, registry::CONFIG_CHANGED);
        assert_eq!(event.dedup_key.as_deref(), Some("config:Mapping"));
    }

    #[test]
    fn different_config_files_do_not_dedup_against_each_other() {
        let mapping = to_notification(&EventMessage::ConfigChange(ConfigType::Mapping)).expect("mapped");
        let sources = to_notification(&EventMessage::ConfigChange(ConfigType::Sources)).expect("mapped");
        assert_ne!(mapping.dedup_key, sources.dedup_key);
    }

    #[test]
    fn server_errors_map_to_the_error_event() {
        let event = to_notification(&EventMessage::ServerError("bang\nsecond line".to_string())).expect("mapped");
        assert_eq!(event.id, registry::SYSTEM_ERROR);
        assert_eq!(event.severity, Severity::Error);
        assert_eq!(event.title, "bang", "title should be the first line only");
        assert!(event.body.contains("second line"), "body should keep everything");
    }

    #[test]
    fn every_mapped_event_has_a_title_and_body() {
        let messages = vec![
            EventMessage::ServerError("x".to_string()),
            EventMessage::PlaylistUpdate(PlaylistUpdateState::Success),
            EventMessage::ConfigChange(ConfigType::Config),
            EventMessage::InputMetadataUpdatesStarted("input-a".into()),
            EventMessage::InputMetadataUpdatesCompleted("input-a".into()),
            EventMessage::ActiveProvider("provider-a".into(), 3),
            EventMessage::RecordingChanged,
            EventMessage::RecordingRulesChanged,
        ];
        for message in messages {
            let event = to_notification(&message).expect("mapped");
            assert!(!event.title.trim().is_empty(), "empty title for {:?}", event.id);
            assert!(!event.body.trim().is_empty(), "empty body for {:?}", event.id);
        }
    }

    #[test]
    fn every_mapped_event_id_is_registered() {
        // An unregistered id can never be matched by a `notify_on` pattern,
        // so the bridge would emit into the void.
        let messages = vec![
            EventMessage::ServerError("x".to_string()),
            EventMessage::PlaylistUpdate(PlaylistUpdateState::Failure),
            EventMessage::ConfigChange(ConfigType::Config),
            EventMessage::InputMetadataUpdatesStarted("a".into()),
            EventMessage::InputMetadataUpdatesCompleted("a".into()),
            EventMessage::ActiveProvider("a".into(), 1),
            EventMessage::RecordingChanged,
            EventMessage::RecordingRulesChanged,
        ];
        for message in messages {
            let event = to_notification(&message).expect("mapped");
            assert!(registry::describe(event.id).is_some(), "unregistered id {}", event.id);
        }
    }

    fn recording_lifecycle(event: MsgKind) -> EventMessage {
        EventMessage::RecordingLifecycle(RecordingLifecycleMessage {
            event,
            programme_title: None,
            channel: None,
            effective_start: None,
            effective_end: None,
            visibility: None,
            output_filename: None,
            failure_reason: None,
        })
    }

    fn provider_account(state: ProviderAccountState) -> EventMessage {
        EventMessage::ProviderAccount(ProviderAccountEvent {
            state,
            username: "u".to_string(),
            provider: "p".to_string(),
            status: None,
            expires_at: None,
            message: "m".to_string(),
        })
    }
}
