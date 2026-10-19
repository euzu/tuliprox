use super::*;
use shared::{
    model::{
        AutoGroupOverrideDto, ChannelMemberRefDto, EpgConflictMode, FailoverGroupsConfig,
        FailoverOverrideDiagnosticsDto, InactiveManualGroupDto, LiveStreamProperties, ManualFailoverGroupDto,
        PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType, StreamProperties,
        TargetFailoverOverridesDto, XtreamCluster,
    },
    utils::Internable,
};
use tuliprox_repository::{FailoverGroupOrigin, PendingFailoverGroup};

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

fn cfg() -> FailoverGroupsConfig { FailoverGroupsConfig::default() }

fn with_epg(mut item: PlaylistItem, epg: &str) -> PlaylistItem {
    item.header.epg_channel_id = Some(epg.intern());
    item
}

fn overrides_with_manual(id: &str, members: &[(&str, &str)]) -> TargetFailoverOverridesDto {
    let mut overrides = TargetFailoverOverridesDto::new("T");
    overrides.groups.push(ManualFailoverGroupDto {
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
    let pending = group_playlist(&cfg(), None, "T", &mut playlist).groups;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].members.len(), 2);
    assert_eq!(pending[0].origin, FailoverGroupOrigin::Auto);
    let titles: Vec<_> = playlist[0].channels.iter().map(|c| c.header.title.to_string()).collect();
    assert_eq!(titles, vec!["ZDF HD", "ARD"]); // group item at representative position
    assert_eq!(playlist[0].channels[0].header.uuid, pending[0].group_uuid);
}

#[test]
fn single_member_key_is_not_grouped() {
    let mut playlist = vec![group("News", vec![live("solis", "1", "ZDF HD"), live("solis", "2", "ARD")])];
    assert!(group_playlist(&cfg(), None, "T", &mut playlist).groups.is_empty());
    assert_eq!(playlist[0].channels.len(), 2);
}

#[test]
fn dsl_opt_out_and_forced_key() {
    let mut a = live("solis", "1", "ZDF HD");
    a.header.failover_group = Some("".intern());
    let mut b = live("korath", "7", "Zweites");
    b.header.failover_group = Some("zdf hd".intern());
    let c = live("veyra", "3", "ZDF");
    let mut playlist = vec![group("News", vec![a, b, c])];
    let pending = group_playlist(&cfg(), None, "T", &mut playlist).groups;
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
    let pending = group_playlist(&cfg(), Some(&overrides), "T", &mut playlist).groups;
    let manual = pending.iter().find(|p| p.origin == FailoverGroupOrigin::Manual).expect("manual");
    let inputs: Vec<_> = manual.members.iter().map(|m| m.header.input_name.to_string()).collect();
    assert_eq!(inputs, vec!["korath", "solis"]);
    // veyra alone is a single member and stays a normal channel
    assert!(playlist[0].channels.iter().any(|c| c.header.input_name.as_ref() == "veyra"));
}

#[test]
fn excluded_member_never_grouped() {
    let mut overrides = TargetFailoverOverridesDto::new("T");
    overrides.excluded.push(ChannelMemberRefDto { input: "solis".into(), stream_id: "1".into() });
    let mut playlist = vec![group("News", vec![live("solis", "1", "ZDF HD"), live("korath", "7", "ZDF FHD")])];
    assert!(group_playlist(&cfg(), Some(&overrides), "T", &mut playlist).groups.is_empty());
}

#[test]
fn auto_override_orders_and_marks_edited() {
    let mut overrides = TargetFailoverOverridesDto::new("T");
    overrides.auto_overrides.push(AutoGroupOverrideDto {
        auto_key: "zdf".into(),
        order: vec![ChannelMemberRefDto { input: "korath".into(), stream_id: "7".into() }],
        ..Default::default()
    });
    let mut playlist = vec![group("News", vec![live("solis", "1", "ZDF HD"), live("korath", "7", "ZDF FHD")])];
    let pending = group_playlist(&cfg(), Some(&overrides), "T", &mut playlist).groups;
    assert_eq!(pending[0].origin, FailoverGroupOrigin::AutoEdited);
    assert_eq!(pending[0].members[0].header.input_name.as_ref(), "korath");
    assert_eq!(playlist[0].channels[0].header.title.as_ref(), "ZDF FHD"); // representative
}

