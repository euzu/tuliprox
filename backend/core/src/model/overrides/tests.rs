use super::*;
use crate::model::config::{ConfigInput, ConfigInputAlias};
use shared::{
    model::{
        FieldDelta, InputType, ItemOverride, OverrideItemKind, PlaylistItemHeader, PlaylistItemType, StreamProperties,
        VideoStreamProperties,
    },
    utils::{extract_id_from_url, Internable},
};
use std::sync::Arc;

fn make_alias(name: &str, url: &str, username: Option<&str>, password: Option<&str>) -> ConfigInputAlias {
    ConfigInputAlias {
        id: 0,
        name: Arc::from(name),
        url: url.to_string(),
        username: username.map(String::from),
        password: password.map(String::from),
        priority: 0,
        max_connections: 1,
        exp_date: None,
        enabled: true,
        stalker: None,
    }
}

fn make_input(
    name: &str,
    url: &str,
    username: Option<&str>,
    password: Option<&str>,
    aliases: Vec<ConfigInputAlias>,
) -> Arc<ConfigInput> {
    Arc::new(ConfigInput {
        name: Arc::from(name),
        input_type: InputType::M3u,
        url: url.to_string(),
        username: username.map(String::from),
        password: password.map(String::from),
        aliases: if aliases.is_empty() { None } else { Some(aliases) },
        ..Default::default()
    })
}

#[test]
fn alias_items_resolve_to_parent() {
    let parent_a = make_input(
        "provider-a",
        "http://provider-a.example/playlist.m3u",
        Some("usr_a"),
        Some("pwd_a"),
        vec![
            make_alias("alias-a1", "http://alias-a1.example/live", Some("u1"), Some("p1")),
            make_alias("alias-a2", "http://alias-a2.example/live", Some("u2"), Some("p2")),
        ],
    );
    let parent_b = make_input("provider-b", "http://provider-b.example/playlist.m3u", None, None, vec![]);

    let map = InputAliasMap::from_inputs(&[parent_a, parent_b]);

    assert_eq!(map.parent_of("provider-a"), Some("provider-a"));
    assert_eq!(map.parent_of("alias-a1"), Some("provider-a"));
    assert_eq!(map.parent_of("alias-a2"), Some("provider-a"));
    assert_eq!(map.parent_of("provider-b"), Some("provider-b"));
    assert_eq!(map.parent_of("unknown-input"), None);
}

