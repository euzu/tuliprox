use super::*;

#[tokio::test]
async fn touch_http_activity_is_processed_by_socket_activity_worker() {
    let manager = create_test_connection_manager();
    let addr: SocketAddr = "127.0.0.1:3234".parse().unwrap_or_else(|_| unreachable!());
    let user = create_test_proxy_user("user1");

    manager.add_connection(&addr).await;
    let _ = manager
        .user_manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-touch",
            virtual_id: 1,
            provider: "provider_1",
            stream_url: "http://provider-1.example/live.ts",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::ConnectionKind::Normal),
            socket_bound: true,
        })
        .await;

    manager.touch_http_activity(&user.username, "tok-touch", &addr).await;

    assert!(tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if manager.user_manager.socket_expiry_deadline(&addr).await.is_some()
                && manager.user_manager.get_username_for_addr(&addr).await.as_deref() == Some(user.username.as_str())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok());
}
