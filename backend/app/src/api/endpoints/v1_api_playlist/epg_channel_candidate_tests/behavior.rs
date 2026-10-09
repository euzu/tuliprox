use super::{select_epg_channel_candidate, ResolvedRecordingSource};

pub(in crate::api::endpoints::v1_api_playlist::epg_channel_candidate_tests) fn candidate(
    virtual_id: u32,
    title: &str,
) -> ResolvedRecordingSource {
    ResolvedRecordingSource {
        virtual_id,
        input_name: "input".to_string(),
        title: title.to_string(),
        group: None,
        series_name: None,
        extension: None,
        downloadable: true,
    }
}

pub(in crate::api::endpoints::v1_api_playlist::epg_channel_candidate_tests) fn shared_epg_id_candidates(
) -> Vec<ResolvedRecordingSource> {
    vec![candidate(1, "┃NL┃ SBS 6 4K"), candidate(2, "┃CANAL+┃ SBS 6 HD"), candidate(3, "┃NLZIET┃ SBS 6 HD")]
}

#[test]
fn shared_epg_id_prefers_exact_channel_name() {
    let selected = select_epg_channel_candidate(shared_epg_id_candidates(), Some("┃NLZIET┃ SBS 6 HD"));
    assert_eq!(selected.map(|c| c.virtual_id), Some(3));
}

#[test]
fn shared_epg_id_matches_lowercased_display_name() {
    let selected = select_epg_channel_candidate(shared_epg_id_candidates(), Some(" ┃canal+┃ sbs 6 hd "));
    assert_eq!(selected.map(|c| c.virtual_id), Some(2));
}

#[test]
fn shared_epg_id_without_matching_name_takes_first() {
    assert_eq!(select_epg_channel_candidate(shared_epg_id_candidates(), Some("SBS 6")).map(|c| c.virtual_id), Some(1));
    assert_eq!(select_epg_channel_candidate(shared_epg_id_candidates(), None).map(|c| c.virtual_id), Some(1));
}

#[test]
fn episode_candidate_carries_series_name_and_group() {
    use super::super::{recording_candidate, RecordingCandidateFields};
    use shared::model::{PlaylistItemType, VirtualId};
    let fields = |item_type, group| RecordingCandidateFields {
        virtual_id: VirtualId::new(7),
        input_name: "input",
        name: "The Show",
        title: "The Show S01E02 Pilot",
        group,
        url: "http://example.test/series/7.mkv",
        item_type,
    };
    let episode = recording_candidate(&fields(PlaylistItemType::Series, "Crime"));
    assert_eq!(episode.series_name.as_deref(), Some("The Show"));
    assert_eq!(episode.group.as_deref(), Some("Crime"));
    assert_eq!(episode.extension.as_deref().map(|ext| ext.trim_start_matches('.')), Some("mkv"));

    let film = recording_candidate(&fields(PlaylistItemType::Video, "  "));
    assert_eq!(film.series_name, None, "only episodes carry a series name");
    assert_eq!(film.group, None, "a blank group is no group");
}

#[test]
fn no_candidate_resolves_nothing() {
    assert!(select_epg_channel_candidate(Vec::new(), Some("SBS 6")).is_none());
}
