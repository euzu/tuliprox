use crate::model::macros;
use crate::utils::arc_str_serde;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use shared::model::{PlaylistItemType, QosSnapshotDailyBucketDto, QosSnapshotRecordDto, QosSnapshotWindowDto};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use crate::repository::{BPlusTree, BPlusTreeUpdate, FlushPolicy};

const SNAPSHOT_FILE_NAME: &str = "qos_snapshot.db";
const CHECKPOINT_FILE_NAME: &str = "qos_snapshot_meta.db";
const CHECKPOINT_KEY: u8 = 0;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QosSnapshotWindow {
    pub connect_count: u64,
    pub connect_failed_count: u64,
    pub startup_capacity_failure_count: u64,
    pub provider_open_failure_count: u64,
    pub first_byte_failure_count: u64,
    pub runtime_abort_count: u64,
    pub provider_closed_count: u64,
    pub preempt_count: u64,
    pub avg_first_byte_latency_ms: Option<u64>,
    pub avg_session_duration_secs: Option<u64>,
    pub avg_provider_reconnect_count: Option<u64>,
    pub last_success_ts: Option<u64>,
    pub last_failure_ts: Option<u64>,
    pub successive_failure_streak: u32,
    pub sample_size: u64,
    pub score: u8,
    pub confidence: u8,
}

macros::from_impl!(QosSnapshotWindow);

impl From<&QosSnapshotWindowDto> for QosSnapshotWindow {
    fn from(dto: &QosSnapshotWindowDto) -> Self {
        Self {
            connect_count: dto.connect_count,
            connect_failed_count: dto.connect_failed_count,
            startup_capacity_failure_count: dto.startup_capacity_failure_count,
            provider_open_failure_count: dto.provider_open_failure_count,
            first_byte_failure_count: dto.first_byte_failure_count,
            runtime_abort_count: dto.runtime_abort_count,
            provider_closed_count: dto.provider_closed_count,
            preempt_count: dto.preempt_count,
            avg_first_byte_latency_ms: dto.avg_first_byte_latency_ms,
            avg_session_duration_secs: dto.avg_session_duration_secs,
            avg_provider_reconnect_count: dto.avg_provider_reconnect_count,
            last_success_ts: dto.last_success_ts,
            last_failure_ts: dto.last_failure_ts,
            successive_failure_streak: dto.successive_failure_streak,
            sample_size: dto.sample_size,
            score: dto.score,
            confidence: dto.confidence,
        }
    }
}

