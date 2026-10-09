use super::{
    apply_staged_overlay_groups, exec_rename, execute_pipe, map_channel, playlist_update_run_result,
    report_input_job_completion, test_group, InputJobState, PlaylistRunCollectSink,
};
use crate::fetched_playlist::FetchedPlaylist;
use shared::{
    error::TuliproxError,
    foundation::{get_filter, MapperScript},
    model::{
        ClusterFlags, ConfigRenameDto, ConfigTargetDto, EventMessage, FieldSetAccessor, InputType, ItemField,
        M3uPlaylistItem, PlaylistEntry, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType,
        PlaylistUpdateRunId, PlaylistUpdateRunOrder, PlaylistUpdateState, XtreamCluster, XtreamPlaylistItem,
    },
    utils::Internable,
};
use std::collections::HashSet;
use tuliprox_core::model::{
    CompiledMapping, CompiledMappingRule, ConfigInput, ConfigInputFlags, ConfigInputOptions, ConfigRename,
    ConfigTarget, MappingProgram,
};
use tuliprox_repository::MemoryPlaylistSource;

#[test]
fn playlist_update_run_failure_text_makes_no_unverified_previous_data_claim() {
    let events = PlaylistRunCollectSink::default();
    let run_id = PlaylistUpdateRunId::from("neutral-failure-run");
    let execution_order = PlaylistUpdateRunOrder::from(18);
    let failed_first_run = playlist_update_run_result(
        7,
        InputJobState::Failed,
        vec![TuliproxError::RepositoryPlaylist("https://first:secret@provider.example/playlist".to_string())],
        false,
    );
    let persisted_then_failed = playlist_update_run_result(
        8,
        InputJobState::Ready,
        vec![TuliproxError::RepositoryPlaylist("https://epg:secret@provider.example/guide".to_string())],
        false,
    );

    report_input_job_completion(&events, &run_id, execution_order, &failed_first_run);
    report_input_job_completion(&events, &run_id, execution_order, &persisted_then_failed);

    let emitted = events.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let progress = emitted
        .iter()
        .filter_map(|event| match event {
            EventMessage::PlaylistUpdateProgress(progress) => Some(progress),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(progress.len(), 2);
    assert_eq!(progress[0].input_id, Some(7));
    assert_eq!(progress[0].message, "Input 'input-7' failed during update");
    assert_eq!(progress[1].input_id, Some(8));
    assert_eq!(progress[1].message, "Input 'input-8' failed during update");
    assert!(progress.iter().all(|event| event.run_id.as_ref() == Some(&run_id)));
    assert!(progress.iter().all(|event| event.execution_order == Some(execution_order)));
    assert!(progress.iter().all(|event| event.state == Some(PlaylistUpdateState::Failure)));
    assert!(progress.iter().all(|event| !event.message.contains("secret")));
    assert!(progress.iter().all(|event| !event.message.contains("previous accepted data")));
}

pub(in crate::processor::playlist::tests) fn serialize_without_trailing_fields<T: serde::Serialize>(
    value: &T,
    trailing_fields: &[u8],
) -> Vec<u8> {
    let mut encoded = rmp_serde::to_vec(value).expect("playlist item should serialize");
    for expected in trailing_fields {
        assert_eq!(encoded.pop(), Some(*expected), "unexpected trailing MessagePack field");
    }
    let removed = trailing_fields.len();
    match encoded[0] {
        marker @ 0x92..=0x9f => {
            let len = usize::from(marker - 0x90);
            assert!(len >= removed, "trailing field count exceeds MessagePack sequence length");
            encoded[0] = 0x90 + u8::try_from(len - removed).unwrap_or_default();
        }
        0xdc => {
            let len = u16::from_be_bytes([encoded[1], encoded[2]]);
            let removed = u16::try_from(removed).unwrap_or(u16::MAX);
            assert!(len >= removed, "trailing field count exceeds MessagePack sequence length");
            encoded[1..3].copy_from_slice(&(len - removed).to_be_bytes());
        }
        0xdd => {
            let len = u32::from_be_bytes([encoded[1], encoded[2], encoded[3], encoded[4]]);
            let removed = u32::try_from(removed).unwrap_or(u32::MAX);
            assert!(len >= removed, "trailing field count exceeds MessagePack sequence length");
            encoded[1..5].copy_from_slice(&(len - removed).to_be_bytes());
        }
        marker => panic!("unexpected MessagePack sequence marker {marker:#x}"),
    }
    encoded
}

#[test]
fn rename_preserves_input_stream_id_captured_at_target_boundary() {
    let mut item = PlaylistItem {
        header: PlaylistItemHeader {
            id: "origin-alpha".intern(),
            url: "http://provider.example/channel.m3u8".intern(),
            ..Default::default()
        },
    };
    item.header.freeze_input_stream_id();
    let rename = ConfigRename::from(&ConfigRenameDto {
        field: ItemField::Url,
        pattern: "provider".to_string(),
        new_name: "target".to_string(),
        t_pattern: None,
    });

    exec_rename(&mut item, Some(&vec![rename]));

    assert_eq!(item.header.url.as_ref(), "http://target.example/channel.m3u8");
    assert_eq!(item.header.input_stream_id.as_ref(), "origin-alpha");
}

#[test]
fn mapper_changes_id_without_changing_frozen_input_stream_id() {
    let mut item = PlaylistItem {
        header: PlaylistItemHeader { id: "origin-alpha".intern(), name: "Channel".intern(), ..Default::default() },
    };
    item.header.freeze_input_stream_id();
    let mapping = CompiledMapping {
        rules: vec![CompiledMappingRule {
            name: None,
            filter: get_filter(r#"name ~ ".*""#, None).expect("filter should parse"),
            program: MappingProgram::Script(
                MapperScript::parse(r#"@id = "target-id""#, None).expect("mapper should parse"),
            ),
        }],
        ..Default::default()
    };

    let outcome = map_channel(item, &mapping);

    assert_eq!(outcome.matched_rules, 1);
    assert_eq!(outcome.channel.header.id.as_ref(), "target-id");
    assert_eq!(outcome.channel.header.input_stream_id.as_ref(), "origin-alpha");
}

#[test]
fn mapper_cannot_resurrect_missing_legacy_input_stream_id_from_target_id() {
    let mut source = PlaylistItem {
        header: PlaylistItemHeader {
            id: "80510".intern(),
            url: "http://provider.example/live/user/pass/80510.ts".intern(),
            input_name: "input".intern(),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            ..Default::default()
        },
    };
    source.header.freeze_input_stream_id();
    let mut legacy_xtream = XtreamPlaylistItem::from(&source);
    legacy_xtream.provider_id = 0;
    legacy_xtream.input_stream_id = "".intern();
    legacy_xtream.url = "http://provider.example/live/channel.m3u8".intern();
    let mut legacy_item = PlaylistItem::from(&legacy_xtream);
    legacy_item.header.freeze_input_stream_id();
    let mapping = CompiledMapping {
        rules: vec![CompiledMappingRule {
            name: None,
            filter: get_filter(r#"name ~ ".*""#, None).expect("filter should parse"),
            program: MappingProgram::Script(
                MapperScript::parse(r#"@id = "target-id""#, None).expect("mapper should parse"),
            ),
        }],
        ..Default::default()
    };

    let outcome = map_channel(legacy_item, &mapping);
    let materialized_m3u = M3uPlaylistItem::from(&outcome.channel);
    let materialized_xtream = XtreamPlaylistItem::from(&outcome.channel);

    assert_eq!(outcome.matched_rules, 1);
    assert_eq!(outcome.channel.header.id.as_ref(), "target-id");
    assert_eq!(outcome.channel.get_input_stream_id(), None);
    assert!(materialized_m3u.provider_id.is_empty());
    assert_eq!(materialized_m3u.get_input_stream_id(), None);
    assert_eq!(materialized_xtream.provider_id, 0);
    assert_eq!(materialized_xtream.get_input_stream_id(), None);
}

#[test]
fn execute_pipe_freezes_input_stream_id_without_rename_or_mapper() {
    let input = ConfigInput::default();
    let item = PlaylistItem { header: PlaylistItemHeader { id: "origin-alpha".intern(), ..Default::default() } };
    let source = MemoryPlaylistSource::new(vec![PlaylistGroup {
        id: 1,
        title: "Group".intern(),
        channels: vec![item],
        xtream_cluster: XtreamCluster::Live,
    }])
    .into_source();
    let mut fetched = FetchedPlaylist { input: &input, source, epg: None };
    let mut duplicates = HashSet::new();
    let target = ConfigTarget::from(&ConfigTargetDto::default());

    let (mut processed, _outcome) = execute_pipe(&target, &vec![], &mut fetched, &mut duplicates, false, None)
        .expect("target processing should succeed");
    let mut groups = processed.source.take_groups();

    assert_eq!(groups[0].channels[0].header.input_stream_id.as_ref(), "origin-alpha");
    assert!(groups[0].channels[0].header.set_field("id", "late-target-id"));
    assert_eq!(groups[0].channels[0].header.input_stream_id.as_ref(), "origin-alpha");
}

#[test]
fn legacy_messagepack_playlist_items_default_missing_input_stream_id() {
    let mut source = PlaylistItem {
        header: PlaylistItemHeader {
            id: "origin-alpha".intern(),
            url: "http://provider.example/live/user/pass/80510.ts".intern(),
            input_name: "input".intern(),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            ..Default::default()
        },
    };

    let header_bytes = serialize_without_trailing_fields(&source.header, &[0xc0, 0xa0]);
    let decoded_header: PlaylistItemHeader =
        rmp_serde::from_slice(&header_bytes).expect("legacy header should deserialize");
    assert!(decoded_header.input_stream_id.is_empty());
    assert_eq!(decoded_header.get_input_stream_id(), None);
    assert_eq!(decoded_header.upstream_user_agent, None);
    let mut decoded_header = decoded_header;
    decoded_header.freeze_input_stream_id();
    assert_eq!(decoded_header.get_input_stream_id().as_deref(), Some("origin-alpha"));

    source.header.freeze_input_stream_id();
    let mut m3u_item = M3uPlaylistItem::from(&source);
    m3u_item.input_stream_id = "".intern();
    let m3u_bytes = serialize_without_trailing_fields(&m3u_item, &[0xc0, 0xa0]);
    let decoded_m3u: M3uPlaylistItem = rmp_serde::from_slice(&m3u_bytes).expect("legacy M3U item should deserialize");
    assert!(decoded_m3u.input_stream_id.is_empty());
    assert_eq!(decoded_m3u.get_input_stream_id().as_deref(), Some("origin-alpha"));
    assert_eq!(decoded_m3u.upstream_user_agent, None);

    let mut xtream_item = XtreamPlaylistItem::from(&source);
    xtream_item.input_stream_id = "".intern();
    let xtream_bytes = serialize_without_trailing_fields(&xtream_item, &[0xc0, 0xa0]);
    let decoded_xtream: XtreamPlaylistItem =
        rmp_serde::from_slice(&xtream_bytes).expect("legacy Xtream item should deserialize");
    assert!(decoded_xtream.input_stream_id.is_empty());
    assert_eq!(decoded_xtream.get_input_stream_id().as_deref(), Some("80510"));
    assert_eq!(decoded_xtream.upstream_user_agent, None);
}

#[test]
fn previous_messagepack_playlist_items_default_missing_upstream_user_agent() {
    let source = PlaylistItem {
        header: PlaylistItemHeader {
            id: "80510".intern(),
            input_stream_id: "origin-alpha".intern(),
            ..Default::default()
        },
    };

    let header: PlaylistItemHeader = rmp_serde::from_slice(&serialize_without_trailing_fields(&source.header, &[0xc0]))
        .expect("previous header should deserialize");
    let m3u: M3uPlaylistItem =
        rmp_serde::from_slice(&serialize_without_trailing_fields(&M3uPlaylistItem::from(&source), &[0xc0]))
            .expect("previous M3U item should deserialize");
    let xtream: XtreamPlaylistItem =
        rmp_serde::from_slice(&serialize_without_trailing_fields(&XtreamPlaylistItem::from(&source), &[0xc0]))
            .expect("previous Xtream item should deserialize");

    assert_eq!(header.input_stream_id.as_ref(), "origin-alpha");
    assert_eq!(m3u.input_stream_id.as_ref(), "origin-alpha");
    assert_eq!(xtream.input_stream_id.as_ref(), "origin-alpha");
    assert_eq!(header.upstream_user_agent, None);
    assert_eq!(m3u.upstream_user_agent, None);
    assert_eq!(xtream.upstream_user_agent, None);
}

#[test]
fn messagepack_playlist_items_preserve_upstream_user_agent() -> Result<(), Box<dyn std::error::Error>> {
    let source = PlaylistItem {
        header: PlaylistItemHeader { upstream_user_agent: Some("Provider-UA".intern()), ..Default::default() },
    };

    let header: PlaylistItemHeader = rmp_serde::from_slice(&rmp_serde::to_vec(&source.header)?)?;
    let m3u: M3uPlaylistItem = rmp_serde::from_slice(&rmp_serde::to_vec(&M3uPlaylistItem::from(&source))?)?;
    let xtream: XtreamPlaylistItem = rmp_serde::from_slice(&rmp_serde::to_vec(&XtreamPlaylistItem::from(&source))?)?;

    assert_eq!(header.upstream_user_agent.as_deref(), Some("Provider-UA"));
    assert_eq!(m3u.upstream_user_agent.as_deref(), Some("Provider-UA"));
    assert_eq!(xtream.upstream_user_agent.as_deref(), Some("Provider-UA"));
    Ok(())
}

#[test]
fn staged_xtream_overlay_recomputes_uuid_on_group_load() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        options: Some(ConfigInputOptions {
            flags: ConfigInputFlags::XtreamLiveStreamUsePrefix.into(),
            ..ConfigInputOptions::defaults().clone()
        }),
        ..Default::default()
    };
    let mut staged_live = test_group(XtreamCluster::Live, "staged-live", "staged");
    staged_live.channels[0].header.id = "11203".intern();
    staged_live.channels[0].header.freeze_input_stream_id();
    staged_live.channels[0].header.url = "http://iptvhost.example/live/fake-user/fake-pass/11203.ts".intern();
    staged_live.channels[0].header.gen_uuid();
    let old_uuid = *staged_live.channels[0].header.get_uuid();

    let mut groups = apply_staged_overlay_groups(&provider, ClusterFlags::Live, Vec::new(), vec![staged_live]);

    assert_eq!(groups[0].channels[0].header.input_name.as_ref(), "provider");
    let reconstructed_url = "http://provider.example/live/real-user/real-pass/11203.ts";
    assert_eq!(groups[0].channels[0].header.url.as_ref(), reconstructed_url);
    assert_eq!(groups[0].channels[0].header.get_input_stream_id().as_deref(), Some("11203"));

    groups[0].on_load();

    let expected_uuid = shared::utils::generate_runtime_playlist_uuid(
        "provider",
        "11203",
        shared::model::PlaylistItemType::Live,
        reconstructed_url,
    );
    assert_ne!(groups[0].channels[0].header.get_uuid(), &old_uuid);
    assert_eq!(groups[0].channels[0].header.get_uuid(), &expected_uuid);
    assert_eq!(groups[0].channels[0].header.get_input_stream_id().as_deref(), Some("11203"));
}
