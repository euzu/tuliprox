use super::{create_test_app_config_single_provider_pool, ActiveProviderManager, ConnectionKind, PlaybackLeaseRef};
use crate::EventManager;
use shared::{defaults::default_user_priority, utils::Internable};
use std::{net::SocketAddr, sync::Arc};
use tuliprox_core::model::{PlaybackKind, PlaybackRequestOutcome};

#[tokio::test(start_paused = true)]
async fn stale_refresh_does_not_recreate_terminal_request() {
    let app_cfg = create_test_app_config_single_provider_pool();
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveProviderManager::new(&app_cfg, &event_manager);
    let input_name = "provider_1".intern();
    let owner = "terminal-refresh-owner";
    let handle = manager
        .acquire_connection_with_lease_for_session(
            &input_name,
            &SocketAddr::from(([172, 18, 0, 9], 55_002)),
            false,
            default_user_priority(),
            ConnectionKind::Normal,
            Some(PlaybackLeaseRef::new(owner, PlaybackKind::LiveHls)),
        )
        .expect("acquire identified request");
    let request_id = handle.playback_request_id.expect("identified request id");
    let lease_ref = PlaybackLeaseRef { owner, kind: PlaybackKind::LiveHls, request_id };

    manager.release_handle(&handle);
    manager.finish_identified_playback_request(owner, request_id, PlaybackRequestOutcome::ProviderFailed);
    assert_eq!(manager.provider_lease_usage(&input_name).total(), 0);

    manager.refresh_playback_lease(&input_name, &lease_ref, 15);
    manager.confirm_identified_playback_activity(owner, request_id);
    assert_eq!(manager.provider_lease_usage(&input_name).total(), 0);
}
