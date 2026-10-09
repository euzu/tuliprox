#[cfg(unix)]
use super::refresh_staging_path;
use super::{
    count_input_xtream_cluster, get_collection_path, load_input_xtream_playlist, merge_preserved_stream_properties,
    needs_update_info_details, persist_input_xtream_playlist, persist_input_xtream_playlist_cluster_to_disk,
    persist_input_xtream_playlist_clusters_to_disk, persist_input_xtream_playlist_clusters_to_disk_with_operations,
    persists_input_series_info, preserve_details_input_xtream_playlist_cluster_to_disk,
    preserve_details_with_injected_operation_failure, publish_staged_file_same_directory, target_category_lock_path,
    xtream_cluster_category_collection, xtream_get_playlist_categories, xtream_write_playlist,
    xtream_write_playlist_with_injected_empty_replacement_failure, xtream_write_playlist_with_mode,
    DetailPreservationOperation, PreserveDetailsOutcome, TargetEmptyPublicationHook, TargetEmptyReplacementFailure,
    TargetEmptyReplacementMode, XtreamClusterPublishOutcome, XtreamClusterQualityPolicy, XtreamClusterRefreshRequest,
    XtreamClusterStageOperations, XtreamRefreshLease, XtreamRefreshPaths,
};

mod behavior;
mod lifecycle;
mod persistence;
mod query;
mod support;

use self::support::{
    fixed_refresh_paths, make_live_item, target_writer_config, target_writer_group, test_app_config,
    write_detail_preservation_fixture, write_single_item,
};

mod behavior_detail_preservation;
mod behavior_quality_acceptance;
mod behavior_refresh_publication;
