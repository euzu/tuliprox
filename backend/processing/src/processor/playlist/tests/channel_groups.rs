use crate::processor::playlist::target::run_finalize_until_persist;
use shared::{
    model::{
        ConfigFavouritesDto, ConfigTargetDto, ConfigTargetOptions, PlaylistGroup, PlaylistItem, PlaylistItemHeader,
        PlaylistItemType, Prepare, XtreamCluster,
    },
    utils::Internable,
};
use tuliprox_core::model::ConfigTarget;

fn live(input: &str, stream_id: &str, title: &str) -> PlaylistItem {
    let mut header = PlaylistItemHeader {
        input_name: input.intern(),
        id: stream_id.intern(),
        title: title.intern(),
        name: title.intern(),
        url: format!("http://{input}.invalid/live/u/p/{stream_id}.ts").intern(),
        item_type: PlaylistItemType::Live,
        xtream_cluster: XtreamCluster::Live,
        ..Default::default()
    };
    header.freeze_input_stream_id();
    header.gen_uuid();
    PlaylistItem { header }
}

fn group(title: &str, channels: Vec<PlaylistItem>) -> PlaylistGroup {
    PlaylistGroup { id: 1, title: title.intern(), channels, xtream_cluster: XtreamCluster::Live }
}

fn target_with_options(options_json: &str) -> ConfigTarget {
    let mut options: ConfigTargetOptions = serde_json::from_str(options_json).expect("valid options");
    options.prepare().expect("prepared");
    let dto = ConfigTargetDto { name: "T".to_string(), options: Some(options), ..Default::default() };
    ConfigTarget::from(&dto)
}

fn target_with_options_and_favourites(options_json: &str, group_name: &str, filter_str: &str) -> ConfigTarget {
    let mut options: ConfigTargetOptions = serde_json::from_str(options_json).expect("valid options");
    options.prepare().expect("prepared");
    let mut fav = ConfigFavouritesDto {
        cluster: XtreamCluster::Live,
        group: group_name.intern(),
        filter: filter_str.to_string(),
        match_as_ascii: false,
        t_filter: None,
    };
    fav.prepare(None).expect("prepared filter");
    let dto = ConfigTargetDto {
        name: "T".to_string(),
        options: Some(options),
        favourites: Some(vec![fav]),
        ..Default::default()
    };
    ConfigTarget::from(&dto)
}

#[tokio::test]
async fn favourites_copy_the_group_item_instead_of_joining_the_group() {
    let target = target_with_options_and_favourites(
        r#"{"channel_groups":{"enabled":true}}"#,
        "My Favourites",
        r#"Caption ~ "ZDF.*""#,
    );
    let playlist = vec![group("News", vec![live("solis", "1", "ZDF HD"), live("korath", "7", "ZDF FHD")])];
    let (finalized, pending) = run_finalize_until_persist(&target, playlist).await;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].members.len(), 2);
    let fav = finalized.iter().find(|g| g.title.as_ref() == "My Favourites").expect("favourites group");
    assert_eq!(fav.channels.len(), 1);
    assert_eq!(fav.channels[0].header.channel_group_uuid, Some(pending[0].group_uuid));
}

#[tokio::test]
async fn grouping_runs_before_deduplicate_and_dedup_ignores_group_members() {
    let target = target_with_options(r#"{"channel_groups":{"enabled":true},"deduplicate":{}}"#);
    let playlist = vec![group(
        "News",
        vec![
            live("solis", "1", "ZDF HD"),
            live("korath", "7", "ZDF FHD"),
            live("veyra", "3", "ARD HD"),
            live("veyra", "4", "ARD"),
        ],
    )];
    let (finalized, pending) = run_finalize_until_persist(&target, playlist).await;
    assert_eq!(pending.len(), 2);
    assert_eq!(finalized[0].channels.len(), 2);
}
