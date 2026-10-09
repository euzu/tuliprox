use super::{
    create_provider_header_session, provider_header_test_manager, session_identity, ActiveUserManager,
    CreateUserSessionParams, SessionProviderHeaders,
};
use crate::{active_provider_manager::ConnectionKind, EventManager};
use arc_swap::ArcSwapOption;
use futures::FutureExt;
use shared::{model::UserConnectionPermission, utils::Internable};
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use tuliprox_core::model::{Config, ProxyUserCredentials};
use tuliprox_repository::GeoIp;

#[tokio::test]
async fn user_agent_stream_index_is_stable_per_session_and_unique_between_sessions() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);
    let addr = SocketAddr::from(([127, 0, 0, 1], 55_500));
    let mut user = ProxyUserCredentials::default();
    user.username = "indexed-user".to_string();

    for token in ["indexed-session-a", "indexed-session-b"] {
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: token,
                virtual_id: 42,
                provider: "provider-a",
                stream_url: "http://localhost/live.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
    }

    let first = manager.get_or_assign_user_agent_stream_index(&user.username, "indexed-session-a").await;
    let repeated = manager.get_or_assign_user_agent_stream_index(&user.username, "indexed-session-a").await;
    let second = manager.get_or_assign_user_agent_stream_index(&user.username, "indexed-session-b").await;

    assert!(first.is_some());
    assert_eq!(repeated, first);
    assert!(second.is_some());
    assert_ne!(second, first);
}

#[tokio::test]
async fn update_session_provider_headers_updates_existing_session_and_timestamp() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55402".parse().unwrap_or_else(|_| unreachable!());
    let mut user = ProxyUserCredentials::default();
    user.username = "user-provider-headers".to_string();

    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-provider-headers",
            virtual_id: 7003,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    let before = manager
        .get_and_update_user_session(&user.username, "tok-provider-headers")
        .await
        .expect("session should exist");
    let previous_ts = before.ts;
    let headers = HashMap::from([(String::from("cookie"), String::from("sid=abc"))]);

    assert!(manager.update_session_provider_headers(&user.username, "tok-provider-headers", &headers).await);

    let after = manager
        .get_and_update_user_session(&user.username, "tok-provider-headers")
        .await
        .expect("session should exist");
    assert_eq!(after.provider_session_headers, headers);
    assert!(after.ts >= previous_ts);
}

#[tokio::test]
async fn update_session_provider_headers_returns_false_for_missing_user_or_token() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);
    let headers = HashMap::from([(String::from("cookie"), String::from("sid=abc"))]);

    assert!(!manager.update_session_provider_headers("missing-user", "missing-token", &headers).await);

    let addr: SocketAddr = "127.0.0.1:55403".parse().unwrap_or_else(|_| unreachable!());
    let mut user = ProxyUserCredentials::default();
    user.username = "user-missing-token".to_string();
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-existing",
            virtual_id: 7004,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    assert!(!manager.update_session_provider_headers(&user.username, "tok-missing", &headers).await);
}

#[tokio::test]
async fn create_user_session_clears_provider_headers_when_provider_or_stream_url_changes() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55404".parse().unwrap_or_else(|_| unreachable!());
    let mut user = ProxyUserCredentials::default();
    user.username = "user-provider-header-reset".to_string();
    let headers = HashMap::from([(String::from("cookie"), String::from("sid=abc"))]);

    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-reset",
            virtual_id: 7005,
            provider: "provider-a",
            stream_url: "http://localhost/live-a.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    assert!(manager.update_session_provider_headers(&user.username, "tok-reset", &headers).await);

    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-reset",
            virtual_id: 7005,
            provider: "provider-b",
            stream_url: "http://localhost/live-b.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    let session = manager.get_and_update_user_session(&user.username, "tok-reset").await.expect("session should exist");
    assert!(session.provider_session_headers.is_empty());
}

/// Provider cookies survive child playlist changes on the same host and are only sent to that host.
#[tokio::test]
async fn provider_session_headers_are_scoped_to_the_host_that_set_them() {
    let manager = provider_header_test_manager();
    let mut user = ProxyUserCredentials::default();
    user.username = "user-provider-header-host".to_string();
    let headers = HashMap::from([(String::from("cookie"), String::from("sid=abc"))]);

    create_provider_header_session(&manager, &user, "http://cdn.example/a/video.m3u8").await;
    assert!(store_cookie_header(&manager, &user.username, "sid=abc", "http://cdn.example/a/video.m3u8").await);
    create_provider_header_session(&manager, &user, "http://cdn.example/a/audio.m3u8").await;

    let session = manager.get_and_update_user_session(&user.username, "tok-host").await.expect("session exists");
    assert_eq!(session.provider_session_headers_for("http://cdn.example/a/video.m3u8").as_deref(), Some(&headers));
    assert_eq!(session.provider_session_headers_for("http://entry.example/live/1.m3u8"), None);
    assert_eq!(
        session.provider_session_headers_for("https://cdn.example:80/a/video.m3u8"),
        None,
        "same host and port with another scheme is another origin"
    );

    create_provider_header_session(&manager, &user, "http://other.example/a/video.m3u8").await;
    let session = manager.get_and_update_user_session(&user.username, "tok-host").await.expect("session exists");
    assert!(session.provider_session_headers.is_empty());
}

