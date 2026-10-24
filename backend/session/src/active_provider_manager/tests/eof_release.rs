use super::{create_test_app_config_single_provider_pool, ActiveProviderManager, ConnectionKind};
use crate::EventManager;
use shared::utils::Internable;
use std::{net::SocketAddr, sync::Arc};
use tokio_util::sync::CancellationToken;
use tuliprox_core::model::{ProviderCloseReason, ProviderHandle};

fn new_manager() -> Arc<ActiveProviderManager> {
    let app_cfg = create_test_app_config_single_provider_pool();
    Arc::new(ActiveProviderManager::new(&app_cfg, &Arc::new(EventManager::new())))
}

fn acquire(manager: &ActiveProviderManager, port: u16) -> Option<ProviderHandle> {
    manager.acquire_connection_with_grace_for_session(
        &"provider_1".intern(),
        &SocketAddr::from(([127, 0, 0, 1], port)),
        false,
        0,
        ConnectionKind::Normal,
        None,
    )
}

fn opened_with_body_owner(manager: &ActiveProviderManager, port: u16) -> ProviderHandle {
    let handle = acquire(manager, port).expect("slot available");
    manager.mark_opening(handle.allocation_id);
    assert!(manager.register_body_owner(handle.allocation_id));
    handle
}

async fn wait_for_no_connections(manager: &ActiveProviderManager) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while manager.get_provider_connections_count() != 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("slot is released");
}

fn is_cancelled(token: Option<&CancellationToken>) -> bool { token.is_some_and(CancellationToken::is_cancelled) }

#[tokio::test]
async fn clean_upstream_eof_frees_slot_without_cancelling_handle() {
    let manager = new_manager();
    let first = opened_with_body_owner(&manager, 51_001);
    assert!(acquire(&manager, 51_002).is_none(), "the single slot is held");

    manager.release_slot_on_clean_upstream_eof(first.allocation_id);
    first.completion_token.as_ref().expect("completion token").cancel();

    let notify = manager.provider_capacity_notify(&"provider_1".intern());
    let second = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let notified = notify.notified();
            if let Some(handle) = acquire(&manager, 51_002) {
                return handle;
            }
            notified.await;
        }
    })
    .await
    .expect("the EOF release frees the slot and wakes waiters");

    assert!(
        !is_cancelled(first.cancel_token.as_ref()),
        "buffered bytes must keep flowing, so the handle token stays untouched"
    );
    assert_eq!(manager.get_provider_connections_count(), 1);

    // The late drop of the first handle must not free the slot of the newer allocation.
    manager.release_handle(&first);
    assert_eq!(manager.get_provider_connections_count(), 1);
    assert!(acquire(&manager, 51_003).is_none(), "the second allocation still owns the slot");
    assert!(!is_cancelled(second.cancel_token.as_ref()));

    manager.release_handle(&second);
    assert_eq!(manager.get_provider_connections_count(), 0);
}

#[tokio::test]
async fn eof_release_is_skipped_when_handle_token_was_cancelled() {
    let manager = new_manager();
    let first = opened_with_body_owner(&manager, 51_011);

    manager.release_slot_on_clean_upstream_eof(first.allocation_id);
    first.cancel_token.as_ref().expect("cancel token").cancel();
    first.completion_token.as_ref().expect("completion token").cancel();
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert_eq!(manager.get_provider_connections_count(), 1, "cancelled handles keep the regular release path");
    assert!(acquire(&manager, 51_012).is_none());

    manager.release_handle(&first);
    assert_eq!(manager.get_provider_connections_count(), 0);
}

#[tokio::test]
async fn eof_release_is_skipped_when_close_reason_was_recorded() {
    let manager = new_manager();
    let first = opened_with_body_owner(&manager, 51_021);

    manager.release_slot_on_clean_upstream_eof(first.allocation_id);
    first.set_close_reason(ProviderCloseReason::ProviderError);
    first.completion_token.as_ref().expect("completion token").cancel();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert_eq!(manager.get_provider_connections_count(), 1, "provider errors keep the regular release path");

    manager.release_handle(&first);
    assert_eq!(manager.get_provider_connections_count(), 0);
}

#[tokio::test]
async fn eof_release_is_skipped_after_regular_release_started() {
    let manager = new_manager();
    let first = opened_with_body_owner(&manager, 51_031);

    manager.release_slot_on_clean_upstream_eof(first.allocation_id);
    // Regular release first: the body owner has not completed, so the slot stays Closing until it does.
    manager.release_handle(&first);
    assert_eq!(manager.get_provider_connections_count(), 1);
    first.completion_token.as_ref().expect("completion token").cancel();
    wait_for_no_connections(&manager).await;

    assert_eq!(manager.get_provider_connections_count(), 0, "released exactly once");
    let second = acquire(&manager, 51_032).expect("slot available");
    manager.release_handle(&second);
    assert_eq!(manager.get_provider_connections_count(), 0, "counter must not underflow");
}

#[tokio::test]
async fn eof_release_ignores_stale_generation() {
    let manager = new_manager();
    let first = opened_with_body_owner(&manager, 51_041);
    let completion = first.completion_token.clone().expect("completion token");

    manager.core.release_after_clean_upstream_eof(first.allocation_id, first.open_generation + 1);
    assert_eq!(manager.get_provider_connections_count(), 1, "stale generation is ignored");

    completion.cancel();
    manager.core.release_after_clean_upstream_eof(first.allocation_id, first.open_generation);
    assert_eq!(manager.get_provider_connections_count(), 0);
    manager.release_handle(&first);
    assert_eq!(manager.get_provider_connections_count(), 0);
}

#[tokio::test]
async fn eof_release_for_unknown_allocation_is_a_noop() {
    let manager = new_manager();
    let first = opened_with_body_owner(&manager, 51_051);
    manager.release_handle(&first);
    first.completion_token.as_ref().expect("completion token").cancel();
    wait_for_no_connections(&manager).await;

    // Allocation is gone: nothing to watch, nothing to release.
    manager.release_slot_on_clean_upstream_eof(first.allocation_id);
    assert_eq!(manager.get_provider_connections_count(), 0);
}
