use crate::{api::model::UpdateGuard, model::ProcessTargets};
use arc_swap::ArcSwap;
use shared::model::PlaylistUpdateRunId;
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct ManualPlaylistUpdateRequest {
    pub run_id: PlaylistUpdateRunId,
    pub targets: Arc<ProcessTargets>,
    pub input_action: Option<shared::model::InputUpdateRequest>,
}

/// What starts, serializes and narrows playlist updates.
pub struct PlaylistUpdateControl {
    /// Targets forced by program arguments.
    pub forced_targets: ArcSwap<ProcessTargets>,
    pub update_guard: UpdateGuard,
    /// Bounded channel (capacity 1) for manual playlist update requests.
    /// `try_send` deduplicates rapid clicks: if an update is already pending
    /// or the channel is full, the request is silently dropped so at most one
    /// update is queued at any time regardless of how many times the button is clicked.
    pub manual_update_sender: mpsc::Sender<ManualPlaylistUpdateRequest>,
}

impl PlaylistUpdateControl {
    pub fn new(
        forced_targets: Arc<ProcessTargets>,
        manual_update_sender: mpsc::Sender<ManualPlaylistUpdateRequest>,
    ) -> Self {
        Self { forced_targets: ArcSwap::new(forced_targets), update_guard: UpdateGuard::new(), manual_update_sender }
    }

    /// No forced targets and a sender whose receiver is already dropped.
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        let (manual_update_sender, _) = mpsc::channel::<ManualPlaylistUpdateRequest>(1);
        Self::for_tests_with_sender(manual_update_sender)
    }

    /// No forced targets.
    #[cfg(test)]
    pub(crate) fn for_tests_with_sender(manual_update_sender: mpsc::Sender<ManualPlaylistUpdateRequest>) -> Self {
        Self::new(
            Arc::new(ProcessTargets {
                enabled: false,
                inputs: Vec::new(),
                targets: Vec::new(),
                target_names: Vec::new(),
            }),
            manual_update_sender,
        )
    }
}
