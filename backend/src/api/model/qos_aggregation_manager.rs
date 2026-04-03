#![cfg_attr(not(test), allow(dead_code))]

use chrono::NaiveDate;
use log::{error, info, warn};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::api::model::AppState;
use crate::model::Config;
use crate::repository::{
    current_utc_day, extract_day_from_filename, now_utc_secs, ConnectFailureReason, DisconnectReason, EventType,
    FailureStage, QosSnapshotDailyBucket, QosSnapshotRecord, QosSnapshotRepository, QosSnapshotWindow,
    StreamHistoryFileReader, StreamHistoryRecord,
};
use tokio_util::sync::CancellationToken;

const MAX_COMPLETED_DAY_PARTITIONS_PER_RUN: usize = 3;

#[derive(Debug, Clone)]
struct AggregatedDayEntry {
    input_name: String,
    target_id: u16,
    provider_name: String,
    provider_id: u32,
    virtual_id: u32,
    item_type: String,
    bucket: QosSnapshotDailyBucket,
}

pub(in crate::api) fn qos_aggregation_is_enabled(config: &Config) -> bool {
    config
        .reverse_proxy
        .as_ref()
        .and_then(|rp| rp.qos_aggregation.as_ref())
        .is_some_and(|qos| {
            qos.enabled
                && config
                    .reverse_proxy
                    .as_ref()
                    .and_then(|rp| rp.stream_history.as_ref())
                    .is_some_and(|history| history.stream_history_enabled)
        })
}

pub(in crate::api) fn exec_qos_aggregation(app_state: &Arc<AppState>, cancel_token: &CancellationToken) {
    let config = app_state.app_config.config.load();
    let Some(reverse_proxy) = config.reverse_proxy.as_ref() else {
        return;
    };
    let Some(history) = reverse_proxy.stream_history.as_ref() else {
        return;
    };
    let Some(qos) = reverse_proxy.qos_aggregation.as_ref() else {
        return;
    };
    if !qos.enabled || !history.stream_history_enabled {
        return;
    }

    let history_dir = PathBuf::from(history.stream_history_directory.clone());
    let storage_dir = PathBuf::from(config.storage_dir.clone());
    let interval = Duration::from_secs(qos.interval_secs.max(1));
    let cancel = cancel_token.clone();

    tokio::spawn(async move {
        let repo = match QosSnapshotRepository::open(&storage_dir) {
            Ok(repo) => repo,
            Err(err) => {
                error!("Failed to open QoS snapshot repository: {err}");
                return;
            }
        };

        loop {
            let today = current_utc_day();
            if let Err(err) = run_aggregation_once(&repo, &history_dir, &today) {
                warn!("QoS aggregation run failed: {err}");
            }

            tokio::select! {
                () = cancel.cancelled() => {
                    info!("QoS aggregation loop stopped");
                    break;
                }
                () = tokio::time::sleep(interval) => {}
            }
        }
    });
}

pub(crate) fn run_aggregation_once(
    repo: &QosSnapshotRepository,
    history_dir: &Path,
    today_utc: &str,
) -> io::Result<()> {
    let mut checkpoint = repo.load_checkpoint()?;
    let days = discover_history_days(history_dir)?;
    let completed_days = select_completed_days_to_process(&days, checkpoint.last_completed_day_utc.as_deref(), today_utc);

    for day in completed_days.iter().take(MAX_COMPLETED_DAY_PARTITIONS_PER_RUN) {
        let day_entries = aggregate_day_entries(history_dir, day)?;
        apply_day_entries(repo, day, &day_entries, today_utc)?;
    }

    let current_day_revision = history_day_revision(history_dir, today_utc)?;
    let should_refresh_current_day = checkpoint.current_day_utc.as_deref() != Some(today_utc)
        || checkpoint.current_day_revision_secs != current_day_revision.map(|revision| revision.0)
        || checkpoint.current_day_revision_len != current_day_revision.map(|revision| revision.1);
    if should_refresh_current_day {
        let current_day_entries = aggregate_day_entries(history_dir, today_utc)?;
        apply_day_entries(repo, today_utc, &current_day_entries, today_utc)?;
    }

    if let Some(last_completed_day_utc) = completed_days
        .into_iter()
        .take(MAX_COMPLETED_DAY_PARTITIONS_PER_RUN)
        .next_back()
    {
        checkpoint.last_completed_day_utc = Some(last_completed_day_utc);
    }
    checkpoint.last_successful_run_ts_utc = now_utc_secs();
    checkpoint.current_day_utc = Some(today_utc.to_string());
    checkpoint.current_day_revision_secs = current_day_revision.map(|revision| revision.0);
    checkpoint.current_day_revision_len = current_day_revision.map(|revision| revision.1);
    repo.store_checkpoint(&checkpoint)?;

    Ok(())
}

