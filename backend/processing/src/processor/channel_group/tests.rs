use super::*;
use shared::{
    model::{
        AutoGroupOverrideDto, ChannelGroupOverridesDto, ChannelGroupsConfig, ChannelMemberRefDto, EpgConflictMode,
        LiveStreamProperties, ManualChannelGroupDto, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType,
        StreamProperties, XtreamCluster,
    },
    utils::Internable,
};
use tuliprox_repository::ChannelGroupOrigin;

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

fn cfg() -> ChannelGroupsConfig { ChannelGroupsConfig::default() }

fn with_epg(mut item: PlaylistItem, epg: &str) -> PlaylistItem {
    item.header.epg_channel_id = Some(epg.intern());
    item
}

fn overrides_with_manual(id: &str, members: &[(&str, &str)]) -> ChannelGroupOverridesDto {
    let mut overrides = ChannelGroupOverridesDto::new("T");
    overrides.groups.push(ManualChannelGroupDto {
        id: id.into(),
        name: id.into(),
        members: members
            .iter()
            .map(|(input, stream_id)| ChannelMemberRefDto { input: (*input).into(), stream_id: (*stream_id).into() })
            .collect(),
        ..Default::default()
    });
    overrides
}

fn set_archive(item: &mut PlaylistItem, duration: i32) {
    let props = LiveStreamProperties { tv_archive: Some(1), tv_archive_duration: Some(duration), ..Default::default() };
    item.header.additional_properties = Some(StreamProperties::Live(Box::new(props)));
}

fn archive_of(item: &PlaylistItem) -> Option<i32> {
    match &item.header.additional_properties {
        Some(StreamProperties::Live(live)) => live.tv_archive_duration,
        _ => None,
    }
}

#[test]
fn auto_groups_same_caption_across_inputs() {
    let mut playlist = vec![group(
        "News",
        vec![live("solis", "1", "ZDF HD"), live("korath", "7", "ZDF FHD"), live("solis", "2", "ARD")],
    )];
    let pending = group_playlist(&cfg(), None, "T", &mut playlist);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].members.len(), 2);
    assert_eq!(pending[0].origin, ChannelGroupOrigin::Auto);
    let titles: Vec<_> = playlist[0].channels.iter().map(|c| c.header.title.to_string()).collect();
    assert_eq!(titles, vec!["ZDF HD", "ARD"]); // group item at representative position
    assert_eq!(playlist[0].channels[0].header.uuid, pending[0].group_uuid);
}

#[test]
fn single_member_key_is_not_grouped() {
    let mut playlist = vec![group("News", vec![live("solis", "1", "ZDF HD"), live("solis", "2", "ARD")])];
    assert!(group_playlist(&cfg(), None, "T", &mut playlist).is_empty());
    assert_eq!(playlist[0].channels.len(), 2);
}

#[test]
fn dsl_opt_out_and_forced_key() {
    let mut a = live("solis", "1", "ZDF HD");
    a.header.channel_group = Some("".intern());
    let mut b = live("korath", "7", "Zweites");
    b.header.channel_group = Some("zdf hd".intern());
    let c = live("veyra", "3", "ZDF");
    let mut playlist = vec![group("News", vec![a, b, c])];
    let pending = group_playlist(&cfg(), None, "T", &mut playlist);
    assert_eq!(pending.len(), 1);
    let inputs: Vec<_> = pending[0].members.iter().map(|m| m.header.input_name.to_string()).collect();
    assert_eq!(inputs, vec!["korath", "veyra"]);
}

#[test]
fn manual_group_wins_over_auto_and_keeps_order() {
    let overrides = overrides_with_manual("zdf", &[("korath", "7"), ("solis", "1")]);
    let mut playlist = vec![group(
        "News",
        vec![live("solis", "1", "ZDF HD"), live("korath", "7", "ZDF FHD"), live("veyra", "3", "ZDF")],
    )];
    let pending = group_playlist(&cfg(), Some(&overrides), "T", &mut playlist);
    let manual = pending.iter().find(|p| p.origin == ChannelGroupOrigin::Manual).expect("manual");
    let inputs: Vec<_> = manual.members.iter().map(|m| m.header.input_name.to_string()).collect();
    assert_eq!(inputs, vec!["korath", "solis"]);
    // veyra alone is a single member and stays a normal channel
    assert!(playlist[0].channels.iter().any(|c| c.header.input_name.as_ref() == "veyra"));
}