#[test]
fn orphaned_manual_member_is_skipped() {
    let overrides = overrides_with_manual("zdf", &[("solis", "1"), ("korath", "7"), ("gone", "99")]);
    let mut playlist = vec![group("News", vec![live("solis", "1", "ZDF HD"), live("korath", "7", "ZDF FHD")])];
    let pending = group_playlist(&cfg(), Some(&overrides), "T", &mut playlist).groups;
    assert_eq!(pending[0].members.len(), 2);
    assert_eq!(pending[0].orphaned, vec![ChannelMemberRefDto { input: "gone".into(), stream_id: "99".into() }]);
}

#[test]
fn manual_group_with_one_member_dissolves() {
    let overrides = overrides_with_manual("zdf", &[("solis", "1"), ("gone", "99")]);
    let mut playlist = vec![group("News", vec![live("solis", "1", "ZDF HD"), live("veyra", "2", "ARD")])];
    assert!(group_playlist(&cfg(), Some(&overrides), "T", &mut playlist).groups.is_empty());
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
    assert!(group_playlist(&cfg(), None, "T", &mut playlist).groups.is_empty());
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
    let pending = group_playlist(&cfg(), None, "T", &mut playlist).groups;
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
    let pending = group_playlist(&config, None, "T", &mut playlist).groups;
    assert_eq!((pending.len(), pending[0].epg_split), (1, false));
}

#[test]
fn dsl_forced_key_is_never_split() {
    let mut a = with_epg(live("solis", "1", "Eins"), "a.de");
    a.header.failover_group = Some("eins".intern());
    let mut b = with_epg(live("korath", "7", "One"), "b.uk");
    b.header.failover_group = Some("eins".intern());
    let mut playlist = vec![group("News", vec![a, b])];
    let pending = group_playlist(&cfg(), None, "T", &mut playlist).groups;
    assert_eq!((pending.len(), pending[0].members.len()), (1, 2));
}

#[test]
fn match_normalize_groups_prefixed_and_unprefixed_captions() {
    let mut config = cfg();
    config.match_normalize.strip_regex = Some(r"^\s*(?:\|[A-Z]{2,3}\||[A-Z]{2,3}\s*[:|])\s*".to_string());
    config.match_normalize.ignore_tokens = vec!["hevc".to_string()];
    config.prepare().expect("valid");
    let mut playlist = vec![group("News", vec![live("solis", "1", "DE: ZDF HEVC"), live("korath", "7", "ZDF")])];
    let pending = group_playlist(&config, None, "T", &mut playlist).groups;
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
    let p1 = group_playlist(&cfg(), None, "T", &mut first).groups;
    let mut second = build();
    let p2 = group_playlist(&cfg(), None, "T", &mut second).groups;
    assert_eq!(first.len(), 1);
    assert_eq!(p1[0].group_uuid, p2[0].group_uuid);
}

#[test]
fn members_keep_original_uuid_and_group_item_is_marked() {
    let a = live("solis", "1", "ZDF HD");
    let b = live("korath", "7", "ZDF FHD");
    let (uuid_a, uuid_b) = (a.header.uuid, b.header.uuid);
    let mut playlist = vec![group("News", vec![a, b])];
    let pending = group_playlist(&cfg(), None, "T", &mut playlist).groups;
    assert_eq!(pending[0].members.iter().map(|m| m.header.uuid).collect::<Vec<_>>(), vec![uuid_a, uuid_b]);
    assert_eq!(playlist[0].channels[0].header.failover_group_uuid, Some(pending[0].group_uuid));
}

