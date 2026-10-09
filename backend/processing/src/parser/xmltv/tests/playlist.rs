use super::*;

#[test]
fn epg_priority_merge_preserves_high_priority_metadata_and_fills_programme_gaps() {
    let merged = merge_epg_channels_by_priority(vec![
        (
            10,
            vec![epg_channel(
                "demo.channel",
                Some("Low"),
                Some("http://low/icon.png"),
                vec![epg_programme("demo.channel", 10, 20, Some("Low Show"), None)],
            )],
        ),
        (
            0,
            vec![epg_channel(
                "demo.channel",
                Some("High"),
                Some("http://high/icon.png"),
                vec![epg_programme("demo.channel", 20, 30, Some("High Show"), None)],
            )],
        ),
    ]);

    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].title.as_deref(), Some("High"));
    assert_eq!(merged[0].icon.as_deref(), Some("http://high/icon.png"));
    assert_eq!(
        merged[0].programmes.iter().map(|programme| (programme.start, programme.stop)).collect::<Vec<_>>(),
        vec![(10, 20), (20, 30)],
    );
}

#[test]
fn epg_priority_merge_backfills_programme_tags() {
    let low_priority_categories = vec![
        EpgCategory { value: "Sports".intern(), lang: None },
        EpgCategory { value: "Live".intern(), lang: Some("en".intern()) },
    ];
    let mut low_programme = epg_programme("demo.channel", 10, 20, None, None);
    low_programme.icon = Some("https://example.com/programme.jpg".intern());
    low_programme.categories = low_priority_categories.clone();
    low_programme.is_live = true;
    low_programme.is_new = true;

    let merged = merge_epg_channels_by_priority(vec![
        (
            0,
            vec![epg_channel(
                "demo.channel",
                Some("High"),
                None,
                vec![epg_programme("demo.channel", 10, 20, Some("High Title"), None)],
            )],
        ),
        (10, vec![epg_channel("demo.channel", None, None, vec![low_programme])]),
    ]);

    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].programmes.len(), 1);
    assert_eq!(merged[0].programmes[0].icon.as_deref(), Some("https://example.com/programme.jpg"));
    assert_eq!(merged[0].programmes[0].categories, low_priority_categories);
    assert!(merged[0].programmes[0].is_live);
    assert!(merged[0].programmes[0].is_new);

    let high_programme_with_categories = {
        let mut programme = epg_programme("demo.channel", 30, 40, Some("High Title 2"), None);
        programme.categories = vec![EpgCategory { value: "Drama".intern(), lang: None }];
        programme.is_new = true;
        programme
    };
    let low_programme_extra = {
        let mut programme = epg_programme("demo.channel", 30, 40, None, None);
        programme.categories = vec![EpgCategory { value: "ShouldNotWin".intern(), lang: None }];
        programme.is_live = true;
        programme
    };
    let merged = merge_epg_channels_by_priority(vec![
        (0, vec![epg_channel("demo.channel", None, None, vec![high_programme_with_categories])]),
        (10, vec![epg_channel("demo.channel", None, None, vec![low_programme_extra])]),
    ]);

    assert_eq!(merged.len(), 1);
    let programme = &merged[0].programmes[0];
    assert_eq!(programme.categories, vec![EpgCategory { value: "Drama".intern(), lang: None }],);
    assert!(programme.is_live);
    assert!(programme.is_new);
}

#[test]
fn epg_priority_merge_keeps_same_priority_deterministic_order() {
    let merged = merge_epg_channels_by_priority(vec![
        (
            0,
            vec![epg_channel(
                "demo.channel",
                Some("First"),
                None,
                vec![epg_programme("demo.channel", 10, 20, Some("First Title"), None)],
            )],
        ),
        (
            0,
            vec![epg_channel(
                "demo.channel",
                Some("Second"),
                Some("http://second/icon.png"),
                vec![
                    epg_programme("demo.channel", 10, 20, Some("Second Title"), Some("Second desc")),
                    epg_programme("demo.channel", 20, 30, Some("Second Programme"), None),
                ],
            )],
        ),
    ]);

    assert_eq!(merged[0].title.as_deref(), Some("First"));
    assert_eq!(merged[0].icon.as_deref(), Some("http://second/icon.png"));
    assert_eq!(merged[0].programmes[0].title.as_deref(), Some("First Title"));
    assert_eq!(merged[0].programmes[0].desc.as_deref(), Some("Second desc"));
    assert_eq!(merged[0].programmes.len(), 2);
}

