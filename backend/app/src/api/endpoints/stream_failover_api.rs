use crate::{api::model::AppState, model::ConfigTarget, repository::FailoverGroupRecord};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use shared::model::{
    ChannelMemberRefDto, FailoverGroupMemberDto, PlaylistItemType, StreamFailoverMembersDto,
    StreamFailoverSwitchRequest, StreamInfo, VirtualId,
};
use std::sync::Arc;
use tuliprox_repository::target_stream_item::{classify_virtual_id, ResolvedStreamTarget};

struct ResolvedFailoverStream {
    stream: StreamInfo,
    _target: Arc<ConfigTarget>,
    group: Arc<FailoverGroupRecord>,
    group_vid: u32,
}

async fn resolve_failover_stream(
    app_state: &AppState,
    stream_id: u32,
) -> Result<ResolvedFailoverStream, (StatusCode, &'static str)> {
    let Some(stream) = app_state.active_users.find_panel_stream_by_uid(stream_id).await else {
        return Err((StatusCode::NOT_FOUND, "stream not found"));
    };

    if stream.channel.item_type != PlaylistItemType::Live {
        return Err((StatusCode::BAD_REQUEST, "manual switch only supported for MPEG-TS live streams"));
    }

    let Some(target) = app_state.app_config.get_target_by_id(stream.channel.target_id) else {
        return Err((StatusCode::NOT_FOUND, "target not found"));
    };

    let group_vid = stream.channel.group_virtual_id.unwrap_or(stream.channel.virtual_id);
    let classified =
        classify_virtual_id(&app_state.app_config, &app_state.playlists, &target, VirtualId::new(group_vid)).await;
    let ResolvedStreamTarget::Group(group) = classified else {
        return Err((StatusCode::BAD_REQUEST, "not a failover group stream"));
    };

    Ok(ResolvedFailoverStream { stream, _target: target, group, group_vid })
}

pub async fn get_stream_failover_members(
    State(app_state): State<Arc<AppState>>,
    Path(stream_id): Path<u32>,
) -> Response {
    let ResolvedFailoverStream { stream, group, .. } = match resolve_failover_stream(&app_state, stream_id).await {
        Ok(resolved) => resolved,
        Err((code, msg)) => return (code, msg).into_response(),
    };

    let current_member_vid = stream.member_info.as_ref().and_then(|m| m.member_vid);
    let members = group
        .members
        .iter()
        .map(|m| FailoverGroupMemberDto {
            member_vid: Some(m.member_vid),
            member_ref: ChannelMemberRefDto {
                input: m.input_name.to_string(),
                stream_id: m.input_stream_id.to_string(),
            },
            title: m.title.to_string(),
            input_name: m.input_name.to_string(),
            provider_id: m.provider_id,
            has_archive: m.archive.is_some(),
        })
        .collect();

    let dto = StreamFailoverMembersDto {
        group_name: if stream.channel.title.is_empty() {
            group.key.to_string()
        } else {
            stream.channel.title.to_string()
        },
        current_member_vid,
        members,
    };

    Json(dto).into_response()
}

