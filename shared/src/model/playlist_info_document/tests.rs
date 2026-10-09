use super::*;
use crate::model::{
    PlaylistItemTypeSet, SeriesStreamDetailEpisodeProperties, SeriesStreamDetailProperties, SeriesStreamProperties,
    VirtualId, XtreamMappingFlags,
};
use std::sync::Arc;

fn sample_options() -> XtreamMappingOptions {
    XtreamMappingOptions {
        flags: XtreamMappingFlags::RewriteResourceUrl.into(),
        force_redirect: None,
        reverse_item_types: PlaylistItemTypeSet::empty(),
        resource_proxy_item_types: PlaylistItemTypeSet::empty(),
        username: "user".to_string(),
        password: "pass".to_string(),
        base_url: "http://proxy.example".to_string(),
        web_ui_request: false,
        encrypt_secret: [0u8; 16],
        resource_host_policies: Arc::default(),
    }
}

#[test]
fn series_episode_info_document_includes_episode_plot() {
    let properties = StreamProperties::Series(Box::new(SeriesStreamProperties {
        name: "Example Show".into(),
        series_id: 1,
        details: Some(SeriesStreamDetailProperties {
            year: None,
            seasons: None,
            episodes: Some(vec![SeriesStreamDetailEpisodeProperties {
                id: 101,
                episode_num: 1,
                season: 1,
                title: "Pilot".into(),
                container_extension: "mkv".into(),
                custom_sid: None,
                added: "1700000000".into(),
                direct_source: "".into(),
                tmdb: None,
                release_date: "2024-01-01".into(),
                series_release_date: None,
                plot: Some("Episode plot".into()),
                crew: None,
                duration_secs: 0,
                duration: "".into(),
                movie_image: "".into(),
                bitrate: 0,
                rating: None,
                video: None,
                audio: None,
            }]),
        }),
        ..SeriesStreamProperties::default()
    }));

    let XtreamInfoDocument::Series(doc) =
        properties.to_info_document(&sample_options(), PlaylistItemType::SeriesInfo, VirtualId::new(42), 7)
    else {
        panic!("expected series info document");
    };

    let episode = doc.episodes.get("1").and_then(|episodes| episodes.first()).expect("episode missing");
    assert_eq!(episode.info.plot.as_deref(), Some("Episode plot"));
}