#[test]
fn epg_priority_merge_flatten_uses_same_backfill_rules() {
    let flattened = flatten_tvguide(vec![
        Epg {
            logo_override: false,
            priority: 0,
            attributes: None,
            children: vec![Arc::new(epg_channel(
                "demo.channel",
                Some("High"),
                None,
                vec![epg_programme("demo.channel", 10, 20, Some("High Show"), None)],
            ))],
        },
        Epg {
            logo_override: false,
            priority: 10,
            attributes: None,
            children: vec![Arc::new(epg_channel(
                "demo.channel",
                Some("Low"),
                Some("http://low/icon.png"),
                vec![epg_programme("demo.channel", 20, 30, Some("Low Show"), None)],
            ))],
        },
    ])
    .expect("flattened epg");

    assert_eq!(flattened.children.len(), 1);
    assert_eq!(flattened.children[0].title.as_deref(), Some("High"));
    assert_eq!(flattened.children[0].icon.as_deref(), Some("http://low/icon.png"));
    assert_eq!(flattened.children[0].programmes.len(), 2);
}

#[test]
fn epg_priority_merge_flatten_keeps_shared_arc_channels() {
    let shared_channel = Arc::new(epg_channel(
        "demo.channel",
        Some("Shared"),
        Some("http://shared/icon.png"),
        vec![epg_programme("demo.channel", 10, 20, Some("Shared Show"), None)],
    ));

    let flattened = flatten_tvguide(vec![Epg {
        logo_override: false,
        priority: 0,
        attributes: None,
        children: vec![Arc::clone(&shared_channel)],
    }])
    .expect("flattened epg");

    assert_eq!(flattened.children.len(), 1);
    assert_eq!(flattened.children[0].title.as_deref(), Some("Shared"));
    assert_eq!(flattened.children[0].icon.as_deref(), Some("http://shared/icon.png"));
}

#[test]
fn epg_priority_merge_tracks_icon_override_per_channel() {
    let mut accumulator = EpgMergeAccumulator::new();
    accumulator.add_channel_with_programmes(
        0,
        0,
        false,
        epg_channel("demo.keep", Some("Keep"), Some("http://keep/icon.png"), vec![]),
    );
    accumulator.add_channel_with_programmes(
        1,
        1,
        true,
        epg_channel("demo.override", Some("Override"), Some("http://override/icon.png"), vec![]),
    );

    let (_epg, icon_override_channels) = accumulator.finish_epg_with_icon_overrides().expect("merged epg");

    let keep_id: Arc<str> = "demo.keep".intern();
    let override_id: Arc<str> = "demo.override".intern();

    assert!(!icon_override_channels.contains(&keep_id));
    assert!(icon_override_channels.contains(&override_id));
}