#[test]
fn parent_and_two_aliases_hit_one_key() {
    let parent = make_input(
        "provider1",
        "http://example.com/playlist.m3u?username=usr&password=pwd",
        Some("usr"),
        Some("pwd"),
        vec![
            make_alias(
                "alias1",
                "http://alias1.com:8080/get.php?username=a1usr&password=a1pwd",
                Some("a1usr"),
                Some("a1pwd"),
            ),
            make_alias("alias2", "http://alias2.com/live", Some("a2usr"), Some("a2pwd")),
        ],
    );

    let map = InputAliasMap::from_inputs(&[parent]);

    // 1. Numeric variant: all have id "5555" and URL last segment "5555.ts"
    let h_p_num = PlaylistItemHeader {
        input_name: "provider1".intern(),
        id: "5555".intern(),
        url: "http://example.com/live/usr/pwd/5555.ts".intern(),
        item_type: PlaylistItemType::Live,
        ..Default::default()
    };
    let h_a1_num = PlaylistItemHeader {
        input_name: "alias1".intern(),
        id: "5555".intern(),
        url: "http://alias1.com:8080/live/a1usr/a1pwd/5555.ts".intern(),
        item_type: PlaylistItemType::Live,
        ..Default::default()
    };
    let h_a2_num = PlaylistItemHeader {
        input_name: "alias2".intern(),
        id: "5555".intern(),
        url: "http://alias2.com/live/a2usr/a2pwd/5555.ts".intern(),
        item_type: PlaylistItemType::Live,
        ..Default::default()
    };

    let key_p_num = override_key(&h_p_num, &map);
    let key_a1_num = override_key(&h_a1_num, &map);
    let key_a2_num = override_key(&h_a2_num, &map);

    assert_eq!(key_p_num, Some(OverrideKey { kind: OverrideItemKind::Live, id: "5555".into() }));
    assert_eq!(key_p_num, key_a1_num);
    assert_eq!(key_p_num, key_a2_num);

    // 2. URL-hash variant: non-numeric URL segments
    let h_p_hash = PlaylistItemHeader {
        input_name: "provider1".intern(),
        url: "http://example.com/live/usr/pwd/ch_stream.m3u8?token=xyz".intern(),
        id: extract_id_from_url("http://example.com/live/usr/pwd/ch_stream.m3u8?token=xyz").intern(),
        item_type: PlaylistItemType::Live,
        ..Default::default()
    };
    let h_a1_hash = PlaylistItemHeader {
        input_name: "alias1".intern(),
        url: "http://alias1.com:8080/live/a1usr/a1pwd/ch_stream.m3u8?token=xyz".intern(),
        id: extract_id_from_url("http://alias1.com:8080/live/a1usr/a1pwd/ch_stream.m3u8?token=xyz").intern(),
        item_type: PlaylistItemType::Live,
        ..Default::default()
    };
    let h_a2_hash = PlaylistItemHeader {
        input_name: "alias2".intern(),
        url: "https://alias2.com/live/a2usr/a2pwd/ch_stream.m3u8?token=xyz".intern(),
        id: extract_id_from_url("https://alias2.com/live/a2usr/a2pwd/ch_stream.m3u8?token=xyz").intern(),
        item_type: PlaylistItemType::Live,
        ..Default::default()
    };

    let key_p_hash = override_key(&h_p_hash, &map);
    let key_a1_hash = override_key(&h_a1_hash, &map);
    let key_a2_hash = override_key(&h_a2_hash, &map);

    assert!(key_p_hash.is_some(), "url-hash key must be computed");
    assert!(key_p_hash.as_ref().unwrap().id.starts_with("urlhash-v1:"));
    assert_eq!(key_p_hash, key_a1_hash);
    assert_eq!(key_p_hash, key_a2_hash);
}

#[test]
fn alias_replaced_by_new_account_still_matches() {
    let parent_initial = make_input(
        "my-provider",
        "http://prov.example/playlist.m3u",
        Some("p_user"),
        Some("p_pass"),
        vec![make_alias("old-alias", "http://old.example/live", Some("old_user"), Some("old_pass"))],
    );
    let initial_map = InputAliasMap::from_inputs(&[parent_initial]);

    let item_initial = PlaylistItemHeader {
        input_name: "old-alias".intern(),
        url: "http://old.example/live/old_user/old_pass/channel.m3u8".intern(),
        id: extract_id_from_url("http://old.example/live/old_user/old_pass/channel.m3u8").intern(),
        item_type: PlaylistItemType::Live,
        ..Default::default()
    };

    let key_initial = override_key(&item_initial, &initial_map).expect("key computed");

    let mut lookup_map = HashMap::new();
    let override_entry =
        Arc::new(ItemOverride { name: FieldDelta::Set("Fixed Channel Name".to_string()), ..Default::default() });
    lookup_map.insert(key_initial.clone(), override_entry.clone());
    let lookup = OverrideLookup::new(lookup_map);

    // Simulate config change: old-alias removed, new-alias added with new credentials
    let parent_updated = make_input(
        "my-provider",
        "http://prov.example/playlist.m3u",
        Some("p_user"),
        Some("p_pass"),
        vec![make_alias("new-alias", "http://new.example/live", Some("new_user"), Some("new_pass"))],
    );
    let updated_map = InputAliasMap::from_inputs(&[parent_updated]);

    let item_new = PlaylistItemHeader {
        input_name: "new-alias".intern(),
        url: "http://new.example/live/new_user/new_pass/channel.m3u8".intern(),
        id: extract_id_from_url("http://new.example/live/new_user/new_pass/channel.m3u8").intern(),
        item_type: PlaylistItemType::Live,
        ..Default::default()
    };

    let key_new = override_key(&item_new, &updated_map).expect("key computed for new alias");

    assert_eq!(key_new, key_initial);
    assert_eq!(lookup.get(&key_new), Some(&override_entry));
}