fn rf(input: &str, stream_id: &str) -> ChannelMemberRefDto {
    ChannelMemberRefDto { input: input.into(), stream_id: stream_id.into() }
}

fn auto_override(key: &str) -> AutoGroupOverrideDto {
    AutoGroupOverrideDto { auto_key: key.into(), ..Default::default() }
}

fn diagnostics_of(
    config: &FailoverGroupsConfig,
    overrides: Option<&TargetFailoverOverridesDto>,
    playlist: &[PlaylistGroup],
) -> FailoverOverrideDiagnosticsDto {
    collect_group_candidates(config, overrides, playlist).1
}

fn refs_of(group: &PendingFailoverGroup) -> Vec<(String, String)> {
    group.members.iter().map(|m| (m.header.input_name.to_string(), m.header.id.to_string())).collect()
}

fn pair(input: &str, stream_id: &str) -> (String, String) { (input.to_string(), stream_id.to_string()) }

fn find<'a>(pending: &'a [PendingFailoverGroup], origin: FailoverGroupOrigin, key: &str) -> &'a PendingFailoverGroup {
    pending.iter().find(|p| p.origin == origin && p.key.as_ref() == key).expect("group present")
}

#[test]
fn manual_id_equal_to_auto_key_yields_two_groups() {
    let overrides = overrides_with_manual("zdf", &[("veyra", "3"), ("orun", "4")]);
    let mut playlist = vec![group(
        "News",
        vec![
            live("solis", "1", "ZDF HD"),
            live("korath", "7", "ZDF FHD"),
            live("veyra", "3", "RTL"),
            live("orun", "4", "Sat"),
        ],
    )];
    let pending = group_playlist(&cfg(), Some(&overrides), "T", &mut playlist).groups;
    assert_eq!(pending.len(), 2);
    let manual = find(&pending, FailoverGroupOrigin::Manual, "zdf");
    let auto = find(&pending, FailoverGroupOrigin::Auto, "zdf");
    assert_ne!(manual.group_uuid, auto.group_uuid);
    assert_eq!(manual.group_uuid, group_uuid("T", CandidateOrigin::Manual, "zdf"));
    assert_eq!(auto.group_uuid, group_uuid("T", CandidateOrigin::Auto, "zdf"));
    assert_eq!(group_uuid("T", CandidateOrigin::AutoEdited, "zdf"), auto.group_uuid);
}

#[test]
fn auto_add_is_exclusive_regardless_of_bucket_order() {
    let build_playlist = || {
        vec![group(
            "All",
            vec![
                live("p", "a1", "Alpha"),
                live("p", "a2", "Alpha"),
                live("p", "a3", "Alpha"),
                live("p", "b1", "Beta"),
                live("p", "b2", "Beta"),
                live("p", "b3", "Beta"),
                live("p", "z1", "Zulu"),
                live("p", "z2", "Zulu"),
            ],
        )]
    };
    let mut to_alpha = auto_override("alpha");
    to_alpha.add.push(rf("p", "b1"));
    let mut to_zulu = auto_override("zulu");
    to_zulu.add.push(rf("p", "a1"));

    for reversed in [false, true] {
        let mut overrides = TargetFailoverOverridesDto::new("T");
        if reversed {
            overrides.auto_overrides.extend([to_zulu.clone(), to_alpha.clone()]);
        } else {
            overrides.auto_overrides.extend([to_alpha.clone(), to_zulu.clone()]);
        }
        let mut playlist = build_playlist();
        let pending = group_playlist(&cfg(), Some(&overrides), "T", &mut playlist).groups;
        assert_eq!(pending.len(), 3);

        let mut all: Vec<_> = pending.iter().flat_map(refs_of).collect();
        let total = all.len();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), total, "no channel may be in two groups");
        assert_eq!(total, 8, "no member lost");

        assert!(refs_of(find(&pending, FailoverGroupOrigin::AutoEdited, "alpha")).contains(&pair("p", "b1")));
        assert!(refs_of(find(&pending, FailoverGroupOrigin::AutoEdited, "zulu")).contains(&pair("p", "a1")));
        assert!(!refs_of(find(&pending, FailoverGroupOrigin::Auto, "beta")).contains(&pair("p", "b1")));
        assert!(!refs_of(find(&pending, FailoverGroupOrigin::AutoEdited, "alpha")).contains(&pair("p", "a1")));
    }
}

