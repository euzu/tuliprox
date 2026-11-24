use serde::{Deserialize, Serialize};
use shared::model::{WatchProgressDto, WatchProgressKind, WatchProgressUpdateDto};
use std::path::{Path, PathBuf};
use tuliprox_core::utils::{atomic_json_store::write_file_atomic, FileLockManager};

pub const MAX_PROGRESS_ENTRIES_PER_USER: usize = 500;
pub const PROGRESS_RETENTION_SECS: u64 = 180 * 24 * 60 * 60; // 180 days

#[derive(Debug, thiserror::Error)]
pub enum WatchProgressError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid username")]
    InvalidUsername,
    #[error("unsafe path")]
    UnsafeFile,
    #[error("user not found")]
    UserNotFound,
    #[error("task join: {0}")]
    Task(#[from] tokio::task::JoinError),
    #[error("serialization: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Internal domain representation of a watch progress record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchProgressEntry {
    pub kind: WatchProgressKind,
    pub item_id: u32,
    pub position_secs: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u32>,
    pub watched: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series_id: Option<u32>,
    pub updated_at: u64,
}

impl From<&WatchProgressEntry> for WatchProgressDto {
    fn from(entry: &WatchProgressEntry) -> Self {
        Self {
            kind: entry.kind,
            item_id: entry.item_id,
            position_secs: entry.position_secs,
            duration_secs: entry.duration_secs,
            watched: entry.watched,
            series_id: entry.series_id,
            updated_at: entry.updated_at,
        }
    }
}

/// Determines the watched status according to the domain rules:
/// 1. Explicit flag wins.
/// 2. If duration > 0 and position * 100 / duration >= 90, entry is watched.
/// 3. Otherwise if previously watched, stays watched unless position is less than 60 seconds.
#[must_use]
pub fn calculate_watched(
    explicit_watched: Option<bool>,
    position_secs: u32,
    duration_secs: Option<u32>,
    previous_watched: bool,
) -> bool {
    if let Some(explicit) = explicit_watched {
        return explicit;
    }
    if let Some(duration) = duration_secs {
        if duration > 0 && (u64::from(position_secs) * 100) / u64::from(duration) >= 90 {
            return true;
        }
    }
    if previous_watched {
        position_secs >= 60
    } else {
        false
    }
}

/// Drops entries older than 180 days and retains at most the 500 newest entries.
pub fn apply_retention_and_limits(entries: &mut Vec<WatchProgressEntry>, now_secs: u64) {
    let cutoff = now_secs.saturating_sub(PROGRESS_RETENTION_SECS);
    entries.retain(|e| e.updated_at >= cutoff);
    if entries.len() > MAX_PROGRESS_ENTRIES_PER_USER {
        sort_newest_first(entries);
        entries.truncate(MAX_PROGRESS_ENTRIES_PER_USER);
    }
}

fn sort_newest_first(entries: &mut [WatchProgressEntry]) {
    entries.sort_by(|a, b| {
        b.updated_at.cmp(&a.updated_at).then_with(|| b.kind.cmp(&a.kind)).then_with(|| b.item_id.cmp(&a.item_id))
    });
}

/// Computes the portable progress filename for an API username.
pub fn watch_progress_filename(username: &str) -> Result<String, WatchProgressError> {
    let stem = crate::user_settings_repository::encode_settings_stem(username)
        .map_err(|_| WatchProgressError::InvalidUsername)?;
    Ok(format!("{stem}.json"))
}

/// Computes the filesystem path for an API user's watch progress storage file.
pub fn watch_progress_path(config_path: &Path, username: &str) -> Result<PathBuf, WatchProgressError> {
    Ok(config_path.join("user_settings").join("api").join(watch_progress_filename(username)?))
}

