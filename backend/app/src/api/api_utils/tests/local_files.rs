use super::{
    create_session_fingerprint, create_test_app_config, create_test_app_state, create_test_app_state_for_config,
    create_test_fingerprint, create_test_local_channel, local_stream_response,
};
use crate::model::{Config, ConfigInput, ConfigTarget, ProxyUserCredentials};
use arc_swap::{ArcSwap, ArcSwapOption};
use axum::{
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use shared::{
    foundation::Filter,
    model::{InputType, PlaylistItemType, ProcessingOrder, UserConnectionPermission},
};
use std::sync::Arc;
use tuliprox_core::utils::response_compression::should_compress_response;

#[tokio::test]
async fn local_stream_response_registers_active_local_stream() {
    let app_state = create_test_app_state();
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let file_path = temp_dir.path().join("local-test.mkv");
    tokio::fs::write(&file_path, Bytes::from_static(b"local-stream")).await.expect("write local file");

    let addr = "127.0.0.1:55123".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let channel = create_test_local_channel(&format!("file://{}", file_path.display()));
    let input = ConfigInput { input_type: InputType::Library, ..ConfigInput::default() };
    let user = ProxyUserCredentials::default();
    let target = ConfigTarget {
        managed_channels: Vec::new(),
        curation: None,
        id: 1,
        enabled: true,
        name: "test".to_string(),
        options: None,
        sort: None,
        filter: Filter::default().into(),
        output: Vec::new(),
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::default()),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    };

    let _response = local_stream_response(
        &fingerprint,
        &app_state,
        channel,
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        None,
        None,
        false,
    )
    .await
    .into_response();

    let active_streams = app_state.active_users.active_streams().await;
    assert_eq!(active_streams.len(), 1, "local file streaming should register an active stream");
    assert_eq!(active_streams[0].channel.item_type, PlaylistItemType::LocalVideo);
}

#[tokio::test]
async fn local_stream_response_rechecks_limits_before_registering_socket_bound_streams() {
    let mut app_cfg = create_test_app_config();
    let config = Config { user_access_control: true, ..Config::default() };
    app_cfg.config = Arc::new(ArcSwap::from_pointee(config));
    let app_state = create_test_app_state_for_config(Arc::new(app_cfg));
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let file_path = temp_dir.path().join("local-race-test.mkv");
    tokio::fs::write(&file_path, Bytes::from_static(b"local-stream")).await.expect("write local file");

    let first_addr = "127.0.0.1:55131".parse().unwrap_or_else(|_| unreachable!());
    let second_addr = "127.0.0.1:55132".parse().unwrap_or_else(|_| unreachable!());
    let first_fingerprint = create_test_fingerprint(first_addr);
    let second_fingerprint = create_test_fingerprint(second_addr);
    let channel = create_test_local_channel(&format!("file://{}", file_path.display()));
    let input = ConfigInput { input_type: InputType::Library, ..ConfigInput::default() };
    let mut user = ProxyUserCredentials::default();
    user.username = "local-limit-user".to_string();
    user.max_connections = 1;
    let target = ConfigTarget {
        managed_channels: Vec::new(),
        curation: None,
        id: 1,
        enabled: true,
        name: "test".to_string(),
        options: None,
        sort: None,
        filter: Filter::default().into(),
        output: Vec::new(),
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::default()),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    };
    let first_token = create_session_fingerprint(&first_fingerprint, &user.username, channel.virtual_id, true);
    let second_token = create_session_fingerprint(&second_fingerprint, &user.username, channel.virtual_id, true);

    let _first_response = local_stream_response(
        &first_fingerprint,
        &app_state,
        channel.clone(),
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        Some(&first_token),
        None,
        false,
    )
    .await
    .into_response();

    let _second_response = local_stream_response(
        &second_fingerprint,
        &app_state,
        channel,
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        Some(&second_token),
        None,
        false,
    )
    .await
    .into_response();

    assert_eq!(app_state.active_users.user_connections(&user.username).await, 1);
    assert_eq!(app_state.active_users.active_streams().await.len(), 1);
    assert_eq!(
        app_state
            .active_users
            .connection_admission_for_session(
                &user.username,
                user.max_connections,
                user.soft_connections,
                &second_token
            )
            .await
            .permission(),
        UserConnectionPermission::Exhausted,
        "failed second open must not leave a placeholder session that bypasses admission"
    );
}

