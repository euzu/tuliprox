use super::{epg_dt, ics_source_dto, test_app_config, test_app_state, xmltv_source_dto};
use crate::{
    model::{Config, ConfigInput, ConfigProvider, ConfigSource},
    processing::epg::{get_input_raw_epg_file_path, get_input_raw_xmltv_file_path},
};
use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use shared::{
    model::{ConfigProviderDto, EpgConfigDto, IcsDummyConfigDto, IcsEpgSourceConfigDto},
    utils::Internable,
};
use std::sync::Arc;
use tempfile::tempdir;
use tower::ServiceExt;

#[tokio::test]
async fn load_epg_channels_for_input_uses_resolved_provider_epg_cache_file() {
    let temp_dir = tempdir().expect("temp dir");
    let provider = ConfigProvider::from(&ConfigProviderDto {
        name: "demo".intern(),
        urls: vec!["http://provider.example".intern()],
        provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
        dns: None,
    });
    let input = Arc::new(ConfigInput {
        id: 7,
        name: "input".intern(),
        epg: Some(crate::model::EpgConfig::from(&EpgConfigDto {
            sources: Some(vec![xmltv_source_dto("provider://demo/xmltv.php?username=user&password=pass", 0)]),
            t_sources: vec![xmltv_source_dto("provider://demo/xmltv.php?username=user&password=pass", 0)],
            smart_match: None,
        })),
        provider_configs: Some(vec![Arc::new(provider)]),
        ..Default::default()
    });
    let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
    let app_config = test_app_config(input.clone(), source);
    app_config
        .config
        .store(Arc::new(Config { storage_dir: temp_dir.path().to_string_lossy().to_string(), ..Default::default() }));
    let raw_epg_path = get_input_raw_xmltv_file_path(
        "provider://demo/xmltv.php?username=user&password=pass",
        input.as_ref(),
        temp_dir.path().to_string_lossy().as_ref(),
    )
    .await
    .expect("epg path");
    if let Some(parent) = raw_epg_path.parent() {
        tokio::fs::create_dir_all(parent).await.expect("epg dir");
    }
    let prog_start = epg_dt(0);
    let prog_stop = epg_dt(1);
    tokio::fs::write(
        &raw_epg_path,
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<tv>
  <channel id="demo.channel">
    <display-name>Demo Channel</display-name>
  </channel>
  <programme start="{prog_start}" stop="{prog_stop}" channel="demo.channel">
    <title>Morning Show</title>
  </programme>
</tv>"#
        ),
    )
    .await
    .expect("write epg");

    let app_state = test_app_state(Arc::new(app_config));
    let channels = super::super::load_epg_channels_for_input(&app_state, input.as_ref())
        .await
        .expect("load epg")
        .expect("channels");

    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0].id.as_ref(), "demo.channel");
    assert_eq!(channels[0].programmes.len(), 1);
    assert_eq!(channels[0].programmes[0].title.as_ref().map(std::convert::AsRef::as_ref), Some("Morning Show"));
}

#[tokio::test]
async fn load_epg_channels_for_input_reads_cached_ics_source() {
    let temp_dir = tempdir().expect("temp dir");
    let ics_dto = ics_source_dto("https://example.com/f1.ics", "f1.calendar", -10);
    let input = Arc::new(ConfigInput {
        id: 7,
        name: "input".intern(),
        epg: Some(crate::model::EpgConfig::from(&EpgConfigDto {
            sources: Some(vec![ics_dto.clone()]),
            t_sources: vec![ics_dto],
            smart_match: None,
        })),
        ..Default::default()
    });
    let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
    let app_config = test_app_config(input.clone(), source);
    app_config
        .config
        .store(Arc::new(Config { storage_dir: temp_dir.path().to_string_lossy().to_string(), ..Default::default() }));
    let epg_source = &input.epg.as_ref().expect("epg").sources[0];
    let raw_epg_path =
        get_input_raw_epg_file_path(epg_source, input.as_ref(), temp_dir.path().to_string_lossy().as_ref())
            .await
            .expect("epg path");
    if let Some(parent) = raw_epg_path.parent() {
        tokio::fs::create_dir_all(parent).await.expect("epg dir");
    }
    tokio::fs::write(
            &raw_epg_path,
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:Practice 1\nDTSTART:20260306T123000Z\nDTEND:20260306T133000Z\nEND:VEVENT\nEND:VCALENDAR",
        )
        .await
        .expect("write ics");

    let app_state = test_app_state(Arc::new(app_config));
    let channels = super::super::load_epg_channels_for_input(&app_state, input.as_ref())
        .await
        .expect("load epg")
        .expect("channels");

    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0].id.as_ref(), "f1.calendar");
    assert_eq!(channels[0].title.as_deref(), Some("Formula 1"));
    assert_eq!(channels[0].programmes[0].title.as_deref(), Some("Practice 1"));
}