#[test]
fn same_id_other_input_no_match() {
    let parent_a = make_input("provider_a", "http://a.example/playlist.m3u", None, None, vec![]);
    let parent_b = make_input("provider_b", "http://b.example/playlist.m3u", None, None, vec![]);
    let map = InputAliasMap::from_inputs(&[parent_a, parent_b]);

    assert_eq!(map.parent_of("provider_a"), Some("provider_a"));
    assert_eq!(map.parent_of("provider_b"), Some("provider_b"));
    assert_ne!(map.parent_of("provider_a"), map.parent_of("provider_b"));

    let h_a = PlaylistItemHeader {
        input_name: "provider_a".intern(),
        id: "1001".intern(),
        item_type: PlaylistItemType::Live,
        ..Default::default()
    };

    let h_b = PlaylistItemHeader {
        input_name: "provider_b".intern(),
        id: "1001".intern(),
        item_type: PlaylistItemType::Live,
        ..Default::default()
    };

    let key_a = override_key(&h_a, &map).expect("key for a");
    let key_b = override_key(&h_b, &map).expect("key for b");
    assert_eq!(key_a, key_b);

    // Lookups are scoped per parent input: HashMap<parent_name, OverrideLookup>
    let mut lookup_a_map = HashMap::new();
    let ov_a = Arc::new(ItemOverride { name: FieldDelta::Set("Override For A".into()), ..Default::default() });
    lookup_a_map.insert(key_a.clone(), ov_a.clone());

    let mut parent_lookups: HashMap<Arc<str>, OverrideLookup> = HashMap::new();
    parent_lookups.insert(Arc::from("provider_a"), OverrideLookup::new(lookup_a_map));
    parent_lookups.insert(Arc::from("provider_b"), OverrideLookup::default());

    let parent_for_a = map.parent_of(h_a.input_name.as_ref()).expect("parent for a");
    let a_match = parent_lookups.get(parent_for_a).and_then(|lookup| lookup.get(&key_a));
    assert_eq!(a_match, Some(&ov_a));

    let parent_for_b = map.parent_of(h_b.input_name.as_ref()).expect("parent for b");
    let b_match = parent_lookups.get(parent_for_b).and_then(|lookup| lookup.get(&key_b));
    assert_eq!(b_match, None);

    let h_unknown = PlaylistItemHeader {
        input_name: "unknown_provider".intern(),
        id: "1001".intern(),
        item_type: PlaylistItemType::Live,
        ..Default::default()
    };
    assert!(override_key(&h_unknown, &map).is_none());
}

#[test]
fn same_id_other_kind_no_match() {
    let parent = make_input("provider1", "http://prov.example/playlist.m3u", None, None, vec![]);
    let map = InputAliasMap::from_inputs(&[parent]);

    let h_live = PlaylistItemHeader {
        input_name: "provider1".intern(),
        id: "2002".intern(),
        item_type: PlaylistItemType::Live,
        ..Default::default()
    };

    let h_vod = PlaylistItemHeader {
        input_name: "provider1".intern(),
        id: "2002".intern(),
        item_type: PlaylistItemType::Video,
        ..Default::default()
    };

    let key_live = override_key(&h_live, &map).unwrap();
    let key_vod = override_key(&h_vod, &map).unwrap();

    assert_eq!(key_live.kind, OverrideItemKind::Live);
    assert_eq!(key_vod.kind, OverrideItemKind::Vod);
    assert_ne!(key_live, key_vod);

    let mut lookup_map = HashMap::new();
    let ov = Arc::new(ItemOverride::default());
    lookup_map.insert(key_live.clone(), ov.clone());
    let lookup = OverrideLookup::new(lookup_map);

    assert_eq!(lookup.get(&key_live), Some(&ov));
    assert_eq!(lookup.get(&key_vod), None);
}

#[test]
fn empty_header_id_returns_none() {
    let parent = make_input("provider", "http://example.com/playlist.m3u", None, None, vec![]);
    let map = InputAliasMap::from_inputs(&[parent]);

    let header = PlaylistItemHeader {
        input_name: "provider".intern(),
        id: "".intern(),
        item_type: PlaylistItemType::Live,
        ..Default::default()
    };

    assert!(override_key(&header, &map).is_none());
}

