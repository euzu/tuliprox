use super::{
    test_adaptive_channel, test_user_credentials, ActiveUserConnectionParams, ActiveUserManager,
    CreateUserSessionParams,
};
use crate::{ConnectionKind, EventManager};
use arc_swap::ArcSwapOption;
use shared::{
    model::{ActiveUserConnectionChange, EventMessage, StreamInfo, UserConnectionPermission},
    utils::Internable,
};
use std::{borrow::Cow, sync::Arc};
use tuliprox_core::model::{Config, Fingerprint};

async fn viewer(manager: &ActiveUserManager, uid: u32, username: &str, shared_id: Option<u64>) -> StreamInfo {
    let addr = format!("127.0.0.1:{}", 14000 + uid).parse().unwrap();
    let fingerprint = Fingerprint::new(format!("viewer-{uid}"), "127.0.0.1".into(), addr);
    let user = test_user_credentials(username, 10, 0);
    let token = format!("session-{uid}");
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: &token,
            virtual_id: uid,
            provider: "account-a",
            stream_url: "http://origin.test/live.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager.add_connection(&addr).await;
    let mut channel = test_adaptive_channel(uid);
    channel.shared = shared_id.is_some();
    channel.shared_stream_id = shared_id;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid,
            meter_uid: uid + 100,
            username,
            max_connections: 10,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "account-a".intern(),
            stream_channel: &channel,
            user_agent: Cow::Borrowed("handover-test"),
            session_token: Some(&token),
        })
        .await
        .expect("viewer registers")
}

async fn fixture() -> Arc<ActiveUserManager> {
    let events = Arc::new(EventManager::new());
    let manager = Arc::new(ActiveUserManager::new(&Config::default(), &Arc::new(ArcSwapOption::default()), &events));
    viewer(&manager, 1, "alice", Some(42)).await;
    viewer(&manager, 2, "bob", Some(42)).await;
    viewer(&manager, 3, "alice", Some(42)).await;
    manager
}

async fn project(manager: &ActiveUserManager) -> usize {
    manager.lock_shared_stream_provider_projection(42).await.update_provider(&"account-b".intern())
}

#[tokio::test]
async fn handover_updates_preserved_viewer_rows() {
    let manager = fixture().await;
    manager.release_connection(&"127.0.0.1:14002".parse().unwrap()).await;
    let before = manager.panel_streams().await;
    assert!(before.iter().any(|row| row.preserved));
    assert_eq!(project(&manager).await, 3);
    let after = manager.panel_streams().await;
    assert_eq!(after.len(), 3);
    assert!(after.iter().all(|row| row.provider.as_ref() == "account-b"));
    assert_eq!(after.iter().filter(|row| row.preserved).count(), 1);
}

#[tokio::test]
async fn handover_emits_one_update_per_changed_viewer() {
    let manager = fixture().await;
    let mut events = manager.events().get_event_channel();
    assert_eq!(project(&manager).await, 3);
    assert_eq!(project(&manager).await, 0);
    let mut received = Vec::new();
    while let Ok(event) = events.try_recv() {
        match event {
            EventMessage::ActiveUser(ActiveUserConnectionChange::Updated(row)) => received.push(row),
            other => panic!("projection must emit only full Updated events, got {other:?}"),
        }
    }
    let mut rows = manager.panel_streams().await;
    rows.sort_by_key(|row| row.uid);
    received.sort_by_key(|row| row.uid);
    assert_eq!(received, rows);
}