#[test]
fn too_small_override_group_releases_its_claims() {
    let mut to_alpha = auto_override("alpha");
    to_alpha.remove.push(rf("p", "a1"));
    to_alpha.add.push(rf("p", "b1"));
    let mut overrides = TargetFailoverOverridesDto::new("T");
    overrides.auto_overrides.push(to_alpha);
    let mut playlist = vec![group(
        "All",
        vec![live("p", "a1", "Alpha"), live("p", "b1", "Beta"), live("p", "b2", "Beta"), live("p", "b3", "Beta")],
    )];
    let pending = group_playlist(&cfg(), Some(&overrides), "T", &mut playlist).groups;
    assert_eq!(pending.len(), 1, "the one-member override group is not materialized");
    let beta = find(&pending, FailoverGroupOrigin::Auto, "beta");
    assert_eq!(refs_of(beta), vec![pair("p", "b1"), pair("p", "b2"), pair("p", "b3")]);
    assert!(playlist[0].channels.iter().any(|c| c.header.id.as_ref() == "a1"), "a1 stays a normal channel");
}

#[test]
fn stale_auto_override_is_reported() {
    let mut stale = auto_override("gone");
    stale.add.push(rf("solis", "2"));
    let mut overrides = TargetFailoverOverridesDto::new("T");
    overrides.auto_overrides.push(stale);
    let mut playlist = vec![group(
        "News",
        vec![
            live("solis", "1", "ZDF HD"),
            live("korath", "7", "ZDF FHD"),
            live("solis", "2", "ARD"),
            live("veyra", "5", "ARD"),
        ],
    )];
    assert_eq!(diagnostics_of(&cfg(), Some(&overrides), &playlist).stale_auto_overrides, vec!["gone".to_string()]);
    let pending = group_playlist(&cfg(), Some(&overrides), "T", &mut playlist).groups;
    // the stale add is not claimed: solis/2 stays in its natural bucket
    assert!(refs_of(find(&pending, FailoverGroupOrigin::Auto, "ard")).contains(&pair("solis", "2")));
}

#[test]
fn unresolved_refs_reported_for_auto_and_manual_groups() {
    let mut zdf = auto_override("zdf");
    zdf.order = vec![rf("gone", "9"), rf("korath", "7")];
    let mut overrides = overrides_with_manual("ard", &[("solis", "2"), ("veyra", "5"), ("gone", "99")]);
    overrides.auto_overrides.push(zdf);
    let mut playlist = vec![group(
        "News",
        vec![
            live("solis", "1", "ZDF HD"),
            live("korath", "7", "ZDF FHD"),
            live("solis", "2", "ARD"),
            live("veyra", "5", "ARD"),
        ],
    )];
    let diagnostics = diagnostics_of(&cfg(), Some(&overrides), &playlist);
    assert_eq!(diagnostics.unresolved_refs.len(), 2);
    assert!(diagnostics.unresolved_refs.iter().any(|u| u.group == "zdf" && u.member_ref == rf("gone", "9")));
    assert!(diagnostics.unresolved_refs.iter().any(|u| u.group == "ard" && u.member_ref == rf("gone", "99")));

    let pending = group_playlist(&cfg(), Some(&overrides), "T", &mut playlist).groups;
    assert_eq!(find(&pending, FailoverGroupOrigin::AutoEdited, "zdf").orphaned, vec![rf("gone", "9")]);
    assert_eq!(find(&pending, FailoverGroupOrigin::Manual, "ard").orphaned, vec![rf("gone", "99")]);
}