#[test]
fn credentials_in_url_query_only_removed_for_url_hash() {
    let parent = make_input(
        "provider",
        "http://example.com/playlist.m3u?username=query_usr&password=query_pwd",
        None,
        None,
        vec![],
    );
    let map = InputAliasMap::from_inputs(&[parent]);

    let creds = map.credentials_for("provider").expect("credentials");
    assert!(creds.iter().any(|c| c.as_ref() == "query_usr"));
    assert!(creds.iter().any(|c| c.as_ref() == "query_pwd"));

    let h_with_creds = PlaylistItemHeader {
        input_name: "provider".intern(),
        url: "http://example.com/live/query_usr/query_pwd/ch.m3u8".intern(),
        id: extract_id_from_url("http://example.com/live/query_usr/query_pwd/ch.m3u8").intern(),
        item_type: PlaylistItemType::Live,
        ..Default::default()
    };

    let h_plain = PlaylistItemHeader {
        input_name: "provider".intern(),
        url: "http://example.com/live/ch.m3u8".intern(),
        id: extract_id_from_url("http://example.com/live/ch.m3u8").intern(),
        item_type: PlaylistItemType::Live,
        ..Default::default()
    };

    let key_with_creds = override_key(&h_with_creds, &map);
    let key_plain = override_key(&h_plain, &map);

    assert!(key_with_creds.is_some());
    assert_eq!(key_with_creds, key_plain);
}

#[test]
fn empty_username_and_password_excluded() {
    let parent = make_input(
        "provider",
        "http://example.com/playlist.m3u?username=&password=   ",
        Some(""),
        Some("   "),
        vec![make_alias("alias1", "http://example.com/live?username=&password=", Some(""), Some(" "))],
    );
    let map = InputAliasMap::from_inputs(&[parent]);

    let creds = map.credentials_for("provider").expect("creds");
    assert!(creds.is_empty(), "empty/whitespace credentials must be excluded");
}

#[test]
fn input_type_m3u_batch_treated_as_m3u() {
    let parent = Arc::new(ConfigInput {
        name: "provider".intern(),
        input_type: InputType::M3uBatch,
        url: "http://example.com/playlist.m3u".to_string(),
        ..Default::default()
    });
    let map = InputAliasMap::from_inputs(&[parent]);

    let h_num = PlaylistItemHeader {
        input_name: "provider".intern(),
        id: "12345".intern(),
        url: "http://example.com/live/12345.ts".intern(),
        item_type: PlaylistItemType::Live,
        ..Default::default()
    };
    assert_eq!(classify_id(&h_num, InputType::M3uBatch), IdSource::UrlNumeric);
    assert_eq!(override_key(&h_num, &map), Some(OverrideKey { kind: OverrideItemKind::Live, id: "12345".into() }));

    let h_hash = PlaylistItemHeader {
        input_name: "provider".intern(),
        id: extract_id_from_url("http://example.com/live/stream.m3u8").intern(),
        url: "http://example.com/live/stream.m3u8".intern(),
        item_type: PlaylistItemType::Live,
        ..Default::default()
    };
    assert_eq!(classify_id(&h_hash, InputType::M3uBatch), IdSource::UrlHash);
    assert!(override_key(&h_hash, &map).is_some());
}

#[test]
fn alias_with_different_extra_query_token_keeps_different_key() {
    // Expected no-match: when two streams differ in query parameters that are not credentials,
    // they represent distinct streams and must produce distinct override keys.
    let parent = make_input(
        "provider",
        "http://example.com/playlist.m3u",
        Some("usr"),
        Some("pwd"),
        vec![make_alias("alias1", "http://alias1.com/live", Some("a_usr"), Some("a_pwd"))],
    );
    let map = InputAliasMap::from_inputs(&[parent]);

    let h_parent = PlaylistItemHeader {
        input_name: "provider".intern(),
        url: "http://example.com/live/usr/pwd/ch.m3u8?token=tok_parent".intern(),
        id: extract_id_from_url("http://example.com/live/usr/pwd/ch.m3u8?token=tok_parent").intern(),
        item_type: PlaylistItemType::Live,
        ..Default::default()
    };

    let h_alias = PlaylistItemHeader {
        input_name: "alias1".intern(),
        url: "http://alias1.com/live/a_usr/a_pwd/ch.m3u8?token=tok_alias".intern(),
        id: extract_id_from_url("http://alias1.com/live/a_usr/a_pwd/ch.m3u8?token=tok_alias").intern(),
        item_type: PlaylistItemType::Live,
        ..Default::default()
    };

    let key_parent = override_key(&h_parent, &map).expect("parent key");
    let key_alias = override_key(&h_alias, &map).expect("alias key");

    assert_ne!(key_parent, key_alias, "distinct query tokens must produce distinct keys (expected no-match)");
}