#[tokio::test]
async fn account_switch_ends_resource_identity_and_rejects_stale_cookie_updates(
) -> Result<(), Box<dyn std::error::Error>> {
    let manager = Arc::new(provider_header_test_manager());
    let mut user = ProxyUserCredentials::default();
    user.username = "session-binding".to_string();
    let url = "http://provider.example/index.m3u8";
    create_provider_header_session(&manager, &user, url).await;
    let identity = session_identity(&manager, &user.username, "tok-host").await.ok_or("session missing")?;
    let cookie = |value: &str| crate::ProviderSessionHeaders {
        headers: HashMap::new(),
        cookies: vec![format!("sid={value}; Path=/")],
    };
    assert!(
        manager
            .update_current_session_provider_response_headers_from(
                &user.username,
                "tok-host",
                identity,
                &cookie("old"),
                url
            )
            .await
    );
    // Rotation while a resource waits is visible to the send-time lookup.
    assert!(
        manager.update_session_provider_response_headers_from(&user.username, "tok-host", &cookie("fresh"), url).await
    );
    assert_eq!(
        manager.current_session_provider_headers(&user.username, "tok-host", identity, url).await,
        SessionProviderHeaders::Headers(HashMap::from([("cookie".to_string(), "sid=fresh".to_string())]))
    );
    assert_eq!(
        manager.current_session_provider_headers(&user.username, "tok-host", identity, "http://other.example/x").await,
        SessionProviderHeaders::NoHeaders,
        "a valid session without headers for the target is not a missing session"
    );

    let notify = {
        let users = manager.connections.read().await;
        let (_, session) = users.current_session(&user.username, "tok-host", identity).ok_or("session missing")?;
        Arc::clone(&session.change_signal.notify)
    };
    let woken = notify.notified();
    tokio::pin!(woken);
    woken.as_mut().enable();
    // Writes that keep the session and binding, of this or another user, wake nobody.
    manager.update_session_addr(&user.username, "tok-host", &"127.0.0.1:55499".parse()?).await;
    let mut other = ProxyUserCredentials::default();
    other.username = "session-binding-other".to_string();
    let other_addr: SocketAddr = "127.0.0.1:55498".parse()?;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &other,
            session_token: "tok-other",
            virtual_id: 7012,
            provider: "provider-a",
            stream_url: url,
            addr: &other_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    assert!(manager.terminate_session(&other.username, "tok-other").await);
    assert!(woken.as_mut().now_or_never().is_none(), "unrelated writes must not wake the waiter");
    let waiter = {
        let manager = Arc::clone(&manager);
        let username = user.username.clone();
        tokio::spawn(async move { manager.wait_for_playback_session_end(&username, "tok-host", identity).await })
    };
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());
    manager.update_session_provider_binding(&user.username, "tok-host", "provider-b".intern(), url.intern()).await;
    tokio::time::timeout(Duration::from_secs(1), waiter).await??;
    assert!(woken.as_mut().now_or_never().is_some(), "the account switch wakes the waiter");
    assert!(!manager.playback_session_is_current(&user.username, "tok-host", identity).await);
    assert_eq!(
        manager.current_session_provider_headers(&user.username, "tok-host", identity, url).await,
        SessionProviderHeaders::NoSession
    );
    assert!(
        !manager
            .update_current_session_provider_response_headers_from(
                &user.username,
                "tok-host",
                identity,
                &cookie("stale"),
                url
            )
            .await,
        "responses from the previous account must not enter the new account's jar"
    );
    let switched = session_identity(&manager, &user.username, "tok-host").await.ok_or("session missing")?;
    assert_eq!(switched.incarnation, identity.incarnation);
    assert_ne!(switched.binding_generation, identity.binding_generation);
    Ok(())
}

#[tokio::test]
async fn provider_session_cookies_survive_origin_changes_and_clear_on_account_switch(
) -> Result<(), Box<dyn std::error::Error>> {
    let manager = provider_header_test_manager();
    let mut user = ProxyUserCredentials::default();
    user.username = "cookie-account".to_string();
    let entry = "https://entry.example/index.m3u8";
    let cdn = "https://cdn.example/video/init.mp4";
    create_provider_header_session(&manager, &user, entry).await;
    for (url, cookie) in [(entry, "sid=entry"), (cdn, "sid=cdn")] {
        assert!(store_cookie_header(&manager, &user.username, cookie, url).await);
    }
    create_provider_header_session(&manager, &user, cdn).await;
    let session = manager.get_and_update_user_session(&user.username, "tok-host").await.ok_or("session missing")?;
    assert_eq!(
        session.provider_session_headers_for(entry).and_then(|headers| headers.get("cookie").cloned()).as_deref(),
        Some("sid=entry")
    );
    assert_eq!(
        session.provider_session_headers_for(cdn).and_then(|headers| headers.get("cookie").cloned()).as_deref(),
        Some("sid=cdn")
    );
    manager.update_session_provider_binding(&user.username, "tok-host", "provider-b".intern(), entry.intern()).await;
    let session = manager.get_and_update_user_session(&user.username, "tok-host").await.ok_or("session missing")?;
    assert!(session.provider_session_cookies.is_empty());
    assert!(session.provider_session_headers_for(entry).is_none());
    assert!(session.provider_session_headers_for(cdn).is_none());
    Ok(())
}

/// Stores the pairs of a `Cookie` header as origin-wide provider cookies set by `source_url`.
pub(in crate::active_user_manager::tests) async fn store_cookie_header(
    manager: &ActiveUserManager,
    username: &str,
    cookie_header: &str,
    source_url: &str,
) -> bool {
    let response = crate::ProviderSessionHeaders {
        headers: HashMap::new(),
        cookies: cookie_header
            .split(';')
            .map(str::trim)
            .filter(|pair| !pair.is_empty())
            .map(|pair| format!("{pair}; Path=/"))
            .collect(),
    };
    manager.update_session_provider_response_headers_from(username, "tok-host", &response, source_url).await
}