#[tokio::test]
async fn load_epg_channels_for_input_redownloads_invalid_cached_ics_source() {
    let temp_dir = tempdir().expect("temp dir");
    tokio::fs::write(
            temp_dir.path().join("valid.ics"),
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:Downloaded\nDTSTART:20260306T123000Z\nDTEND:20260306T133000Z\nEND:VEVENT\nEND:VCALENDAR",
        )
        .await
        .expect("write valid ics source");
    let ics_dto = ics_source_dto("valid.ics", "f1.calendar", -10);
    let input = Arc::new(ConfigInput {
        id: 7,
        name: "input".intern(),
        epg: Some(crate::model::EpgConfig::from(&EpgConfigDto {
            sources: Some(vec![ics_dto.clone()]),
            t_sources: vec![ics_dto],
            smart_match: None,
        })),
        ..Default::default()
    });
    let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
    let app_config = test_app_config(input.clone(), source);
    app_config
        .config
        .store(Arc::new(Config { storage_dir: temp_dir.path().to_string_lossy().to_string(), ..Default::default() }));
    let epg_source = &input.epg.as_ref().expect("epg").sources[0];
    let raw_epg_path =
        get_input_raw_epg_file_path(epg_source, input.as_ref(), temp_dir.path().to_string_lossy().as_ref())
            .await
            .expect("epg path");
    if let Some(parent) = raw_epg_path.parent() {
        tokio::fs::create_dir_all(parent).await.expect("epg dir");
    }
    tokio::fs::write(&raw_epg_path, "upstream returned an error page").await.expect("write invalid cache");

    let app_state = test_app_state(Arc::new(app_config));
    let channels = super::super::load_epg_channels_for_input(&app_state, input.as_ref())
        .await
        .expect("load epg")
        .expect("channels");

    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0].programmes[0].title.as_deref(), Some("Downloaded"));
    assert_eq!(
            tokio::fs::read_to_string(&raw_epg_path).await.expect("updated cache"),
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:Downloaded\nDTSTART:20260306T123000Z\nDTEND:20260306T133000Z\nEND:VEVENT\nEND:VCALENDAR"
        );
}

#[tokio::test]
async fn load_epg_channels_for_input_preserves_invalid_cache_when_refresh_fails() {
    let temp_dir = tempdir().expect("temp dir");
    let invalid_cache = "upstream returned an error page";
    let ics_dto = ics_source_dto("missing.ics", "f1.calendar", -10);
    let input = Arc::new(ConfigInput {
        id: 7,
        name: "input".intern(),
        epg: Some(crate::model::EpgConfig::from(&EpgConfigDto {
            sources: Some(vec![ics_dto.clone()]),
            t_sources: vec![ics_dto],
            smart_match: None,
        })),
        ..Default::default()
    });
    let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
    let app_config = test_app_config(input.clone(), source);
    app_config
        .config
        .store(Arc::new(Config { storage_dir: temp_dir.path().to_string_lossy().to_string(), ..Default::default() }));
    let epg_source = &input.epg.as_ref().expect("epg").sources[0];
    let raw_epg_path =
        get_input_raw_epg_file_path(epg_source, input.as_ref(), temp_dir.path().to_string_lossy().as_ref())
            .await
            .expect("epg path");
    if let Some(parent) = raw_epg_path.parent() {
        tokio::fs::create_dir_all(parent).await.expect("epg dir");
    }
    tokio::fs::write(&raw_epg_path, invalid_cache).await.expect("write invalid cache");

    let app_state = test_app_state(Arc::new(app_config));
    assert!(super::super::load_epg_channels_for_input(&app_state, input.as_ref()).await.is_err());
    assert_eq!(tokio::fs::read_to_string(&raw_epg_path).await.expect("preserved cache"), invalid_cache);
}

#[tokio::test]
async fn load_epg_channels_for_input_applies_ics_dummy_policy_in_preview() {
    let temp_dir = tempdir().expect("temp dir");
    tokio::fs::write(temp_dir.path().join("empty.ics"), "BEGIN:VCALENDAR\nEND:VCALENDAR")
        .await
        .expect("write empty ics source");
    let mut ics_dto = ics_source_dto("empty.ics", "f1.calendar", -10);
    ics_dto.ics = Some(IcsEpgSourceConfigDto {
        dummy: IcsDummyConfigDto {
            enabled: true,
            title: "No F1".to_string(),
            days_past: 0,
            days_future: 0,
            block_hours: 24,
            ..IcsDummyConfigDto::default()
        },
        ..IcsEpgSourceConfigDto::default()
    });
    let input = Arc::new(ConfigInput {
        id: 7,
        name: "input".intern(),
        epg: Some(crate::model::EpgConfig::from(&EpgConfigDto {
            sources: Some(vec![ics_dto.clone()]),
            t_sources: vec![ics_dto],
            smart_match: None,
        })),
        ..Default::default()
    });
    let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
    let app_config = test_app_config(input.clone(), source);
    app_config
        .config
        .store(Arc::new(Config { storage_dir: temp_dir.path().to_string_lossy().to_string(), ..Default::default() }));

    let app_state = test_app_state(Arc::new(app_config));
    let channels = super::super::load_epg_channels_for_input(&app_state, input.as_ref())
        .await
        .expect("load epg")
        .expect("channels");

    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0].id.as_ref(), "f1.calendar");
    assert!(!channels[0].programmes.is_empty());
    assert!(channels[0].programmes.iter().all(|programme| programme.title.as_deref() == Some("No F1")));
}