fn load_entries_sync(path: &Path) -> Result<Vec<WatchProgressEntry>, WatchProgressError> {
    let content = match std::fs::read(path) {
        Ok(c) => c,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(WatchProgressError::Io(err)),
    };
    if content.is_empty() {
        return Ok(Vec::new());
    }
    match serde_json::from_slice::<Vec<WatchProgressEntry>>(&content) {
        Ok(entries) => Ok(entries),
        Err(err) => {
            log::warn!("Corrupt watch progress file {}: {err}. Treating as empty.", path.display());
            Ok(Vec::new())
        }
    }
}

async fn read_progress<T, F>(path: &Path, locks: &FileLockManager, f: F) -> Result<T, WatchProgressError>
where
    T: Send + 'static,
    F: FnOnce(Vec<WatchProgressEntry>) -> T + Send + 'static,
{
    crate::user_settings_repository::safe_path(path).await.map_err(|_| WatchProgressError::UnsafeFile)?;
    let guard = locks.read_lock(path).await;
    let path_buf = path.to_path_buf();
    let result = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        let entries = load_entries_sync(&path_buf)?;
        Ok::<T, WatchProgressError>(f(entries))
    })
    .await
    .map_err(WatchProgressError::Task)??;

    Ok(result)
}

async fn mutate_progress<F, C>(
    path: &Path,
    locks: &FileLockManager,
    check: C,
    mutate: F,
) -> Result<(), WatchProgressError>
where
    C: FnOnce() -> bool + Send + 'static,
    F: FnOnce(&mut Vec<WatchProgressEntry>) -> bool + Send + 'static,
{
    crate::user_settings_repository::safe_path(path).await.map_err(|_| WatchProgressError::UnsafeFile)?;
    let guard = locks.write_lock(path).await;
    if !check() {
        return Err(WatchProgressError::UserNotFound);
    }
    let path_buf = path.to_path_buf();
    let (should_write, content) = tokio::task::spawn_blocking(move || {
        let mut entries = load_entries_sync(&path_buf)?;
        let should_write = mutate(&mut entries);
        if !should_write {
            return Ok::<_, WatchProgressError>((false, Vec::new()));
        }
        let content = serde_json::to_vec(&entries).map_err(WatchProgressError::Serialization)?;
        Ok((true, content))
    })
    .await
    .map_err(WatchProgressError::Task)??;

    if should_write {
        write_file_atomic(path, &content).await.map_err(WatchProgressError::Io)?;
        locks.mark_internal_write_content(path, &content).await;
    }
    drop(guard);
    Ok(())
}

/// Inserts or updates a watch progress entry under the write lock.
pub async fn update_progress<F>(
    path: &Path,
    locks: &FileLockManager,
    kind: WatchProgressKind,
    item_id: u32,
    update: WatchProgressUpdateDto,
    server_now_secs: u64,
    check_user_exists: F,
) -> Result<(), WatchProgressError>
where
    F: FnOnce() -> bool + Send + 'static,
{
    mutate_progress(path, locks, check_user_exists, move |entries| {
        if let Some(existing) = entries.iter_mut().find(|e| e.kind == kind && e.item_id == item_id) {
            let duration_secs = update.duration_secs.or(existing.duration_secs);
            let series_id = update.series_id.or(existing.series_id);
            let watched = calculate_watched(update.watched, update.position_secs, duration_secs, existing.watched);
            existing.position_secs = update.position_secs;
            existing.duration_secs = duration_secs;
            existing.watched = watched;
            existing.series_id = series_id;
            existing.updated_at = server_now_secs;
        } else {
            let watched = calculate_watched(update.watched, update.position_secs, update.duration_secs, false);
            entries.push(WatchProgressEntry {
                kind,
                item_id,
                position_secs: update.position_secs,
                duration_secs: update.duration_secs,
                watched,
                series_id: update.series_id,
                updated_at: server_now_secs,
            });
        }
        apply_retention_and_limits(entries, server_now_secs);
        true
    })
    .await
}