impl From<&QosSnapshotWindow> for QosSnapshotWindowDto {
    fn from(entity: &QosSnapshotWindow) -> Self {
        Self {
            connect_count: entity.connect_count,
            connect_failed_count: entity.connect_failed_count,
            startup_capacity_failure_count: entity.startup_capacity_failure_count,
            provider_open_failure_count: entity.provider_open_failure_count,
            first_byte_failure_count: entity.first_byte_failure_count,
            runtime_abort_count: entity.runtime_abort_count,
            provider_closed_count: entity.provider_closed_count,
            preempt_count: entity.preempt_count,
            avg_first_byte_latency_ms: entity.avg_first_byte_latency_ms,
            avg_session_duration_secs: entity.avg_session_duration_secs,
            avg_provider_reconnect_count: entity.avg_provider_reconnect_count,
            last_success_ts: entity.last_success_ts,
            last_failure_ts: entity.last_failure_ts,
            successive_failure_streak: entity.successive_failure_streak,
            sample_size: entity.sample_size,
            score: entity.score,
            confidence: entity.confidence,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QosSnapshotDailyBucket {
    pub connect_count: u64,
    pub connect_failed_count: u64,
    pub startup_capacity_failure_count: u64,
    pub provider_open_failure_count: u64,
    pub first_byte_failure_count: u64,
    pub runtime_abort_count: u64,
    pub provider_closed_count: u64,
    pub preempt_count: u64,
    pub total_first_byte_latency_ms: u64,
    pub total_first_byte_latency_samples: u64,
    pub total_session_duration_secs: u64,
    pub total_session_duration_samples: u64,
    pub total_provider_reconnect_count: u64,
    pub total_provider_reconnect_samples: u64,
    pub last_success_ts: Option<u64>,
    pub last_failure_ts: Option<u64>,
}

macros::from_impl!(QosSnapshotDailyBucket);

impl From<&QosSnapshotDailyBucketDto> for QosSnapshotDailyBucket {
    fn from(dto: &QosSnapshotDailyBucketDto) -> Self {
        Self {
            connect_count: dto.connect_count,
            connect_failed_count: dto.connect_failed_count,
            startup_capacity_failure_count: dto.startup_capacity_failure_count,
            provider_open_failure_count: dto.provider_open_failure_count,
            first_byte_failure_count: dto.first_byte_failure_count,
            runtime_abort_count: dto.runtime_abort_count,
            provider_closed_count: dto.provider_closed_count,
            preempt_count: dto.preempt_count,
            total_first_byte_latency_ms: dto.total_first_byte_latency_ms,
            total_first_byte_latency_samples: dto.total_first_byte_latency_samples,
            total_session_duration_secs: dto.total_session_duration_secs,
            total_session_duration_samples: dto.total_session_duration_samples,
            total_provider_reconnect_count: dto.total_provider_reconnect_count,
            total_provider_reconnect_samples: dto.total_provider_reconnect_samples,
            last_success_ts: dto.last_success_ts,
            last_failure_ts: dto.last_failure_ts,
        }
    }
}

impl From<&QosSnapshotDailyBucket> for QosSnapshotDailyBucketDto {
    fn from(entity: &QosSnapshotDailyBucket) -> Self {
        Self {
            connect_count: entity.connect_count,
            connect_failed_count: entity.connect_failed_count,
            startup_capacity_failure_count: entity.startup_capacity_failure_count,
            provider_open_failure_count: entity.provider_open_failure_count,
            first_byte_failure_count: entity.first_byte_failure_count,
            runtime_abort_count: entity.runtime_abort_count,
            provider_closed_count: entity.provider_closed_count,
            preempt_count: entity.preempt_count,
            total_first_byte_latency_ms: entity.total_first_byte_latency_ms,
            total_first_byte_latency_samples: entity.total_first_byte_latency_samples,
            total_session_duration_secs: entity.total_session_duration_secs,
            total_session_duration_samples: entity.total_session_duration_samples,
            total_provider_reconnect_count: entity.total_provider_reconnect_count,
            total_provider_reconnect_samples: entity.total_provider_reconnect_samples,
            last_success_ts: entity.last_success_ts,
            last_failure_ts: entity.last_failure_ts,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QosSnapshotRecord {
    pub stream_identity_key: String,
    #[serde(with = "arc_str_serde")]
    pub input_name: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub target_name: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub provider_name: Arc<str>,
    pub provider_id: u32,
    pub virtual_id: u32,
    pub item_type: PlaylistItemType,
    pub updated_at: u64,
    pub last_event_at: u64,
    pub window_24h: QosSnapshotWindow,
    pub window_7d: QosSnapshotWindow,
    pub window_30d: QosSnapshotWindow,
    pub daily_buckets: BTreeMap<String, QosSnapshotDailyBucket>,
}

macros::from_impl!(QosSnapshotRecord);

impl From<&QosSnapshotRecordDto> for QosSnapshotRecord {
    fn from(dto: &QosSnapshotRecordDto) -> Self {
        Self {
            stream_identity_key: dto.stream_identity_key.clone(),
            input_name: Arc::from(dto.input_name.as_str()),
            target_name: Arc::from(dto.target_name.as_str()),
            provider_name: Arc::from(dto.provider_name.as_str()),
            provider_id: dto.provider_id,
            virtual_id: dto.virtual_id,
            item_type: dto.item_type,
            updated_at: dto.updated_at.unwrap_or(0),
            last_event_at: dto.last_event_at.unwrap_or(0),
            window_24h: QosSnapshotWindow::from(&dto.window_24h),
            window_7d: QosSnapshotWindow::from(&dto.window_7d),
            window_30d: QosSnapshotWindow::from(&dto.window_30d),
            daily_buckets: dto
                .daily_buckets
                .iter()
                .map(|(day, bucket)| (day.clone(), QosSnapshotDailyBucket::from(bucket)))
                .collect(),
        }
    }
}

impl From<&QosSnapshotRecord> for QosSnapshotRecordDto {
    fn from(entity: &QosSnapshotRecord) -> Self {
        Self {
            stream_identity_key: entity.stream_identity_key.clone(),
            input_name: entity.input_name.to_string(),
            target_name: entity.target_name.to_string(),
            provider_name: entity.provider_name.to_string(),
            provider_id: entity.provider_id,
            virtual_id: entity.virtual_id,
            item_type: entity.item_type,
            updated_at: Some(entity.updated_at),
            last_event_at: Some(entity.last_event_at),
            window_24h: QosSnapshotWindowDto::from(&entity.window_24h),
            window_7d: QosSnapshotWindowDto::from(&entity.window_7d),
            window_30d: QosSnapshotWindowDto::from(&entity.window_30d),
            daily_buckets: entity
                .daily_buckets
                .iter()
                .map(|(day, bucket)| (day.clone(), QosSnapshotDailyBucketDto::from(bucket)))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QosAggregationCheckpoint {
    pub last_completed_day_utc: Option<String>,
    pub last_successful_run_ts_utc: u64,
    pub current_day_utc: Option<String>,
    pub current_day_revision_secs: Option<u64>,
    pub current_day_revision_len: Option<u64>,
}

pub struct QosSnapshotRepository {
    snapshot_path: PathBuf,
    checkpoint_path: PathBuf,
    snapshot_tree: Mutex<BPlusTreeUpdate<String, QosSnapshotRecord>>,
    checkpoint_tree: Mutex<BPlusTreeUpdate<u8, QosAggregationCheckpoint>>,
}

impl QosSnapshotRepository {
    pub fn snapshot_db_path(storage_dir: &Path) -> PathBuf { storage_dir.join(SNAPSHOT_FILE_NAME) }

    pub fn checkpoint_db_path(storage_dir: &Path) -> PathBuf { storage_dir.join(CHECKPOINT_FILE_NAME) }

    pub fn open(storage_dir: &Path) -> io::Result<Self> {
        let snapshot_path = Self::snapshot_db_path(storage_dir);
        let checkpoint_path = Self::checkpoint_db_path(storage_dir);

        ensure_tree_file::<String, QosSnapshotRecord>(&snapshot_path)?;
        ensure_tree_file::<u8, QosAggregationCheckpoint>(&checkpoint_path)?;

        let mut snapshot_tree = BPlusTreeUpdate::<String, QosSnapshotRecord>::try_new_with_backoff(&snapshot_path)?;
        snapshot_tree.set_flush_policy(FlushPolicy::Batch);
        let mut checkpoint_tree = BPlusTreeUpdate::<u8, QosAggregationCheckpoint>::try_new_with_backoff(&checkpoint_path)?;
        checkpoint_tree.set_flush_policy(FlushPolicy::Batch);

        Ok(Self {
            snapshot_path,
            checkpoint_path,
            snapshot_tree: Mutex::new(snapshot_tree),
            checkpoint_tree: Mutex::new(checkpoint_tree),
        })
    }

    pub fn snapshot_path(&self) -> &Path { &self.snapshot_path }

    pub fn checkpoint_path(&self) -> &Path { &self.checkpoint_path }

    pub fn get_snapshot(&self, stream_identity_key: &str) -> io::Result<Option<QosSnapshotRecord>> {
        let mut tree = self.snapshot_tree.lock();
        tree.query(&stream_identity_key.to_string())
            .map_err(|err| io::Error::other(err.to_string()))
    }

    pub fn put_snapshot(&self, snapshot: &QosSnapshotRecord) -> io::Result<()> {
        let key = &snapshot.stream_identity_key;
        let mut tree = self.snapshot_tree.lock();
        tree.upsert_batch(&[(key, snapshot)])?;
        tree.commit()
    }

    pub fn delete_snapshot(&self, stream_identity_key: &str) -> io::Result<bool> {
        let key = stream_identity_key.to_string();
        let mut tree = self.snapshot_tree.lock();
        let deleted = tree.delete(&key)?;
        tree.commit()?;
        Ok(deleted)
    }

    pub fn for_each_snapshot<F>(&self, mut visit: F) -> io::Result<()>
    where
        F: FnMut(&QosSnapshotRecord),
    {
        let tree = load_snapshot_tree(&self.snapshot_path)?;
        tree.traverse(|_keys, values| {
            for snapshot in values {
                visit(snapshot);
            }
        });
        Ok(())
    }

    pub fn get_snapshot_read_only(storage_dir: &Path, stream_identity_key: &str) -> io::Result<Option<QosSnapshotRecord>> {
        let snapshot_path = Self::snapshot_db_path(storage_dir);
        let tree = load_snapshot_tree(&snapshot_path)?;
        Ok(tree.query(&stream_identity_key.to_string()).cloned())
    }

    pub fn for_each_snapshot_read_only<F>(storage_dir: &Path, mut visit: F) -> io::Result<()>
    where
        F: FnMut(&QosSnapshotRecord),
    {
        let snapshot_path = Self::snapshot_db_path(storage_dir);
        let tree = load_snapshot_tree(&snapshot_path)?;
        tree.traverse(|_keys, values| {
            for snapshot in values {
                visit(snapshot);
            }
        });
        Ok(())
    }

    pub fn load_checkpoint(&self) -> io::Result<QosAggregationCheckpoint> {
        let mut tree = self.checkpoint_tree.lock();
        Ok(tree
            .query(&CHECKPOINT_KEY)
            .map_err(|err| io::Error::other(err.to_string()))?
            .unwrap_or_default())
    }

    pub fn store_checkpoint(&self, checkpoint: &QosAggregationCheckpoint) -> io::Result<()> {
        let mut tree = self.checkpoint_tree.lock();
        tree.upsert_batch(&[(&CHECKPOINT_KEY, checkpoint)])?;
        tree.commit()
    }
}

fn ensure_tree_file<K, V>(path: &Path) -> io::Result<()>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        BPlusTree::<K, V>::new().store(path)?;
    }
    Ok(())
}

fn load_snapshot_tree(path: &Path) -> io::Result<BPlusTree<String, QosSnapshotRecord>> {
    ensure_tree_file::<String, QosSnapshotRecord>(path)?;
    BPlusTree::<String, QosSnapshotRecord>::load(path)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use tempfile::tempdir;
    use shared::model::PlaylistItemType;
    use shared::utils::Internable;
    use super::{QosAggregationCheckpoint, QosSnapshotRecord, QosSnapshotRepository, QosSnapshotWindow};

    fn make_test_snapshot(identity_key: &str, score: u8) -> QosSnapshotRecord {
        QosSnapshotRecord {
            stream_identity_key: identity_key.to_string(),
            input_name: "input-a".intern(),
            target_name: "target-a".intern(),
            provider_name: "provider-a".intern(),
            provider_id: 22,
            virtual_id: 33,
            item_type: PlaylistItemType::Live,
            updated_at: 1_700_000_000,
            last_event_at: 1_700_000_123,
            window_24h: QosSnapshotWindow {
                connect_count: 3,
                score,
                confidence: 60,
                ..QosSnapshotWindow::default()
            },
            window_7d: QosSnapshotWindow::default(),
            window_30d: QosSnapshotWindow::default(),
            daily_buckets: BTreeMap::default(),
        }
    }

    #[test]
    fn qos_snapshot_repository_roundtrips_snapshot_and_checkpoint() {
        let temp = tempdir().expect("tempdir should succeed");
        let repo = QosSnapshotRepository::open(temp.path()).expect("repo should open");

        let snapshot = make_test_snapshot("stream-a", 81);

        repo.put_snapshot(&snapshot).expect("put snapshot should succeed");
        let loaded = repo
            .get_snapshot("stream-a")
            .expect("get snapshot should succeed")
            .expect("snapshot should exist");
        assert_eq!(loaded.stream_identity_key, snapshot.stream_identity_key);
        assert_eq!(loaded.window_24h.score, 81);

        let checkpoint = QosAggregationCheckpoint {
            last_completed_day_utc: Some("2026-04-01".to_string()),
            last_successful_run_ts_utc: 1_700_000_999,
            current_day_utc: Some("2026-04-02".to_string()),
            current_day_revision_secs: Some(1_700_001_000),
            current_day_revision_len: Some(4_096),
        };
        repo.store_checkpoint(&checkpoint).expect("store checkpoint should succeed");
        let loaded_checkpoint = repo.load_checkpoint().expect("load checkpoint should succeed");
        assert_eq!(loaded_checkpoint, checkpoint);
    }

    #[test]
    fn qos_snapshot_repository_deletes_snapshot() {
        let temp = tempdir().expect("tempdir should succeed");
        let repo = QosSnapshotRepository::open(temp.path()).expect("repo should open");

        let snapshot = make_test_snapshot("stream-a", 0);

        repo.put_snapshot(&snapshot).expect("put snapshot should succeed");
        assert!(repo.delete_snapshot("stream-a").expect("delete should succeed"));
        assert!(repo
            .get_snapshot("stream-a")
            .expect("get snapshot should succeed")
            .is_none());
    }

    #[test]
    fn qos_snapshot_repository_paths_live_under_storage_dir() {
        let temp = tempdir().expect("tempdir should succeed");
        let repo = QosSnapshotRepository::open(temp.path()).expect("repo should open");
        assert!(repo.snapshot_path().starts_with(Path::new(temp.path())));
        assert!(repo.checkpoint_path().starts_with(Path::new(temp.path())));
    }

    #[test]
    fn qos_snapshot_repository_read_only_access_works_while_update_repo_is_open() {
        let temp = tempdir().expect("tempdir should succeed");
        let repo = QosSnapshotRepository::open(temp.path()).expect("repo should open");

        let snapshot = make_test_snapshot("stream-a", 81);

        repo.put_snapshot(&snapshot).expect("put snapshot should succeed");
        let loaded = QosSnapshotRepository::get_snapshot_read_only(temp.path(), "stream-a")
            .expect("read-only get should succeed")
            .expect("snapshot should exist");
        assert_eq!(loaded.stream_identity_key, "stream-a");

        let mut seen = Vec::new();
        QosSnapshotRepository::for_each_snapshot_read_only(temp.path(), |record| {
            seen.push(record.stream_identity_key.clone());
        })
        .expect("read-only traversal should succeed");
        assert_eq!(seen, vec!["stream-a".to_string()]);
    }
}