#[test]
fn excluded_member_never_grouped() {
    let mut overrides = ChannelGroupOverridesDto::new("T");
    overrides.excluded.push(ChannelMemberRefDto { input: "solis".into(), stream_id: "1".into() });
    let mut playlist = vec![group("News", vec![live("solis", "1", "ZDF HD"), live("korath", "7", "ZDF FHD")])];
    assert!(group_playlist(&cfg(), Some(&overrides), "T", &mut playlist).is_empty());
}

#[test]
fn auto_override_orders_and_marks_edited() {
    let mut overrides = ChannelGroupOverridesDto::new("T");
    overrides.auto_overrides.push(AutoGroupOverrideDto {
        auto_key: "zdf".into(),
        order: vec![ChannelMemberRefDto { input: "korath".into(), stream_id: "7".into() }],
        ..Default::default()
    });
    let mut playlist = vec![group("News", vec![live("solis", "1", "ZDF HD"), live("korath", "7", "ZDF FHD")])];
    let pending = group_playlist(&cfg(), Some(&overrides), "T", &mut playlist);
    assert_eq!(pending[0].origin, ChannelGroupOrigin::AutoEdited);
    assert_eq!(pending[0].members[0].header.input_name.as_ref(), "korath");
    assert_eq!(playlist[0].channels[0].header.title.as_ref(), "ZDF FHD"); // representative
}

#[test]
fn orphaned_manual_member_is_skipped() {
    let overrides = overrides_with_manual("zdf", &[("solis", "1"), ("korath", "7"), ("gone", "99")]);
    let mut playlist = vec![group("News", vec![live("solis", "1", "ZDF HD"), live("korath", "7", "ZDF FHD")])];
    let pending = group_playlist(&cfg(), Some(&overrides), "T", &mut playlist);
    assert_eq!(pending[0].members.len(), 2);
    assert_eq!(pending[0].orphaned, vec![ChannelMemberRefDto { input: "gone".into(), stream_id: "99".into() }]);
}

#[test]
fn manual_group_with_one_member_dissolves() {
    let overrides = overrides_with_manual("zdf", &[("solis", "1"), ("gone", "99")]);
    let mut playlist = vec![group("News", vec![live("solis", "1", "ZDF HD"), live("veyra", "2", "ARD")])];
    assert!(group_playlist(&cfg(), Some(&overrides), "T", &mut playlist).is_empty());
    assert_eq!(playlist[0].channels.len(), 2);
}

#[test]
fn archive_is_max_of_members() {
    let mut a = live("solis", "1", "ZDF HD");
    set_archive(&mut a, 3);
    let mut b = live("korath", "7", "ZDF FHD");
    set_archive(&mut b, 7);
    let mut playlist = vec![group("News", vec![a, b])];
    group_playlist(&cfg(), None, "T", &mut playlist);
    assert_eq!(archive_of(&playlist[0].channels[0]), Some(7));
}

#[test]
fn vod_and_series_are_never_grouped() {
    let mut a = live("solis", "1", "Film");
    a.header.item_type = PlaylistItemType::Video;
    a.header.xtream_cluster = XtreamCluster::Video;
    let mut b = live("korath", "2", "Film");
    b.header.item_type = PlaylistItemType::Video;
    b.header.xtream_cluster = XtreamCluster::Video;
    let mut playlist = vec![PlaylistGroup {
        id: 1,
        title: "Movies".intern(),
        channels: vec![a, b],
        xtream_cluster: XtreamCluster::Video,
    }];
    assert!(group_playlist(&cfg(), None, "T", &mut playlist).is_empty());
}

#[test]
fn different_epg_ids_split_the_auto_bucket() {
    let mut playlist = vec![group(
        "Sport",
        vec![
            with_epg(live("solis", "1", "Sport 1 HD"), "sport1.de"),
            with_epg(live("korath", "7", "Sport 1"), "sport1.de"),
            with_epg(live("veyra", "3", "Sport 1 FHD"), "sport1.uk"),
            with_epg(live("orun", "4", "Sport 1"), "sport1.uk"),
            live("solis", "9", "Sport 1"), // no EPG id → stays a normal channel
        ],
    )];
    let pending = group_playlist(&cfg(), None, "T", &mut playlist);
    assert_eq!(pending.len(), 2);
    assert!(pending.iter().all(|p| p.epg_split && p.members.len() == 2));
    assert!(playlist[0]
        .channels
        .iter()
        .any(|c| c.header.input_name.as_ref() == "solis" && c.header.id.as_ref() == "9"));
}

