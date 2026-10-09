use super::*;

#[test]
fn header_field_parse_round_trips_every_variant() {
    for field in [
        HeaderField::Id,
        HeaderField::ProviderId,
        HeaderField::Title,
        HeaderField::Name,
        HeaderField::Logo,
        HeaderField::LogoSmall,
        HeaderField::ParentCode,
        HeaderField::AudioTrack,
        HeaderField::TimeShift,
        HeaderField::Rec,
        HeaderField::Url,
        HeaderField::Group,
        HeaderField::Caption,
        HeaderField::Input,
        HeaderField::Type,
        HeaderField::EpgChannelId,
        HeaderField::Chno,
        HeaderField::Genre,
    ] {
        assert_eq!(HeaderField::parse(field.as_str()), Some(field), "{field} did not round-trip");
        // Lookup stayed case-insensitive.
        assert_eq!(HeaderField::parse(&field.as_str().to_uppercase()), Some(field));
    }
    assert_eq!(HeaderField::parse("epg_id"), Some(HeaderField::EpgChannelId), "legacy alias must still parse");
    assert_eq!(HeaderField::parse("input_stream_id"), None);
    assert_eq!(HeaderField::parse(""), None);
}

#[test]
fn m3u_item_exposes_provider_id_but_not_the_header_only_fields() {
    // The M3U resource endpoint resolves a URL path segment through
    // `get_field`, so which names resolve is externally visible behaviour.
    // M3uPlaylistItem carries input_name, item_type and additional_properties,
    // but none of them were ever addressable by name and must stay that way.
    let mut header = PlaylistItemHeader { chno: 7, ..PlaylistItemHeader::default() };
    header.name = "Channel".intern();
    let item = M3uPlaylistItem::from(&PlaylistItem { header });

    assert_eq!(item.get_field("name").as_deref(), Some("Channel"));
    assert_eq!(item.get_field("chno").as_deref(), Some("7"));
    assert_eq!(item.get_field("caption").as_deref(), Some("Channel"));

    for absent in ["id", "input", "type", "genre", "not_a_field"] {
        assert!(item.get_field(absent).is_none(), "{absent} must not resolve on an M3U item");
    }
}

