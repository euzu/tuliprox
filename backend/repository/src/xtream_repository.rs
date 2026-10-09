use super::playlist_mem_cache::PlaylistStorageState;
use serde::{Deserialize, Serialize};
use shared::{error::TuliproxError, model::XtreamCluster, utils::arc_str_serde};
use std::sync::Arc;
use tuliprox_core::{
    model::{ClusterForceUpdate, ClusterUpdateAcceptance, ClusterUpdateRejection},
    utils::request::DynReader,
};

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetEmptyReplacementFailure {
    CategoryPersistence,
    BTreePersistence,
    Publication,
}

// `CategoryKey` lives in `model`; re-exported for this layer's call sites.
pub use tuliprox_core::model::CategoryKey;

#[derive(Serialize, Deserialize)]
pub struct CategoryEntry {
    pub category_id: u32,
    #[serde(with = "arc_str_serde")]
    pub category_name: Arc<str>,
    pub parent_id: u32,
}

/// The stored cluster if the mapping has one, otherwise the item type's own.
///
/// Was a `try_cluster!` returning a `Result` whose error arm was unreachable:
/// the fallback went through `XtreamCluster::try_from(..).ok()`, which is always
/// `Some`, so `ok_or_else` never fired.
macro_rules! cluster_or_item_type {
    ($xtream_cluster:expr, $item_type:expr) => {
        $xtream_cluster.unwrap_or_else(|| $item_type.cluster())
    };
}

const BATCH_SIZE: usize = 1000;

/// Result of publishing one fully staged Xtream input cluster.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum XtreamClusterPublishOutcome {
    /// The staged database and categories replaced the active cluster.
    Published,
    /// The staged candidate passed its configured Quality guard and was published.
    QualityAccepted(ClusterUpdateAcceptance),
    /// The staged candidate was published through a request-local quality bypass.
    ForcePublished(ClusterForceUpdate),
    /// The staged candidate was rejected and the active cluster was retained.
    RetainedPrevious(ClusterUpdateRejection),
}

/// Decision reports, completed publications, and technical errors from one
/// ordered disk-based Xtream batch.
///
/// Quality reports are recorded when the existing guard evaluates a cluster;
/// `outcomes` records only the later publication/retention result. Keeping the
/// two facts separate lets callers retain an evaluated decision when a
/// subsequent technical step fails.
#[derive(Debug, Default)]
pub struct XtreamClusterPublishBatchResult {
    /// Clusters whose publication or retention completed.
    pub outcomes: Vec<XtreamClusterPublishOutcome>,
    /// Quality acceptances evaluated before any later technical failure.
    pub quality_acceptances: Vec<ClusterUpdateAcceptance>,
    /// Quality rejections evaluated before any later technical failure.
    pub quality_rejections: Vec<ClusterUpdateRejection>,
    /// Request-local Quality bypasses evaluated before any later technical failure.
    pub force_updates: Vec<ClusterForceUpdate>,
    /// Technical failures that stopped the ordered batch.
    pub errors: Vec<TuliproxError>,
    /// Known failing cluster; batch-wide failures remain unscoped.
    pub failed_cluster: Option<XtreamCluster>,
}

/// Quality behavior for one staged Xtream cluster publication.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum XtreamClusterQualityPolicy {
    Enforce { threshold: u8 },
    Bypass { configured_threshold: u8 },
}

/// Readers and publication policy for one fully independent Xtream cluster refresh.
pub struct XtreamClusterRefreshRequest {
    pub cluster: XtreamCluster,
    pub quality: XtreamClusterQualityPolicy,
    pub categories: DynReader,
    pub streams: DynReader,
}

#[cfg(test)]
mod tests;

mod details;
mod input_refresh;
mod paths;
mod publish;
mod read;
mod target_publish;

#[cfg(test)]
use self::details::preserve_details_with_injected_operation_failure;
#[cfg(test)]
use self::details::DetailPreservationOperation;
#[cfg(test)]
use self::input_refresh::{
    count_xtream_tree_entries, evaluate_staged_xtream_cluster_quality,
    persist_input_xtream_playlist_clusters_to_disk_with_operations, XtreamClusterStageOperations,
};
#[cfg(windows)]
use self::paths::encode_windows_path;
#[cfg(all(test, unix))]
use self::paths::refresh_staging_path;
#[cfg(all(test, not(windows)))]
use self::publish::publish_staged_file_with_parent_sync;
#[cfg(all(not(unix), not(windows)))]
use self::publish::sync_published_file_parent;
#[cfg(unix)]
use self::publish::sync_published_file_parent;
#[cfg(windows)]
use self::publish::sync_published_file_parent;
#[cfg(all(not(unix), not(windows)))]
use self::target_publish::move_target_file_platform;
#[cfg(windows)]
use self::target_publish::move_target_file_platform;
#[cfg(test)]
pub(crate) use self::target_publish::xtream_write_playlist_with_injected_empty_replacement_failure;
#[cfg(test)]
use self::target_publish::TargetEmptyPublicationHook;
#[cfg(test)]
use self::target_publish::{xtream_write_playlist_with_mode, TargetEmptyReplacementMode};
use self::{
    details::{cluster_flag, preserve_details_input_xtream_playlist_cluster_to_disk, PreserveDetailsOutcome},
    input_refresh::{XtreamClusterEvaluationReport, XtreamRefreshLease},
    paths::{target_category_lock_path, XtreamRefreshPaths},
    publish::{publish_staged_file_same_directory, save_xtream_categories_to_file},
    read::{get_map_item_as_str, xtream_get_file_path_for_name},
    target_publish::write_playlists_to_file,
};
pub use self::{
    details::{
        merge_preserved_stream_properties, needs_update_info_details, persist_input_info_batch,
        persist_input_live_info, persist_input_live_info_batch, persist_input_series_info_batch,
        persist_input_vod_info, persist_input_vod_info_batch, persists_input_series_info,
    },
    input_refresh::{persist_input_xtream_playlist_cluster_to_disk, persist_input_xtream_playlist_clusters_to_disk},
    paths::{
        ensure_xtream_storage_path, get_collection_path, get_live_cat_collection_path, get_series_cat_collection_path,
        get_vod_cat_collection_path, xtream_cluster_category_collection, xtream_get_collection_path,
        xtream_get_file_path, xtream_get_storage_path,
    },
    read::{
        count_input_xtream_cluster, iter_raw_xtream_input_playlist, iter_raw_xtream_target_playlist,
        load_input_xtream_playlist, persist_input_xtream_playlist,
        persist_input_xtream_playlist_with_empty_replacements, playlist_iter_to_stream,
        xtream_get_epg_file_path_for_target, xtream_get_item_for_stream_id, xtream_get_playlist_categories,
        xtream_load_rewrite_playlist,
    },
    target_publish::{write_playlist_batch_item_upsert, write_playlist_item_update, xtream_write_playlist},
};
