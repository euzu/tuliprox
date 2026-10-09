use super::{BLOCKING_DB_MAX_CONCURRENCY, BLOCKING_DB_MIN_CONCURRENCY};
use crate::ctx::MetadataUpdateCtx;
use shared::model::EventSink;
use std::{sync::OnceLock, time::Duration};
use tokio::sync::Semaphore;
use tuliprox_core::model::MetadataUpdateConfig;

fn metadata_blocking_concurrency_limit() -> usize {
    let parallelism =
        std::thread::available_parallelism().map_or(BLOCKING_DB_MIN_CONCURRENCY, std::num::NonZeroUsize::get);
    parallelism.saturating_mul(2).clamp(BLOCKING_DB_MIN_CONCURRENCY, BLOCKING_DB_MAX_CONCURRENCY)
}

fn metadata_blocking_semaphore() -> &'static Semaphore {
    static BLOCKING_SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();
    BLOCKING_SEMAPHORE.get_or_init(|| Semaphore::new(metadata_blocking_concurrency_limit()))
}

pub(super) async fn spawn_blocking_limited<F, R>(task: F) -> Result<R, tokio::task::JoinError>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    // Throttle B+Tree and file-heavy blocking tasks to avoid saturating Tokio's blocking pool.
    let permit = metadata_blocking_semaphore().acquire().await.ok();
    let result = tokio::task::spawn_blocking(task).await;
    drop(permit);
    result
}

#[derive(Debug, Clone)]
pub(super) struct MetadataUpdateRuntimeSettings {
    pub(super) queue_log_interval: Duration,
    pub(super) progress_log_interval: Duration,
    pub(super) max_resolve_retry_backoff_secs: u64,
    pub(super) resolve_min_retry_base_secs: u64,
    pub(super) max_attempts_resolve: u8,
    pub(super) max_attempts_probe: u8,
    pub(super) resolve_exhaustion_reset_gap_secs: i64,
    pub(super) probe_cooldown_secs: i64,
    pub(super) tmdb_cooldown_secs: i64,
    pub(super) retry_delay_secs: u64,
    pub(super) metadata_retry_load_retry_delay_secs: i64,
    pub(super) worker_idle_timeout_secs: u64,
    pub(super) max_queue_size: usize,
    pub(super) no_change_cache_ttl_secs: u64,
    pub(super) probe_fairness_resolve_burst: usize,
    pub(super) probe_retry_backoff_step_1_secs: u64,
    pub(super) probe_retry_backoff_step_2_secs: u64,
    pub(super) probe_retry_backoff_step_3_secs: u64,
    pub(super) backoff_jitter_percent: u8,
}

impl Default for MetadataUpdateRuntimeSettings {
    fn default() -> Self {
        let defaults = MetadataUpdateConfig::default();
        Self::from_metadata_update(&defaults)
    }
}

impl MetadataUpdateRuntimeSettings {
    pub(super) fn from_ctx<E: EventSink + Clone + 'static>(ctx: Option<&MetadataUpdateCtx<E>>) -> Self {
        let metadata_update = ctx.map_or_else(MetadataUpdateConfig::default, |ctx| {
            ctx.app_config
                .config
                .load()
                .metadata_update
                .as_ref()
                .map_or_else(MetadataUpdateConfig::default, Clone::clone)
        });
        Self::from_metadata_update(&metadata_update)
    }

    pub(super) fn from_metadata_update(cfg: &MetadataUpdateConfig) -> Self {
        let to_i64 = |v: u64| i64::try_from(v.max(1)).unwrap_or(i64::MAX);
        Self {
            queue_log_interval: Duration::from_secs(cfg.log.queue_interval_secs.max(1)),
            progress_log_interval: Duration::from_secs(cfg.log.progress_interval_secs.max(1)),
            max_resolve_retry_backoff_secs: cfg.resolve.max_retry_backoff_secs.max(1),
            resolve_min_retry_base_secs: cfg.resolve.min_retry_base_secs.max(1),
            max_attempts_resolve: cfg.resolve.max_attempts.max(1),
            max_attempts_probe: cfg.probe.max_attempts.max(1),
            resolve_exhaustion_reset_gap_secs: to_i64(cfg.resolve.exhaustion_reset_gap_secs),
            probe_cooldown_secs: to_i64(cfg.probe.cooldown_secs),
            tmdb_cooldown_secs: to_i64(cfg.tmdb.cooldown_secs),
            retry_delay_secs: cfg.retry_delay_secs.max(1),
            metadata_retry_load_retry_delay_secs: to_i64(cfg.probe.retry_load_retry_delay_secs),
            worker_idle_timeout_secs: cfg.worker_idle_timeout_secs.max(1),
            max_queue_size: cfg.max_queue_size.max(1),
            no_change_cache_ttl_secs: cfg.no_change_cache_ttl_secs.max(1),
            probe_fairness_resolve_burst: cfg.probe_fairness_resolve_burst.max(1),
            probe_retry_backoff_step_1_secs: cfg.probe.retry_backoff_step_1_secs.max(1),
            probe_retry_backoff_step_2_secs: cfg.probe.retry_backoff_step_2_secs.max(1),
            probe_retry_backoff_step_3_secs: cfg.probe.retry_backoff_step_3_secs.max(1),
            backoff_jitter_percent: cfg.probe.backoff_jitter_percent.min(95),
        }
    }
}