#[tokio::test]
async fn playlist_epg_custom_route_rejects_invalid_url_scheme() {
    let input = Arc::new(ConfigInput { id: 7, name: "input".intern(), ..Default::default() });
    let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
    let app_state = test_app_state(Arc::new(test_app_config(input, source)));
    let router = super::super::v1_api_playlist_register_protected(Router::new()).with_state(app_state);
    let request = Request::builder()
        .method("POST")
        .uri("/playlist/epg")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"Custom":"ftp://example.com/epg.xml"}"#))
        .expect("request");

    let response = router.into_service::<Body>().oneshot(request).await.expect("response");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn playlist_epg_input_route_returns_cached_provider_epg() {
    let temp_dir = tempdir().expect("temp dir");
    let provider = ConfigProvider::from(&ConfigProviderDto {
        name: "demo".intern(),
        urls: vec!["http://provider.example".intern()],
        provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
        dns: None,
    });
    let input = Arc::new(ConfigInput {
        id: 7,
        name: "input".intern(),
        epg: Some(crate::model::EpgConfig::from(&EpgConfigDto {
            sources: Some(vec![xmltv_source_dto("provider://demo/xmltv.php?username=user&password=pass", 0)]),
            t_sources: vec![xmltv_source_dto("provider://demo/xmltv.php?username=user&password=pass", 0)],
            smart_match: None,
        })),
        provider_configs: Some(vec![Arc::new(provider)]),
        ..Default::default()
    });
    let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
    let app_config = test_app_config(input.clone(), source);
    app_config
        .config
        .store(Arc::new(Config { storage_dir: temp_dir.path().to_string_lossy().to_string(), ..Default::default() }));
    let raw_epg_path = get_input_raw_xmltv_file_path(
        "provider://demo/xmltv.php?username=user&password=pass",
        input.as_ref(),
        temp_dir.path().to_string_lossy().as_ref(),
    )
    .await
    .expect("epg path");
    if let Some(parent) = raw_epg_path.parent() {
        tokio::fs::create_dir_all(parent).await.expect("epg dir");
    }
    let prog_start = epg_dt(0);
    let prog_stop = epg_dt(1);
    tokio::fs::write(
        &raw_epg_path,
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<tv>
  <channel id="demo.channel">
    <display-name>Demo Channel</display-name>
  </channel>
  <programme start="{prog_start}" stop="{prog_stop}" channel="demo.channel">
    <title>Morning Show</title>
  </programme>
</tv>"#
        ),
    )
    .await
    .expect("write epg");

    let app_state = test_app_state(Arc::new(app_config));
    let router = super::super::v1_api_playlist_register_protected(Router::new()).with_state(app_state);
    let request = Request::builder()
        .method("POST")
        .uri("/playlist/epg")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"Input":"input"}"#))
        .expect("request");

    let response = router.into_service::<Body>().oneshot(request).await.expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body");
    let body_text = String::from_utf8(body.to_vec()).expect("utf8");
    assert!(body_text.contains("demo.channel"), "{body_text}");
    assert!(body_text.contains("Morning Show"), "{body_text}");
}

#[tokio::test]
async fn playlist_epg_input_route_returns_no_content_for_unknown_input_name() {
    let provider = ConfigProvider::from(&ConfigProviderDto {
        name: "demo".intern(),
        urls: vec!["http://provider.example".intern()],
        provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
        dns: None,
    });
    let input = Arc::new(ConfigInput {
        id: 7,
        name: "input".intern(),
        provider_configs: Some(vec![Arc::new(provider)]),
        ..Default::default()
    });
    let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
    let app_state = test_app_state(Arc::new(test_app_config(input, source)));
    let router = super::super::v1_api_playlist_register_protected(Router::new()).with_state(app_state);
    let request = Request::builder()
        .method("POST")
        .uri("/playlist/epg")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"Input":"missing"}"#))
        .expect("request");

    let response = router.into_service::<Body>().oneshot(request).await.expect("response");

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}
