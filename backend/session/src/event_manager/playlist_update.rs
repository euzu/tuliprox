use shared::model::{
    EventMessage, PlaylistUpdateProgressEvent, PlaylistUpdateRunId, PlaylistUpdateRunOrder, PlaylistUpdateState,
};
use std::collections::{hash_map::Entry, HashMap};

struct InputUpdate {
    run_id: PlaylistUpdateRunId,
    order: PlaylistUpdateRunOrder,
    progress: Option<PlaylistUpdateProgressEvent>,
}

/// Latest active facts per input, not a log. Completion drops the payload;
/// only its identity/order remain to reject late events from that execution.
#[derive(Default)]
pub(super) struct PlaylistUpdates {
    inputs: HashMap<u16, InputUpdate>,
}

impl PlaylistUpdates {
    pub(super) fn record(&mut self, event: &EventMessage) {
        match event {
            EventMessage::PlaylistUpdateProgress(progress) => {
                let (Some(run_id), Some(order), Some(input_id)) =
                    (&progress.run_id, progress.execution_order, progress.input_id)
                else {
                    return;
                };
                match self.inputs.entry(input_id) {
                    Entry::Vacant(entry) => {
                        entry.insert(InputUpdate { run_id: run_id.clone(), order, progress: Some(progress.clone()) });
                    }
                    Entry::Occupied(mut entry) => {
                        let current = entry.get_mut();
                        if order > current.order {
                            *current = InputUpdate { run_id: run_id.clone(), order, progress: Some(progress.clone()) };
                        } else if order == current.order && *run_id == current.run_id {
                            if let Some(previous) = current.progress.as_mut() {
                                let mut next = progress.clone();
                                // A later text-only/reuse event is not a new acquisition
                                // and must not erase proven facts or a stronger result.
                                next.state = match (previous.state, next.state) {
                                    (Some(PlaylistUpdateState::Failure), _)
                                    | (_, Some(PlaylistUpdateState::Failure)) => Some(PlaylistUpdateState::Failure),
                                    (Some(PlaylistUpdateState::Partial), _)
                                    | (_, Some(PlaylistUpdateState::Partial)) => Some(PlaylistUpdateState::Partial),
                                    (Some(PlaylistUpdateState::Success), _)
                                    | (_, Some(PlaylistUpdateState::Success)) => Some(PlaylistUpdateState::Success),
                                    (None, None) => None,
                                };
                                next.input_telemetry =
                                    next.input_telemetry.or_else(|| previous.input_telemetry.clone());
                                next.library_scan_result =
                                    next.library_scan_result.or_else(|| previous.library_scan_result.clone());
                                *previous = next;
                            }
                        }
                    }
                }
            }
            EventMessage::PlaylistUpdate(completed) => {
                if let (Some(run_id), Some(order)) = (&completed.run_id, completed.execution_order) {
                    for input in self.inputs.values_mut() {
                        if input.run_id == *run_id && input.order == order {
                            input.progress = None;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn snapshot(&self) -> Vec<PlaylistUpdateProgressEvent> {
        let mut progress: Vec<_> = self.inputs.values().filter_map(|input| input.progress.clone()).collect();
        progress.sort_by_key(|event| (event.execution_order, event.input_id));
        progress
    }
}

#[cfg(test)]
mod tests {
    use crate::EventManager;
    use shared::model::{
        EventMessage, InputRefreshPolicy, LibraryScanResult, PlaylistUpdateDataSource, PlaylistUpdateInputTelemetry,
        PlaylistUpdateProgressEvent, PlaylistUpdateState, PlaylistUpdateSummary,
    };

    fn progress(run: &str, order: u64, input: u16) -> PlaylistUpdateProgressEvent {
        PlaylistUpdateProgressEvent::for_run_input(run.into(), order.into(), input, "input", "working")
    }

    fn completed(run: &str, order: u64) -> EventMessage {
        EventMessage::PlaylistUpdate(PlaylistUpdateSummary::for_run(
            run.into(),
            order.into(),
            PlaylistUpdateState::Success,
        ))
    }

    #[test]
    fn playlist_update_status_snapshot_survives_late_subscription_until_correlated_run_completion() {
        let manager = EventManager::new();
        let mut own = progress("r1", 1, 7);
        manager.send_event(EventMessage::PlaylistUpdateProgress(own.clone()));
        assert_eq!(manager.playlist_update_snapshot(), vec![own.clone()]);
        own.state = Some(PlaylistUpdateState::Partial);
        manager.send_event(EventMessage::PlaylistUpdateProgress(own.clone()));
        assert_eq!(
            manager.playlist_update_snapshot(),
            vec![own.clone()],
            "input completion does not finish target rebuild"
        );
        manager.send_event(completed("unrelated", 1));
        manager.send_event(completed("r1", 2));
        assert_eq!(manager.playlist_update_snapshot(), vec![own]);
        manager.send_event(completed("r1", 1));
        assert!(manager.playlist_update_snapshot().is_empty());
        manager.send_event(EventMessage::PlaylistUpdateProgress(progress("r1", 1, 7)));
        assert!(manager.playlist_update_snapshot().is_empty(), "late progress cannot resurrect a finished input/run");
        let next = progress("r2", 2, 7);
        manager.send_event(EventMessage::PlaylistUpdateProgress(next.clone()));
        assert_eq!(manager.playlist_update_snapshot(), vec![next]);
        assert!(EventManager::new().playlist_update_snapshot().is_empty(), "restart retains no transient update");
    }

    #[test]
    fn playlist_update_status_snapshot_is_input_scoped_ordered_and_not_a_progress_log() {
        let manager = EventManager::new();
        for input in [9, 7, 8] {
            for _ in 0..300 {
                manager.send_event(EventMessage::PlaylistUpdateProgress(progress("r1", 1, input)));
            }
        }
        assert_eq!(
            manager.playlist_update_snapshot().iter().map(|event| event.input_id).collect::<Vec<_>>(),
            vec![Some(7), Some(8), Some(9)]
        );
        manager.send_event(EventMessage::PlaylistUpdateProgress(progress("r2", 2, 7)));
        manager.send_event(EventMessage::PlaylistUpdateProgress(progress("r0", 0, 7)));
        manager.send_event(completed("r1", 1));
        assert_eq!(manager.playlist_update_snapshot(), vec![progress("r2", 2, 7)]);
    }

    #[test]
    fn playlist_update_status_snapshot_preserves_own_facts_on_reuse_and_clears_them_for_a_new_run() {
        let manager = EventManager::new();
        let mut own = progress("r1", 1, 7);
        own.state = Some(PlaylistUpdateState::Partial);
        own.input_telemetry = Some(PlaylistUpdateInputTelemetry {
            refresh_policy: InputRefreshPolicy::REFRESH,
            source: Some(PlaylistUpdateDataSource::Cache),
            clusters: Vec::new(),
        });
        own.library_scan_result = Some(LibraryScanResult {
            files_scanned: 9,
            groups_scanned: 3,
            files_added: 1,
            files_updated: 2,
            files_removed: 3,
            errors: 4,
        });
        manager.send_event(EventMessage::PlaylistUpdateProgress(own.clone()));
        let mut reused = progress("r1", 1, 7);
        reused.state = Some(PlaylistUpdateState::Success);
        manager.send_event(EventMessage::PlaylistUpdateProgress(reused));
        assert_eq!(manager.playlist_update_snapshot(), vec![own.clone()]);
        let mut failure = progress("r1", 1, 7);
        failure.state = Some(PlaylistUpdateState::Failure);
        manager.send_event(EventMessage::PlaylistUpdateProgress(failure));
        own.state = Some(PlaylistUpdateState::Failure);
        assert_eq!(manager.playlist_update_snapshot(), vec![own]);
        manager.send_event(EventMessage::PlaylistUpdateProgress(progress("r2", 2, 7)));
        assert_eq!(manager.playlist_update_snapshot(), vec![progress("r2", 2, 7)]);
    }

    #[test]
    fn playlist_update_status_snapshot_ignores_global_legacy_and_ambiguous_events() {
        let manager = EventManager::new();
        let valid = progress("r1", 1, 7);
        let mut no_order = valid.clone();
        no_order.execution_order = None;
        let mut no_run = valid.clone();
        no_run.run_id = None;
        for event in [PlaylistUpdateProgressEvent::global("target", "working"), no_order, no_run] {
            manager.send_event(EventMessage::PlaylistUpdateProgress(event));
        }
        assert!(manager.playlist_update_snapshot().is_empty());
        manager.send_event(EventMessage::PlaylistUpdateProgress(valid.clone()));
        manager.send_event(EventMessage::PlaylistUpdateProgress(progress("conflicting", 1, 7)));
        assert_eq!(manager.playlist_update_snapshot(), vec![valid]);
    }
}
