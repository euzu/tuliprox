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
        default_container_extensions: Arc::default(),
    }
}

fn series_properties_with_episode(container_extension: &str) -> StreamProperties {
    StreamProperties::Series(Box::new(SeriesStreamProperties {
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
                container_extension: container_extension.into(),
                ..SeriesStreamDetailEpisodeProperties::default()
            }]),
        }),
        ..SeriesStreamProperties::default()
    }))
}

fn episode_extension(
    properties: &StreamProperties,
    options: &XtreamMappingOptions,
    configured_extension: Option<&Arc<str>>,
) -> std::sync::Arc<str> {
    let XtreamInfoDocument::Series(doc) = properties.to_info_document(
        options,
        PlaylistItemType::SeriesInfo,
        VirtualId::new(42),
        7,
        "",
        configured_extension,
    ) else {
        panic!("expected series info document");
    };
    let episode = doc.episodes.get("1").and_then(|episodes| episodes.first()).expect("episode missing");
    std::sync::Arc::clone(&episode.container_extension)
}

#[test]
fn series_episode_container_extension_is_filled_from_the_configured_default() {
    let properties = series_properties_with_episode("");
    let mut options = sample_options();
    let mut defaults = std::collections::HashMap::new();
    defaults.insert(Arc::<str>::from("provider"), Arc::<str>::from("mp4"));
    options.default_container_extensions = Arc::new(defaults);

    assert_eq!(episode_extension(&properties, &options, Some(&Arc::<str>::from("mp4"))).as_ref(), "mp4");
}

#[test]
fn series_episode_container_extension_keeps_the_provider_value() {
    let properties = series_properties_with_episode("mkv");
    let mut options = sample_options();
    let mut defaults = std::collections::HashMap::new();
    defaults.insert(Arc::<str>::from("provider"), Arc::<str>::from("mp4"));
    options.default_container_extensions = Arc::new(defaults);

    assert_eq!(episode_extension(&properties, &options, Some(&Arc::<str>::from("mp4"))).as_ref(), "mkv");
}

#[test]
fn series_episode_container_extension_falls_back_to_the_builtin_default() {
    let properties = series_properties_with_episode("");
    assert_eq!(
        episode_extension(&properties, &sample_options(), None).as_ref(),
        crate::defaults::DEFAULT_CONTAINER_EXTENSION
    );
}

#[test]
fn series_episode_container_extension_replaces_a_placeholder() {
    let properties = series_properties_with_episode("null");
    assert_eq!(
        episode_extension(&properties, &sample_options(), None).as_ref(),
        crate::defaults::DEFAULT_CONTAINER_EXTENSION
    );
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
        properties.to_info_document(&sample_options(), PlaylistItemType::SeriesInfo, VirtualId::new(42), 7, "", None)
    else {
        panic!("expected series info document");
    };

    let episode = doc.episodes.get("1").and_then(|episodes| episodes.first()).expect("episode missing");
    assert_eq!(episode.info.plot.as_deref(), Some("Episode plot"));
}
