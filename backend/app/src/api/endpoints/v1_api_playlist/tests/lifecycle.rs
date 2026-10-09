use super::{playlist_update_target, test_app_config, test_app_state_with_manual_update_sender};
use crate::model::{ConfigInput, ConfigSource};
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use shared::{
    model::{
        InputRefreshOverride, InputRefreshPolicy, InputType, PlaylistUpdateRequestDto, PlaylistUpdateRequestPayload,
        PlaylistUpdateRunId,
    },
    utils::Internable,
};
use std::sync::Arc;
use tokio::sync::mpsc;

pub(in crate::api::endpoints::v1_api_playlist::tests) fn manual_update_request(
    input_id: u16,
    policy: InputRefreshPolicy,
) -> crate::api::model::ManualPlaylistUpdateRequest {
    crate::api::model::ManualPlaylistUpdateRequest {
        run_id: PlaylistUpdateRunId::generate(),
        targets: Arc::new(crate::model::ProcessTargets {
            enabled: true,
            inputs: vec![input_id],
            targets: Vec::new(),
            target_names: Vec::new(),
        }),
        input_action: Some(shared::model::InputUpdateRequest {
            input_id,
            action: shared::model::InputUpdateAction::Provider(policy),
        }),
    }
}

#[tokio::test]
async fn manual_update_target_ids_keep_conflict_instead_of_accepting_a_dropped_force_request() {
    for pending_policy in [InputRefreshPolicy::NORMAL, InputRefreshPolicy::REFRESH] {
        let input = Arc::new(ConfigInput {
            id: 17,
            name: "force-input".intern(),
            input_type: InputType::Xtream,
            enabled: true,
            ..ConfigInput::default()
        });
        let app_config = Arc::new(test_app_config(
            Arc::clone(&input),
            ConfigSource {
                inputs: vec![Arc::clone(&input.name)],
                targets: vec![playlist_update_target(1, "queued-target")],
            },
        ));
        let (sender, mut receiver) = mpsc::channel(1);
        sender.send(manual_update_request(1, InputRefreshPolicy::NORMAL)).await.unwrap();
        let running = receiver.recv().await.expect("running request");
        assert_eq!(
            running.input_action.map(|request| request.action),
            Some(shared::model::InputUpdateAction::Provider(InputRefreshPolicy::NORMAL))
        );
        sender.send(manual_update_request(2, pending_policy)).await.unwrap();
        let app_state = test_app_state_with_manual_update_sender(app_config, sender);

        let response = super::super::playlist_update(
            State(app_state),
            None,
            Json(PlaylistUpdateRequestPayload::Current(PlaylistUpdateRequestDto {
                targets: Vec::new(),
                target_ids: Some(vec![1]),
                input_refresh: Some(InputRefreshOverride { input_id: input.id, policy: InputRefreshPolicy::FORCE }),
                input_action: None,
            })),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::CONFLICT);
        let pending = receiver.recv().await.expect("pending request remains queued");
        assert_eq!(
            pending.input_action,
            Some(shared::model::InputUpdateRequest {
                input_id: 2,
                action: shared::model::InputUpdateAction::Provider(pending_policy)
            })
        );
        assert!(receiver.try_recv().is_err());
    }
}