#[test]
fn epg_priority_merge_preserves_exact_id_match_when_smart_match_is_enabled() {
    run_async_test(async move {
        let dir = tempdir().unwrap();
        let epg_path = dir.path().join("smart-exact.xml");

        fs::write(
            &epg_path,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<tv>
  <channel id="demo.channel">
    <display-name>Completely Different Name</display-name>
  </channel>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="demo.channel">
    <title>Exact Match Source</title>
  </programme>
</tv>"#,
        )
        .unwrap();

        let mut smart_cfg = EpgSmartMatchConfigDto { enabled: true, ..Default::default() };
        smart_cfg.prepare().expect("smart match config");
        let tv_guide = TVGuide::new(vec![xmltv_source(epg_path, 0, false)]);
        let mut id_cache = EpgIdCache::new(Some(&tuliprox_core::model::EpgConfig {
            sources: vec![],
            smart_match: Some(EpgSmartMatchConfig::from(smart_cfg)),
        }));
        id_cache.insert_channel_epg_id("demo.channel");

        let merged = tv_guide.filter_merged(&mut id_cache).await.expect("merged epg");

        assert_eq!(merged.children.len(), 1);
        assert_eq!(merged.children[0].id.as_ref(), "demo.channel");
        assert_eq!(merged.children[0].programmes.len(), 1);
    });
}

#[test]
fn epg_channel_id_match_is_case_insensitive_and_preserves_guide_case() {
    run_async_test(async move {
        let dir = tempdir().unwrap();
        let epg_path = dir.path().join("mixed-case.xml");

        // Guide channel ids are MixedCase and differ in case from the playlist ids.
        fs::write(
            &epg_path,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<tv>
  <channel id="Sport.Extra1.DE">
    <display-name>Sport Extra</display-name>
  </channel>
  <channel id="CNN.US">
    <display-name>CNN</display-name>
  </channel>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="Sport.Extra1.DE">
    <title>Match A</title>
  </programme>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="CNN.US">
    <title>Match B</title>
  </programme>
</tv>"#,
        )
        .unwrap();

        let tv_guide = TVGuide::new(vec![xmltv_source(epg_path, 0, false)]);
        let mut id_cache = EpgIdCache::new(None);
        // Playlist epg ids arrive in a *different* case than the guide. Both origins
        // (an Xtream source and a mapper-script literal) go through the same
        // `insert_channel_epg_id`, which folds the membership key.
        id_cache.insert_channel_epg_id("sport.EXTRA1.de"); // e.g. from an Xtream source
        id_cache.insert_channel_epg_id("cnn.us"); // e.g. set by a mapper literal @epg_channel_id = "cnn.us"

        let merged = tv_guide.filter_merged(&mut id_cache).await.expect("merged epg");

        // Both MixedCase guide channels matched despite the case difference.
        assert_eq!(merged.children.len(), 2);
        let ids: HashSet<&str> = merged.children.iter().map(|c| c.id.as_ref()).collect();
        // Emitted <channel id> preserves the guide's ORIGINAL case (not folded).
        assert!(ids.contains("Sport.Extra1.DE"), "emitted ids: {ids:?}");
        assert!(ids.contains("CNN.US"), "emitted ids: {ids:?}");
        for channel in &merged.children {
            assert_eq!(channel.programmes.len(), 1, "channel {} programmes", channel.id);
        }
    });
}

#[test]
fn epg_programme_channel_match_is_case_insensitive_within_source() {
    run_async_test(async move {
        let dir = tempdir().unwrap();
        let epg_path = dir.path().join("mixed-case-programme.xml");

        fs::write(
            &epg_path,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<tv>
  <channel id="Demo.Channel">
    <display-name>Demo</display-name>
  </channel>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="demo.channel">
    <title>Case Variant Programme</title>
  </programme>
</tv>"#,
        )
        .unwrap();

        let tv_guide = TVGuide::new(vec![xmltv_source(epg_path, 0, false)]);
        let mut id_cache = EpgIdCache::new(None);
        id_cache.insert_channel_epg_id("demo.channel");

        let merged = tv_guide.filter_merged(&mut id_cache).await.expect("merged epg");

        assert_eq!(merged.children.len(), 1);
        assert_eq!(merged.children[0].id.as_ref(), "Demo.Channel");
        assert_eq!(merged.children[0].programmes.len(), 1);
        assert_eq!(merged.children[0].programmes[0].get_transient_channel_id().as_ref(), "demo.channel");
    });
}

