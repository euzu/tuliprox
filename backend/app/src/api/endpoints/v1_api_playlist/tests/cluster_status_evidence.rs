use super::{test_app_config, test_app_state};
use crate::model::{Config, ConfigInput, ConfigInputOptions, ConfigSource};
use axum::{extract::State, Json};
use shared::{
    model::{
        ConfigInputOptionsDto, ConfigInputUpdateQualityDto, InputRefreshPolicy, InputType,
        PersistedPlaylistUpdateTechnicalState as Technical, XtreamCluster,
    },
    utils::Internable,
};
use std::{path::PathBuf, sync::Arc};

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "Keeps publication, file read, fresh-instance and optional browser assertions in one auditable sequence."
)]
async fn playlist_update_status_real_publication_file_read_and_fresh_instance() {
    let temporary = tempfile::tempdir().unwrap();
    let export = std::env::var_os("TULIPROX_STATUS_EVIDENCE_DIR").map(PathBuf::from);
    if let Some(path) = &export {
        std::fs::create_dir_all(path).unwrap();
    }
    let mut router = axum::Router::new();
    let cases = [
        ("F01-K0", None, None),
        ("F01-K1", Some([0, 0, 0]), None),
        ("F01-K2", Some([0, 0, 0]), None),
        ("F02-K3", Some([90, 0, 0]), None),
        ("F02-K4", Some([90, 90, 90]), None),
        ("F03", None, Some(XtreamCluster::Series)),
        ("F04", Some([90, 90, 90]), Some(XtreamCluster::Series)),
        ("F05", Some([90, 90, 90]), Some(XtreamCluster::Video)),
        ("F07", None, None),
    ];
    for (name, thresholds, failure) in cases {
        let mut input = ConfigInput {
            id: 17,
            name: name.intern(),
            input_type: InputType::Xtream,
            enabled: true,
            url: "http://fixture.invalid".into(),
            username: Some("fixture".into()),
            password: None,
            ..Default::default()
        };
        input.options = thresholds.map(|t| {
            ConfigInputOptions::from(&ConfigInputOptionsDto {
                update_quality: ConfigInputUpdateQualityDto { live: t[0], vod: t[1], series: t[2] },
                ..Default::default()
            })
        });
        if name == "F01-K1" {
            let absent_quality: ConfigInputOptionsDto = serde_json::from_value(serde_json::json!({})).unwrap();
            input.options = Some(ConfigInputOptions::from(&absent_quality));
        }
        let input = Arc::new(input);
        let config = test_app_config(input.clone(), ConfigSource { inputs: vec![input.name.clone()], targets: vec![] });
        let root = temporary.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        config
            .config
            .store(Arc::new(Config { storage_dir: root.to_string_lossy().into_owned(), ..Default::default() }));
        let config = Arc::new(config);
        let requested = [XtreamCluster::Live, XtreamCluster::Video, XtreamCluster::Series];
        let (mut event, mut saved) = tuliprox_processing::processor::publication_fixture_status(
            &config,
            &input,
            &requested,
            failure,
            InputRefreshPolicy::NORMAL,
        )
        .await;
        let path =
            crate::processing::input_cache::resolve_input_storage_path(&root.to_string_lossy(), &input.name).await;
        if name == "F07" {
            saved.last_input_update = Some(shared::model::PersistedPlaylistUpdateInputResult {
                state: shared::model::PlaylistUpdateState::Success,
                timestamp: 19,
            });
            crate::processing::input_cache::update_epg_source_success(&mut saved, 0, "epg-fixture".into(), 23);
            crate::processing::input_cache::save_input_status(&path, &saved);
            (event, saved) = tuliprox_processing::processor::cache_fixture_status(&config, &input).await;
        }
        let bytes = std::fs::read(path.join("status.json")).unwrap();
        let running = test_app_state(config.clone());
        running.event_manager.send_event(shared::model::EventMessage::PlaylistUpdateProgress(
            shared::model::PlaylistUpdateProgressEvent::for_run_input(
                "publication-fixture-R2".into(),
                42.into(),
                input.id,
                input.name.to_string(),
                "updating",
            ),
        ));
        let live = test_app_state(config.clone());
        live.event_manager.send_event(shared::model::EventMessage::PlaylistUpdateProgress(event.clone()));
        let Json(live_status) = super::super::playlist_update_status(State(live.clone())).await.unwrap();
        assert_eq!(live_status.active_updates, vec![event.clone()]);
        let fresh_input = Arc::new((*input).clone());
        assert_eq!(fresh_input.id, input.id);
        let fresh_config =
            test_app_config(fresh_input, ConfigSource { inputs: vec![input.name.clone()], targets: vec![] });
        fresh_config.config.store(config.config.load_full());
        let fresh = test_app_state(Arc::new(fresh_config));
        let Json(reloaded) = super::super::playlist_update_status(State(fresh.clone())).await.unwrap();
        assert_eq!(reloaded.active_updates, Vec::new());
        assert_eq!(reloaded.inputs, live_status.inputs);
        assert_eq!(std::fs::read(path.join("status.json")).unwrap(), bytes, "endpoint is read only");
        for fact in &reloaded.inputs[0].clusters {
            let telemetry =
                event.input_telemetry.as_ref().unwrap().clusters.iter().find(|c| c.cluster == fact.cluster).unwrap();
            assert_eq!(fact.last_update.unwrap().technical_state, telemetry.technical_state);
            assert_eq!(fact.last_update.unwrap().source, telemetry.source);
            assert_eq!(saved.clusters[fact.cluster.as_ref()].last_update, fact.last_update);
            if fact.cluster == XtreamCluster::Live {
                assert_eq!(fact.last_update.unwrap().technical_state, Some(Technical::Succeeded));
            }
        }
        if let Some(export) = &export {
            let artifact = serde_json::json!({"case":name,"event":event,"live":live_status,"reload":reloaded,"status_file":serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()});
            std::fs::write(export.join(format!("{name}.json")), serde_json::to_vec_pretty(&artifact).unwrap()).unwrap();
        }
        router = router
            .nest(
                &format!("/{name}/running"),
                axum::Router::new()
                    .route("/status", axum::routing::get(super::super::playlist_update_status))
                    .with_state(running),
            )
            .nest(
                &format!("/{name}/live"),
                axum::Router::new()
                    .route("/status", axum::routing::get(super::super::playlist_update_status))
                    .with_state(live),
            )
            .nest(
                &format!("/{name}/reload"),
                axum::Router::new()
                    .route("/status", axum::routing::get(super::super::playlist_update_status))
                    .with_state(fresh),
            );
    }
    if let Some(export) = export.filter(|_| std::env::var_os("TULIPROX_STATUS_BROWSER_AUDIT").is_some()) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:4178").await.unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let output = tokio::process::Command::new("node")
            .current_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
            .arg(export.join("browser-audit.mjs"))
            .output()
            .await
            .unwrap();
        std::fs::write(export.join("browser-audit.log"), [output.stdout.as_slice(), output.stderr.as_slice()].concat())
            .unwrap();
        server.abort();
        assert!(output.status.success(), "browser audit failed: {}", String::from_utf8_lossy(&output.stderr));
    }
}

