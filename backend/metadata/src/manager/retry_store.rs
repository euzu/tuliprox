use super::{RetryState, TaskKey, TaskRetryState};
use std::{collections::HashMap, io, path::Path, sync::Arc};
use tuliprox_repository::{
    BPlusTree, BPlusTreeQuery, BPlusTreeUpdate, MetadataRetryDbKey, MetadataRetryDbValue, RetryStateDbValue,
};

fn metadata_retry_key_from_task_key(task_key: &TaskKey) -> MetadataRetryDbKey {
    match task_key {
        TaskKey::Vod(id) => MetadataRetryDbKey::VodId(*id),
        TaskKey::VodStr(id) => MetadataRetryDbKey::VodText(id.as_ref().to_owned()),
        TaskKey::Series(id) => MetadataRetryDbKey::SeriesId(*id),
        TaskKey::SeriesStr(id) => MetadataRetryDbKey::SeriesText(id.as_ref().to_owned()),
        TaskKey::Live(id) => MetadataRetryDbKey::LiveId(*id),
        TaskKey::LiveStr(id) => MetadataRetryDbKey::LiveText(id.as_ref().to_owned()),
        TaskKey::Stream { scope, id } => {
            MetadataRetryDbKey::Stream { scope: scope.as_ref().to_owned(), id: id.as_ref().to_owned() }
        }
    }
}

fn metadata_retry_key_into_task_key(value: MetadataRetryDbKey) -> TaskKey {
    match value {
        MetadataRetryDbKey::VodId(id) => TaskKey::Vod(id),
        MetadataRetryDbKey::VodText(id) => TaskKey::VodStr(Arc::from(id)),
        MetadataRetryDbKey::SeriesId(id) => TaskKey::Series(id),
        MetadataRetryDbKey::SeriesText(id) => TaskKey::SeriesStr(Arc::from(id)),
        MetadataRetryDbKey::LiveId(id) => TaskKey::Live(id),
        MetadataRetryDbKey::LiveText(id) => TaskKey::LiveStr(Arc::from(id)),
        MetadataRetryDbKey::Stream { scope, id } => TaskKey::Stream { scope: Arc::from(scope), id: Arc::from(id) },
    }
}

fn retry_state_db_from_retry_state(state: &RetryState) -> RetryStateDbValue {
    RetryStateDbValue {
        attempts: state.attempts,
        next_allowed_at_ts: state.next_allowed_at_ts,
        cooldown_until_ts: state.cooldown_until_ts,
        last_error: state.last_error.clone(),
        source_last_modified: state.source_last_modified,
    }
}

fn retry_state_db_into_retry_state(value: RetryStateDbValue) -> Option<RetryState> {
    if value.attempts == 0
        && value.next_allowed_at_ts <= 0
        && value.cooldown_until_ts.is_none()
        && value.source_last_modified.is_none()
    {
        return None;
    }
    Some(RetryState {
        attempts: value.attempts,
        next_allowed_at_ts: value.next_allowed_at_ts,
        cooldown_until_ts: value.cooldown_until_ts,
        last_error: value.last_error,
        source_last_modified: value.source_last_modified,
    })
}

fn metadata_retry_value_from_task_retry_state(state: &TaskRetryState, updated_at_ts: i64) -> MetadataRetryDbValue {
    MetadataRetryDbValue {
        resolve: state.resolve.as_ref().map(retry_state_db_from_retry_state),
        probe: state.probe.as_ref().map(retry_state_db_from_retry_state),
        tmdb: state.tmdb.as_ref().map(retry_state_db_from_retry_state),
        updated_at_ts,
    }
}

fn metadata_retry_value_into_task_retry_state(value: MetadataRetryDbValue) -> Option<TaskRetryState> {
    let mut state = TaskRetryState {
        resolve: value.resolve.and_then(retry_state_db_into_retry_state),
        probe: value.probe.and_then(retry_state_db_into_retry_state),
        tmdb: value.tmdb.and_then(retry_state_db_into_retry_state),
        updated_at_ts: value.updated_at_ts,
    };
    if state.is_empty() {
        return None;
    }
    if state.updated_at_ts <= 0 {
        state.updated_at_ts = state.max_domain_timestamp();
    }
    Some(state)
}

fn ensure_metadata_retry_db(path: &Path) -> io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let mut tree = BPlusTree::<MetadataRetryDbKey, MetadataRetryDbValue>::new();
    tree.store(path).map(|_| ())
}

pub(super) fn load_metadata_retry_states_from_disk(path: &Path) -> io::Result<HashMap<TaskKey, TaskRetryState>> {
    ensure_metadata_retry_db(path)?;

    let mut result = HashMap::new();
    let mut stale_keys: Vec<MetadataRetryDbKey> = Vec::new();
    let mut query = BPlusTreeQuery::<MetadataRetryDbKey, MetadataRetryDbValue>::try_new(path)?;
    for entry in query.iter() {
        let (key, value) = entry?;
        if let Some(state) = metadata_retry_value_into_task_retry_state(value.clone()) {
            result.insert(metadata_retry_key_into_task_key(key), state);
        } else {
            stale_keys.push(key);
        }
    }
    drop(query);

    if !stale_keys.is_empty() {
        let delete_refs: Vec<&MetadataRetryDbKey> = stale_keys.iter().collect();
        let mut update = BPlusTreeUpdate::<MetadataRetryDbKey, MetadataRetryDbValue>::try_new_with_backoff(path)?;
        update
            .delete_batch(&delete_refs)
            .map_err(|e| io::Error::other(format!("cleanup metadata retry tombstones failed: {e}")))?;
    }

    Ok(result)
}

pub(super) fn persist_metadata_retry_state_to_disk(
    path: &Path,
    task_key: &TaskKey,
    state: Option<&TaskRetryState>,
) -> io::Result<()> {
    let db_key = metadata_retry_key_from_task_key(task_key);

    ensure_metadata_retry_db(path)?;

    let mut update = BPlusTreeUpdate::<MetadataRetryDbKey, MetadataRetryDbValue>::try_new_with_backoff(path)?;
    if let Some(retry_state) = state {
        let now_ts = chrono::Utc::now().timestamp();
        let value = metadata_retry_value_from_task_retry_state(retry_state, now_ts);
        update
            .upsert_batch(&[(&db_key, &value)])
            .map_err(|e| io::Error::other(format!("persist metadata retry state failed: {e}")))?;
    } else {
        update.delete(&db_key).map_err(|e| io::Error::other(format!("delete metadata retry state failed: {e}")))?;
    }
    Ok(())
}