#[tokio::test]
async fn provider_projection_preserves_stream_identity_and_accounting() {
    let manager = fixture().await;
    manager.release_connection(&"127.0.0.1:14002".parse().unwrap()).await;
    let mut expected = manager.panel_streams().await;
    let counts = manager.active_users_and_connections().await;
    let resources = manager.playback_resource_counts().await;
    let sessions = {
        let connections = manager.connections.read().await;
        connections
            .by_key
            .iter()
            .map(|(user, data)| {
                (
                    user.clone(),
                    data.sessions
                        .iter()
                        .map(|session| {
                            (
                                session.token.clone(),
                                session.provider.clone(),
                                session.ts,
                                session.started_at,
                                session.identity(),
                                session.lifecycle.clone(),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    project(&manager).await;
    for row in &mut expected {
        row.provider = "account-b".intern();
    }
    let mut actual = manager.panel_streams().await;
    expected.sort_by_key(|row| row.uid);
    actual.sort_by_key(|row| row.uid);
    assert_eq!(actual, expected);
    assert_eq!(manager.active_users_and_connections().await, counts);
    assert_eq!(manager.playback_resource_counts().await, resources);
    let connections = manager.connections.read().await;
    let after = connections
        .by_key
        .iter()
        .map(|(user, data)| {
            (
                user.clone(),
                data.sessions
                    .iter()
                    .map(|session| {
                        (
                            session.token.clone(),
                            session.provider.clone(),
                            session.ts,
                            session.started_at,
                            session.identity(),
                            session.lifecycle.clone(),
                        )
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(after, sessions);
}

#[tokio::test]
async fn handover_only_updates_rows_of_the_matching_shared_session() {
    let manager = fixture().await;
    viewer(&manager, 4, "alice", Some(43)).await;
    viewer(&manager, 5, "bob", None).await;
    manager.terminate_session("alice", "session-3").await;
    let before = manager.panel_streams().await;
    assert_eq!(project(&manager).await, 2);
    let mut expected = before;
    for row in &mut expected {
        if row.channel.shared_stream_id == Some(42) {
            row.provider = "account-b".intern();
        }
    }
    let mut actual = manager.panel_streams().await;
    expected.sort_by_key(|row| row.uid);
    actual.sort_by_key(|row| row.uid);
    assert_eq!(actual, expected);
    assert!(actual.iter().all(|row| row.session_token.as_deref() != Some("session-3")));
}

#[derive(Clone, Copy)]
enum ViewerRelease {
    Connection,
    Stream,
}

#[derive(Clone, Copy)]
enum ConcurrentViewerChange {
    Handover,
    NewerHandover,
    Removal,
    Reactivation,
}

async fn reactivate_viewer(manager: &ActiveUserManager, row: &StreamInfo) {
    let fingerprint = Fingerprint::new("viewer-2".into(), row.client_ip.clone(), row.addr);
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 22,
            meter_uid: row.meter_uid,
            username: &row.username,
            max_connections: 10,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: row.provider.clone(),
            stream_channel: &row.channel,
            user_agent: Cow::Borrowed(&row.user_agent),
            session_token: row.session_token.as_deref(),
        })
        .await
        .expect("viewer reactivates the same row");
}

async fn assert_delayed_release_publishes_current_viewer(release_kind: ViewerRelease, change: ConcurrentViewerChange) {
    let manager = fixture().await;
    if matches!(change, ConcurrentViewerChange::NewerHandover) {
        assert_eq!(project(&manager).await, 3);
    }
    let addr = "127.0.0.1:14002".parse().unwrap();
    let mut events = manager.events().get_event_channel();
    let expiry = manager.adaptive_expiry_index.lock().await;
    let mut release = Box::pin(async {
        match release_kind {
            ViewerRelease::Connection => {
                manager.release_connection(&addr).await;
            }
            ViewerRelease::Stream => {
                assert!(matches!(
                    manager.release_stream_request_by_uid(&addr, 2).await,
                    super::StreamRequestDetach::Preserved(_)
                ));
            }
        }
    });
    assert!(futures::poll!(release.as_mut()).is_pending());
    assert!(manager.panel_streams().await.iter().any(|row| row.uid == 2 && row.preserved));
    let provider = match change {
        ConcurrentViewerChange::NewerHandover => "account-c",
        ConcurrentViewerChange::Handover | ConcurrentViewerChange::Removal | ConcurrentViewerChange::Reactivation => {
            "account-b"
        }
    };
    assert_eq!(manager.lock_shared_stream_provider_projection(42).await.update_provider(&provider.intern()), 3);
    match change {
        ConcurrentViewerChange::Handover | ConcurrentViewerChange::NewerHandover => {}
        ConcurrentViewerChange::Removal => {
            assert!(manager.terminate_session("bob", "session-2").await);
            // Any further Updated frame would resurrect the row after its removal event.
            events = manager.events().get_event_channel();
        }
        ConcurrentViewerChange::Reactivation => {
            let row = manager.panel_streams().await.into_iter().find(|row| row.uid == 2).unwrap();
            reactivate_viewer(&manager, &row).await;
        }
    }
    let expected = manager.panel_streams().await.into_iter().find(|row| row.uid == 2);
    drop(expiry);
    release.await;
    let mut delivered = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let EventMessage::ActiveUser(ActiveUserConnectionChange::Updated(row)) = event {
            if row.uid == 2 {
                delivered.push(row);
            }
        }
    }
    if let Some(expected) = expected {
        assert_eq!(
            delivered.last(),
            Some(&expected),
            "last delivered update must match the complete authoritative row"
        );
        assert!(delivered.iter().all(|row| row.provider.as_ref() != "account-a"), "no old account after handover");
    } else {
        assert_eq!(delivered, [], "a delayed release must not republish a removed viewer");
    }
    assert_eq!(manager.panel_streams().await.iter().filter(|row| row.uid != 2).count(), 2);
}

#[tokio::test]
async fn delayed_connection_release_cannot_restore_previous_provider() {
    assert_delayed_release_publishes_current_viewer(ViewerRelease::Connection, ConcurrentViewerChange::Handover).await;
}

#[tokio::test]
async fn delayed_stream_release_cannot_restore_previous_provider() {
    assert_delayed_release_publishes_current_viewer(ViewerRelease::Stream, ConcurrentViewerChange::Handover).await;
}

#[tokio::test]
async fn delayed_release_cannot_overwrite_newer_handover() {
    for release in [ViewerRelease::Connection, ViewerRelease::Stream] {
        assert_delayed_release_publishes_current_viewer(release, ConcurrentViewerChange::NewerHandover).await;
    }
}

#[tokio::test]
async fn delayed_release_does_not_republish_removed_viewer() {
    for release in [ViewerRelease::Connection, ViewerRelease::Stream] {
        assert_delayed_release_publishes_current_viewer(release, ConcurrentViewerChange::Removal).await;
    }
}

#[tokio::test]
async fn delayed_release_publishes_current_reactivated_viewer() {
    for release in [ViewerRelease::Connection, ViewerRelease::Stream] {
        assert_delayed_release_publishes_current_viewer(release, ConcurrentViewerChange::Reactivation).await;
    }
}