#[tokio::test]
async fn local_stream_response_disables_response_compression() {
    let app_state = create_test_app_state();
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let file_path = temp_dir.path().join("local-test.mkv");
    tokio::fs::write(&file_path, Bytes::from_static(b"local-stream")).await.expect("write local file");

    let addr = "127.0.0.1:55124".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let channel = create_test_local_channel(&format!("file://{}", file_path.display()));
    let input = ConfigInput { input_type: InputType::Library, ..ConfigInput::default() };
    let user = ProxyUserCredentials::default();
    let target = ConfigTarget {
        managed_channels: Vec::new(),
        curation: None,
        id: 1,
        enabled: true,
        name: "test".to_string(),
        options: None,
        sort: None,
        filter: Filter::default().into(),
        output: Vec::new(),
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::default()),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    };

    let response = local_stream_response(
        &fingerprint,
        &app_state,
        channel,
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        None,
        None,
        false,
    )
    .await
    .into_response();

    assert!(!should_compress_response(&response));
}

#[tokio::test]
async fn local_stream_response_reuses_stable_playback_session_token_across_reopens() {
    let app_state = create_test_app_state();
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let file_path = temp_dir.path().join("local-test.mkv");
    tokio::fs::write(&file_path, Bytes::from_static(b"local-stream")).await.expect("write local file");

    let channel = create_test_local_channel(&format!("file://{}", file_path.display()));
    let input = ConfigInput { input_type: InputType::Library, ..ConfigInput::default() };
    let user = ProxyUserCredentials::default();
    let target = ConfigTarget {
        managed_channels: Vec::new(),
        curation: None,
        id: 1,
        enabled: true,
        name: "test".to_string(),
        options: None,
        sort: None,
        filter: Filter::default().into(),
        output: Vec::new(),
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::default()),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    };
    let playback_session_token = "local-playback-token";

    let first_fingerprint = create_test_fingerprint("127.0.0.1:55125".parse().unwrap_or_else(|_| unreachable!()));
    let second_fingerprint = create_test_fingerprint("127.0.0.1:55126".parse().unwrap_or_else(|_| unreachable!()));

    let _first_response = local_stream_response(
        &first_fingerprint,
        &app_state,
        channel.clone(),
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        Some(playback_session_token),
        None,
        false,
    )
    .await
    .into_response();

    let _second_response = local_stream_response(
        &second_fingerprint,
        &app_state,
        channel,
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        Some(playback_session_token),
        None,
        false,
    )
    .await
    .into_response();

    let active_streams = app_state.active_users.active_streams().await;
    assert_eq!(active_streams.len(), 1, "stable playback token should reuse the tracked local connection");
    assert_eq!(active_streams[0].session_token.as_deref(), Some(playback_session_token));
    assert_eq!(active_streams[0].addr, second_fingerprint.addr);
}

#[tokio::test]
async fn local_stream_response_allows_exhausted_reopen_for_same_playback_session_token() {
    let app_state = create_test_app_state();
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let file_path = temp_dir.path().join("local-test.mkv");
    tokio::fs::write(&file_path, Bytes::from_static(b"local-stream")).await.expect("write local file");

    let channel = create_test_local_channel(&format!("file://{}", file_path.display()));
    let input = ConfigInput { input_type: InputType::Library, ..ConfigInput::default() };
    let mut user = ProxyUserCredentials::default();
    user.username = "user1".to_string();
    user.max_connections = 1;
    let target = ConfigTarget {
        managed_channels: Vec::new(),
        curation: None,
        id: 1,
        enabled: true,
        name: "test".to_string(),
        options: None,
        sort: None,
        filter: Filter::default().into(),
        output: Vec::new(),
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::default()),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    };
    let playback_session_token = "local-playback-token";

    let first_fingerprint = create_test_fingerprint("127.0.0.1:55127".parse().unwrap_or_else(|_| unreachable!()));
    let second_fingerprint = create_test_fingerprint("127.0.0.1:55128".parse().unwrap_or_else(|_| unreachable!()));

    let _first_response = local_stream_response(
        &first_fingerprint,
        &app_state,
        channel.clone(),
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        Some(playback_session_token),
        None,
        false,
    )
    .await
    .into_response();

    let second_response = local_stream_response(
        &second_fingerprint,
        &app_state,
        channel,
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Exhausted,
        crate::api::model::ConnectionKind::Normal,
        Some(playback_session_token),
        None,
        false,
    )
    .await
    .into_response();

    assert_eq!(second_response.status(), StatusCode::OK);

    let active_streams = app_state.active_users.active_streams().await;
    assert_eq!(active_streams.len(), 1);
    assert_eq!(active_streams[0].session_token.as_deref(), Some(playback_session_token));
    assert_eq!(active_streams[0].addr, second_fingerprint.addr);
}