#[test]
fn ics_dummy_policy_fills_after_real_programme_merge_without_overwriting_xmltv() {
    run_async_test(async move {
        use chrono::{Datelike, TimeZone, Utc};

        let dir = tempdir().unwrap();
        let xml_path = dir.path().join("guide.xml");
        let ics_path = dir.path().join("empty.ics");
        let now = Utc::now();
        let real_start = Utc.with_ymd_and_hms(now.year(), now.month(), now.day(), 4, 0, 0).single().expect("start");
        let real_stop = Utc.with_ymd_and_hms(now.year(), now.month(), now.day(), 6, 0, 0).single().expect("stop");

        fs::write(
            &xml_path,
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<tv>
  <channel id="f1.calendar">
    <display-name>Formula 1</display-name>
  </channel>
  <programme start="{start}" stop="{stop}" channel="f1.calendar">
    <title>Real Show</title>
  </programme>
</tv>"#,
                start = real_start.format("%Y%m%d%H%M%S %z"),
                stop = real_stop.format("%Y%m%d%H%M%S %z"),
            ),
        )
        .unwrap();
        fs::write(&ics_path, "BEGIN:VCALENDAR\nEND:VCALENDAR\n").unwrap();

        let guide = TVGuide::new(vec![
            xmltv_source(xml_path, 0, false),
            PersistedEpgSource {
                file_path: ics_path,
                priority: 1,
                logo_override: false,
                kind: PersistedEpgSourceKind::Ics {
                    channel_id: "f1.calendar".intern(),
                    channel_title: Some("Formula 1".intern()),
                    match_names: Vec::new(),
                    config: Box::new(IcsEpgSourceConfig {
                        timezone: "UTC".to_string(),
                        dummy: IcsDummyConfig {
                            enabled: true,
                            title: "No programme".to_string(),
                            description: String::new(),
                            days_past: 0,
                            days_future: 0,
                            block_hours: 4,
                            min_gap_minutes: 1,
                        },
                        ..IcsEpgSourceConfig::default()
                    }),
                },
            },
        ]);
        let mut id_cache = EpgIdCache::new(None);
        id_cache.insert_channel_epg_id("f1.calendar");

        let merged = guide.filter_merged(&mut id_cache).await.expect("merged");
        let channel = &merged.children[0];
        assert_eq!(channel.id.as_ref(), "f1.calendar");
        assert_eq!(
            channel.programmes.iter().filter(|programme| programme.title.as_deref() == Some("Real Show")).count(),
            1
        );
        let dummy_programmes = channel
            .programmes
            .iter()
            .filter(|programme| programme.title.as_deref() == Some("No programme"))
            .collect::<Vec<_>>();
        assert!(!dummy_programmes.is_empty());
        for dummy in dummy_programmes {
            assert!(
                dummy.stop <= real_start.timestamp() || dummy.start >= real_stop.timestamp(),
                "dummy overlaps real programme: {}-{}",
                dummy.start,
                dummy.stop
            );
        }
    });
}

#[test]
fn dummy_policy_selection_uses_priority_then_source_order() {
    let channel = || EpgChannel {
        id: "f1.calendar".intern(),
        title: Some("Formula 1".intern()),
        icon: None,
        programmes: Vec::new(),
    };
    let merge = |policies| {
        merge_epg_channels_by_priority_with_dummy_policies(vec![(0, vec![channel()])], policies)
            .into_iter()
            .next()
            .expect("merged channel")
    };

    let priority_winner =
        merge(vec![dummy_policy_source(10, 0, "Low priority"), dummy_policy_source(-10, 1, "High priority")]);
    assert!(!priority_winner.programmes.is_empty());
    assert!(priority_winner.programmes.iter().all(|programme| programme.title.as_deref() == Some("High priority")));

    let source_order_winner =
        merge(vec![dummy_policy_source(0, 2, "Later source"), dummy_policy_source(0, 1, "Earlier source")]);
    assert!(!source_order_winner.programmes.is_empty());
    assert!(source_order_winner
        .programmes
        .iter()
        .all(|programme| programme.title.as_deref() == Some("Earlier source")));
}