fn verify_video_overrides(set_ov: &ItemOverride, clear_ov: &ItemOverride) {
    let video = VideoStreamProperties {
        stream_icon: "http://orig.example/logo.png".intern(),
        tmdb: Some(10),
        rating: Some(5.0),
        rating_5based: Some(2.5),
        ..Default::default()
    };
    let mut header = PlaylistItemHeader {
        name: "Original Name".intern(),
        title: "Original Title".intern(),
        group: "Original Group".intern(),
        logo: "http://orig.example/logo.png".intern(),
        logo_small: "http://orig.example/small.png".intern(),
        epg_channel_id: Some("orig.epg".intern()),
        chno: 100,
        parent_code: "1111".intern(),
        audio_track: "deu".intern(),
        time_shift: "0".intern(),
        rec: "0".intern(),
        additional_properties: Some(StreamProperties::Video(Box::new(video))),
        ..Default::default()
    };

    apply_override(&mut header, set_ov);

    assert_eq!(header.name.as_ref(), "New Name");
    assert_eq!(header.title.as_ref(), "New Title");
    assert_eq!(header.group.as_ref(), "New Group");
    assert_eq!(header.logo.as_ref(), "http://new.example/logo.png");
    assert_eq!(header.logo_small.as_ref(), "http://new.example/small.png");
    assert_eq!(header.epg_channel_id.as_deref(), Some("new.epg"));
    assert_eq!(header.chno, 42);
    assert_eq!(header.parent_code.as_ref(), "9999");
    assert_eq!(header.audio_track.as_ref(), "eng");
    assert_eq!(header.time_shift.as_ref(), "2");
    assert_eq!(header.rec.as_ref(), "1");

    let Some(StreamProperties::Video(v)) = &header.additional_properties else {
        panic!("expected VideoStreamProperties");
    };
    assert_eq!(v.name.as_ref(), "New Name");
    assert_eq!(v.stream_icon.as_ref(), "http://new.example/logo.png");
    assert_eq!(v.tmdb, Some(12345));
    assert_eq!(v.rating, Some(8.5));
    assert_eq!(v.rating_5based, Some(4.25));
    let details = v.details.as_ref().expect("details created");
    assert_eq!(details.plot.as_deref(), Some("A great movie plot"));
    assert_eq!(details.genre.as_deref(), Some("Action / Sci-Fi"));
    assert_eq!(details.cast.as_deref(), Some("Lead Actor, Supporting Actor"));
    assert_eq!(details.director.as_deref(), Some("Famous Director"));
    assert_eq!(details.release_date.as_deref(), Some("2024-05-01"));

    apply_override(&mut header, clear_ov);

    assert_eq!(header.name.as_ref(), "");
    assert_eq!(header.title.as_ref(), "");
    assert_eq!(header.group.as_ref(), "");
    assert_eq!(header.logo.as_ref(), "");
    assert_eq!(header.logo_small.as_ref(), "");
    assert_eq!(header.epg_channel_id, None);
    assert_eq!(header.chno, 0);
    assert_eq!(header.parent_code.as_ref(), "");
    assert_eq!(header.audio_track.as_ref(), "");
    assert_eq!(header.time_shift.as_ref(), "");
    assert_eq!(header.rec.as_ref(), "");

    let Some(StreamProperties::Video(v)) = &header.additional_properties else {
        panic!("expected VideoStreamProperties");
    };
    assert_eq!(v.name.as_ref(), "");
    assert_eq!(v.stream_icon.as_ref(), "");
    assert_eq!(v.tmdb, None);
    assert_eq!(v.rating, None);
    assert_eq!(v.rating_5based, None);
    let details = v.details.as_ref().expect("details exists");
    assert_eq!(details.plot, None);
    assert_eq!(details.genre, None);
    assert_eq!(details.cast, None);
    assert_eq!(details.director, None);
    assert_eq!(details.release_date, None);
}