#[tokio::test]
async fn playlist_update_status_legacy_unknown_and_corrupt_files_stay_read_only() {
    for contents in [
        Some(r#"{"clusters":{"live":{"status":"Ok","timestamp":7},"video":{"status":"Failed","timestamp":8}}}"#),
        Some(
            r#"{"clusters":{"live":{"status":"Ok","timestamp":7,"last_update":{"source":"provider","quality_guard_threshold":0}},"video":{"status":"Failed","timestamp":8,"last_update":{"source":"provider","quality_guard_threshold":0}}}}"#,
        ),
        Some("{broken"),
        None,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let input = Arc::new(ConfigInput {
            id: 17,
            name: "legacy".intern(),
            input_type: InputType::Xtream,
            ..Default::default()
        });
        let config = test_app_config(input.clone(), ConfigSource { inputs: vec![input.name.clone()], targets: vec![] });
        config
            .config
            .store(Arc::new(Config { storage_dir: dir.path().to_string_lossy().into_owned(), ..Default::default() }));
        let path =
            crate::processing::input_cache::resolve_input_storage_path(&dir.path().to_string_lossy(), &input.name)
                .await;
        std::fs::create_dir_all(&path).unwrap();
        if let Some(contents) = contents {
            std::fs::write(path.join("status.json"), contents).unwrap();
        }
        let app = test_app_state(Arc::new(config));
        let Json(status) = super::super::playlist_update_status(State(app)).await.unwrap();
        assert_eq!(status.inputs[0].clusters.len(), if contents.is_some_and(|c| c != "{broken") { 2 } else { 0 });
        assert_eq!(std::fs::read_to_string(path.join("status.json")).ok().as_deref(), contents);
    }
}