pub(crate) fn fold_record_into_bucket(bucket: &mut QosSnapshotDailyBucket, record: &StreamHistoryRecord) {
    match record.event_type {
        EventType::Connect => {
            if record.shared_joined_existing.unwrap_or(false) {
                return;
            }
            bucket.connect_count = bucket.connect_count.saturating_add(1);
            bucket.last_success_ts = Some(bucket.last_success_ts.map_or(record.event_ts_utc, |ts| ts.max(record.event_ts_utc)));
        }
        EventType::ConnectFailed => {
            bucket.connect_failed_count = bucket.connect_failed_count.saturating_add(1);
            if matches!(
                record.connect_failure_reason,
                Some(ConnectFailureReason::UserConnectionsExhausted | ConnectFailureReason::ProviderConnectionsExhausted)
            ) {
                bucket.startup_capacity_failure_count = bucket.startup_capacity_failure_count.saturating_add(1);
            }
            if matches!(record.failure_stage, Some(FailureStage::ProviderOpen)) {
                bucket.provider_open_failure_count = bucket.provider_open_failure_count.saturating_add(1);
            }
            bucket.last_failure_ts = Some(bucket.last_failure_ts.map_or(record.event_ts_utc, |ts| ts.max(record.event_ts_utc)));
        }
        EventType::Disconnect => {
            if matches!(record.failure_stage, Some(FailureStage::FirstByte)) {
                bucket.first_byte_failure_count = bucket.first_byte_failure_count.saturating_add(1);
            }
            if matches!(record.failure_stage, Some(FailureStage::Streaming))
                && matches!(
                    record.disconnect_reason,
                    Some(DisconnectReason::ProviderError | DisconnectReason::ProviderClosed)
                )
            {
                bucket.runtime_abort_count = bucket.runtime_abort_count.saturating_add(1);
            }
            if matches!(record.disconnect_reason, Some(DisconnectReason::ProviderClosed)) {
                bucket.provider_closed_count = bucket.provider_closed_count.saturating_add(1);
            }
            if matches!(record.disconnect_reason, Some(DisconnectReason::Preempted)) {
                bucket.preempt_count = bucket.preempt_count.saturating_add(1);
            }
            if let Some(latency) = record.first_byte_latency_ms {
                bucket.total_first_byte_latency_ms = bucket.total_first_byte_latency_ms.saturating_add(latency);
                bucket.total_first_byte_latency_samples = bucket.total_first_byte_latency_samples.saturating_add(1);
            }
            if let Some(duration) = record.session_duration {
                bucket.total_session_duration_secs = bucket.total_session_duration_secs.saturating_add(duration);
                bucket.total_session_duration_samples = bucket.total_session_duration_samples.saturating_add(1);
            }
            if let Some(reconnects) = record.provider_reconnect_count {
                bucket.total_provider_reconnect_count =
                    bucket.total_provider_reconnect_count.saturating_add(u64::from(reconnects));
                bucket.total_provider_reconnect_samples = bucket.total_provider_reconnect_samples.saturating_add(1);
            }
            if matches!(
                record.disconnect_reason,
                Some(DisconnectReason::ProviderError | DisconnectReason::ProviderClosed | DisconnectReason::Preempted)
            ) {
                bucket.last_failure_ts =
                    Some(bucket.last_failure_ts.map_or(record.event_ts_utc, |ts| ts.max(record.event_ts_utc)));
            }
        }
    }
}

pub(crate) fn rebuild_windows(snapshot: &mut QosSnapshotRecord, today_utc: &str) {
    snapshot.window_24h = build_window(snapshot, today_utc, 1);
    snapshot.window_7d = build_window(snapshot, today_utc, 6);
    snapshot.window_30d = build_window(snapshot, today_utc, 29);
}