/// Reads filtered progress entries under the read lock, newest first.
pub async fn get_progress_list(
    path: &Path,
    locks: &FileLockManager,
    kind_filter: Option<WatchProgressKind>,
    watched_filter: Option<bool>,
    limit: usize,
) -> Result<Vec<WatchProgressDto>, WatchProgressError> {
    read_progress(path, locks, move |mut entries| {
        if let Some(kind) = kind_filter {
            entries.retain(|e| e.kind == kind);
        }
        if let Some(watched) = watched_filter {
            entries.retain(|e| e.watched == watched);
        }
        sort_newest_first(&mut entries);
        entries.truncate(limit);
        entries.iter().map(WatchProgressDto::from).collect()
    })
    .await
}

/// Reads a single progress entry under the read lock.
pub async fn get_progress_one(
    path: &Path,
    locks: &FileLockManager,
    kind: WatchProgressKind,
    item_id: u32,
) -> Result<Option<WatchProgressDto>, WatchProgressError> {
    read_progress(path, locks, move |entries| {
        let entry = entries.iter().find(|e| e.kind == kind && e.item_id == item_id);
        entry.map(WatchProgressDto::from)
    })
    .await
}

/// Deletes a single watch progress entry under the write lock.
pub async fn delete_progress_one(
    path: &Path,
    locks: &FileLockManager,
    kind: WatchProgressKind,
    item_id: u32,
) -> Result<(), WatchProgressError> {
    mutate_progress(
        path,
        locks,
        || true,
        move |entries| {
            let prev_len = entries.len();
            entries.retain(|e| !(e.kind == kind && e.item_id == item_id));
            entries.len() != prev_len
        },
    )
    .await
}

/// Deletes all watch progress entries for a user under the write lock by removing the file.
pub async fn delete_all_progress(path: &Path, locks: &FileLockManager) -> Result<(), WatchProgressError> {
    crate::user_settings_repository::safe_path(path).await.map_err(|_| WatchProgressError::UnsafeFile)?;
    let guard = locks.write_lock(path).await;
    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(WatchProgressError::Io(err)),
    }

    locks.mark_internal_write_content(path, &[]).await;
    drop(guard);
    Ok(())
}

