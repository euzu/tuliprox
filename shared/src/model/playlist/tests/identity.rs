use super::*;

#[test]
fn cluster_is_the_single_source_of_truth_for_every_item_type() {
    use strum::IntoEnumIterator;

    for item_type in PlaylistItemType::iter() {
        let cluster = item_type.cluster();

        // `From` and the inherent method cannot disagree: one delegates.
        assert_eq!(XtreamCluster::from(item_type), cluster, "{item_type:?}");

        // is_cluster agrees with cluster() for the right one and rejects the
        // other two. This is what used to be a separately written match.
        assert!(item_type.is_cluster(cluster), "{item_type:?} should be in its own cluster");
        for other in [XtreamCluster::Live, XtreamCluster::Video, XtreamCluster::Series] {
            assert_eq!(item_type.is_cluster(other), other == cluster, "{item_type:?} vs {other:?}");
        }
    }
}

#[test]
fn from_stalker_uuid_is_seeded_with_input_name_not_category() {
    let mut item = StalkerPlaylistItem {
        stream_id: 42,
        category_name: Internable::intern("News".to_string()),
        ..StalkerPlaylistItem::default()
    };
    let converted_a = PlaylistItem::from_stalker(&item, "input_a");
    // A category rename must not change the identity of the channel.
    item.category_name = Internable::intern("World News".to_string());
    let converted_b = PlaylistItem::from_stalker(&item, "input_a");
    assert_eq!(converted_a.header.uuid, converted_b.header.uuid);
    // A different input owning the same stream id must produce a distinct identity.
    let converted_c = PlaylistItem::from_stalker(&item, "input_b");
    assert_ne!(converted_a.header.uuid, converted_c.header.uuid);
}

#[test]
fn xtream_playlist_item_conversion_falls_back_to_numeric_id_in_url() {
    let mut item = PlaylistItem {
        header: PlaylistItemHeader {
            id: "channel-alpha".intern(),
            url: "http://provider.example/live/user/pass/12345.ts".intern(),
            input_name: "input".intern(),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            ..PlaylistItemHeader::default()
        },
    };
    item.header.freeze_input_stream_id();

    let xtream_item = XtreamPlaylistItem::from(&item);
    assert_eq!(xtream_item.provider_id, 12345);
}

#[test]
fn xtream_playlist_item_preserves_numeric_input_stream_id_as_string() {
    let mut item = PlaylistItem {
        header: PlaylistItemHeader {
            id: "80510".intern(),
            virtual_id: VirtualId::new(1001),
            url: "http://provider.example/live/user/pass/80510.ts".intern(),
            input_name: "input".intern(),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            ..PlaylistItemHeader::default()
        },
    };
    item.header.freeze_input_stream_id();

    let xtream_item = XtreamPlaylistItem::from(&item);

    assert_eq!(xtream_item.provider_id, 80510);
    assert_eq!(xtream_item.input_stream_id.as_ref(), "80510");
    assert_eq!(xtream_item.get_input_stream_id().as_deref(), Some("80510"));
}

#[test]
fn alphanumeric_m3u_input_stream_id_survives_xtream_materialization() {
    let mut source = PlaylistItem {
        header: PlaylistItemHeader {
            id: "channel-alpha".intern(),
            url: "http://provider.example/live/user/pass/12345.ts".intern(),
            input_name: "input".intern(),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            ..PlaylistItemHeader::default()
        },
    };
    source.header.freeze_input_stream_id();

    let m3u_item = M3uPlaylistItem::from(&source);
    let common_item = PlaylistItem::from(&m3u_item);
    let xtream_item = XtreamPlaylistItem::from(&common_item);

    assert_eq!(m3u_item.input_stream_id.as_ref(), "channel-alpha");
    assert_eq!(xtream_item.provider_id, 12345);
    assert_eq!(xtream_item.input_stream_id.as_ref(), "channel-alpha");
}