fn build_window(snapshot: &QosSnapshotRecord, today_utc: &str, max_day_distance: i64) -> QosSnapshotWindow {
    let today = parse_utc_day(today_utc);
    let mut window = QosSnapshotWindow::default();
    let mut latency_total = 0u64;
    let mut latency_samples = 0u64;
    let mut duration_total = 0u64;
    let mut duration_samples = 0u64;
    let mut reconnect_total = 0u64;
    let mut reconnect_samples = 0u64;

    for (day, bucket) in &snapshot.daily_buckets {
        if !bucket_in_window(today, day, max_day_distance) {
            continue;
        }
        window.connect_count = window.connect_count.saturating_add(bucket.connect_count);
        window.connect_failed_count = window.connect_failed_count.saturating_add(bucket.connect_failed_count);
        window.startup_capacity_failure_count =
            window.startup_capacity_failure_count.saturating_add(bucket.startup_capacity_failure_count);
        window.provider_open_failure_count =
            window.provider_open_failure_count.saturating_add(bucket.provider_open_failure_count);
        window.first_byte_failure_count = window.first_byte_failure_count.saturating_add(bucket.first_byte_failure_count);
        window.runtime_abort_count = window.runtime_abort_count.saturating_add(bucket.runtime_abort_count);
        window.provider_closed_count = window.provider_closed_count.saturating_add(bucket.provider_closed_count);
        window.preempt_count = window.preempt_count.saturating_add(bucket.preempt_count);
        window.last_success_ts = max_opt(window.last_success_ts, bucket.last_success_ts);
        window.last_failure_ts = max_opt(window.last_failure_ts, bucket.last_failure_ts);
        latency_total = latency_total.saturating_add(bucket.total_first_byte_latency_ms);
        latency_samples = latency_samples.saturating_add(bucket.total_first_byte_latency_samples);
        duration_total = duration_total.saturating_add(bucket.total_session_duration_secs);
        duration_samples = duration_samples.saturating_add(bucket.total_session_duration_samples);
        reconnect_total = reconnect_total.saturating_add(bucket.total_provider_reconnect_count);
        reconnect_samples = reconnect_samples.saturating_add(bucket.total_provider_reconnect_samples);
    }

    window.avg_first_byte_latency_ms = average_opt(latency_total, latency_samples);
    window.avg_session_duration_secs = average_opt(duration_total, duration_samples);
    window.avg_provider_reconnect_count = average_opt(reconnect_total, reconnect_samples);
    window.sample_size = window
        .connect_count
        .saturating_add(window.connect_failed_count)
        .saturating_add(window.runtime_abort_count)
        .saturating_add(window.first_byte_failure_count);
    window.successive_failure_streak = u32::from(matches!(
        (window.last_success_ts, window.last_failure_ts),
        (_, Some(failure)) if window.last_success_ts.is_none_or(|success| failure > success)
    ));
    window.score = compute_score(&window);
    window.confidence = compute_confidence(window.sample_size);
    window
}

fn compute_score(window: &QosSnapshotWindow) -> u8 {
    let startup_total = window.connect_count.saturating_add(window.connect_failed_count);
    let startup_success = ratio_score(window.connect_count, startup_total);
    let runtime_total = window
        .connect_count
        .saturating_add(window.runtime_abort_count)
        .saturating_add(window.provider_closed_count);
    let runtime_success = ratio_score(
        runtime_total.saturating_sub(window.runtime_abort_count.saturating_add(window.provider_closed_count)),
        runtime_total,
    );
    let first_byte_total = window.connect_count.saturating_add(window.first_byte_failure_count);
    let first_byte_success = ratio_score(first_byte_total.saturating_sub(window.first_byte_failure_count), first_byte_total);
    let reconnect_score = window
        .avg_provider_reconnect_count
        .map_or(100, |avg| 100u8.saturating_sub(u8::try_from(avg.min(100)).unwrap_or(100)));
    let capacity_total = startup_total.max(window.startup_capacity_failure_count);
    let capacity_score = ratio_score(
        capacity_total.saturating_sub(window.startup_capacity_failure_count),
        capacity_total,
    );
    let freshness_score: u8 = if window.last_success_ts.is_some() { 100 } else { 0 };

    let weighted = u32::from(startup_success) * 30
        + u32::from(runtime_success) * 25
        + u32::from(first_byte_success) * 15
        + u32::from(reconnect_score) * 10
        + u32::from(capacity_score) * 10
        + u32::from(freshness_score) * 10;
    u8::try_from((weighted / 100).min(100)).unwrap_or(100)
}

fn compute_confidence(sample_size: u64) -> u8 {
    let scaled = sample_size.saturating_mul(10).min(100);
    u8::try_from(scaled).unwrap_or(100)
}