#[test]
fn disabled_auto_group_is_dissolved_and_restorable() {
    let build_playlist = || {
        vec![group(
            "News",
            vec![live("solis", "1", "ZDF HD"), live("korath", "7", "ZDF FHD"), live("veyra", "3", "ARD")],
        )]
    };
    let mut disabled = auto_override("zdf");
    disabled.disabled = true;
    let mut overrides = TargetFailoverOverridesDto::new("T");
    overrides.auto_overrides.push(disabled);

    let mut playlist = build_playlist();
    assert_eq!(diagnostics_of(&cfg(), Some(&overrides), &playlist).dissolved, vec!["zdf".to_string()]);
    assert!(group_playlist(&cfg(), Some(&overrides), "T", &mut playlist).groups.is_empty());
    assert_eq!(playlist[0].channels.len(), 3, "members stay ungrouped");

    overrides.auto_overrides.clear();
    let mut playlist = build_playlist();
    let pending = group_playlist(&cfg(), Some(&overrides), "T", &mut playlist).groups;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].members.len(), 2);
}

#[test]
fn inactive_manual_group_is_reported() {
    let overrides = overrides_with_manual("m", &[("solis", "1"), ("gone", "99")]);
    let mut playlist = vec![group("News", vec![live("solis", "1", "ZDF HD"), live("korath", "7", "ZDF FHD")])];
    let diagnostics = diagnostics_of(&cfg(), Some(&overrides), &playlist);
    assert_eq!(diagnostics.inactive_manual_groups, vec![InactiveManualGroupDto { id: "m".into(), resolved: 1 }]);
    assert_eq!(diagnostics.unresolved_refs.len(), 1);
    // an inactive group does not hold its members: they regroup naturally
    let pending = group_playlist(&cfg(), Some(&overrides), "T", &mut playlist).groups;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].origin, FailoverGroupOrigin::Auto);
    assert_eq!(pending[0].members.len(), 2);
}

#[test]
fn new_channel_joins_edited_auto_group_after_ordered_members() {
    let mut zdf = auto_override("zdf");
    zdf.order = vec![rf("korath", "7")];
    let mut overrides = TargetFailoverOverridesDto::new("T");
    overrides.auto_overrides.push(zdf);
    let mut playlist = vec![group(
        "News",
        vec![live("solis", "1", "ZDF HD"), live("korath", "7", "ZDF FHD"), live("veyra", "3", "ZDF")],
    )];
    let pending = group_playlist(&cfg(), Some(&overrides), "T", &mut playlist).groups;
    assert_eq!(refs_of(&pending[0]), vec![pair("korath", "7"), pair("solis", "1"), pair("veyra", "3")]);
}

#[test]
fn manual_beats_auto_add_and_reports_conflict() {
    let mut overrides = overrides_with_manual("m", &[("p", "a1"), ("p", "a2")]);
    let mut beta = auto_override("beta");
    beta.add.push(rf("p", "a1"));
    overrides.auto_overrides.push(beta);
    let mut playlist = vec![group(
        "All",
        vec![live("p", "a1", "Alpha"), live("p", "a2", "Alpha"), live("p", "b1", "Beta"), live("p", "b2", "Beta")],
    )];
    assert_eq!(diagnostics_of(&cfg(), Some(&overrides), &playlist).conflicting_refs, vec![rf("p", "a1")]);
    let pending = group_playlist(&cfg(), Some(&overrides), "T", &mut playlist).groups;
    assert_eq!(refs_of(find(&pending, FailoverGroupOrigin::Manual, "m")), vec![pair("p", "a1"), pair("p", "a2")]);
    assert_eq!(
        refs_of(find(&pending, FailoverGroupOrigin::AutoEdited, "beta")),
        vec![pair("p", "b1"), pair("p", "b2")]
    );
}

