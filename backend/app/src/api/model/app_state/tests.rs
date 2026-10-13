use super::change_detection::{qos_aggregation_changed, recording_changed, schedules_changed};
use crate::model::{
    should_use_manual_redirect_for_proxy, should_use_manual_redirects_for_env_vars, Config, RecordingConfig,
    ScheduleConfig,
};
use shared::model::{
    QosAggregationConfigDto, ReverseProxyConfigDto, ScheduleTaskType, StreamHistoryConfigDto, WebAuthConfigDto,
    WebUiConfigDto,
};

fn config_with_web_auth(secret: &str) -> Config {
    let web_ui = WebUiConfigDto {
        auth: Some(WebAuthConfigDto {
            enabled: true,
            issuer: "test".to_string(),
            secret: secret.to_string(),
            ..WebAuthConfigDto::default()
        }),
        ..WebUiConfigDto::default()
    };
    Config { web_ui: Some((&web_ui).into()), ..Config::default() }
}

#[tokio::test]
async fn config_reload_rejects_enabling_web_auth_before_swap() {
    let state = super::create_test_app_state(Config::default());

    let result = state.set_config(config_with_web_auth("secret")).await;

    assert!(matches!(&result, Err(err) if err.kind() == shared::error::ErrorKind::ConfigWebUi));
    assert!(state.app_config.config.load().web_ui.is_none());
}

#[tokio::test]
async fn config_reload_rejects_web_auth_secret_change_before_swap() {
    let state = super::create_test_app_state(config_with_web_auth("old-secret"));

    let result = state.set_config(config_with_web_auth("new-secret")).await;

    assert!(matches!(&result, Err(err) if err.kind() == shared::error::ErrorKind::ConfigWebUi));
    assert_eq!(
        state
            .app_config
            .config
            .load()
            .web_ui
            .as_ref()
            .and_then(|web_ui| web_ui.auth.as_ref())
            .map(|auth| auth.secret.as_str()),
        Some("old-secret")
    );
}

#[tokio::test]
async fn config_reload_allows_unrelated_change() {
    let state = super::create_test_app_state(Config::default());
    let config = Config { default_user_agent: Some("changed".to_string()), ..Config::default() };

    assert!(state.set_config(config).await.is_ok());
    assert_eq!(state.app_config.config.load().default_user_agent.as_deref(), Some("changed"));
}

#[test]
fn should_use_manual_redirect_for_proxy_only_http_or_https() {
    assert!(should_use_manual_redirect_for_proxy("http://proxy.local:8080"));
    assert!(should_use_manual_redirect_for_proxy("https://proxy.local:8443"));
    assert!(should_use_manual_redirect_for_proxy("proxy.local:8080"));
    assert!(should_use_manual_redirect_for_proxy("127.0.0.1:8888"));
    assert!(!should_use_manual_redirect_for_proxy("socks5://proxy.local:1080"));
    assert!(!should_use_manual_redirect_for_proxy("socks5h://proxy.local:1080"));
    assert!(!should_use_manual_redirect_for_proxy("://invalid"));
    assert!(!should_use_manual_redirect_for_proxy("/tmp/proxy.socket"));
}

#[test]
fn should_use_manual_redirects_for_env_vars_only_when_http_proxy_is_present() {
    assert!(should_use_manual_redirects_for_env_vars(vec![(
        "HTTP_PROXY".to_string(),
        "http://proxy.local:8080".to_string(),
    )]));
    assert!(should_use_manual_redirects_for_env_vars(vec![(
        "all_proxy".to_string(),
        "https://proxy.local:8443".to_string(),
    )]));
    assert!(should_use_manual_redirects_for_env_vars(vec![("HTTP_PROXY".to_string(), "127.0.0.1:8888".to_string(),)]));
    assert!(!should_use_manual_redirects_for_env_vars(vec![(
        "ALL_PROXY".to_string(),
        "socks5://proxy.local:1080".to_string(),
    )]));
    assert!(!should_use_manual_redirects_for_env_vars(vec![("NO_PROXY".to_string(), "http://localhost".to_string(),)]));
}

#[test]
fn schedules_changed_detects_task_type_changes() {
    let a = vec![ScheduleConfig {
        schedule: "0 0 3 * * * *".to_string(),
        task_type: ScheduleTaskType::PlaylistUpdate,
        targets: None,
    }];
    let b = vec![ScheduleConfig {
        schedule: "0 0 3 * * * *".to_string(),
        task_type: ScheduleTaskType::GeoIpUpdate,
        targets: None,
    }];
    assert!(schedules_changed(&a, &b));
}

#[test]
fn schedules_changed_treats_same_entries_as_unchanged() {
    let a = vec![
        ScheduleConfig {
            schedule: "0 0 3 * * * *".to_string(),
            task_type: ScheduleTaskType::GeoIpUpdate,
            targets: None,
        },
        ScheduleConfig {
            schedule: "0 0 8 * * * *".to_string(),
            task_type: ScheduleTaskType::PlaylistUpdate,
            targets: Some(vec!["a".to_string(), "b".to_string()]),
        },
    ];
    let b = vec![
        ScheduleConfig {
            schedule: "0 0 8 * * * *".to_string(),
            task_type: ScheduleTaskType::PlaylistUpdate,
            targets: Some(vec!["b".to_string(), "a".to_string()]),
        },
        ScheduleConfig {
            schedule: "0 0 3 * * * *".to_string(),
            task_type: ScheduleTaskType::GeoIpUpdate,
            targets: None,
        },
    ];
    assert!(!schedules_changed(&a, &b));
}