fn ratio_score(successes: u64, total: u64) -> u8 {
    if total == 0 {
        return 100;
    }
    let percent = successes.saturating_mul(100) / total;
    u8::try_from(percent.min(100)).unwrap_or(100)
}

fn average_opt(total: u64, samples: u64) -> Option<u64> {
    total.checked_div(samples)
}

fn max_opt(lhs: Option<u64>, rhs: Option<u64>) -> Option<u64> {
    match (lhs, rhs) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn bucket_in_window(today: Option<NaiveDate>, bucket_day_utc: &str, max_day_distance: i64) -> bool {
    let Some(today) = today else {
        return false;
    };
    let Some(bucket_day) = parse_utc_day(bucket_day_utc) else {
        return false;
    };
    let days = today.signed_duration_since(bucket_day).num_days();
    (0..=max_day_distance).contains(&days)
}

fn parse_utc_day(day: &str) -> Option<NaiveDate> { NaiveDate::parse_from_str(day, "%Y-%m-%d").ok() }

fn discover_history_days(history_dir: &Path) -> io::Result<Vec<String>> {
    if !history_dir.exists() {
        return Ok(Vec::new());
    }

    let mut days = BTreeSet::new();
    for entry in std::fs::read_dir(history_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(day) = extract_day_from_filename(&name) {
            days.insert(day.to_string());
        }
    }
    Ok(days.into_iter().collect())
}

fn select_completed_days_to_process(days: &[String], checkpoint_day: Option<&str>, today_utc: &str) -> Vec<String> {
    days.iter()
        .filter(|day| day.as_str() < today_utc)
        .filter(|day| checkpoint_day.is_none_or(|checkpoint| day.as_str() > checkpoint))
        .cloned()
        .collect()
}

fn aggregate_day_entries(history_dir: &Path, day_utc: &str) -> io::Result<HashMap<String, AggregatedDayEntry>> {
    let files = discover_day_files(history_dir, day_utc)?;
    let mut entries: HashMap<String, AggregatedDayEntry> = HashMap::new();

    for file in files {
        if file.is_archive {
            let reader = StreamHistoryFileReader::from_archive(&file.path, None).map(|(reader, _)| reader)?;
            aggregate_reader_entries(day_utc, reader, &mut entries)?;
        } else {
            let reader = StreamHistoryFileReader::from_pending(&file.path, None).map(|(reader, _)| reader)?;
            aggregate_reader_entries(day_utc, reader, &mut entries)?;
        }
    }

    Ok(entries)
}

fn aggregate_reader_entries<R: io::Read>(
    day_utc: &str,
    reader: StreamHistoryFileReader<R>,
    entries: &mut HashMap<String, AggregatedDayEntry>,
) -> io::Result<()> {
    for record in reader {
        let record = record?;
        if record.partition_day_utc != day_utc {
            continue;
        }
        let Some(stream_identity_key) = record.stream_identity_key.clone() else {
            continue;
        };
        let (Some(input_name), Some(target_id), Some(provider_name), Some(provider_id), Some(virtual_id), Some(item_type)) = (
            record.input_name.clone(),
            record.target_id,
            record.provider_name.clone(),
            record.provider_id,
            record.virtual_id,
            record.item_type.clone(),
        ) else {
            continue;
        };

        let entry = entries.entry(stream_identity_key).or_insert_with(|| AggregatedDayEntry {
            input_name,
            target_id,
            provider_name,
            provider_id,
            virtual_id,
            item_type,
            bucket: QosSnapshotDailyBucket::default(),
        });
        fold_record_into_bucket(&mut entry.bucket, &record);
    }
    Ok(())
}

fn discover_day_files(history_dir: &Path, day_utc: &str) -> io::Result<Vec<HistoryDayFile>> {
    if !history_dir.exists() {
        return Ok(Vec::new());
    }

    let mut archives = Vec::new();
    let mut pending = Vec::new();
    for entry in std::fs::read_dir(history_dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if extract_day_from_filename(&name) != Some(day_utc) {
            continue;
        }
        if name.ends_with(".archive.lz4") {
            archives.push(HistoryDayFile { path, is_archive: true });
        } else if name.ends_with(".pending") {
            pending.push(HistoryDayFile { path, is_archive: false });
        }
    }

    if archives.is_empty() {
        pending.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(pending)
    } else {
        archives.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(archives)
    }
}

fn history_day_revision(history_dir: &Path, day_utc: &str) -> io::Result<Option<(u64, u64)>> {
    let day_files = discover_day_files(history_dir, day_utc)?;
    let mut revision: Option<(u64, u64)> = None;
    for file in day_files {
        let metadata = std::fs::metadata(&file.path)?;
        let modified = metadata.modified()?;
        let secs = modified
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|err| io::Error::other(err.to_string()))?
            .as_secs();
        let len = metadata.len();
        revision = Some(revision.map_or((secs, len), |current| (current.0.max(secs), current.1.saturating_add(len))));
    }
    Ok(revision)
}

