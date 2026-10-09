#![allow(clippy::wildcard_imports)]
use super::*;
use crate::parser::xmltv::TVGuide;
use shared::{
    foundation::{get_filter, MapperScript},
    model::{
        ClusterFlags, ConfigInputDto, ConfigTargetDto, ConfigTargetOptions, M3uTargetOutputDto, MappingStage,
        PersistedPlaylistUpdateClusterSnapshot, PersistedPlaylistUpdateQualityDecision,
        PersistedPlaylistUpdateTechnicalState, PlaylistItem, PlaylistItemHeader, PlaylistItemType,
        PlaylistUpdateClusterDecision, PlaylistUpdateDataSource, PlaylistUpdateRunOrder, TargetOutputDto,
        TraktApiConfigDto, TraktCatalogSelection, TraktConfigDto, TraktContentType, TraktListConfigDto, UUIDType,
        XtreamCluster, XtreamPlaylistItem, XtreamTargetOutputDto,
    },
    utils::Internable,
};
use tuliprox_core::model::{CompiledMappingRule, CompiledTargetMappings, Config, ConfigInput, ConfigInputAlias};
use tuliprox_repository::load_input_playlist;

mod curation_publication;

mod mapping_stage;

mod curation_effect_gate;

#[cfg(test)]
mod quality_rejection_fallback;

#[cfg(test)]
mod disk_epg_wireup_tests {
    use super::spill_epg_to_disk;
    use shared::model::EpgChannel;
    use std::sync::Arc;
    use tuliprox_core::model::Epg;

    /// Build an `Epg` with `channel_count` channels whose ids follow the
    /// `id_base` prefix. Two sources built with the same `id_base` and
    /// `channel_count` will share all channel ids, which is what we need to
    /// exercise the priority-override `Occupied` branch in
    /// `EpgMergeAccumulator::upsert_channel`.
    fn build_epg(id_base: &str, priority: i16, channel_count: usize) -> Epg {
        Epg {
            priority,
            logo_override: false,
            attributes: None,
            children: (0..channel_count)
                .map(|i| {
                    let id: Arc<str> = format!("{id_base}-ch-{i:04}").into();
                    Arc::new(EpgChannel {
                        id: Arc::clone(&id),
                        title: Some(format!("title-{priority}-{i}").into()),
                        icon: None,
                        programmes: vec![shared::model::EpgProgramme::new(
                            i64::try_from(i).expect("test index fits in i64"),
                            i64::try_from(i + 1).expect("test index fits in i64"),
                            id,
                        )],
                    })
                })
                .collect(),
        }
    }

    /// Wire-up regression guard: `spill_epg_to_disk` is the function called
    /// by `finalize_prepared_target` when `disk_based_processing = true`. It
    /// must (a) preserve per-source priority on shared channels, (b) clean up
    /// its temp files, and (c) merge into a single `Epg` of the right size.
    ///
    /// Both sources share channel ids (`shared-ch-NNNN`), forcing the merge
    /// to take the `Occupied` branch in `EpgMergeAccumulator::upsert_channel`.
    /// The lower-priority source (priority 3) must win, the higher-priority
    /// (priority 7) must be discarded for shared ids. Without this assertion
    /// the test would pass even if priority resolution were broken — the
    /// earlier version used unique ids and therefore never hit the merge path.
    #[test]
    fn spill_epg_to_disk_merges_shared_channels_by_priority() {
        let epg_low = build_epg("shared", 3, 50); // wins on every shared channel
        let epg_high = build_epg("shared", 7, 50); // discarded on every shared channel

        let merged = spill_epg_to_disk(vec![epg_low, epg_high])
            .expect("disk merge returned an error")
            .expect("merged Epg is unexpectedly None for two non-empty sources");

        // 50 distinct channels, not 100 — the merge must have collapsed the
        // shared ids.
        assert_eq!(merged.children.len(), 50, "shared channel ids must collapse to one entry, not be duplicated");

        // Every channel title comes from the lower-priority source. If the
        // merge logic is wrong, some titles will carry the "-7-" marker.
        for ch in &merged.children {
            let title = ch.title.as_deref().expect("title preserved through merge");
            assert!(
                title.starts_with("title-3-"),
                "channel {:?} kept title {title:?} from higher-priority source; \
                 priority override is broken",
                ch.id,
            );
            // `add_channel_with_programmes` on the disk-merge path must
            // preserve the lower-priority source's single programme per
            // channel — `upsert_channel` would silently drop them.
            assert_eq!(ch.programmes.len(), 1, "channel {:?} lost programmes through the disk-merge path", ch.id);
            let prog = &ch.programmes[0];
            assert!(prog.title.is_none() || prog.title.as_deref() != Some("title-7"));
        }
    }

    /// The non-shared case: sources with disjoint channel ids. Both
    /// sources' channels appear in the result with no priority loss (no
    /// `Occupied` branch is taken).
    #[test]
    fn spill_epg_to_disk_keeps_disjoint_sources_intact() {
        let epg_low = build_epg("src-a", 3, 50);
        let epg_high = build_epg("src-b", 7, 50);

        let merged = spill_epg_to_disk(vec![epg_low, epg_high])
            .expect("disk merge returned an error")
            .expect("merged Epg is unexpectedly None for two non-empty sources");

        assert_eq!(merged.children.len(), 100, "disjoint ids must not collapse");
        assert!(merged.children.iter().any(|ch| ch.title.as_deref() == Some("title-3-0")));
        assert!(merged.children.iter().any(|ch| ch.title.as_deref() == Some("title-7-0")));
    }

    #[test]
    fn spill_epg_to_disk_returns_none_for_empty_input() {
        let merged = spill_epg_to_disk(vec![]).expect("disk merge returned an error");
        assert!(merged.is_none());
    }
}

mod curation_selection;

mod filter_rename;

mod identity_serialization;

mod pipeline_transparency;

mod probe_details;

mod scheduling;

mod staged_overlay;

mod support;

use self::support::{
    apply_staged_overlay_groups, catalog_membership, catalog_test_item, complete_catalog_evaluation,
    playlist_update_run_result, test_group, PlaylistRunCollectSink,
};
