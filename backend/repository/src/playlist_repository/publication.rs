use crate::xtream_write_playlist;
use shared::{
    error::TuliproxError,
    model::{ClusterFlags, PlaylistGroup, XtreamCluster},
};
use std::sync::Arc;
use tuliprox_core::model::{apply_filter_to_playlist, AppConfig, ConfigTarget, TargetOutput};

pub(super) fn playlist_has_items(playlist: &[PlaylistGroup]) -> bool {
    playlist.iter().any(|group| !group.channels.is_empty())
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) enum TargetPersistenceMode {
    #[default]
    Persist,
    #[cfg(test)]
    FailCacheReloadAt(TargetCacheReloadStage),
    #[cfg(test)]
    FailEmptyReplacementAt(crate::TargetEmptyReplacementFailure),
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TargetCacheReloadStage {
    IdMapping,
    XtreamStorage,
}

/// Scoped persistence behavior derived from technically successful forced clusters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputPlaylistPersistOptions {
    pub accepted_empty_clusters: ClusterFlags,
}

impl Default for InputPlaylistPersistOptions {
    fn default() -> Self { Self { accepted_empty_clusters: ClusterFlags::empty() } }
}

/// Empty Library contribution proven by successful input jobs, before target transforms.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LibraryEmptyPublication {
    #[default]
    None,
    /// Empty Library contribution without permission to clear the complete target/output.
    Contribution,
    /// All inputs are ready, non-Library inputs are populated, and no forced-empty cluster is involved.
    /// Configured transformations may remove the remaining entries from a target or individual output.
    FilterableContribution,
    /// All required inputs are empty Library catalogs, or the proven complete inputs
    /// have been reduced to an empty target/output by the configured transformations.
    CompleteTarget,
}

impl LibraryEmptyPublication {
    #[must_use]
    pub const fn replaces_empty_target(self) -> bool { matches!(self, Self::CompleteTarget) }

    /// Resolve the input-level authorization against the actual prepared target/output.
    /// A plain contribution never authorizes a fully empty result from a foreign input.
    #[must_use]
    pub fn for_filtered_playlist(self, playlist: &[PlaylistGroup]) -> Self {
        match self {
            Self::FilterableContribution if !playlist_has_items(playlist) => Self::CompleteTarget,
            Self::None | Self::Contribution | Self::FilterableContribution | Self::CompleteTarget => self,
        }
    }

    pub(super) fn replacement_clusters(self) -> ClusterFlags {
        match self {
            Self::None => ClusterFlags::empty(),
            Self::Contribution | Self::FilterableContribution => ClusterFlags::Vod | ClusterFlags::Series,
            Self::CompleteTarget => ClusterFlags::all(),
        }
    }
}

/// Output replacement authority derived only from a complete curation run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistPublicationPlan {
    Ordinary,
    CompleteCuration {
        intentionally_empty_base_vod: bool,
        intentionally_empty_base_series: bool,
        intentionally_empty_xtream_vod: bool,
        intentionally_empty_xtream_series: bool,
    },
}

impl PlaylistPublicationPlan {
    #[must_use]
    pub const fn complete_curation(curated_catalog: bool, xtream_base_suppressed: bool) -> Self {
        Self::complete_curation_with_filter(curated_catalog, xtream_base_suppressed, false)
    }

    #[must_use]
    pub const fn complete_curation_with_filter(
        curated_catalog: bool,
        xtream_base_suppressed: bool,
        appearance_filter_configured: bool,
    ) -> Self {
        Self::CompleteCuration {
            intentionally_empty_base_vod: curated_catalog || appearance_filter_configured,
            intentionally_empty_base_series: curated_catalog || appearance_filter_configured,
            intentionally_empty_xtream_vod: curated_catalog || xtream_base_suppressed || appearance_filter_configured,
            intentionally_empty_xtream_series: curated_catalog
                || xtream_base_suppressed
                || appearance_filter_configured,
        }
    }

    #[must_use]
    pub const fn with_output_filter(self, output_filter_configured: bool) -> Self {
        if !output_filter_configured {
            return self;
        }
        match self {
            Self::Ordinary => Self::Ordinary,
            Self::CompleteCuration { .. } => Self::CompleteCuration {
                intentionally_empty_base_vod: true,
                intentionally_empty_base_series: true,
                intentionally_empty_xtream_vod: true,
                intentionally_empty_xtream_series: true,
            },
        }
    }

    #[must_use]
    pub const fn allows_empty_xtream_cluster(self, cluster: XtreamCluster) -> bool {
        matches!(
            (self, cluster),
            (Self::CompleteCuration { intentionally_empty_xtream_vod: true, .. }, XtreamCluster::Video)
                | (Self::CompleteCuration { intentionally_empty_xtream_series: true, .. }, XtreamCluster::Series)
        )
    }

