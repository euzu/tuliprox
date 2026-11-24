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

pub(crate) struct ResolvedFailoverStream {
    pub(crate) stream: StreamInfo,
    pub(crate) target: Arc<ConfigTarget>,
    pub(crate) group: Arc<FailoverGroupRecord>,
    pub(crate) group_vid: u32,
}

pub(crate) async fn resolve_failover_stream(
    app_state: &AppState,
    stream_id: u32,
    owner_filter: Option<&str>,
    allow_hls: bool,
) -> Result<ResolvedFailoverStream, (StatusCode, &'static str)> {
    let Some(stream) = app_state.active_users.find_panel_stream_by_uid(stream_id).await else {
        return Err((StatusCode::NOT_FOUND, "stream not found"));
    };

    if let Some(owner) = owner_filter {
        if stream.username != owner {
            return Err((StatusCode::NOT_FOUND, "stream not found"));
        }
    }

    if allow_hls {
        if stream.channel.item_type != PlaylistItemType::Live && stream.channel.item_type != PlaylistItemType::LiveHls {
            return Err((StatusCode::BAD_REQUEST, "manual switch only supported for live streams"));
        }
    } else if stream.channel.item_type != PlaylistItemType::Live {
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

    Ok(ResolvedFailoverStream { stream, target, group, group_vid })
}

pub(crate) fn map_failover_members(
    group: &FailoverGroupRecord,
    current_member_vid: Option<u32>,
) -> impl Iterator<Item = (usize, &tuliprox_repository::GroupMember, bool)> {
    group.members.iter().enumerate().map(move |(idx, m)| {
        let is_current = current_member_vid.is_some_and(|vid| vid == m.member_vid.get());
        (idx, m, is_current)
    })
}

pub(crate) fn build_admin_failover_members_dto(
    group: &FailoverGroupRecord,
    current_member_vid: Option<u32>,
    group_name: &str,
) -> StreamFailoverMembersDto {
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

    StreamFailoverMembersDto { group_name: group_name.to_string(), current_member_vid, members }
}

pub(crate) fn user_member_health(health: &tuliprox_session::MemberHealth) -> shared::model::UserMemberHealthDto {
    let (state, retry_after_secs) = match health.circuit {
        tuliprox_session::CircuitView::Open { remaining } => {
            let secs = remaining.as_secs().saturating_add(u64::from(remaining.subsec_nanos() > 0));
            (shared::model::UserMemberHealthState::Failing, Some(secs))
        }
        tuliprox_session::CircuitView::HalfOpenInFlight => (shared::model::UserMemberHealthState::Degraded, None),
        tuliprox_session::CircuitView::Counting(n) if n > 0 => (shared::model::UserMemberHealthState::Degraded, None),
        _ if health.qos.as_ref().is_some_and(|q| q.failure_streak >= 2) => {
            (shared::model::UserMemberHealthState::Degraded, None)
        }
        _ => (shared::model::UserMemberHealthState::Ok, None),
    };

    let (resolution, video_codec, bitrate_kbps) = if let Some(probe) = &health.probe {
        let resolution =
            (probe.resolution != shared::model::VideoResolution::Unknown).then(|| probe.resolution.clone());
        let video_codec = (probe.codec != shared::model::VideoCodec::Other).then(|| probe.codec.clone());
        let bitrate_kbps = (probe.bitrate_kbps != 0).then_some(probe.bitrate_kbps);
        (resolution, video_codec, bitrate_kbps)
    } else {
        (None, None, None)
    };

    let qos_24h = health.qos.as_ref().map(|q| q.score_24h);
    let qos_7d = health.qos.as_ref().map(|q| q.score_7d);

    shared::model::UserMemberHealthDto {
        state,
        resolution,
        video_codec,
        bitrate_kbps,
        qos_24h,
        qos_7d,
        retry_after_secs,
    }
}

pub(crate) fn build_user_failover_members_dto(
    group: &FailoverGroupRecord,
    current_member_vid: Option<u32>,
    group_name: &str,
    target_name: &str,
    member_health: &tuliprox_session::MemberHealthIndex,
    now: std::time::Instant,
) -> shared::model::UserFailoverMembersDto {
    let members = map_failover_members(group, current_member_vid)
        .map(|(idx, m, is_current)| {
            let health = member_health.health(target_name, m, now);
            shared::model::UserFailoverMemberDto {
                member_vid: m.member_vid.get(),
                index: idx,
                title: m.title.to_string(),
                has_archive: m.archive.is_some(),
                current: is_current,
                health: Some(user_member_health(&health)),
            }
        })
        .collect();

    shared::model::UserFailoverMembersDto { group_name: group_name.to_string(), current_member_vid, members }
}

pub async fn get_stream_failover_members(
    State(app_state): State<Arc<AppState>>,
    Path(stream_id): Path<u32>,
) -> Response {
    let ResolvedFailoverStream { stream, group, .. } =
        match resolve_failover_stream(&app_state, stream_id, None, false).await {
            Ok(resolved) => resolved,
            Err((code, msg)) => return (code, msg).into_response(),
        };

    let current_member_vid = stream.member_info.as_ref().and_then(|m| m.member_vid);
    let group_name = if stream.channel.title.is_empty() { group.key.as_ref() } else { stream.channel.title.as_ref() };
    let dto = build_admin_failover_members_dto(&group, current_member_vid, group_name);

    Json(dto).into_response()
}

pub async fn post_stream_failover_switch(
    State(app_state): State<Arc<AppState>>,
    Path(stream_id): Path<u32>,
    Json(payload): Json<StreamFailoverSwitchRequest>,
) -> Response {
    let ResolvedFailoverStream { stream, group, group_vid, .. } =
        match resolve_failover_stream(&app_state, stream_id, None, false).await {
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
        Err(err) => (StatusCode::BAD_GATEWAY, err.to_string()).into_response(),
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

    #[tokio::test]
    async fn admin_failover_routes_reject_hls_streams_with_exact_mpegts_message(
    ) -> Result<(), Box<dyn std::error::Error>> {
        use axum::body::to_bytes;

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

        let addr: SocketAddr = "127.0.0.1:8080".parse()?;
        let fp = Fingerprint::new("fp".into(), "127.0.0.1".into(), addr);
        let channel = StreamChannel {
            target_id: 1,
            virtual_id: 20,
            group_virtual_id: None,
            provider_id: 1,
            input_name: "test-in".into(),
            item_type: PlaylistItemType::LiveHls,
            cluster: XtreamCluster::Live,
            group: "test-group".into(),
            title: "Test HLS Stream".into(),
            url: "http://example.com/live.m3u8".into(),
            shared: false,
            ..Default::default()
        };
        let connection = ConnectionParams {
            meter_uid: 2,
            username: "testuser2",
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
            active_streams.into_iter().find(|s| s.username == "testuser2").expect("registered stream");
        let stream_id = registered_stream.uid;

        // 1. GET /streams/{id}/failover-members on HLS stream -> 400 "manual switch only supported for MPEG-TS live streams"
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
        let body = to_bytes(res.into_body(), 4096).await?;
        assert_eq!(&body[..], b"manual switch only supported for MPEG-TS live streams");

        // 2. POST /streams/{id}/failover-switch on HLS stream -> 400 "manual switch only supported for MPEG-TS live streams"
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
        let body = to_bytes(res.into_body(), 4096).await?;
        assert_eq!(&body[..], b"manual switch only supported for MPEG-TS live streams");

        Ok(())
    }

    #[test]
    fn admin_failover_switch_error_responses_preserve_status_and_body() {
        use crate::api::api_utils::failover_group_runtime::SwitchError;

        let cases = [
            (SwitchError::NotActive, StatusCode::BAD_GATEWAY, "Stream runtime failover driver not active or not found"),
            (SwitchError::InProgress, StatusCode::BAD_GATEWAY, "Switch already in progress"),
            (SwitchError::MemberNotFound, StatusCode::BAD_GATEWAY, "Target member not found in group"),
            (
                SwitchError::Cooldown { retry_after: std::time::Duration::from_secs(2) },
                StatusCode::BAD_GATEWAY,
                "Switch cooldown active",
            ),
            (SwitchError::ChannelClosed, StatusCode::BAD_GATEWAY, "Stream driver channel closed"),
            (SwitchError::Preempted, StatusCode::BAD_GATEWAY, "Stream member preempted"),
            (
                SwitchError::TargetFailedRestored,
                StatusCode::BAD_GATEWAY,
                "Failed to open target member; returned to current member",
            ),
            (
                SwitchError::TargetFailedUnrestored,
                StatusCode::BAD_GATEWAY,
                "Failed to open target member and failed to restore current member",
            ),
            (SwitchError::RespondentDropped, StatusCode::BAD_GATEWAY, "Switch respondent dropped"),
            (SwitchError::Timeout, StatusCode::BAD_GATEWAY, "Switch operation timed out"),
        ];

        for (err, expected_status, expected_body) in cases {
            let resp = (StatusCode::BAD_GATEWAY, err.to_string()).into_response();
            assert_eq!(resp.status(), expected_status);
            assert_eq!(err.to_string(), expected_body);
        }
    }

    #[test]
    fn test_user_member_health_table() {
        use shared::model::{UserMemberHealthDto, UserMemberHealthState, VideoCodec, VideoResolution};
        use std::time::Duration;
        use tuliprox_session::{CircuitView, MemberHealth, ProbePart, QosPart};

        // 1. Closed without data => Ok and all Options None
        let health = MemberHealth { qos: None, probe: None, circuit: CircuitView::Closed };
        let dto = user_member_health(&health);
        assert_eq!(
            dto,
            UserMemberHealthDto {
                state: UserMemberHealthState::Ok,
                resolution: None,
                video_codec: None,
                bitrate_kbps: None,
                qos_24h: None,
                qos_7d: None,
                retry_after_secs: None,
            }
        );

        // 2. Counting(1) => Degraded
        let health = MemberHealth { qos: None, probe: None, circuit: CircuitView::Counting(1) };
        let dto = user_member_health(&health);
        assert_eq!(dto.state, UserMemberHealthState::Degraded);
        assert_eq!(dto.retry_after_secs, None);

        // 3. Open{remaining 2.5s} => Failing, retry_after_secs == Some(3)
        let health = MemberHealth {
            qos: None,
            probe: None,
            circuit: CircuitView::Open { remaining: Duration::from_millis(2500) },
        };
        let dto = user_member_health(&health);
        assert_eq!(dto.state, UserMemberHealthState::Failing);
        assert_eq!(dto.retry_after_secs, Some(3));

        // 4. HalfOpenInFlight => Degraded
        let health = MemberHealth { qos: None, probe: None, circuit: CircuitView::HalfOpenInFlight };
        let dto = user_member_health(&health);
        assert_eq!(dto.state, UserMemberHealthState::Degraded);
        assert_eq!(dto.retry_after_secs, None);

        // 5. failure_streak 2 => Degraded
        let health = MemberHealth {
            qos: Some(QosPart { score_24h: 80, score_7d: 85, confidence: 50, failure_streak: 2 }),
            probe: None,
            circuit: CircuitView::Closed,
        };
        let dto = user_member_health(&health);
        assert_eq!(dto.state, UserMemberHealthState::Degraded);
        assert_eq!(dto.qos_24h, Some(80));
        assert_eq!(dto.qos_7d, Some(85));
        assert_eq!(dto.retry_after_secs, None);

        // 6. failure_streak 1 => Ok
        let health = MemberHealth {
            qos: Some(QosPart { score_24h: 80, score_7d: 85, confidence: 50, failure_streak: 1 }),
            probe: None,
            circuit: CircuitView::Closed,
        };
        let dto = user_member_health(&health);
        assert_eq!(dto.state, UserMemberHealthState::Ok);
        assert_eq!(dto.retry_after_secs, None);

        // 7. probe with resolution/codec/bitrate => exact values
        let health = MemberHealth {
            qos: None,
            probe: Some(ProbePart {
                resolution: VideoResolution::P1080,
                codec: VideoCodec::H264,
                bitrate_kbps: 4500,
                audio_codec: shared::model::AudioCodec::AAC,
                audio_channels: shared::model::AudioChannels::Stereo,
                audio_language: None,
            }),
            circuit: CircuitView::Closed,
        };
        let dto = user_member_health(&health);
        assert_eq!(dto.resolution, Some(VideoResolution::P1080));
        assert_eq!(dto.video_codec, Some(VideoCodec::H264));
        assert_eq!(dto.bitrate_kbps, Some(4500));

        // 8. bitrate 0 => None
        let health = MemberHealth {
            qos: None,
            probe: Some(ProbePart {
                resolution: VideoResolution::P720,
                codec: VideoCodec::H265,
                bitrate_kbps: 0,
                audio_codec: shared::model::AudioCodec::AAC,
                audio_channels: shared::model::AudioChannels::Stereo,
                audio_language: None,
            }),
            circuit: CircuitView::Closed,
        };
        let dto = user_member_health(&health);
        assert_eq!(dto.resolution, Some(VideoResolution::P720));
        assert_eq!(dto.video_codec, Some(VideoCodec::H265));
        assert_eq!(dto.bitrate_kbps, None);

        // 9. unknown resolution/codec => None
        let health = MemberHealth {
            qos: None,
            probe: Some(ProbePart {
                resolution: VideoResolution::Unknown,
                codec: VideoCodec::Other,
                bitrate_kbps: 2000,
                audio_codec: shared::model::AudioCodec::AAC,
                audio_channels: shared::model::AudioChannels::Stereo,
                audio_language: None,
            }),
            circuit: CircuitView::Closed,
        };
        let dto = user_member_health(&health);
        assert_eq!(dto.resolution, None);
        assert_eq!(dto.video_codec, None);
        assert_eq!(dto.bitrate_kbps, Some(2000));
    }
}