fn apply_day_entries(
    repo: &QosSnapshotRepository,
    day_utc: &str,
    day_entries: &HashMap<String, AggregatedDayEntry>,
    today_utc: &str,
) -> io::Result<()> {
    let mut existing_by_key = HashMap::new();
    repo.for_each_snapshot(|snapshot| {
        existing_by_key.insert(snapshot.stream_identity_key.clone(), snapshot.clone());
    })?;
    let mut all_keys = existing_by_key.keys().cloned().collect::<BTreeSet<_>>();
    all_keys.extend(day_entries.keys().cloned());

    for key in all_keys {
        let Some(mut snapshot) = existing_by_key.remove(&key).or_else(|| {
            day_entries.get(&key).map(|entry| QosSnapshotRecord {
                stream_identity_key: key.clone(),
                input_name: entry.input_name.clone(),
                target_id: entry.target_id,
                provider_name: entry.provider_name.clone(),
                provider_id: entry.provider_id,
                virtual_id: entry.virtual_id,
                item_type: entry.item_type.clone(),
                updated_at: now_utc_secs(),
                last_event_at: 0,
                window_24h: QosSnapshotWindow::default(),
                window_7d: QosSnapshotWindow::default(),
                window_30d: QosSnapshotWindow::default(),
                daily_buckets: BTreeMap::new(),
            })
        }) else {
            continue;
        };

        if let Some(entry) = day_entries.get(&key) {
            snapshot.daily_buckets.insert(day_utc.to_string(), entry.bucket.clone());
            snapshot.last_event_at = max_opt(Some(snapshot.last_event_at), max_opt(entry.bucket.last_success_ts, entry.bucket.last_failure_ts)).unwrap_or(snapshot.last_event_at);
        } else {
            snapshot.daily_buckets.remove(day_utc);
        }

        prune_expired_buckets(&mut snapshot, today_utc);
        if snapshot.daily_buckets.is_empty() {
            let _ = repo.delete_snapshot(&snapshot.stream_identity_key)?;
            continue;
        }
        snapshot.updated_at = now_utc_secs();
        rebuild_windows(&mut snapshot, today_utc);
        repo.put_snapshot(&snapshot)?;
    }

    Ok(())
}

fn prune_expired_buckets(snapshot: &mut QosSnapshotRecord, today_utc: &str) {
    let today = parse_utc_day(today_utc);
    snapshot.daily_buckets.retain(|day, _| bucket_in_window(today, day, 29));
}

#[derive(Debug, Clone)]
struct HistoryDayFile {
    path: PathBuf,
    is_archive: bool,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::tempdir;

    use crate::model::{Config, QosAggregationConfig, ReverseProxyConfig, StreamHistoryConfig};
    use crate::repository::{
        current_utc_day, serialize_named, write_block_magic, write_file_magic, write_framed, BlockHeaderBody,
        CompressionKind, ConnectFailureReason, CONTAINER_FORMAT_VERSION, DisconnectReason, EventType, FailureStage,
        FileHeaderBody, QosSnapshotDailyBucket, QosSnapshotRecord, QosSnapshotRepository, QosSnapshotWindow,
        RecordEncodingKind, RECORD_SCHEMA_VERSION, SOURCE_KIND_STREAM_HISTORY, StreamHistoryRecord,
    };

    use super::{fold_record_into_bucket, history_day_revision, qos_aggregation_is_enabled, rebuild_windows, run_aggregation_once};