    #[must_use]
    pub const fn allows_empty_base_output(self) -> bool {
        matches!(
            self,
            Self::CompleteCuration { intentionally_empty_base_vod: true, .. }
                | Self::CompleteCuration { intentionally_empty_base_series: true, .. }
        )
    }

    #[must_use]
    pub const fn allows_any_empty_output(self) -> bool {
        self.allows_empty_base_output()
            || self.allows_empty_xtream_cluster(XtreamCluster::Video)
            || self.allows_empty_xtream_cluster(XtreamCluster::Series)
    }

    pub(super) fn replacement_clusters(self) -> ClusterFlags {
        let mut clusters = ClusterFlags::empty();
        if self.allows_empty_xtream_cluster(XtreamCluster::Video) {
            clusters |= ClusterFlags::Vod;
        }
        if self.allows_empty_xtream_cluster(XtreamCluster::Series) {
            clusters |= ClusterFlags::Series;
        }
        clusters
    }
}

/// Target persistence behavior authorized by successful input and curation results, not by Quality inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetPlaylistPersistOptions {
    pub accepted_empty_clusters: ClusterFlags,
    pub library_empty: LibraryEmptyPublication,
    pub publication_plan: PlaylistPublicationPlan,
}

impl Default for TargetPlaylistPersistOptions {
    fn default() -> Self {
        Self {
            accepted_empty_clusters: ClusterFlags::empty(),
            library_empty: LibraryEmptyPublication::None,
            publication_plan: PlaylistPublicationPlan::Ordinary,
        }
    }
}

pub(super) fn validate_target_playlist_persistence(
    target: &ConfigTarget,
    playlist_is_empty: bool,
    options: TargetPlaylistPersistOptions,
) -> Result<(), TuliproxError> {
    if !playlist_is_empty
        || options.library_empty.replaces_empty_target()
        || options.publication_plan.allows_any_empty_output()
    {
        return Ok(());
    }
    if options.accepted_empty_clusters.is_empty() {
        return Err(TuliproxError::RepositoryPlaylist(format!(
            "Refusing to persist empty playlist for target '{}'; existing data was retained",
            target.name
        )));
    }
    if target.output.iter().any(|output| !matches!(output, TargetOutput::Xtream(_))) {
        return Err(TuliproxError::RepositoryPlaylist(format!(
            "Target '{}' has non-Xtream outputs that cannot safely publish a fully empty forced result; existing data was retained",
            target.name
        )));
    }
    Ok(())
}

pub(super) fn prepare_target_output_playlists(
    target: &ConfigTarget,
    base_playlist: &[PlaylistGroup],
    xtream_playlist: Option<&[PlaylistGroup]>,
) -> Vec<Option<Vec<PlaylistGroup>>> {
    target
        .output
        .iter()
        .map(|output| {
            let source_playlist = match output {
                TargetOutput::Xtream(_) => xtream_playlist.unwrap_or(base_playlist),
                _ => base_playlist,
            };
            output.filter().map(|filter| apply_filter_to_playlist(source_playlist, filter))
        })
        .collect()
}

pub(super) fn validate_force_empty_output_filters(
    target: &ConfigTarget,
    prepared_outputs: &[Option<Vec<PlaylistGroup>>],
    options: TargetPlaylistPersistOptions,
) -> Result<(), TuliproxError> {
    if options.accepted_empty_clusters.is_empty() {
        return Ok(());
    }

    debug_assert_eq!(target.output.len(), prepared_outputs.len());
    for (output, prepared_output) in target.output.iter().zip(prepared_outputs) {
        let output_name = match output {
            TargetOutput::M3u(_) => "M3U",
            TargetOutput::Strm(_) => "STRM",
            TargetOutput::Xtream(_) | TargetOutput::HdHomeRun(_) => continue,
        };
        let Some(filtered_playlist) = prepared_output else {
            continue;
        };
        let curation_allows_empty =
            options.publication_plan.with_output_filter(output.filter().is_some()).allows_empty_base_output();
        if !playlist_has_items(filtered_playlist) && !curation_allows_empty {
            return Err(TuliproxError::RepositoryPlaylist(format!(
                "Refusing to publish force-empty {output_name} output for target '{}' after its output filter; existing data was retained",
                target.name
            )));
        }
    }

    Ok(())
}

pub(super) async fn persist_xtream_target_playlist(
    app_config: &Arc<AppConfig>,
    target: &ConfigTarget,
    playlist: &mut [PlaylistGroup],
    accepted_empty_clusters: ClusterFlags,
    mode: TargetPersistenceMode,
) -> Result<(), TuliproxError> {
    #[cfg(test)]
    if let TargetPersistenceMode::FailEmptyReplacementAt(failure) = mode {
        return crate::xtream_write_playlist_with_injected_empty_replacement_failure(
            app_config,
            target,
            playlist,
            accepted_empty_clusters,
            failure,
        )
        .await;
    }
    #[cfg(not(test))]
    let _ = mode;

    xtream_write_playlist(app_config, target, playlist, accepted_empty_clusters).await
}