#[test]
fn m3u_to_m3u_emits_tvg_id_from_epg_channel_id() {
    let item = M3uPlaylistItem {
        virtual_id: VirtualId::default(),
        provider_id: "prov1".intern(),
        input_stream_id: "prov1".intern(),
        upstream_user_agent: None,
        name: "Test Channel".intern(),
        chno: 0,
        logo: "".intern(),
        logo_small: "".intern(),
        group: "Test Group".intern(),
        title: "Test Title".intern(),
        parent_code: "".intern(),
        audio_track: "".intern(),
        time_shift: "".intern(),
        rec: "".intern(),
        url: "http://example.com/stream".intern(),
        epg_channel_id: Some("epg_channel_123".intern()),
        input_name: "test".intern(),
        item_type: PlaylistItemType::Live,
        t_stream_url: "".intern(),
        t_resource_url: None,
        t_catchup_source: None,
        t_catchup_mode: None,
        source_ordinal: 0,
        additional_properties: None,
    };

    let output = item.to_m3u(None, false);
    assert!(output.contains(r#"tvg-id="epg_channel_123""#), "M3U output must contain tvg-id from epg_channel_id");
}

#[test]
fn m3u_to_m3u_preserves_mixed_case_tvg_id() {
    // EPG matching is case-insensitive, so ids are no longer lowercased when parsed.
    // The M3U tvg-id output must preserve the channel's original source case.
    let item = M3uPlaylistItem {
        virtual_id: VirtualId::default(),
        provider_id: "prov1".intern(),
        input_stream_id: "prov1".intern(),
        upstream_user_agent: None,
        name: "Test Channel".intern(),
        chno: 0,
        logo: "".intern(),
        logo_small: "".intern(),
        group: "Test Group".intern(),
        title: "Test Title".intern(),
        parent_code: "".intern(),
        audio_track: "".intern(),
        time_shift: "".intern(),
        rec: "".intern(),
        url: "http://example.com/stream".intern(),
        epg_channel_id: Some("CNN.us".intern()),
        input_name: "test".intern(),
        item_type: PlaylistItemType::Live,
        t_stream_url: "".intern(),
        t_resource_url: None,
        t_catchup_source: None,
        t_catchup_mode: None,
        source_ordinal: 0,
        additional_properties: None,
    };

    let output = item.to_m3u(None, false);
    assert!(output.contains(r#"tvg-id="CNN.us""#), "M3U tvg-id must preserve original case, got: {output}");
}

#[test]
fn m3u_to_m3u_emits_tvg_chno_from_chno() {
    let item = M3uPlaylistItem {
        virtual_id: VirtualId::default(),
        provider_id: "prov1".intern(),
        input_stream_id: "prov1".intern(),
        upstream_user_agent: None,
        name: "Test Channel".intern(),
        chno: 42,
        logo: "".intern(),
        logo_small: "".intern(),
        group: "Test Group".intern(),
        title: "Test Title".intern(),
        parent_code: "".intern(),
        audio_track: "".intern(),
        time_shift: "".intern(),
        rec: "".intern(),
        url: "http://example.com/stream".intern(),
        epg_channel_id: None,
        input_name: "test".intern(),
        item_type: PlaylistItemType::Live,
        t_stream_url: "".intern(),
        t_resource_url: None,
        t_catchup_source: None,
        t_catchup_mode: None,
        source_ordinal: 0,
        additional_properties: None,
    };

    let output = item.to_m3u(None, false);
    assert!(output.contains(r#"tvg-chno="42""#), "M3U output must contain tvg-chno from chno");
}

#[test]
fn m3u_to_m3u_omits_tvg_chno_when_chno_is_zero() {
    let item = M3uPlaylistItem {
        virtual_id: VirtualId::default(),
        provider_id: "prov1".intern(),
        input_stream_id: "prov1".intern(),
        upstream_user_agent: None,
        name: "Test Channel".intern(),
        chno: 0,
        logo: "".intern(),
        logo_small: "".intern(),
        group: "Test Group".intern(),
        title: "Test Title".intern(),
        parent_code: "".intern(),
        audio_track: "".intern(),
        time_shift: "".intern(),
        rec: "".intern(),
        url: "http://example.com/stream".intern(),
        epg_channel_id: None,
        input_name: "test".intern(),
        item_type: PlaylistItemType::Live,
        t_stream_url: "".intern(),
        t_resource_url: None,
        t_catchup_source: None,
        t_catchup_mode: None,
        source_ordinal: 0,
        additional_properties: None,
    };

    let output = item.to_m3u(None, false);
    assert!(!output.contains("tvg-chno"), "M3U output must not contain tvg-chno when chno is 0");
}

#[test]
fn m3u_to_m3u_preserves_catchup_attributes() {
    let item = M3uPlaylistItem {
        virtual_id: VirtualId::default(),
        provider_id: "prov1".intern(),
        input_stream_id: "prov1".intern(),
        upstream_user_agent: None,
        name: "Test Channel".intern(),
        chno: 0,
        logo: "".intern(),
        logo_small: "".intern(),
        group: "Test Group".intern(),
        title: "Test Title".intern(),
        parent_code: "".intern(),
        audio_track: "".intern(),
        time_shift: "".intern(),
        rec: "".intern(),
        url: "http://example.com/stream".intern(),
        epg_channel_id: Some("channel1".intern()),
        input_name: "test".intern(),
        item_type: PlaylistItemType::Live,
        t_stream_url: "".intern(),
        t_resource_url: None,
        t_catchup_source: None,
        t_catchup_mode: None,
        source_ordinal: 0,
        additional_properties: Some(StreamProperties::Live(Box::new(LiveStreamProperties {
            catchup: Some(CatchupProperties {
                mode: Some("append".intern()),
                days: Some("7".intern()),
                source: Some("?offset=-${offset}&utcstart=${timestamp}".intern()),
                correction: Some("-2.0".intern()),
                catchup_type: Some("xc".intern()),
                extra_attributes: vec![CatchupAttribute { name: "catchup-extra".intern(), value: "keep".intern() }],
                ..CatchupProperties::default()
            }),
            ..LiveStreamProperties::default()
        }))),
    };

    let output = item.to_m3u(None, false);
    assert!(output.contains(r#"catchup="append""#));
    assert!(output.contains(r#"catchup-days="7""#));
    assert!(output.contains(r#"catchup-source="?offset=-${offset}&utcstart=${timestamp}""#));
    assert!(output.contains(r#"catchup-correction="-2.0""#));
    assert!(output.contains(r#"catchup-type="xc""#));
    assert!(output.contains(r#"catchup-extra="keep""#));
}

#[test]
fn m3u_to_m3u_unifies_append_to_catchup_type_only() {
    let item = M3uPlaylistItem {
        virtual_id: VirtualId::default(),
        provider_id: "prov1".intern(),
        input_stream_id: "prov1".intern(),
        upstream_user_agent: None,
        name: "Test Channel".intern(),
        chno: 0,
        logo: "".intern(),
        logo_small: "".intern(),
        group: "Test Group".intern(),
        title: "Test Title".intern(),
        parent_code: "".intern(),
        audio_track: "".intern(),
        time_shift: "".intern(),
        rec: "".intern(),
        url: "http://example.com/stream".intern(),
        epg_channel_id: Some("channel1".intern()),
        input_name: "test".intern(),
        item_type: PlaylistItemType::Live,
        t_stream_url: "".intern(),
        t_resource_url: None,
        t_catchup_source: None,
        t_catchup_mode: None,
        source_ordinal: 0,
        additional_properties: Some(StreamProperties::Live(Box::new(LiveStreamProperties {
            catchup: Some(CatchupProperties {
                mode: Some("append".intern()),
                days: Some("7".intern()),
                ..CatchupProperties::default()
            }),
            ..LiveStreamProperties::default()
        }))),
    };

    let output = item.to_m3u(None, false);
    assert!(!output.contains(r#"catchup="append""#));
    assert!(output.contains(r#"catchup-type="append""#));
    assert!(output.contains(r#"catchup-days="7""#));
    assert!(!output.contains(r#"catchup-type="xc""#));
}

#[test]
fn m3u_to_m3u_uses_rewritten_catchup_mode_and_source() {
    let item = M3uPlaylistItem {
        virtual_id: VirtualId::default(),
        provider_id: "prov1".intern(),
        input_stream_id: "prov1".intern(),
        upstream_user_agent: None,
        name: "Test Channel".intern(),
        chno: 0,
        logo: "".intern(),
        logo_small: "".intern(),
        group: "Test Group".intern(),
        title: "Test Title".intern(),
        parent_code: "".intern(),
        audio_track: "".intern(),
        time_shift: "".intern(),
        rec: "".intern(),
        url: "http://example.com/stream".intern(),
        epg_channel_id: Some("channel1".intern()),
        input_name: "test".intern(),
        item_type: PlaylistItemType::Live,
        t_stream_url: "".intern(),
        t_resource_url: None,
        t_catchup_source: Some("http://proxy.example/m3u-catchup/token?v0={utc}".intern()),
        t_catchup_mode: Some("default".intern()),
        source_ordinal: 0,
        additional_properties: Some(StreamProperties::Live(Box::new(LiveStreamProperties {
            catchup: Some(CatchupProperties {
                mode: Some("append".intern()),
                source: Some("?offset=-${offset}".intern()),
                ..CatchupProperties::default()
            }),
            ..LiveStreamProperties::default()
        }))),
    };

    let output = item.to_m3u(None, false);
    assert!(output.contains(r#"catchup="default""#));
    assert!(output.contains(r#"catchup-source="http://proxy.example/m3u-catchup/token?v0={utc}""#));
    assert!(!output.contains(r#"catchup-source="?offset=-${offset}""#));
}

#[test]
fn m3u_to_m3u_flussonic_source_override_uses_rewritten_mode() {
    let mut item = M3uPlaylistItem::from(&PlaylistItem { header: PlaylistItemHeader::default() });
    item.url = "https://provider.example/channel/mono.m3u8?token=secret".intern();
    item.t_stream_url = "http://proxy.example/m3u-stream/alice/pass/42/index.m3u8".intern();
    item.t_catchup_mode = Some("default".intern());
    item.t_catchup_source = Some("http://proxy.example/m3u-catchup/token?v0={utc}&v1={duration}".intern());
    item.additional_properties = Some(StreamProperties::Live(Box::new(LiveStreamProperties {
        catchup: Some(CatchupProperties {
            mode: Some("fs".intern()),
            catchup_type: Some("flussonic".intern()),
            source: Some("https://provider.example/channel/archive-{utc}-{duration}.m3u8?token=secret".intern()),
            ..CatchupProperties::default()
        }),
        ..LiveStreamProperties::default()
    })));
    let output = item.to_m3u(None, false);
    assert!(output.contains(r#"catchup="default""#));
    assert!(output.contains("v0={utc}&v1={duration}"));
    assert!(!output.contains("catchup-type="));
    assert!(!output.contains("token=secret"));
    assert!(output.ends_with("/42/index.m3u8"));
    item.t_catchup_mode = None;
    item.t_catchup_source = None;
    let native = item.to_m3u(None, false);
    assert!(native.contains(r#"catchup-type="flussonic""#));
    assert!(!native.contains("catchup-source="));
}

#[test]
fn m3u_to_m3u_emits_only_configured_upstream_user_agent() {
    let mut item = M3uPlaylistItem::from(&PlaylistItem { header: PlaylistItemHeader::default() });
    item.upstream_user_agent = Some("Provider-UA".intern());

    assert!(item.to_m3u(None, false).contains("#EXTVLCOPT:http-user-agent=Provider-UA\n"));
    item.upstream_user_agent = None;
    assert!(!item.to_m3u(None, false).contains("#EXTVLCOPT:http-user-agent="));
}