    fn write_pending_history_records(
        history_dir: &std::path::Path,
        records: Vec<StreamHistoryRecord>,
    ) {
        if records.is_empty() {
            return;
        }
        std::fs::create_dir_all(history_dir).expect("history dir should be created");

        let partition_day = records[0].partition_day_utc.clone();
        let path = history_dir.join(format!("stream-history-{partition_day}.pending"));
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path).expect("pending file should be created"));

        let file_header = FileHeaderBody {
            container_format_version: CONTAINER_FORMAT_VERSION,
            record_schema_version: RECORD_SCHEMA_VERSION,
            source_kind: SOURCE_KIND_STREAM_HISTORY.to_string(),
            created_at_ts_utc: records[0].event_ts_utc,
            partition_day_ts_utc: partition_day,
            writer_instance_id: 1,
            host_id: None,
            compression_kind: CompressionKind::None,
            finalized: false,
            record_encoding_kind: RecordEncodingKind::MessagePackNamed,
            finalized_at_ts_utc: None,
            total_block_count: None,
            total_record_count: None,
            min_event_ts_utc: None,
            max_event_ts_utc: None,
        };
        write_file_magic(&mut file).expect("file magic should write");
        write_framed(&mut file, &file_header).expect("file header should write");

        let mut payload = Vec::new();
        let mut first_ts = u64::MAX;
        let mut last_ts = 0_u64;
        for record in &records {
            first_ts = first_ts.min(record.event_ts_utc);
            last_ts = last_ts.max(record.event_ts_utc);
            let encoded = serialize_named(record).expect("record should serialize");
            let len = u32::try_from(encoded.len()).expect("record len should fit");
            payload.extend_from_slice(&len.to_be_bytes());
            payload.extend_from_slice(&encoded);
        }
        let payload_crc = crc32fast::hash(&payload);
        let block_header = BlockHeaderBody {
            block_version: 1,
            record_count: u32::try_from(records.len()).expect("record count should fit"),
            payload_len: u32::try_from(payload.len()).expect("payload len should fit"),
            first_event_ts_utc: first_ts,
            last_event_ts_utc: last_ts,
            payload_crc,
            flags: 0,
        };
        write_block_magic(&mut file).expect("block magic should write");
        write_framed(&mut file, &block_header).expect("block header should write");
        use std::io::Write as _;
        file.write_all(&payload).expect("payload should write");
        file.flush().expect("pending file should flush");
    }

    fn base_record(event_type: EventType) -> StreamHistoryRecord {
        StreamHistoryRecord {
            schema_version: 1,
            event_type,
            event_ts_utc: 1_700_000_000,
            partition_day_utc: "2026-04-02".to_string(),
            session_id: 42,
            source_addr: None,
            api_username: None,
            provider_name: Some("provider-a".to_string()),
            provider_username: None,
            input_name: Some("input-a".to_string()),
            virtual_id: Some(33),
            item_type: Some("live".to_string()),
            title: None,
            group: None,
            country: None,
            user_agent: None,
            shared: Some(false),
            shared_joined_existing: Some(false),
            shared_stream_id: None,
            provider_id: Some(22),
            cluster: None,
            container: None,
            stream_url_hash: None,
            stream_identity_key: Some("stream-a".to_string()),
            video_codec: None,
            audio_codec: None,
            audio_channels: None,
            resolution: None,
            fps: None,
            connect_ts_utc: None,
            disconnect_ts_utc: None,
            session_duration: None,
            bytes_sent: None,
            first_byte_latency_ms: None,
            provider_reconnect_count: None,
            failure_stage: None,
            provider_http_status: None,
            provider_error_class: None,
            connect_failure_reason: None,
            disconnect_reason: None,
            previous_session_id: None,
            target_id: Some(11),
        }
    }

    #[test]
    fn fold_record_into_bucket_tracks_connect_and_connect_failed() {
        let mut bucket = QosSnapshotDailyBucket::default();

        fold_record_into_bucket(&mut bucket, &base_record(EventType::Connect));

        let mut failed = base_record(EventType::ConnectFailed);
        failed.connect_failure_reason = Some(ConnectFailureReason::ProviderConnectionsExhausted);
        fold_record_into_bucket(&mut bucket, &failed);

        assert_eq!(bucket.connect_count, 1);
        assert_eq!(bucket.connect_failed_count, 1);
        assert_eq!(bucket.startup_capacity_failure_count, 1);
        assert_eq!(bucket.last_failure_ts, Some(failed.event_ts_utc));
    }

    #[test]
    fn fold_record_into_bucket_tracks_disconnect_failure_stages() {
        let mut bucket = QosSnapshotDailyBucket::default();

        let mut first_byte = base_record(EventType::Disconnect);
        first_byte.failure_stage = Some(FailureStage::FirstByte);
        first_byte.disconnect_reason = Some(DisconnectReason::ProviderError);
        first_byte.first_byte_latency_ms = Some(250);
        fold_record_into_bucket(&mut bucket, &first_byte);

        let mut streaming = base_record(EventType::Disconnect);
        streaming.failure_stage = Some(FailureStage::Streaming);
        streaming.disconnect_reason = Some(DisconnectReason::ProviderClosed);
        streaming.session_duration = Some(900);
        streaming.provider_reconnect_count = Some(2);
        fold_record_into_bucket(&mut bucket, &streaming);

        assert_eq!(bucket.first_byte_failure_count, 1);
        assert_eq!(bucket.runtime_abort_count, 1);
        assert_eq!(bucket.provider_closed_count, 1);
        assert_eq!(bucket.total_first_byte_latency_ms, 250);
        assert_eq!(bucket.total_session_duration_secs, 900);
        assert_eq!(bucket.total_provider_reconnect_count, 2);
    }

    #[test]
    fn history_day_revision_tracks_seconds_and_file_length() {
        let temp = tempdir().expect("tempdir should succeed");
        let history_dir = temp.path();
        let record = base_record(EventType::Connect);
        write_pending_history_records(history_dir, vec![record.clone(), record]);

        let revision = history_day_revision(history_dir, "2026-04-02")
            .expect("revision should load")
            .expect("revision should exist");
        let pending_path = history_dir.join("stream-history-2026-04-02.pending");
        let metadata = std::fs::metadata(pending_path).expect("metadata should exist");

        assert!(revision.0 > 0);
        assert_eq!(revision.1, metadata.len());
    }

    #[test]
    fn rebuild_windows_uses_recent_daily_buckets() {
        let mut snapshot = QosSnapshotRecord {
            stream_identity_key: "stream-a".to_string(),
            input_name: "input-a".to_string(),
            target_id: 11,
            provider_name: "provider-a".to_string(),
            provider_id: 22,
            virtual_id: 33,
            item_type: "live".to_string(),
            updated_at: 1_700_000_000,
            last_event_at: 1_700_000_123,
            window_24h: QosSnapshotWindow::default(),
            window_7d: QosSnapshotWindow::default(),
            window_30d: QosSnapshotWindow::default(),
            daily_buckets: BTreeMap::from([
                (
                    "2026-04-02".to_string(),
                    QosSnapshotDailyBucket {
                        connect_count: 2,
                        connect_failed_count: 1,
                        ..QosSnapshotDailyBucket::default()
                    },
                ),
                (
                    "2026-04-01".to_string(),
                    QosSnapshotDailyBucket {
                        connect_count: 3,
                        connect_failed_count: 1,
                        ..QosSnapshotDailyBucket::default()
                    },
                ),
                (
                    "2026-03-20".to_string(),
                    QosSnapshotDailyBucket {
                        connect_count: 7,
                        connect_failed_count: 2,
                        ..QosSnapshotDailyBucket::default()
                    },
                ),
            ]),
        };

        rebuild_windows(&mut snapshot, "2026-04-02");

        assert_eq!(snapshot.window_24h.connect_count, 5);
        assert_eq!(snapshot.window_7d.connect_count, 5);
        assert_eq!(snapshot.window_30d.connect_count, 12);
        assert!(snapshot.window_24h.score <= 100);
        assert!(snapshot.window_24h.confidence <= 100);
    }

    #[test]
    fn qos_aggregation_is_enabled_requires_stream_history() {
        let mut config = Config::default();
        config.reverse_proxy = Some(ReverseProxyConfig {
            resource_rewrite_disabled: false,
            rewrite_secret: [0; 16],
            resource_retry: Default::default(),
            disabled_header: None,
            stream: None,
            cache: None,
            rate_limit: None,
            geoip: None,
            stream_history: Some(StreamHistoryConfig {
                stream_history_enabled: false,
                ..Default::default()
            }),
            qos_aggregation: Some(QosAggregationConfig {
                enabled: true,
                interval_secs: 300,
            }),
        });

        assert!(!qos_aggregation_is_enabled(&config));

        if let Some(reverse_proxy) = config.reverse_proxy.as_mut() {
            if let Some(history) = reverse_proxy.stream_history.as_mut() {
                history.stream_history_enabled = true;
            }
        }
        assert!(qos_aggregation_is_enabled(&config));
    }

    #[tokio::test]
    async fn run_aggregation_once_persists_current_day_snapshot() {
        let temp = tempdir().expect("tempdir should succeed");
        let history_dir = temp.path().join("stream_history");
        let today = current_utc_day();
        let mut connect = base_record(EventType::Connect);
        connect.partition_day_utc = today.clone();
        connect.input_name = Some("input-a".to_string());
        connect.provider_name = Some("provider-a".to_string());
        connect.provider_id = Some(22);
        connect.virtual_id = Some(33);
        connect.item_type = Some("live".to_string());
        connect.target_id = Some(11);
        connect.stream_identity_key = Some("stream-a".to_string());
        connect.shared_joined_existing = Some(false);
        write_pending_history_records(&history_dir, vec![connect]);

        let repo = QosSnapshotRepository::open(temp.path()).expect("repo should open");
        run_aggregation_once(&repo, &history_dir, &today).expect("aggregation should succeed");

        let snapshot = repo
            .get_snapshot("stream-a")
            .expect("load snapshot should succeed")
            .expect("snapshot should exist");
        assert_eq!(snapshot.window_24h.connect_count, 1);
        assert!(snapshot.daily_buckets.contains_key(&today));
    }

    #[tokio::test]
    async fn run_aggregation_once_rereads_current_day_without_double_counting() {
        let temp = tempdir().expect("tempdir should succeed");
        let history_dir = temp.path().join("stream_history");
        let today = current_utc_day();

        let mut connect = base_record(EventType::Connect);
        connect.partition_day_utc = today.clone();
        connect.input_name = Some("input-a".to_string());
        connect.provider_name = Some("provider-a".to_string());
        connect.provider_id = Some(22);
        connect.virtual_id = Some(33);
        connect.item_type = Some("live".to_string());
        connect.target_id = Some(11);
        connect.stream_identity_key = Some("stream-a".to_string());
        connect.shared_joined_existing = Some(false);
        write_pending_history_records(&history_dir, vec![connect]);

        let repo = QosSnapshotRepository::open(temp.path()).expect("repo should open");

        run_aggregation_once(&repo, &history_dir, &today).expect("first aggregation should succeed");
        run_aggregation_once(&repo, &history_dir, &today).expect("second aggregation should succeed");

        let snapshot = repo
            .get_snapshot("stream-a")
            .expect("load snapshot should succeed")
            .expect("snapshot should exist");
        assert_eq!(snapshot.window_24h.connect_count, 1);
        assert_eq!(snapshot.daily_buckets.get(&today).map(|bucket| bucket.connect_count), Some(1));
    }

    #[tokio::test]
    async fn run_aggregation_once_advances_checkpoint_across_multiple_completed_days() {
        let temp = tempdir().expect("tempdir should succeed");
        let history_dir = temp.path().join("stream_history");

        for (day, session_id) in [
            ("2036-04-01", 1_u64),
            ("2036-04-02", 2_u64),
            ("2036-04-03", 3_u64),
            ("2036-04-04", 4_u64),
        ] {
            let mut connect = base_record(EventType::Connect);
            connect.partition_day_utc = day.to_string();
            connect.event_ts_utc = connect.event_ts_utc.saturating_add(session_id);
            connect.session_id = session_id;
            connect.input_name = Some("input-a".to_string());
            connect.provider_name = Some("provider-a".to_string());
            connect.provider_id = Some(22);
            connect.virtual_id = Some(33);
            connect.item_type = Some("live".to_string());
            connect.target_id = Some(11);
            connect.stream_identity_key = Some("stream-a".to_string());
            connect.shared_joined_existing = Some(false);
            write_pending_history_records(&history_dir, vec![connect]);
        }

        let repo = QosSnapshotRepository::open(temp.path()).expect("repo should open");

        run_aggregation_once(&repo, &history_dir, "2036-04-05").expect("first aggregation should succeed");
        let checkpoint = repo.load_checkpoint().expect("checkpoint should load");
        assert_eq!(checkpoint.last_completed_day_utc.as_deref(), Some("2036-04-03"));

        let snapshot = repo
            .get_snapshot("stream-a")
            .expect("load snapshot should succeed")
            .expect("snapshot should exist");
        assert_eq!(snapshot.daily_buckets.len(), 3);
        assert_eq!(snapshot.window_7d.connect_count, 3);

        run_aggregation_once(&repo, &history_dir, "2036-04-05").expect("second aggregation should succeed");
        let checkpoint = repo.load_checkpoint().expect("checkpoint should load");
        assert_eq!(checkpoint.last_completed_day_utc.as_deref(), Some("2036-04-04"));

        let snapshot = repo
            .get_snapshot("stream-a")
            .expect("load snapshot should succeed")
            .expect("snapshot should exist");
        assert_eq!(snapshot.daily_buckets.len(), 4);
        assert_eq!(snapshot.window_7d.connect_count, 4);
    }
}
