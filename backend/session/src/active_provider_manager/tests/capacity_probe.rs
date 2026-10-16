use super::super::{ActiveProviderManager, CapacityTier, ConnectionKind};
use crate::EventManager;
use arc_swap::{ArcSwap, ArcSwapOption};
use shared::{
    model::{ConfigPaths, InputType, ReverseProxyConfigDto, StreamConfigDto},
    utils::Internable,
};
use std::{net::SocketAddr, sync::Arc};
use tuliprox_core::{
    model::{AppConfig, Config, ConfigInput, MediaToolCapabilities, ReverseProxyConfig, SourcesConfig},
    utils::FileLockManager,
};

pub(super) fn addr(port: u16) -> SocketAddr { SocketAddr::from(([127, 0, 0, 1], port)) }

fn app_config_with_sources_and_grace(sources: SourcesConfig, grace_period_millis: u64) -> AppConfig {
    let mut config = Config::default();
    let dto = ReverseProxyConfigDto {
        stream: Some(StreamConfigDto { grace_period_millis, ..StreamConfigDto::default() }),
        ..ReverseProxyConfigDto::default()
    };
    config.reverse_proxy = Some(ReverseProxyConfig::from(&dto));

    AppConfig {
        config: Arc::new(ArcSwap::from_pointee(config)),
        sources: Arc::new(ArcSwap::from_pointee(sources)),
        hdhomerun: Arc::new(ArcSwapOption::default()),
        api_proxy: Arc::new(ArcSwapOption::default()),
        file_locks: Arc::new(FileLockManager::default()),
        paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
            home_path: String::new(),
            config_path: String::new(),
            storage_path: String::new(),
            config_file_path: String::new(),
            sources_file_path: String::new(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        })),
        custom_stream_response: Arc::new(ArcSwapOption::default()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(MediaToolCapabilities::new()),
    }
}

pub(super) fn manager_with_inputs(inputs: &[(&str, u16)]) -> ActiveProviderManager {
    let sources = SourcesConfig {
        inputs: inputs
            .iter()
            .enumerate()
            .map(|(i, (name, max))| {
                Arc::new(ConfigInput {
                    id: u16::try_from(i + 1).unwrap(),
                    name: (*name).intern(),
                    input_type: InputType::Xtream,
                    url: format!("http://{name}.invalid"),
                    username: Some("u".into()),
                    password: Some("p".into()),
                    enabled: true,
                    max_connections: *max,
                    ..ConfigInput::default()
                })
            })
            .collect(),
        ..SourcesConfig::default()
    };
    ActiveProviderManager::new(&app_config_with_sources_and_grace(sources, 2_000), &Arc::new(EventManager::new()))
}

#[test]
fn probe_reports_tiers_without_allocating() {
    let manager = manager_with_inputs(&[("solis", 1)]);
    let solis: Arc<str> = "solis".into();
    assert_eq!(manager.capacity_probe(&solis, 0, ConnectionKind::Normal, None, &[]), CapacityTier::Free);
    let held = manager.acquire_connection(&solis, &addr(1), -1, ConnectionKind::Normal).expect("slot");
    assert_eq!(manager.capacity_probe(&solis, 0, ConnectionKind::Normal, None, &[]), CapacityTier::Grace);
    let total: usize = manager.active_connections().unwrap_or_default().values().sum();
    assert_eq!(total, 1);
    drop(held);
}

#[test]
fn own_pending_release_is_reported_as_own_release() {
    let manager = manager_with_inputs(&[("solis", 1)]);
    let solis: Arc<str> = "solis".into();
    let held = manager.acquire_connection(&solis, &addr(1), 0, ConnectionKind::Normal).expect("slot");
    let own = [held.allocation_id];
    assert_eq!(manager.capacity_probe(&solis, 0, ConnectionKind::Normal, None, &own), CapacityTier::OwnRelease);
}

#[test]
fn higher_priority_sees_preempt_when_grace_is_used() {
    let manager = manager_with_inputs(&[("solis", 1)]);
    let solis: Arc<str> = "solis".into();
    let _low = manager.acquire_connection(&solis, &addr(1), 10, ConnectionKind::Normal).expect("slot");
    let _grace = manager
        .try_acquire_connection_no_preempt(&solis, true, &addr(2), 10, ConnectionKind::Normal, None)
        .expect("grace");
    assert_eq!(manager.capacity_probe(&solis, -10, ConnectionKind::Normal, None, &[]), CapacityTier::Preempt);
    assert_eq!(manager.capacity_probe(&solis, 10, ConnectionKind::Normal, None, &[]), CapacityTier::Exhausted);
}