pub async fn post_stream_failover_switch(
    State(app_state): State<Arc<AppState>>,
    Path(stream_id): Path<u32>,
    Json(payload): Json<StreamFailoverSwitchRequest>,
) -> Response {
    let ResolvedFailoverStream { stream, group, group_vid, .. } =
        match resolve_failover_stream(&app_state, stream_id).await {
            Ok(resolved) => resolved,
            Err((code, msg)) => return (code, msg).into_response(),
        };

    if !group.members.iter().any(|m| m.member_vid.get() == payload.member_vid) {
        return (StatusCode::BAD_REQUEST, "member not in group").into_response();
    }

    let is_shared = stream.channel.shared;
    let target_id = stream.channel.target_id;
    match app_state
        .failover_switch_registry
        .switch_stream(stream.uid, is_shared, target_id, group_vid, payload.member_vid)
        .await
    {
        Ok(()) => StatusCode::OK.into_response(),
        Err(err) => (StatusCode::BAD_GATEWAY, err).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::{
            endpoints::v1_api::v1_api_register,
            model::{create_test_app_state, ConnectionKind, ConnectionParams},
        },
        auth::{create_jwt_web_user, Fingerprint},
        model::Config,
    };
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use shared::model::{Permission, PlaylistItemType, StreamChannel, WebAuthConfigDto, WebUiConfigDto, XtreamCluster};
    use std::net::SocketAddr;
    use tower::ServiceExt;

    fn test_config_with_auth() -> Config {
        let mut config = Config::default();
        let web_ui = WebUiConfigDto {
            auth: Some(WebAuthConfigDto {
                enabled: true,
                issuer: "test".to_string(),
                secret: "test-secret".to_string(),
                ..WebAuthConfigDto::default()
            }),
            ..WebUiConfigDto::default()
        };
        config.web_ui = Some((&web_ui).into());
        config
    }

    async fn request_status(
        router: &axum::Router,
        method: &'static str,
        uri: &'static str,
        token: Option<&str>,
        body: Option<&'static str>,
    ) -> StatusCode {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            builder = builder.header("Authorization", format!("Bearer {token}"));
        }
        let body = if let Some(body) = body {
            builder = builder.header("Content-Type", "application/json");
            Body::from(body)
        } else {
            Body::empty()
        };
        router.clone().oneshot(builder.body(body).unwrap()).await.unwrap().status()
    }

    #[tokio::test]
    async fn stream_failover_endpoints_require_correct_permissions() -> Result<(), Box<dyn std::error::Error>> {
        let config = test_config_with_auth();
        let web_auth = config.web_ui.as_ref().and_then(|w| w.auth.as_ref()).unwrap();
        let reader_token = create_jwt_web_user(
            web_auth,
            "admin",
            Permission::SystemRead.into(),
            0,
            shared::model::UserId::from("web:admin"),
        )?;
        let writer_token = create_jwt_web_user(
            web_auth,
            "operator",
            Permission::SystemWrite.into(),
            0,
            shared::model::UserId::from("web:operator"),
        )?;
        let other_token = create_jwt_web_user(
            web_auth,
            "viewer",
            Permission::PlaylistRead.into(),
            0,
            shared::model::UserId::from("web:viewer"),
        )?;

        let app_state = create_test_app_state(config);
        let router = v1_api_register(true, &app_state, "").with_state(app_state);

        // 1. Unauthenticated requests -> 401 Unauthorized
        assert_eq!(
            request_status(&router, "GET", "/api/v1/streams/1/failover-members", None, None).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request_status(&router, "POST", "/api/v1/streams/1/failover-switch", None, Some(r#"{"member_vid":2}"#))
                .await,
            StatusCode::UNAUTHORIZED
        );

        // 2. Request without SystemRead -> 403 Forbidden on GET
        assert_eq!(
            request_status(&router, "GET", "/api/v1/streams/1/failover-members", Some(&other_token), None).await,
            StatusCode::FORBIDDEN
        );

        // 3. Request without SystemWrite -> 403 Forbidden on POST
        assert_eq!(
            request_status(
                &router,
                "POST",
                "/api/v1/streams/1/failover-switch",
                Some(&other_token),
                Some(r#"{"member_vid":2}"#)
            )
            .await,
            StatusCode::FORBIDDEN
        );

        // 4. Request with SystemRead only:
        //    GET reaches handler -> 404 Not Found (since stream 1 doesn't exist)
        assert_eq!(
            request_status(&router, "GET", "/api/v1/streams/1/failover-members", Some(&reader_token), None).await,
            StatusCode::NOT_FOUND
        );
        //    POST switch with SystemRead only -> 403 Forbidden (requires SystemWrite)
        assert_eq!(
            request_status(
                &router,
                "POST",
                "/api/v1/streams/1/failover-switch",
                Some(&reader_token),
                Some(r#"{"member_vid":2}"#)
            )
            .await,
            StatusCode::FORBIDDEN
        );

        // 5. Request with SystemWrite only:
        //    GET failover-members -> 403 Forbidden (requires SystemRead)
        assert_eq!(
            request_status(&router, "GET", "/api/v1/streams/1/failover-members", Some(&writer_token), None).await,
            StatusCode::FORBIDDEN
        );
        //    POST switch with SystemWrite -> 404 Not Found (stream doesn't exist, handler reached)
        assert_eq!(
            request_status(
                &router,
                "POST",
                "/api/v1/streams/1/failover-switch",
                Some(&writer_token),
                Some(r#"{"member_vid":2}"#)
            )
            .await,
            StatusCode::NOT_FOUND
        );

        Ok(())
    }

    #[tokio::test]
    async fn stream_failover_validation_rejects_invalid_requests() -> Result<(), Box<dyn std::error::Error>> {
        let app_state = create_test_app_state(Config::default());
        let target = Arc::new(crate::model::ConfigTarget {
            managed_channels: Vec::new(),
            curation: None,
            id: 1,
            enabled: true,
            name: "test-target".into(),
            options: None,
            sort: None,
            filter: tuliprox_core::model::StagedFilter::default(),
            output: Vec::new(),
            rename: None,
            mapping_ids: None,
            mapping: Arc::default(),
            favourites: None,
            processing_order: shared::model::ProcessingOrder::default(),
            execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
            watch: None,
            use_memory_cache: false,
        });
        let sources = crate::model::SourcesConfig {
            inputs: Vec::new(),
            sources: vec![crate::model::ConfigSource { inputs: Vec::new(), targets: vec![target] }],
            ..Default::default()
        };
        app_state.app_config.sources.store(Arc::new(sources));

        let router = v1_api_register(false, &app_state, "").with_state(Arc::clone(&app_state));

        // 1. Unknown stream -> 404
        let res = router
            .clone()
            .oneshot(Request::builder().method("GET").uri("/api/v1/streams/999/failover-members").body(Body::empty())?)
            .await?;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // Register a test non-group stream:
        let addr: SocketAddr = "127.0.0.1:8080".parse()?;
        let fp = Fingerprint::new("fp".into(), "127.0.0.1".into(), addr);
        let channel = StreamChannel {
            target_id: 1,
            virtual_id: 10,
            group_virtual_id: None,
            provider_id: 1,
            input_name: "test-in".into(),
            item_type: PlaylistItemType::Live,
            cluster: XtreamCluster::Live,
            group: "test-group".into(),
            title: "Test Stream".into(),
            url: "http://example.com".into(),
            shared: false,
            ..Default::default()
        };
        let connection = ConnectionParams {
            meter_uid: 1,
            username: "testuser",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fp,
            provider: "test-prov".into(),
            stream_channel: &channel,
            user_agent: "test-ua".into(),
            session_token: None,
        };
        let _ = app_state.connection_manager.update_connection(connection).await;

        let active_streams = app_state.active_users.panel_streams().await;
        let registered_stream =
            active_streams.into_iter().find(|s| s.username == "testuser").expect("registered stream");
        let stream_id = registered_stream.uid;

        // 2. Non-group stream -> 400 Bad Request
        let res = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/api/v1/streams/{stream_id}/failover-members"))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        // 3. Switch on non-group stream -> 400 Bad Request
        let res = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/streams/{stream_id}/failover-switch"))
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"member_vid":123}"#))?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        Ok(())
    }
}