#[test]
fn excluded_channel_usable_in_manual_group() {
    let mut overrides = overrides_with_manual("m", &[("solis", "1"), ("korath", "7")]);
    overrides.excluded.push(rf("solis", "1"));
    let mut playlist = vec![group("News", vec![live("solis", "1", "ZDF HD"), live("korath", "7", "ZDF FHD")])];
    let pending = group_playlist(&cfg(), Some(&overrides), "T", &mut playlist).groups;
    assert_eq!(pending.len(), 1);
    assert_eq!(refs_of(&pending[0]), vec![pair("solis", "1"), pair("korath", "7")]);
}

#[test]
fn resolution_works_on_borrowed_catalog_entries() {
    let refs = [rf("p", "1"), rf("p", "2"), rf("p", "3")];
    let catalog: Vec<CatalogEntry<'_>> = refs
        .iter()
        .zip(["news", "news", "other"])
        .map(|(member_ref, key)| CatalogEntry {
            member_ref,
            natural_key: Some(key),
            dsl_forced: false,
            epg_channel_id: None,
        })
        .collect();
    let resolution = resolve_failover_groups(&cfg(), None, &catalog);
    assert_eq!(resolution.candidates.len(), 1);
    assert_eq!(resolution.candidates[0].key, "news");
    assert_eq!(resolution.candidates[0].entries, vec![0, 1]);
    assert!(resolution.diagnostics.is_empty());
}

#[test]
fn catalog_lists_eligible_channels_in_playlist_order_with_natural_keys() {
    let mut forced = live("solis", "2", "ARD");
    forced.header.failover_group = Some("Das Erste".intern());
    let mut optout = live("solis", "3", "Local News");
    optout.header.failover_group = Some("".intern());
    let mut archived = with_epg(live("korath", "7", "ZDF FHD"), "zdf.de");
    set_archive(&mut archived, 3);
    archived.header.logo = "http://logo/zdf.png".intern();
    archived.header.group = "News".intern();
    let mut vod = live("solis", "9", "Movie");
    vod.header.xtream_cluster = XtreamCluster::Video;
    vod.header.item_type = PlaylistItemType::Video;
    let mut playlist = vec![group("News", vec![live("solis", "1", "ZDF HD"), forced, optout, archived, vod])];

    let batch = group_playlist(&cfg(), None, "T", &mut playlist);

    assert!(batch.enabled);
    let catalog = &batch.catalog;
    let titles: Vec<&str> = catalog.iter().map(|record| record.title.as_ref()).collect();
    assert_eq!(titles, ["ZDF HD", "ARD", "Local News", "ZDF FHD"], "playlist order, no VOD");
    let keys: Vec<(Option<&str>, bool)> =
        catalog.iter().map(|record| (record.natural_key.as_deref(), record.dsl_forced)).collect();
    assert!(keys[1].1, "dsl forced");
    assert_eq!(keys[2], (None, false), "opted out channel has no key");
    assert_eq!(keys[0].0, keys[3].0, "same natural key as the group built from both");
    assert!(keys[0].0.is_some());

    let zdf = &catalog[3];
    assert_eq!(zdf.member_ref, rf("korath", "7"));
    assert_eq!(zdf.provider_id, 7);
    assert!(zdf.has_archive);
    assert_eq!(zdf.epg_channel_id.as_deref(), Some("zdf.de"));
    assert_eq!(zdf.logo.as_ref(), "http://logo/zdf.png");
    assert_eq!(zdf.playlist_group.as_ref(), "News");
    assert!(!catalog[0].has_archive);
}

#[test]
fn catalog_keeps_header_name_next_to_display_title() {
    let mut item = live("solis", "1", "ZDF HD");
    item.header.name = "zdf_hd_raw".intern();
    item.header.title = "ZDF HD Display".intern();
    let mut playlist = vec![group("News", vec![item])];

    let batch = group_playlist(&cfg(), None, "T", &mut playlist);

    assert_eq!(batch.catalog[0].name.as_ref(), "zdf_hd_raw");
    assert_eq!(batch.catalog[0].title.as_ref(), "ZDF HD Display");
}