#[test]
fn recording_changed_detects_retry_policy_changes() {
    let base = RecordingConfig::from(&shared::model::RecordingConfigDto {
        directory: Some("/tmp/downloads".to_string()),
        reserve_slots_for_users: 1,
        max_background_per_provider: 2,
        retry_backoff_initial_secs: 3,
        retry_backoff_multiplier: 2.0,
        retry_backoff_max_secs: 60,
        retry_backoff_jitter_percent: 5,
        retry_max_attempts: 5,
        ..Default::default()
    });
    let mut changed = base.clone();
    changed.retry_backoff_multiplier = 3.0;

    assert!(recording_changed(&base, &changed));
}

#[test]
fn recording_changed_treats_equivalent_configs_as_unchanged() {
    let base = RecordingConfig::from(&shared::model::RecordingConfigDto {
        directory: Some("/tmp/downloads".to_string()),
        organize_into_directories: true,
        episode_pattern: Some("S(?P<episode>\\d+)".to_string()),
        priority: 1,
        reserve_slots_for_users: 2,
        max_background_per_provider: 3,
        retry_backoff_initial_secs: 3,
        retry_backoff_multiplier: 2.0,
        retry_backoff_max_secs: 60,
        retry_backoff_jitter_percent: 5,
        retry_max_attempts: 5,
        ..Default::default()
    });

    assert!(!recording_changed(&base, &base.clone()));
}

#[test]
fn qos_aggregation_changed_detects_stream_history_batch_size_changes() {
    let old_config = Config {
        reverse_proxy: Some(crate::model::ReverseProxyConfig::from(&ReverseProxyConfigDto {
            stream_history: Some(StreamHistoryConfigDto {
                stream_history_enabled: true,
                stream_history_batch_size: 64,
                stream_history_retention_days: 7,
                stream_history_directory: "/tmp/history".to_string(),
            }),
            qos_aggregation: Some(QosAggregationConfigDto { enabled: true, interval_secs: 60, ..Default::default() }),
            ..Default::default()
        })),
        ..Config::default()
    };

    let mut new_config = old_config.clone();
    if let Some(reverse_proxy) = new_config.reverse_proxy.as_mut() {
        if let Some(history) = reverse_proxy.stream_history.as_mut() {
            history.stream_history_batch_size = 128;
        }
    }

    assert!(qos_aggregation_changed(&old_config, &new_config));
}

#[test]
fn qos_aggregation_changed_detects_compaction_interval_changes() {
    let old_config = Config {
        reverse_proxy: Some(crate::model::ReverseProxyConfig::from(&ReverseProxyConfigDto {
            qos_aggregation: Some(QosAggregationConfigDto {
                enabled: true,
                interval_secs: 60,
                compaction_interval_secs: 86_400,
            }),
            ..Default::default()
        })),
        ..Config::default()
    };
    let mut new_config = old_config.clone();
    if let Some(qos) = new_config.reverse_proxy.as_mut().and_then(|proxy| proxy.qos_aggregation.as_mut()) {
        qos.compaction_interval_secs = 3_600;
    }

    assert!(qos_aggregation_changed(&old_config, &new_config));
}

#[tokio::test]
async fn source_reload_prunes_removed_inputs_from_maintenance() {
    use crate::{api::model::update_app_state_sources, model::SourcesConfig};
    use shared::model::{ConfigInputDto, EventMessage, SourcesConfigDto};
    use std::sync::Arc;

    let dir = tempfile::TempDir::new().unwrap();
    let config = Config { storage_dir: dir.path().to_string_lossy().to_string(), ..Config::default() };
    let app_state = super::create_test_app_state(config);
    let sources = |names: &[&str]| {
        let dto = SourcesConfigDto {
            inputs: names
                .iter()
                .map(|name| ConfigInputDto {
                    name: (*name).to_string().into(),
                    url: "http://localhost/playlist.m3u".to_string(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        SourcesConfig::try_from(&dto).unwrap()
    };
    app_state.app_config.sources.store(Arc::new(sources(&["provider_a", "provider_b"])));
    app_state.input_maintenance.add_all(&["provider_a", "provider_b"], None, 1).await.unwrap();
    let mut events = app_state.event_manager.get_event_channel();

    update_app_state_sources(&app_state, sources(&["provider_a"]), None).await.unwrap();

    assert!(app_state.input_maintenance.is_in_maintenance("provider_a"));
    assert!(!app_state.input_maintenance.is_in_maintenance("provider_b"));
    let pruned = std::iter::from_fn(|| events.try_recv().ok())
        .find_map(|event| match event {
            EventMessage::InputMaintenanceChanged(entries) => Some(entries),
            _ => None,
        })
        .expect("pruning emits a change event");
    assert_eq!(pruned.len(), 1);
}

#[tokio::test]
async fn contactable_input_hides_inputs_in_maintenance() {
    use crate::model::{ConfigInput, SourcesConfig};
    use std::sync::Arc;

    let dir = tempfile::TempDir::new().unwrap();
    let config = Config { storage_dir: dir.path().to_string_lossy().to_string(), ..Config::default() };
    let app_state = super::create_test_app_state(config);
    let input = Arc::new(ConfigInput { name: Arc::from("provider_a"), ..ConfigInput::default() });
    app_state.app_config.sources.store(Arc::new(SourcesConfig { inputs: vec![input], ..SourcesConfig::default() }));
    let name: Arc<str> = Arc::from("provider_a");

    assert!(app_state.contactable_input(&name).is_some());
    app_state.input_maintenance.add("provider_a", None, 1).await.unwrap();
    assert!(app_state.contactable_input(&name).is_none(), "an input in maintenance is not contacted");
}