#[tokio::test]
async fn local_stream_response_preserves_soft_kind_across_reopens() {
    let app_state = create_test_app_state();
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let file_path = temp_dir.path().join("local-soft-test.mkv");
    tokio::fs::write(&file_path, Bytes::from_static(b"local-stream")).await.expect("write local file");

    let channel = create_test_local_channel(&format!("file://{}", file_path.display()));
    let input = ConfigInput { input_type: InputType::Library, ..ConfigInput::default() };
    let mut user = ProxyUserCredentials::default();
    user.username = "soft-local-user".to_string();
    user.max_connections = 1;
    user.soft_connections = 1;
    let target = ConfigTarget {
        managed_channels: Vec::new(),
        curation: None,
        id: 1,
        enabled: true,
        name: "test".to_string(),
        options: None,
        sort: None,
        filter: Filter::default().into(),
        output: Vec::new(),
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::default()),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    };
    let playback_session_token = "local-soft-playback-token";

    let first_fingerprint = create_test_fingerprint("127.0.0.1:55129".parse().unwrap_or_else(|_| unreachable!()));
    let second_fingerprint = create_test_fingerprint("127.0.0.1:55130".parse().unwrap_or_else(|_| unreachable!()));

    let _first_response = local_stream_response(
        &first_fingerprint,
        &app_state,
        channel.clone(),
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Soft,
        Some(playback_session_token),
        None,
        false,
    )
    .await
    .into_response();

    let second_response = local_stream_response(
        &second_fingerprint,
        &app_state,
        channel,
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Exhausted,
        crate::api::model::ConnectionKind::Normal,
        Some(playback_session_token),
        None,
        false,
    )
    .await
    .into_response();

    assert_eq!(second_response.status(), StatusCode::OK);

    let session_admission = app_state
        .active_users
        .connection_admission_for_session(
            &user.username,
            user.max_connections,
            user.soft_connections,
            playback_session_token,
        )
        .await;
    assert_eq!(session_admission.kind(), Some(crate::api::model::ConnectionKind::Soft));
}

mod library_path_check {
    use crate::api::api_utils::local_files::{is_library_path_allowed, AllowedLibraryRoot};
    use std::path::{Path, PathBuf};

    fn root(lexical: &str, canonical: Option<&str>, follow_symlinks: bool) -> AllowedLibraryRoot {
        AllowedLibraryRoot { lexical: PathBuf::from(lexical), canonical: canonical.map(PathBuf::from), follow_symlinks }
    }

    fn allowed(requested: &str, canonical: &str, roots: &[AllowedLibraryRoot]) -> bool {
        is_library_path_allowed(Path::new(requested), Path::new(canonical), roots)
    }

    #[test]
    fn follow_allows_lexical_inside_with_canonical_outside() {
        let roots = [root("/media/library", Some("/media/library"), true)];
        assert!(allowed("/media/library/linked_movie.mp4", "/mnt/external/storage/linked_movie.mp4", &roots));
    }

    #[test]
    fn follow_denies_dotdot_and_relative() {
        let roots = [root("/media/library", Some("/media/library"), true)];
        assert!(!allowed("/media/library/../etc/passwd", "/etc/passwd", &roots));
        assert!(!allowed("media/library/movie.mp4", "/etc/passwd", &roots));
    }

    #[test]
    fn no_follow_denies_canonical_outside() {
        let roots = [root("/media/library", Some("/media/library"), false)];
        assert!(!allowed("/media/library/linked_movie.mp4", "/mnt/external/storage/linked_movie.mp4", &roots));
    }

    #[test]
    fn no_follow_symlinked_root_allows_file_inside_target() {
        let roots = [root("/media/symlink_root", Some("/mnt/disk/real_root"), false)];
        assert!(allowed("/media/symlink_root/movie.mp4", "/mnt/disk/real_root/movie.mp4", &roots));
    }

    #[test]
    fn path_outside_every_root_denied() {
        let roots = [root("/media/library", Some("/media/library"), true)];
        assert!(!allowed("/var/data/movie.mp4", "/var/data/movie.mp4", &roots));
    }

    #[test]
    fn follow_accepts_root_with_parent_components() {
        // A configured root such as `/srv/app/../media` stays lexical; files below it are allowed.
        let roots = [root("/srv/app/../media", None, true)];
        assert!(allowed("/srv/app/../media/movie.mp4", "/mnt/cloud/movie.mp4", &roots));
        assert!(!allowed("/srv/app/../media/../etc/passwd", "/mnt/cloud/movie.mp4", &roots));
    }

    #[test]
    fn nested_no_follow_root_overrides_outer_follow_root() {
        let roots = [root("/media", None, true), root("/media/private", Some("/media/private"), false)];
        assert!(!allowed("/media/private/link.mp4", "/etc/secret.mp4", &roots));
        assert!(allowed("/media/private/movie.mp4", "/media/private/movie.mp4", &roots));
        assert!(allowed("/media/public/link.mp4", "/mnt/cloud/link.mp4", &roots));
    }

    #[test]
    fn nested_follow_root_overrides_outer_no_follow_root() {
        let roots = [root("/media", Some("/media"), false), root("/media/cloud", None, true)];
        assert!(allowed("/media/cloud/link.mp4", "/mnt/cloud/link.mp4", &roots));
        assert!(!allowed("/media/other/link.mp4", "/mnt/cloud/link.mp4", &roots));
    }
}
