use super::{create_input_stat, InputJobResult, InputJobState};
use shared::{
    error::TuliproxError,
    model::{
        ClusterFlags, EventMessage, EventSink, InputType, PlaylistGroup, PlaylistItem, PlaylistItemHeader,
        PlaylistItemType, StagedInputType, UUIDType, XtreamCluster,
    },
    utils::Internable,
};
use std::sync::Arc;
use tuliprox_core::model::ConfigInput;
use tuliprox_curation::{
    CurationEvaluation, CurationMediaKind, CurationMembership, CurationSelectorKey, CurationSelectorSummary,
};

pub(in crate::processor::playlist::tests) fn apply_staged_overlay_groups(
    provider: &ConfigInput,
    clusters: ClusterFlags,
    provider_groups: Vec<PlaylistGroup>,
    staged_groups: Vec<PlaylistGroup>,
) -> Vec<PlaylistGroup> {
    super::super::apply_staged_overlay_groups(
        provider,
        StagedInputType::Xtream,
        clusters,
        provider_groups,
        staged_groups,
    )
}

#[derive(Clone, Default)]
pub(in crate::processor::playlist::tests) struct PlaylistRunCollectSink(
    pub(in crate::processor::playlist::tests) Arc<std::sync::Mutex<Vec<EventMessage>>>,
);

impl EventSink for PlaylistRunCollectSink {
    fn emit(&self, event: EventMessage) {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(event);
    }
}

pub(in crate::processor::playlist::tests) fn playlist_update_run_result(
    input_id: u16,
    state: InputJobState,
    errors: Vec<TuliproxError>,
    had_quality_rejections: bool,
) -> InputJobResult {
    let input_name = format!("input-{input_id}").intern();
    InputJobResult {
        index: usize::from(input_id),
        input_id,
        input_name: Arc::clone(&input_name),
        state,
        source: None,
        epg: None,
        stat: create_input_stat(0, 0, errors.len(), InputType::Xtream, &input_name, 0),
        errors,
        accepted_empty_clusters: ClusterFlags::empty(),
        had_quality_rejections,
        input_telemetry: None,
    }
}

pub(in crate::processor::playlist::tests) fn test_group(
    cluster: XtreamCluster,
    item_name: &str,
    input_name: &str,
) -> PlaylistGroup {
    PlaylistGroup {
        id: 1,
        title: item_name.intern(),
        xtream_cluster: cluster,
        channels: vec![PlaylistItem {
            header: PlaylistItemHeader {
                name: item_name.intern(),
                input_name: input_name.intern(),
                xtream_cluster: cluster,
                item_type: match cluster {
                    XtreamCluster::Live => PlaylistItemType::Live,
                    XtreamCluster::Video => PlaylistItemType::Video,
                    XtreamCluster::Series => PlaylistItemType::Series,
                },
                ..Default::default()
            },
        }],
    }
}

pub(in crate::processor::playlist::tests) fn catalog_test_item(
    title: &str,
    uuid: UUIDType,
    item_type: PlaylistItemType,
    cluster: XtreamCluster,
    parent_code: Option<&str>,
) -> PlaylistItem {
    PlaylistItem {
        header: PlaylistItemHeader {
            id: title.intern(),
            name: title.intern(),
            title: title.intern(),
            group: match cluster {
                XtreamCluster::Live => "Live".intern(),
                XtreamCluster::Video => "Movies".intern(),
                XtreamCluster::Series => "Series".intern(),
            },
            uuid,
            item_type,
            xtream_cluster: cluster,
            parent_code: parent_code.unwrap_or_default().intern(),
            ..PlaylistItemHeader::default()
        },
    }
}

pub(in crate::processor::playlist::tests) fn complete_catalog_evaluation(
    memberships: Vec<CurationMembership>,
) -> CurationEvaluation {
    CurationEvaluation {
        selectors: vec![CurationSelectorSummary {
            key: CurationSelectorKey(0),
            reference_count: memberships.len(),
            membership_count: memberships.len(),
        }],
        memberships,
    }
}

pub(in crate::processor::playlist::tests) fn catalog_membership(
    uuid: UUIDType,
    media_kind: CurationMediaKind,
    order: usize,
) -> CurationMembership {
    CurationMembership {
        selector_key: CurationSelectorKey(0),
        subject_uuid: uuid,
        media_kind,
        rank: Some(u32::try_from(order + 1).expect("test rank")),
        title_tiebreak: format!("item-{order}"),
        candidate_order: order,
    }
}
