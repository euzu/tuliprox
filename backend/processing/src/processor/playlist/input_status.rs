use super::{InputDownloadResult, InputJobResult, MetadataUpdateSink, PlaylistProcessingContext};
use crate::input_cache::{load_input_status, resolve_input_storage_path, save_input_status};
use shared::model::{EventSink, PersistedPlaylistUpdateInputResult, PlaylistUpdateState};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub(super) async fn persist_input_job_result<E: EventSink + Clone + 'static, M: MetadataUpdateSink>(
    ctx: &PlaylistProcessingContext<E, M>,
    result: &InputJobResult,
) {
    // Successful in-run reuse has no new acquisition facts and must not replace the
    // original result (in particular Partial) or advance its completion timestamp.
    let state = result.update_state();
    if result.input_telemetry.is_some() || state == PlaylistUpdateState::Failure {
        persist_input_completion(ctx, result.input_id, &result.input_name, state).await;
    }
}

pub(super) async fn complete_staged_input<E: EventSink + Clone + 'static, M: MetadataUpdateSink>(
    ctx: &PlaylistProcessingContext<E, M>,
    input: &tuliprox_core::model::ConfigInput,
    result: &mut InputDownloadResult,
) {
    let state = result.update_state();
    if result.input_telemetry.is_some() || state == PlaylistUpdateState::Failure {
        persist_input_completion(ctx, input.id, &input.name, state).await;
        // Report the staged input's own facts before its parent consumes them.
        // Successful in-run reuse has no new acquisition and stays silent.
        super::ingest::report_input_completion(
            &ctx.events,
            &ctx.run_id,
            ctx.execution_order,
            input.id,
            &input.name,
            state,
            result.input_telemetry.as_ref(),
        );
    }
}

pub(super) async fn persist_input_completion<E: EventSink + Clone + 'static, M: MetadataUpdateSink>(
    ctx: &PlaylistProcessingContext<E, M>,
    input_id: u16,
    input_name: &Arc<str>,
    state: PlaylistUpdateState,
) {
    // Same per-input lock and canonical writer as acquisition. Called after its
    // own critical section (the staged caller still owns the parent lock).
    let _input_lock = ctx.get_input_lock(input_name).await;
    let storage_dir = ctx.config.config.load().storage_dir.clone();
    let path = resolve_input_storage_path(&storage_dir, input_name).await;
    {
        // Shared by source contexts of this run only; never compare with the
        // previous run's persisted result. Weaker/equal completions do not move
        // the timestamp belonging to an already recorded stronger result.
        let mut completions = ctx.input_completions.lock().await;
        if let Some(current) = completions.get(&input_id) {
            use PlaylistUpdateState::{Failure, Partial, Success};
            match (current, state) {
                (Failure, _) | (Partial, Partial | Success) | (Success, Success) => return,
                (Partial, Failure) | (Success, Partial | Failure) => {}
            }
        }
        completions.insert(input_id, state);
    }
    let mut status = load_input_status(&path);
    status.last_input_update = Some(PersistedPlaylistUpdateInputResult {
        state,
        timestamp: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
    });
    // Do not touch the cache's cluster status/timestamp or its proven runtime facts.
    save_input_status(&path, &status);
}