#[test]
fn xmltv_programme_tags_are_extracted() {
    run_async_test(async move {
        let dir = tempdir().expect("temp dir");
        let epg_path = dir.path().join("programme-tags.xml");
        fs::write(
            &epg_path,
            r#"<tv>
  <channel id="ESPN.us"><display-name>ESPN</display-name></channel>
  <programme start="20260718180000 +0000" stop="20260718200000 +0000" channel="ESPN.us">
    <title lang="en">Softball</title>
    <icon src="https://example.com/softball.jpg"/>
    <icon src=""/>
    <category lang="en">Softball</category>
    <category>Sports</category>
    <live/>
    <new></new>
  </programme>
</tv>"#,
        )
        .expect("write XMLTV fixture");

        let guide = TVGuide::new(vec![xmltv_source(epg_path, 0, false)]);
        let mut id_cache = EpgIdCache::new(None);
        id_cache.insert_channel_epg_id("ESPN.us");
        let merged = guide.filter_merged(&mut id_cache).await.expect("merged EPG");
        let programme = &merged.children[0].programmes[0];

        assert_eq!(
            programme.categories,
            vec![
                EpgCategory { value: "Softball".intern(), lang: Some("en".intern()) },
                EpgCategory { value: "Sports".intern(), lang: None },
            ],
        );
        assert_eq!(programme.icon.as_deref(), Some("https://example.com/softball.jpg"));
        assert!(programme.is_live);
        assert!(programme.is_new);
    });
}

/// Regression guard for the buffer-preallocation in `parse_tvguide`.
///
/// `quick_xml` uses the caller-provided `Vec<u8>` as a monotonically growing
/// read buffer (it does not call `.clear()` between events; the caller
/// slices the new portion). For an XMLTV feed with a single giant
/// `<programme>` description, that buffer used to start at 0 capacity and
/// double ~25 times on the way to ~163 MiB — every doubling being a
/// copying realloc. Starting with `Vec::with_capacity(64 * 1024)` removes
/// the realloc chain. This test feeds a programme whose description is
/// bigger than that initial capacity and checks that parsing still
/// succeeds; the absence of the test would have allowed a silent
/// regression to `Vec::new()` without breaking any visible behaviour.
#[test]
fn parse_tvguide_handles_giant_programme_description() {
    use crate::parser::xmltv::parse_tvguide;
    use tuliprox_core::model::{EPG_TAG_DESC, EPG_TAG_PROGRAMME};

    run_async_test(async {
        // 200 KiB of text content — well beyond the 64 KiB preallocation.
        let big_text = "x".repeat(200 * 1024);
        // `channel` attribute is required for the parser to fire the
        // callback on a <programme> tag (see `handle_tag_end`).
        let xml = format!(
            r#"<?xml version="1.0"?><tv><programme channel="c1"><title>t</title><desc>{big_text}</desc></programme></tv>"#
        );
        let mut emitted_tags: Vec<tuliprox_core::model::XmlTag> = Vec::new();
        parse_tvguide(xml.as_bytes(), &mut |tag| {
            emitted_tags.push(tag);
        })
        .await;

        // The whole point of the preallocation is that the full payload
        // survives the round trip — not just the presence of a tag.
        let programme = emitted_tags
            .iter()
            .find(|tag| tag.name.as_ref() == EPG_TAG_PROGRAMME)
            .expect("parser emitted no <programme> tag");
        let children = programme.children.as_deref().expect("<programme> has no children");
        let desc = children
            .iter()
            .find(|child| child.name.as_ref() == EPG_TAG_DESC)
            .expect("<programme> emitted no <desc> child");
        assert_eq!(
            desc.value.as_deref().map(str::len),
            Some(big_text.len()),
            "<desc> payload was truncated; got {} bytes, expected {}",
            desc.value.as_deref().map_or(0, str::len),
            big_text.len()
        );
    });
}