/// Removes the progress file without acquiring locks, used during user deletion.
pub async fn remove_progress_unlocked(path: &Path) -> Result<(), WatchProgressError> {
    crate::user_settings_repository::safe_path(path).await.map_err(|_| WatchProgressError::UnsafeFile)?;
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(WatchProgressError::Io(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[test]
    fn watched_rule_explicit_wins() {
        assert!(calculate_watched(Some(true), 10, Some(1000), false));
        assert!(calculate_watched(Some(true), 0, None, false));
        assert!(!calculate_watched(Some(false), 950, Some(1000), false));
        assert!(!calculate_watched(Some(false), 950, Some(1000), true));
    }

    #[test]
    fn watched_rule_boundaries_89_90_percent() {
        // 89% unwatched -> false
        assert!(!calculate_watched(None, 89, Some(100), false));
        // 90% unwatched -> true
        assert!(calculate_watched(None, 90, Some(100), false));
        // duration 0 -> false
        assert!(!calculate_watched(None, 90, Some(0), false));
        // duration None -> false
        assert!(!calculate_watched(None, 90, None, false));
    }

    #[test]
    fn watched_rule_boundaries_position_59_60() {
        // Watched entry updated with position < 60 becomes unwatched
        assert!(!calculate_watched(None, 59, Some(1000), true));
        assert!(!calculate_watched(None, 0, Some(1000), true));
        // Watched entry updated with position >= 60 remains watched
        assert!(calculate_watched(None, 60, Some(1000), true));
        assert!(calculate_watched(None, 100, Some(1000), true));
    }

    #[test]
    fn watched_rule_50s_clip_at_48s_then_49s_stays_watched() {
        // 50s clip at 48s: 48/50 = 96% >= 90% -> watched
        let watched_at_48 = calculate_watched(None, 48, Some(50), false);
        assert!(watched_at_48);
        // At 49s, previous_watched is true: must stay watched
        let watched_at_49 = calculate_watched(None, 49, Some(50), watched_at_48);
        assert!(watched_at_49, "50s clip at 48s then 49s must stay watched");
    }

    #[tokio::test]
    async fn update_progress_position_only_keeps_duration_and_series_id_and_triggers_90_percent() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("user.json");
        let locks = FileLockManager::new();

        // Initial progress: 10s of 100s, series_id 77
        let initial =
            WatchProgressUpdateDto { position_secs: 10, duration_secs: Some(100), watched: None, series_id: Some(77) };
        update_progress(&file_path, &locks, WatchProgressKind::Episode, 42, initial, 1_000, || true).await.unwrap();

        // Position-only update at 95s (duration_secs: None, series_id: None)
        let pos_only =
            WatchProgressUpdateDto { position_secs: 95, duration_secs: None, watched: None, series_id: None };
        update_progress(&file_path, &locks, WatchProgressKind::Episode, 42, pos_only, 2_000, || true).await.unwrap();

        let one = get_progress_one(&file_path, &locks, WatchProgressKind::Episode, 42).await.unwrap().unwrap();
        assert_eq!(one.position_secs, 95);
        assert_eq!(one.duration_secs, Some(100), "duration_secs must be retained from previous entry");
        assert_eq!(one.series_id, Some(77), "series_id must be retained from previous entry");
        assert!(one.watched, "90% completion must trigger with remembered duration (95/100 >= 90%)");
    }

    #[test]
    fn limit_eviction_exact_count() {
        let mut entries = Vec::new();
        let now = 1_000_000_000;
        for i in 1..=505 {
            entries.push(WatchProgressEntry {
                kind: WatchProgressKind::Movie,
                item_id: i,
                position_secs: 100,
                duration_secs: Some(200),
                watched: false,
                series_id: None,
                updated_at: now + u64::from(i),
            });
        }
        apply_retention_and_limits(&mut entries, now + 600);
        assert_eq!(entries.len(), 500);
        // The 5 oldest items (item_id 1..=5) must have been dropped
        assert!(!entries.iter().any(|e| e.item_id <= 5));
        assert!(entries.iter().any(|e| e.item_id == 6));
        assert!(entries.iter().any(|e| e.item_id == 505));
    }

    #[test]
    fn retention_drops_entries_older_than_180_days() {
        let now = 1_000_000_000;
        let mut entries = vec![
            WatchProgressEntry {
                kind: WatchProgressKind::Movie,
                item_id: 1,
                position_secs: 100,
                duration_secs: Some(200),
                watched: false,
                series_id: None,
                updated_at: now - PROGRESS_RETENTION_SECS - 1, // expired
            },
            WatchProgressEntry {
                kind: WatchProgressKind::Movie,
                item_id: 2,
                position_secs: 100,
                duration_secs: Some(200),
                watched: false,
                series_id: None,
                updated_at: now - PROGRESS_RETENTION_SECS, // retained
            },
            WatchProgressEntry {
                kind: WatchProgressKind::Movie,
                item_id: 3,
                position_secs: 100,
                duration_secs: Some(200),
                watched: false,
                series_id: None,
                updated_at: now, // retained
            },
        ];
        apply_retention_and_limits(&mut entries, now);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].item_id, 2);
        assert_eq!(entries[1].item_id, 3);
    }

    #[tokio::test]
    async fn corrupt_file_treated_as_empty_and_overwritten_on_write() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("user.json");
        std::fs::write(&file_path, b"{broken json content").unwrap();

        let locks = FileLockManager::new();
        let list = get_progress_list(&file_path, &locks, None, None, 50).await.unwrap();
        assert_eq!(list, Vec::new());

        let update =
            WatchProgressUpdateDto { position_secs: 120, duration_secs: Some(200), watched: None, series_id: None };
        update_progress(&file_path, &locks, WatchProgressKind::Movie, 42, update.clone(), 1_700_000_000, || true)
            .await
            .unwrap();

        let one = get_progress_one(&file_path, &locks, WatchProgressKind::Movie, 42).await.unwrap().unwrap();
        assert_eq!(one.item_id, 42);
        assert_eq!(one.position_secs, 120);
        assert_eq!(one.duration_secs, Some(200));
        assert!(!one.watched); // 120/200 = 60% < 90%
    }

    #[tokio::test]
    async fn concurrent_writes_keep_both_entries() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("user.json");
        let locks = Arc::new(FileLockManager::new());

        let mut handles = Vec::new();
        for i in 1..=10 {
            let path = file_path.clone();
            let locks_ref = Arc::clone(&locks);
            handles.push(tokio::spawn(async move {
                let update = WatchProgressUpdateDto {
                    position_secs: i * 10,
                    duration_secs: Some(200),
                    watched: None,
                    series_id: None,
                };
                update_progress(
                    &path,
                    &locks_ref,
                    WatchProgressKind::Movie,
                    i,
                    update,
                    1_700_000_000 + u64::from(i),
                    || true,
                )
                .await
                .unwrap();
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        let list = get_progress_list(&file_path, &locks, None, None, 50).await.unwrap();
        assert_eq!(list.len(), 10);
        for i in 1..=10 {
            assert!(list.iter().any(|e| e.item_id == i));
        }
    }

    #[tokio::test]
    async fn delete_progress_one_skips_write_when_nothing_removed() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("user.json");
        let locks = FileLockManager::new();

        // 1. Non-existent file: deleting non-existent item should not create file
        delete_progress_one(&file_path, &locks, WatchProgressKind::Movie, 999).await.unwrap();
        assert!(!file_path.exists(), "non-existent file must not be created when nothing removed");

        // 2. Existing file with 1 item: deleting a different item should not modify file
        let update =
            WatchProgressUpdateDto { position_secs: 50, duration_secs: Some(100), watched: None, series_id: None };
        update_progress(&file_path, &locks, WatchProgressKind::Movie, 42, update.clone(), 1_700_000_000, || true)
            .await
            .unwrap();
        let bytes_before = std::fs::read(&file_path).unwrap();
        // The atomic write replaces the file, so an unchanged inode proves no rewrite happened.
        #[cfg(unix)]
        let inode_before = std::fs::metadata(&file_path).unwrap().ino();

        delete_progress_one(&file_path, &locks, WatchProgressKind::Episode, 42).await.unwrap();
        let bytes_after = std::fs::read(&file_path).unwrap();
        assert_eq!(bytes_before, bytes_after, "file bytes must not change when no item was deleted");
        #[cfg(unix)]
        assert_eq!(
            inode_before,
            std::fs::metadata(&file_path).unwrap().ino(),
            "file inode must not change when no item was deleted"
        );

        // 3. Deleting the actual item: removes it and writes
        delete_progress_one(&file_path, &locks, WatchProgressKind::Movie, 42).await.unwrap();
        let one = get_progress_one(&file_path, &locks, WatchProgressKind::Movie, 42).await.unwrap();
        assert_eq!(one, None);
    }

    #[tokio::test]
    async fn update_progress_fails_and_does_not_create_file_when_check_fails() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("deleted_user.json");
        let locks = FileLockManager::new();

        let update =
            WatchProgressUpdateDto { position_secs: 50, duration_secs: Some(100), watched: None, series_id: None };
        let res =
            update_progress(&file_path, &locks, WatchProgressKind::Movie, 42, update, 1_700_000_000, || false).await;
        assert!(matches!(res, Err(WatchProgressError::UserNotFound)));
        assert!(!file_path.exists(), "watch progress file must not be created for deleted user");
    }
}
