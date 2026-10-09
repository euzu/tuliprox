use super::{epg_dt, ics_source_dto, test_app_config, test_app_state, xmltv_source_dto};
use crate::{
    model::{Config, ConfigInput, ConfigProvider, ConfigSource},
    processing::epg::get_input_raw_xmltv_file_path,
};
use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use shared::{
    model::{ConfigProviderDto, EpgChannel, EpgConfigDto, EpgProgramme, IcsDummyConfigDto, IcsEpgSourceConfigDto},
    utils::Internable,
};
use std::sync::Arc;
use tempfile::tempdir;
use tower::ServiceExt;

#[test]
fn merge_epg_channels_prefers_higher_priority_metadata_and_fills_lower_priority_gaps() {
    let low_priority = EpgChannel {
        id: "demo.channel".intern(),
        title: Some("Low".intern()),
        icon: Some("http://low/icon.png".intern()),
        programmes: vec![EpgProgramme::new_all(10, 20, "demo.channel".intern(), Some("Low Show".intern()), None, None)],
    };
    let high_priority = EpgChannel {
        id: "demo.channel".intern(),
        title: Some("High".intern()),
        icon: Some("http://high/icon.png".intern()),
        programmes: vec![EpgProgramme::new_all(
            30,
            40,
            "demo.channel".intern(),
            Some("High Show".intern()),
            None,
            None,
        )],
    };
    let same_priority = EpgChannel {
        id: "demo.channel".intern(),
        title: Some("Same".intern()),
        icon: Some("http://same/icon.png".intern()),
        programmes: vec![
            EpgProgramme::new_all(30, 40, "demo.channel".intern(), Some("Duplicate".intern()), None, None),
            EpgProgramme::new_all(50, 60, "demo.channel".intern(), Some("Second Show".intern()), None, None),
        ],
    };

    let channels = super::super::merge_epg_channels(vec![
        (10, vec![low_priority]),
        (0, vec![high_priority]),
        (0, vec![same_priority]),
    ]);

    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0].title.as_deref(), Some("High"));
    assert_eq!(channels[0].icon.as_deref(), Some("http://high/icon.png"));
    assert_eq!(channels[0].programmes.len(), 3);
    assert_eq!(
        channels[0].programmes.iter().map(|programme| (programme.start, programme.stop)).collect::<Vec<_>>(),
        vec![(10, 20), (30, 40), (50, 60)],
    );
}

#[tokio::test]
async fn load_epg_channels_for_input_prefers_high_priority_ics_dummy_policy() {
    let temp_dir = tempdir().expect("temp dir");
    for filename in ["low.ics", "high.ics"] {
        tokio::fs::write(temp_dir.path().join(filename), "BEGIN:VCALENDAR\nEND:VCALENDAR")
            .await
            .expect("write empty ics source");
    }

    let mut low_priority = ics_source_dto("low.ics", "f1.calendar", 10);
    low_priority.ics = Some(IcsEpgSourceConfigDto {
        dummy: IcsDummyConfigDto {
            enabled: true,
            title: "Low priority".to_string(),
            days_past: 0,
            days_future: 0,
            block_hours: 24,
            ..IcsDummyConfigDto::default()
        },
        ..IcsEpgSourceConfigDto::default()
    });
    let mut high_priority = ics_source_dto("high.ics", "f1.calendar", -10);
    high_priority.ics = Some(IcsEpgSourceConfigDto {
        dummy: IcsDummyConfigDto {
            enabled: true,
            title: "High priority".to_string(),
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
            sources: Some(vec![low_priority.clone(), high_priority.clone()]),
            t_sources: vec![low_priority, high_priority],
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
    assert!(!channels[0].programmes.is_empty());
    assert!(channels[0].programmes.iter().all(|programme| programme.title.as_deref() == Some("High priority")));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn playlist_epg_input_route_merges_multiple_cached_sources_by_priority() {
    let temp_dir = tempdir().expect("temp dir");
    let provider = ConfigProvider::from(&ConfigProviderDto {
        name: "demo".intern(),
        urls: vec!["http://provider.example".intern()],
        provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
        dns: None,
    });
    let primary_url = "provider://demo/xmltv-primary.php?username=user&password=pass";
    let secondary_url = "provider://demo/xmltv-secondary.php?username=user&password=pass";
    let input = Arc::new(ConfigInput {
        id: 7,
        name: "input".intern(),
        epg: Some(crate::model::EpgConfig::from(&EpgConfigDto {
            sources: Some(vec![
                xmltv_source_dto(secondary_url, 10),
                xmltv_source_dto(primary_url, 0),
                xmltv_source_dto("provider://demo/xmltv-same-priority.php?username=user&password=pass", 0),
            ]),
            t_sources: vec![
                xmltv_source_dto(secondary_url, 10),
                xmltv_source_dto(primary_url, 0),
                xmltv_source_dto("provider://demo/xmltv-same-priority.php?username=user&password=pass", 0),
            ],
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

    let primary_path =
        get_input_raw_xmltv_file_path(primary_url, input.as_ref(), temp_dir.path().to_string_lossy().as_ref())
            .await
            .expect("primary epg path");
    let secondary_path =
        get_input_raw_xmltv_file_path(secondary_url, input.as_ref(), temp_dir.path().to_string_lossy().as_ref())
            .await
            .expect("secondary epg path");
    let same_priority_path = get_input_raw_xmltv_file_path(
        "provider://demo/xmltv-same-priority.php?username=user&password=pass",
        input.as_ref(),
        temp_dir.path().to_string_lossy().as_ref(),
    )
    .await
    .expect("same priority epg path");

    for path in [&primary_path, &secondary_path, &same_priority_path] {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.expect("epg dir");
        }
    }

    let p0 = epg_dt(0);
    let p1 = epg_dt(1);
    let p2 = epg_dt(2);
    let p3 = epg_dt(3);

    tokio::fs::write(
        &secondary_path,
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<tv>
  <channel id="demo.channel">
    <display-name>Secondary Channel</display-name>
    <icon src="http://secondary/icon.png" />
  </channel>
  <programme start="{p0}" stop="{p1}" channel="demo.channel">
    <title>Secondary Show</title>
  </programme>
</tv>"#
        ),
    )
    .await
    .expect("write secondary epg");
    tokio::fs::write(
        &primary_path,
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<tv>
  <channel id="demo.channel">
    <display-name>Primary Channel</display-name>
    <icon src="http://primary/icon.png" />
  </channel>
  <programme start="{p1}" stop="{p2}" channel="demo.channel">
    <title>Primary Show</title>
  </programme>
</tv>"#
        ),
    )
    .await
    .expect("write primary epg");
    tokio::fs::write(
        &same_priority_path,
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<tv>
  <channel id="demo.channel">
    <display-name>Same Priority Channel</display-name>
    <icon src="http://same/icon.png" />
  </channel>
  <programme start="{p0}" stop="{p1}" channel="demo.channel">
    <title>Duplicate Show</title>
  </programme>
  <programme start="{p2}" stop="{p3}" channel="demo.channel">
    <title>Second Show</title>
  </programme>
</tv>"#
        ),
    )
    .await
    .expect("write same priority epg");

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
    assert!(body_text.contains("Primary Channel"), "{body_text}");
    assert!(!body_text.contains("Secondary Channel"), "{body_text}");
    assert!(body_text.contains("Primary Show"), "{body_text}");
    assert!(body_text.contains("Second Show"), "{body_text}");
    assert!(!body_text.contains("Secondary Show"), "{body_text}");
}