#[allow(clippy::float_cmp)]
fn verify_series_overrides(set_ov: &ItemOverride, clear_ov: &ItemOverride) {
    let mut series_header = PlaylistItemHeader {
        additional_properties: Some(StreamProperties::Series(Box::default())),
        ..Default::default()
    };

    apply_override(&mut series_header, set_ov);
    let Some(StreamProperties::Series(s)) = &series_header.additional_properties else {
        panic!("expected SeriesStreamProperties");
    };
    assert_eq!(s.name.as_ref(), "New Name");
    assert_eq!(s.cover.as_ref(), "http://new.example/logo.png");
    assert_eq!(s.tmdb, Some(12345));
    assert_eq!(s.rating, 8.5);
    assert_eq!(s.rating_5based, 4.25);
    assert_eq!(s.plot.as_deref(), Some("A great movie plot"));
    assert_eq!(s.genre.as_deref(), Some("Action / Sci-Fi"));
    assert_eq!(s.cast.as_ref(), "Lead Actor, Supporting Actor");
    assert_eq!(s.director.as_ref(), "Famous Director");
    assert_eq!(s.release_date.as_deref(), Some("2024-05-01"));

    apply_override(&mut series_header, clear_ov);
    let Some(StreamProperties::Series(s)) = &series_header.additional_properties else {
        panic!("expected SeriesStreamProperties");
    };
    assert_eq!(s.name.as_ref(), "");
    assert_eq!(s.cover.as_ref(), "");
    assert_eq!(s.tmdb, None);
    assert_eq!(s.rating, 0.0);
    assert_eq!(s.rating_5based, 0.0);
    assert_eq!(s.plot, None);
    assert_eq!(s.genre, None);
    assert_eq!(s.cast.as_ref(), "");
    assert_eq!(s.director.as_ref(), "");
    assert_eq!(s.release_date, None);
}

fn verify_fallback_overrides(set_ov: &ItemOverride, clear_ov: &ItemOverride) {
    let mut live_header = PlaylistItemHeader {
        additional_properties: Some(StreamProperties::Live(Box::default())),
        ..Default::default()
    };
    apply_override(&mut live_header, set_ov);
    assert_eq!(live_header.name.as_ref(), "New Name");
    let Some(StreamProperties::Live(l)) = &live_header.additional_properties else {
        panic!("expected LiveStreamProperties");
    };
    assert_eq!(l.name.as_ref(), "New Name");
    assert_eq!(l.stream_icon.as_ref(), "http://new.example/logo.png");
    assert_eq!(l.epg_channel_id.as_deref(), Some("new.epg"));

    apply_override(&mut live_header, clear_ov);
    let Some(StreamProperties::Live(l)) = &live_header.additional_properties else {
        panic!("expected LiveStreamProperties");
    };
    assert_eq!(l.name.as_ref(), "");
    assert_eq!(l.stream_icon.as_ref(), "");
    assert_eq!(l.epg_channel_id, None);

    let mut none_header = PlaylistItemHeader { additional_properties: None, ..Default::default() };
    apply_override(&mut none_header, set_ov);
    assert_eq!(none_header.name.as_ref(), "New Name");
    assert!(none_header.additional_properties.is_none());
}