#[test]
fn url_hash_input_stream_id_is_preserved_verbatim() {
    let mut source = PlaylistItem {
        header: PlaylistItemHeader {
            id: "d34db33f".intern(),
            url: "http://provider.example/live/channel.m3u8".intern(),
            ..PlaylistItemHeader::default()
        },
    };
    source.header.freeze_input_stream_id();

    let m3u_item = M3uPlaylistItem::from(&source);

    assert_eq!(m3u_item.input_stream_id.as_ref(), "d34db33f");
}

#[test]
fn target_field_mapping_cannot_change_frozen_input_stream_id() {
    let mut header = PlaylistItemHeader { id: "origin-alpha".intern(), ..PlaylistItemHeader::default() };
    header.freeze_input_stream_id();

    assert!(header.set_field("id", "target-id"));

    assert_eq!(header.id.as_ref(), "target-id");
    assert_eq!(header.input_stream_id.as_ref(), "origin-alpha");
    assert!(!header.set_field("input_stream_id", "unexpected"));
    assert_eq!(header.input_stream_id.as_ref(), "origin-alpha");
}

#[test]
fn legacy_playlist_items_fall_back_to_provider_id_without_virtual_id() {
    let mut source = PlaylistItem {
        header: PlaylistItemHeader {
            id: "legacy-alpha".intern(),
            virtual_id: VirtualId::new(7001),
            url: "http://provider.example/live/user/pass/80510.ts".intern(),
            ..PlaylistItemHeader::default()
        },
    };
    source.header.freeze_input_stream_id();
    let mut m3u_item = M3uPlaylistItem::from(&source);
    m3u_item.input_stream_id = "".intern();
    let mut xtream_item = XtreamPlaylistItem::from(&source);
    xtream_item.input_stream_id = "".intern();

    assert_eq!(m3u_item.get_input_stream_id().as_deref(), Some("legacy-alpha"));
    assert_eq!(xtream_item.get_input_stream_id().as_deref(), Some("80510"));
    xtream_item.provider_id = 0;
    xtream_item.url = "http://provider.example/live/channel.ts".intern();
    assert_eq!(xtream_item.get_input_stream_id(), None);

    let mut legacy_common_item = PlaylistItem::from(&xtream_item);
    assert!(legacy_common_item.header.id.is_empty());
    assert_eq!(legacy_common_item.get_input_stream_id(), None);
    legacy_common_item.header.id = "target-mapped-id".intern();
    let rematerialized_m3u_item = M3uPlaylistItem::from(&legacy_common_item);
    let rematerialized_xtream_item = XtreamPlaylistItem::from(&legacy_common_item);
    assert_eq!(legacy_common_item.header.id.as_ref(), "target-mapped-id");
    assert_eq!(legacy_common_item.get_input_stream_id(), None);
    assert!(rematerialized_m3u_item.provider_id.is_empty());
    assert_eq!(rematerialized_m3u_item.get_input_stream_id(), None);
    assert_eq!(rematerialized_xtream_item.provider_id, 0);
    assert_eq!(rematerialized_xtream_item.get_input_stream_id(), None);

    xtream_item.input_stream_id = "explicit-origin-alpha".intern();
    assert_eq!(xtream_item.get_input_stream_id().as_deref(), Some("explicit-origin-alpha"));
    let identified_common_item = PlaylistItem::from(&xtream_item);
    assert_eq!(identified_common_item.header.input_stream_id.as_ref(), "explicit-origin-alpha");
    assert_eq!(identified_common_item.get_input_stream_id().as_deref(), Some("explicit-origin-alpha"));

    let identified_m3u_item = M3uPlaylistItem::from(&identified_common_item);
    let identified_xtream_item = XtreamPlaylistItem::from(&identified_common_item);
    assert_eq!(identified_m3u_item.input_stream_id.as_ref(), "explicit-origin-alpha");
    assert_eq!(identified_m3u_item.get_input_stream_id().as_deref(), Some("explicit-origin-alpha"));
    assert_eq!(identified_xtream_item.provider_id, 0);
    assert_eq!(identified_xtream_item.input_stream_id.as_ref(), "explicit-origin-alpha");
    assert_eq!(identified_xtream_item.get_input_stream_id().as_deref(), Some("explicit-origin-alpha"));
}