#[test]
fn epg_conflict_ignore_keeps_one_group() {
    let mut config = cfg();
    config.epg_conflict = EpgConflictMode::Ignore;
    let mut playlist = vec![group(
        "Sport",
        vec![
            with_epg(live("solis", "1", "Sport 1"), "sport1.de"),
            with_epg(live("korath", "7", "Sport 1"), "sport1.uk"),
        ],
    )];
    let pending = group_playlist(&config, None, "T", &mut playlist);
    assert_eq!((pending.len(), pending[0].epg_split), (1, false));
}

#[test]
fn dsl_forced_key_is_never_split() {
    let mut a = with_epg(live("solis", "1", "Eins"), "a.de");
    a.header.channel_group = Some("eins".intern());
    let mut b = with_epg(live("korath", "7", "One"), "b.uk");
    b.header.channel_group = Some("eins".intern());
    let mut playlist = vec![group("News", vec![a, b])];
    let pending = group_playlist(&cfg(), None, "T", &mut playlist);
    assert_eq!((pending.len(), pending[0].members.len()), (1, 2));
}

#[test]
fn match_normalize_groups_prefixed_and_unprefixed_captions() {
    let mut config = cfg();
    config.match_normalize.strip_regex = Some(r"^\s*(?:\|[A-Z]{2,3}\||[A-Z]{2,3}\s*[:|])\s*".to_string());
    config.match_normalize.ignore_tokens = vec!["hevc".to_string()];
    config.prepare().expect("valid");
    let mut playlist = vec![group("News", vec![live("solis", "1", "DE: ZDF HEVC"), live("korath", "7", "ZDF")])];
    let pending = group_playlist(&config, None, "T", &mut playlist);
    assert_eq!((pending.len(), pending[0].members.len()), (1, 2));
}

#[test]
fn name_strip_quality_cleans_derived_name_only() {
    assert_eq!(strip_quality_words("ZDF [FHD]"), "ZDF");
    assert_eq!(strip_quality_words("DE: ZDF HD"), "DE: ZDF");
    assert_eq!(strip_quality_words("ZDF-HD"), "ZDF-HD");
    assert_eq!(strip_quality_words("HD"), "HD"); // nothing left → original
    let mut config = cfg();
    config.name_strip_quality = true;
    let mut playlist = vec![group("News", vec![live("solis", "1", "ZDF HD"), live("korath", "7", "ZDF FHD")])];
    group_playlist(&config, None, "T", &mut playlist);
    assert_eq!(playlist[0].channels[0].header.title.as_ref(), "ZDF");
}

#[test]
fn emptied_playlist_group_is_dropped_and_group_uuid_is_stable() {
    let build =
        || vec![group("A", vec![live("solis", "1", "ZDF HD")]), group("B", vec![live("korath", "7", "ZDF FHD")])];
    let mut first = build();
    let p1 = group_playlist(&cfg(), None, "T", &mut first);
    let mut second = build();
    let p2 = group_playlist(&cfg(), None, "T", &mut second);
    assert_eq!(first.len(), 1);
    assert_eq!(p1[0].group_uuid, p2[0].group_uuid);
}

#[test]
fn members_keep_original_uuid_and_group_item_is_marked() {
    let a = live("solis", "1", "ZDF HD");
    let b = live("korath", "7", "ZDF FHD");
    let (uuid_a, uuid_b) = (a.header.uuid, b.header.uuid);
    let mut playlist = vec![group("News", vec![a, b])];
    let pending = group_playlist(&cfg(), None, "T", &mut playlist);
    assert_eq!(pending[0].members.iter().map(|m| m.header.uuid).collect::<Vec<_>>(), vec![uuid_a, uuid_b]);
    assert_eq!(playlist[0].channels[0].header.channel_group_uuid, Some(pending[0].group_uuid));
}