#[test]
fn apply_override_set_and_clear_each_field() {
    let set_ov = ItemOverride {
        name: FieldDelta::Set("New Name".into()),
        title: FieldDelta::Set("New Title".into()),
        group: FieldDelta::Set("New Group".into()),
        logo: FieldDelta::Set("http://new.example/logo.png".into()),
        logo_small: FieldDelta::Set("http://new.example/small.png".into()),
        epg_channel_id: FieldDelta::Set("new.epg".into()),
        chno: FieldDelta::Set(42),
        parent_code: FieldDelta::Set("9999".into()),
        audio_track: FieldDelta::Set("eng".into()),
        time_shift: FieldDelta::Set("2".into()),
        rec: FieldDelta::Set("1".into()),
        tmdb: FieldDelta::Set(12345),
        rating: FieldDelta::Set(8.5),
        plot: FieldDelta::Set("A great movie plot".into()),
        genre: FieldDelta::Set("Action / Sci-Fi".into()),
        cast: FieldDelta::Set("Lead Actor, Supporting Actor".into()),
        director: FieldDelta::Set("Famous Director".into()),
        release_date: FieldDelta::Set("2024-05-01".into()),
        ..Default::default()
    };

    let clear_ov = ItemOverride {
        name: FieldDelta::Clear,
        title: FieldDelta::Clear,
        group: FieldDelta::Clear,
        logo: FieldDelta::Clear,
        logo_small: FieldDelta::Clear,
        epg_channel_id: FieldDelta::Clear,
        chno: FieldDelta::Clear,
        parent_code: FieldDelta::Clear,
        audio_track: FieldDelta::Clear,
        time_shift: FieldDelta::Clear,
        rec: FieldDelta::Clear,
        tmdb: FieldDelta::Clear,
        rating: FieldDelta::Clear,
        plot: FieldDelta::Clear,
        genre: FieldDelta::Clear,
        cast: FieldDelta::Clear,
        director: FieldDelta::Clear,
        release_date: FieldDelta::Clear,
        ..Default::default()
    };

    verify_video_overrides(&set_ov, &clear_ov);
    verify_series_overrides(&set_ov, &clear_ov);
    verify_fallback_overrides(&set_ov, &clear_ov);
}

#[test]
fn test_video_and_series_logo_fallbacks_cleared_and_overridden() {
    use shared::model::VideoStreamDetailProperties;

    // Test Video
    let mut video_header = PlaylistItemHeader {
        additional_properties: Some(StreamProperties::Video(Box::new(VideoStreamProperties {
            stream_icon: Arc::from("http://provider.example/icon.png"),
            details: Some(VideoStreamDetailProperties {
                movie_image: Some(Arc::from("http://provider.example/movie.png")),
                cover_big: Some(Arc::from("http://provider.example/cover.png")),
                backdrop_path: Some(vec![Arc::from("http://provider.example/backdrop.png")]),
                ..Default::default()
            }),
            ..Default::default()
        }))),
        ..Default::default()
    };

    // Test Set: set value should win over fallbacks
    let set_ov =
        ItemOverride { logo: FieldDelta::Set("http://new.example/custom.png".to_string()), ..Default::default() };
    apply_override(&mut video_header, &set_ov);
    let Some(StreamProperties::Video(v)) = &video_header.additional_properties else {
        panic!("expected Video");
    };
    assert_eq!(v.stream_icon.as_ref(), "http://new.example/custom.png");
    let details = v.details.as_ref().unwrap();
    assert_eq!(details.movie_image.as_deref(), Some("http://new.example/custom.png"));
    assert_eq!(details.cover_big.as_deref(), Some("http://new.example/custom.png"));

    // Test Clear: should clear stream_icon and all fallbacks
    let clear_ov = ItemOverride { logo: FieldDelta::Clear, ..Default::default() };
    apply_override(&mut video_header, &clear_ov);
    let Some(StreamProperties::Video(v)) = &video_header.additional_properties else {
        panic!("expected Video");
    };
    assert_eq!(v.stream_icon.as_ref(), "");
    let details = v.details.as_ref().unwrap();
    assert_eq!(details.movie_image, None);
    assert_eq!(details.cover_big, None);
    assert_eq!(details.backdrop_path, None);

    // Test Series
    let mut series_header = PlaylistItemHeader {
        additional_properties: Some(StreamProperties::Series(Box::new(SeriesStreamProperties {
            cover: Arc::from("http://provider.example/cover.png"),
            backdrop_path: Some(vec![Arc::from("http://provider.example/backdrop.png")]),
            ..Default::default()
        }))),
        ..Default::default()
    };

    apply_override(&mut series_header, &clear_ov);
    let Some(StreamProperties::Series(s)) = &series_header.additional_properties else {
        panic!("expected Series");
    };
    assert_eq!(s.cover.as_ref(), "");
    assert_eq!(s.backdrop_path, None);
}
