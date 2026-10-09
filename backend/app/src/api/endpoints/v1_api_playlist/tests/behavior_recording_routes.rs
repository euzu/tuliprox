use super::{test_app_config, test_app_state};
use crate::{
    api::model::{recording::recording_source_resolution::resolve_recording_config, RecordingQueue},
    model::{ConfigInput, ConfigSource, ConfigTarget, RecordingConfig, SourcesConfig},
};
use arc_swap::ArcSwap;
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use shared::{
    foundation::Filter,
    model::{provider_saturation::build_group_lookup, ProcessingOrder, XtreamCluster},
    utils::Internable,
};
use std::{collections::HashMap, sync::Arc};
use url::Url;

#[tokio::test]
async fn stable_recording_route_rejects_target_and_input_from_different_sources() {
    let app_config = Arc::new(test_app_config(
        Arc::new(ConfigInput { id: 7, name: "input-a".intern(), ..Default::default() }),
        ConfigSource {
            inputs: vec!["input-a".intern()],
            targets: vec![Arc::new(ConfigTarget {
                curation: None,
                id: 11,
                enabled: true,
                name: "stable-target".to_string(),
                options: None,
                sort: None,
                filter: Filter::default().into(),
                output: vec![],
                rename: None,
                mapping_ids: None,
                mapping: Arc::default(),
                favourites: None,
                processing_order: ProcessingOrder::default(),
                execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
                watch: None,
                use_memory_cache: false,
            })],
        },
    ));
    let app_state = test_app_state(Arc::clone(&app_config));
    let token =
        crate::auth::create_access_token(&app_config.access_token_secret, 1, crate::auth::scope::INTERNAL_PLAYER);
    let fingerprint = crate::auth::Fingerprint::new(
        "test".to_string(),
        "127.0.0.1".to_string(),
        "127.0.0.1:1234".parse().expect("test socket address"),
    );

    let response = super::super::playlist_recording_stream(
        fingerprint,
        AxumPath((token, "live".to_string(), 42)),
        Query(super::super::RecordingStreamQuery {
            target_name: "stable-target".to_string(),
            input_name: "input-b".to_string(),
            provider_allocation_id: None,
        }),
        State(app_state),
        axum::http::HeaderMap::new(),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn recording_target_resolution_uses_stable_name_and_input_across_runtime_ids() {
    let input_a = Arc::new(ConfigInput { id: 7, name: "input-a".intern(), ..Default::default() });
    let input_b = Arc::new(ConfigInput { id: 8, name: "input-b".intern(), ..Default::default() });
    let target = |id, name: &str| {
        Arc::new(ConfigTarget {
            curation: None,
            id,
            enabled: true,
            name: name.to_string(),
            options: None,
            sort: None,
            filter: Filter::default().into(),
            output: vec![],
            rename: None,
            mapping_ids: None,
            mapping: Arc::default(),
            favourites: None,
            processing_order: ProcessingOrder::default(),
            execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
            watch: None,
            use_memory_cache: false,
        })
    };
    let sources = |later_target_id| {
        let inputs = vec![Arc::clone(&input_a), Arc::clone(&input_b)];
        SourcesConfig {
            batch_files: vec![],
            provider: vec![],
            group_lookup: build_group_lookup(&inputs),
            inputs,
            sources: vec![
                ConfigSource { inputs: vec!["input-a".intern()], targets: vec![target(11, "target-a")] },
                ConfigSource {
                    inputs: vec!["input-b".intern()],
                    targets: vec![target(later_target_id, "stable-target")],
                },
            ],
            templates: None,
        }
    };
    let mut app_config = test_app_config(
        Arc::new(ConfigInput { id: 0, name: "unused".intern(), ..Default::default() }),
        ConfigSource { inputs: vec![], targets: vec![] },
    );
    app_config.sources = Arc::new(ArcSwap::from_pointee(sources(12)));

    let snapshot = app_config.sources.load();
    let resolved = resolve_recording_config(snapshot.as_ref(), "stable-target", "input-b")
        .expect("stable source in first snapshot");
    assert_eq!(resolved.target.id, 12);
    assert_eq!(resolved.input.name.as_ref(), "input-b");
    assert!(resolve_recording_config(snapshot.as_ref(), "stable-target", "input-a").is_none());
    drop(snapshot);

    app_config.sources.store(Arc::new(sources(37)));
    let snapshot = app_config.sources.load();
    let resolved =
        resolve_recording_config(snapshot.as_ref(), "stable-target", "input-b").expect("stable source after reload");
    assert_eq!(resolved.target.id, 37);
}

#[test]
fn recording_source_descriptor_is_token_free_and_percent_encodes_names() {
    let url = crate::api::model::recording::recording_source_resolution::build_recording_source_descriptor(
        "News/HD &+",
        "input/name ?+",
        42,
        XtreamCluster::Video,
    )
    .expect("valid recording source descriptor");
    let parsed = Url::parse(&url).expect("parse recording source descriptor");
    let query = parsed.query_pairs().collect::<HashMap<_, _>>();

    assert_eq!(parsed.scheme(), "tuliprox-recording");
    assert_eq!(parsed.host_str(), Some("source"));
    assert_eq!(query.get("target_name").map(std::convert::AsRef::as_ref), Some("News/HD &+"));
    assert_eq!(query.get("input_name").map(std::convert::AsRef::as_ref), Some("input/name ?+"));
    assert_eq!(query.get("virtual_id").map(std::convert::AsRef::as_ref), Some("42"));
    assert_eq!(query.get("cluster").map(std::convert::AsRef::as_ref), Some("movie"));
    assert!(!url.contains("token"));
}

#[test]
fn future_scheduled_recording_descriptor_round_trips_without_token() {
    let url = crate::api::model::recording::recording_source_resolution::build_recording_source_descriptor(
        "stable-target",
        "input-a",
        42,
        XtreamCluster::Live,
    )
    .expect("valid recording url");
    let download_cfg = RecordingConfig::from(&shared::model::RecordingConfigDto {
        directory: Some("/tmp".to_string()),
        ..Default::default()
    });
    let metadata = shared::model::RecordingMetadata::new_live(
        shared::model::RecordingOwner::User(shared::model::UserId::from("web:alice")),
        shared::model::RecordingVisibility::Private,
        shared::model::recording::RecordingSource::new("stable-target", "42", "input-a"),
        1_700_000_000,
        1_700_003_600,
        0,
        0,
    );
    let recording = crate::api::model::RecordingTask::new(
        shared::model::RecordingKind::Live,
        &url,
        "recording.ts",
        &download_cfg,
        Some("input-a".intern()),
        0,
        metadata,
    )
    .expect("valid recording task");
    let persisted = RecordingQueue::to_persisted(&recording);
    let restored = RecordingQueue::from_persisted(persisted.clone()).expect("restore recording task");

    assert_eq!(persisted.url, url);
    assert_eq!(restored.url.as_str(), url);
    assert!(persisted.url.starts_with("tuliprox-recording://source?"));
    assert!(!persisted.url.contains("token"));
    assert!(!persisted.url.contains("/11/"));
    assert!(persisted.url.contains("target_name=stable-target"));
}

#[test]
fn epg_channel_match_applies_the_target_output_case() {
    let item = "NPO3.nl".intern();
    let requested_lowercase =
        crate::utils::canonicalize_untrusted_epg_id("npo3.nl", crate::utils::EpgIdOutputCase::LowercaseAscii);

    assert!(super::super::epg_channel_id_matches(
        Some(&item),
        &requested_lowercase,
        crate::utils::EpgIdOutputCase::LowercaseAscii
    ));
    assert!(!super::super::epg_channel_id_matches(
        Some(&item),
        &requested_lowercase,
        crate::utils::EpgIdOutputCase::Preserve
    ));
}

#[test]
fn epg_channel_match_rejects_absent_ids() {
    let requested = "npo3.nl".intern();
    assert!(!super::super::epg_channel_id_matches(None, &requested, crate::utils::EpgIdOutputCase::Preserve));
}