#[test]
fn catalog_is_complete_without_candidates_and_for_empty_playlist() {
    let mut playlist = vec![group("News", vec![live("solis", "1", "ZDF HD"), live("solis", "2", "ARD")])];
    let batch = group_playlist(&cfg(), None, "T", &mut playlist);
    assert!(batch.groups.is_empty());
    assert!(batch.enabled);
    assert_eq!(batch.catalog.len(), 2, "the catalog does not depend on groups being built");

    let mut empty = Vec::new();
    let batch = group_playlist(&cfg(), None, "T", &mut empty);
    assert!(batch.enabled && batch.groups.is_empty() && batch.catalog.is_empty());
}

#[test]
fn catalog_contains_channels_that_were_grouped_and_excluded() {
    let mut overrides = TargetFailoverOverridesDto::new("T");
    overrides.excluded.push(rf("solis", "1"));
    let mut playlist = vec![group("News", vec![live("solis", "1", "ZDF HD"), live("korath", "7", "ZDF FHD")])];
    let batch = group_playlist(&cfg(), Some(&overrides), "T", &mut playlist);
    assert!(batch.groups.is_empty());
    assert_eq!(batch.catalog.len(), 2, "excluded channels stay selectable in the editor");
}

fn resolved_candidate() -> ResolvedCandidate {
    ResolvedCandidate {
        key: "zdf".to_string(),
        origin: CandidateOrigin::Auto,
        policy: shared::model::FailoverGroupPolicy::Auto,
        entries: vec![0, 1],
        unresolved: Vec::new(),
        epg_split: false,
        name: None,
        logo: None,
        epg_channel_id: None,
        playlist_group: None,
    }
}

fn member<'a>(
    title: &'a str,
    logo: &'a str,
    epg_channel_id: Option<&'a str>,
    playlist_group: &'a str,
) -> MemberMetadata<'a> {
    MemberMetadata { name: title, title, logo, epg_channel_id, playlist_group }
}

#[test]
fn derive_group_metadata_falls_back_to_first_non_empty_member_values() {
    let members =
        [member("ZDF HD", "", Some("  "), "News"), member("ZDF", "http://logo/b.png", Some("zdf.de"), "Misc")];

    let derived = derive_group_metadata(&resolved_candidate(), members, true);

    assert_eq!((derived.name.as_str(), derived.title.as_str()), ("ZDF", "ZDF"));
    assert_eq!(derived.logo, "http://logo/b.png");
    assert_eq!(derived.epg_channel_id.as_deref(), Some("zdf.de"));
    assert_eq!(derived.playlist_group.as_deref(), Some("News"));
}

#[test]
fn derive_group_metadata_prefers_representative_and_overrides() {
    let members =
        [member("ZDF HD", "http://logo/a.png", Some("a.de"), ""), member("ZDF", "http://logo/b.png", None, "Misc")];

    let plain = derive_group_metadata(&resolved_candidate(), members, false);
    assert_eq!(plain.name, "ZDF HD");
    assert_eq!(plain.logo, "http://logo/a.png");
    assert_eq!(plain.epg_channel_id.as_deref(), Some("a.de"));
    assert_eq!(plain.playlist_group, None);

    let overridden = ResolvedCandidate {
        name: Some("Mine".to_string()),
        logo: Some("http://logo/own.png".to_string()),
        epg_channel_id: Some("own.de".to_string()),
        playlist_group: Some("Sport".to_string()),
        ..resolved_candidate()
    };
    let derived = derive_group_metadata(&overridden, members, true);
    assert_eq!((derived.name.as_str(), derived.title.as_str()), ("Mine", "Mine"));
    assert_eq!(derived.logo, "http://logo/own.png");
    assert_eq!(derived.epg_channel_id.as_deref(), Some("own.de"));
    assert_eq!(derived.playlist_group.as_deref(), Some("Sport"));
}
