use crate::{
    app::{
        components::{Breadcrumbs, InputUpdateCard, TextButton},
        ConfigContext,
    },
    error::Error,
    hooks::use_service_context,
    html_if,
    i18n::use_translation,
    model::{
        build_input_update_card_models, build_input_update_run_views, playlist_update_bulk_scope, EventMessage,
        PlaylistUpdateAcceptedScope, PlaylistUpdateCardStatusAction, PlaylistUpdateCardStatuses,
    },
    services::WebSocketConnectionContext,
};
use gloo_timers::callback::Interval;
use log::warn;
use shared::{
    model::{
        permission::Permission, InputRefreshPolicy, InputUpdateCapabilities, LibraryScanProgressEvent,
        LibraryScanSummaryStatus, LibraryStatus, OperationRunAccepted, PlaylistUpdateRunId, PlaylistUpdateStatusDto,
    },
    utils::current_time_secs,
};
use std::{collections::HashMap, rc::Rc};
use yew::{platform::spawn_local, prelude::*};

#[derive(Clone, Debug, PartialEq, Eq)]
enum PlaylistUpdateBulkOutcome {
    Accepted { run_id: Option<PlaylistUpdateRunId>, input_ids: Vec<u16> },
    Conflict,
    Failed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PlaylistUpdateBulkSubmissionState {
    submitting_for: Option<WebSocketConnectionContext>,
}

impl PlaylistUpdateBulkSubmissionState {
    const fn is_submitting(&self) -> bool { self.submitting_for.is_some() }

    fn is_submitting_for(&self, connection_context: WebSocketConnectionContext) -> bool {
        self.submitting_for == Some(connection_context)
    }
}

enum PlaylistUpdateBulkSubmissionAction {
    Begin(WebSocketConnectionContext),
    ConnectionChanged,
    Finish(WebSocketConnectionContext),
}

fn reduce_playlist_update_bulk_submission(
    mut state: PlaylistUpdateBulkSubmissionState,
    action: PlaylistUpdateBulkSubmissionAction,
) -> PlaylistUpdateBulkSubmissionState {
    match action {
        PlaylistUpdateBulkSubmissionAction::Begin(connection_context) if !state.is_submitting() => {
            state.submitting_for = Some(connection_context);
        }
        PlaylistUpdateBulkSubmissionAction::ConnectionChanged => state.submitting_for = None,
        PlaylistUpdateBulkSubmissionAction::Finish(connection_context)
            if state.is_submitting_for(connection_context) =>
        {
            state.submitting_for = None;
        }
        PlaylistUpdateBulkSubmissionAction::Begin(_) | PlaylistUpdateBulkSubmissionAction::Finish(_) => {}
    }
    state
}

impl Reducible for PlaylistUpdateBulkSubmissionState {
    type Action = PlaylistUpdateBulkSubmissionAction;

    fn reduce(self: Rc<Self>, action: Self::Action) -> Rc<Self> {
        Rc::new(reduce_playlist_update_bulk_submission(*self, action))
    }
}

fn classify_bulk_outcome(
    result: Result<OperationRunAccepted, Error>,
    input_ids: Vec<u16>,
) -> PlaylistUpdateBulkOutcome {
    match result {
        Ok(accepted) => PlaylistUpdateBulkOutcome::Accepted { run_id: accepted.run_id, input_ids },
        Err(Error::Conflict(_)) => PlaylistUpdateBulkOutcome::Conflict,
        Err(_) => PlaylistUpdateBulkOutcome::Failed,
    }
}

fn current_bulk_outcome(
    request_connection_context: WebSocketConnectionContext,
    current_connection_context: WebSocketConnectionContext,
    outcome: PlaylistUpdateBulkOutcome,
) -> Option<PlaylistUpdateBulkOutcome> {
    request_connection_context.is_same_live_connection(current_connection_context).then_some(outcome)
}

fn current_status_response(
    requested_connection: WebSocketConnectionContext,
    current_connection: WebSocketConnectionContext,
    requested_revision: u64,
    current_revision: u64,
) -> bool {
    requested_connection == current_connection && requested_revision == current_revision
}

fn visible_library_progress(progress: Option<&LibraryScanProgressEvent>) -> Option<(&str, bool)> {
    progress.and_then(|progress| {
        let message = progress.summary.message.trim();
        (!message.is_empty()).then_some((message, progress.summary.status == LibraryScanSummaryStatus::Error))
    })
}

fn playlist_update_card_status_action(
    message: &EventMessage,
    connection_context: WebSocketConnectionContext,
) -> Option<PlaylistUpdateCardStatusAction> {
    match message {
        EventMessage::WebSocketStatus(_) => Some(PlaylistUpdateCardStatusAction::ConnectionChanged(connection_context)),
        EventMessage::PlaylistUpdateProgress(progress) => {
            Some(PlaylistUpdateCardStatusAction::Progress { connection_context, progress: progress.clone() })
        }
        EventMessage::PlaylistUpdate(completed) => {
            Some(PlaylistUpdateCardStatusAction::Completed { connection_context, completed: completed.clone() })
        }
        _ => None,
    }
}

#[component]
pub fn PlaylistUpdateView() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("Config context not found");
    let services_ctx = use_service_context();
    let can_write_playlist = services_ctx.auth.has_permission(Permission::PlaylistWrite);
    let can_write_library = services_ctx.auth.has_permission(Permission::LibraryWrite);
    let breadcrumbs = use_state(|| Rc::new(vec![translate.t("LABEL.PLAYLISTS"), translate.t("LABEL.UPDATE")]));
    let bulk_submission = use_reducer(PlaylistUpdateBulkSubmissionState::default);
    let library_progress = use_state(|| None::<LibraryScanProgressEvent>);
    let initial_connection_context = services_ctx.websocket.connection_context();
    let connection_context = use_state(|| initial_connection_context);
    let card_statuses = use_reducer(move || PlaylistUpdateCardStatuses::for_connection(initial_connection_context));
    let update_status = use_state(PlaylistUpdateStatusDto::default);
    let status_request_revision = use_mut_ref(|| 0_u64);
    let library_catalog = use_state(|| None::<LibraryStatus>);
    let now = use_state(current_time_secs);

    let reload_update_status = {
        let services = services_ctx.clone();
        let update_status = update_status.clone();
        let library_catalog = library_catalog.clone();
        let card_statuses = card_statuses.clone();
        let status_request_revision = status_request_revision.clone();
        Callback::from(move |(): ()| {
            let services = services.clone();
            let update_status = update_status.clone();
            let library_catalog = library_catalog.clone();
            let card_statuses = card_statuses.clone();
            let status_request_revision = status_request_revision.clone();
            let request_connection = services.websocket.connection_context();
            let request_revision = {
                let mut revision = status_request_revision.borrow_mut();
                *revision = revision.saturating_add(1);
                *revision
            };
            spawn_local(async move {
                let response = services.playlist.get_update_status().await;
                if !current_status_response(
                    request_connection,
                    services.websocket.connection_context(),
                    request_revision,
                    *status_request_revision.borrow(),
                ) {
                    return;
                }
                match response {
                    Ok(Some(mut status)) => {
                        card_statuses.dispatch(PlaylistUpdateCardStatusAction::RestoreActive {
                            connection_context: request_connection,
                            progress: std::mem::take(&mut status.active_updates),
                        });
                        update_status.set(status);
                    }
                    Ok(None) => {}
                    Err(err) => warn!("Failed to load playlist update status: {err}"),
                }
                if services.auth.has_permission(Permission::LibraryRead) {
                    let response = services.config.get_library_status().await;
                    if !current_status_response(
                        request_connection,
                        services.websocket.connection_context(),
                        request_revision,
                        *status_request_revision.borrow(),
                    ) {
                        return;
                    }
                    match response {
                        Ok(status) => library_catalog.set(status),
                        Err(err) => {
                            library_catalog.set(None);
                            warn!("Failed to load Library catalog status: {err}");
                        }
                    }
                }
            });
        })
    };

    {
        let now = now.clone();
        use_effect_with((), move |()| {
            let interval = Interval::new(60_000, move || now.set(current_time_secs()));
            move || drop(interval)
        });
    }

    // Existing playlist events remain the runtime source of truth. The socket
    // context bounds their run identity and execution order to one connection.
    {
        let card_statuses = card_statuses.clone();
        let library_progress = library_progress.clone();
        let reload_update_status = reload_update_status.clone();
        let services = services_ctx.clone();
        let connection_context = connection_context.clone();
        let bulk_submission = bulk_submission.clone();
        use_effect_with((), move |()| {
            let services_for_cleanup = services.clone();
            let services_for_events = services.clone();
            let sub_id = services.event.subscribe(move |msg| {
                let current_connection_context = services_for_events.websocket.connection_context();
                if let Some(action) = playlist_update_card_status_action(&msg, current_connection_context) {
                    card_statuses.dispatch(action);
                }
                match msg {
                    EventMessage::WebSocketStatus(_) => {
                        connection_context.set(current_connection_context);
                        bulk_submission.dispatch(PlaylistUpdateBulkSubmissionAction::ConnectionChanged);
                        if current_connection_context.is_connected() {
                            reload_update_status.emit(());
                        }
                    }
                    EventMessage::PlaylistUpdate(_) => {
                        reload_update_status.emit(());
                    }
                    EventMessage::LibraryScanProgress(progress) => {
                        if progress.summary.result.is_some() {
                            reload_update_status.emit(());
                        }
                        library_progress.set(Some(progress));
                    }
                    _ => {}
                }
            });
            move || {
                services_for_cleanup.event.unsubscribe(sub_id);
            }
        });
    }

    // Subscribe before loading a snapshot so completion during the request wins.
    {
        let reload_update_status = reload_update_status.clone();
        use_effect_with((), move |()| {
            reload_update_status.emit(());
            || ()
        });
    }

    let library_enabled = config_ctx.config.as_ref().is_some_and(|c| c.config.is_library_enabled());
    let (input_update_cards, input_update_run_views, bulk_scope) = config_ctx.config.as_ref().map_or_else(
        || (Vec::new(), Vec::new(), crate::model::PlaylistUpdateBulkScope::default()),
        |config| {
            let cards = build_input_update_card_models(&config.sources, &update_status);
            let scope = playlist_update_bulk_scope(&config.sources, &cards);
            let views = build_input_update_run_views(&config.sources, &cards, &card_statuses, &update_status);
            (cards, views, scope)
        },
    );
    let input_update_cards_by_id =
        input_update_cards.into_iter().map(|model| (model.input_id, Rc::new(model))).collect::<HashMap<_, _>>();

    let handle_update = {
        let translate = translate.clone();
        let services = services_ctx.clone();
        let card_statuses = card_statuses.clone();
        let bulk_scope = bulk_scope.clone();
        let bulk_submission = bulk_submission.clone();
        let connection_context = connection_context.clone();
        Callback::from(move |_| {
            let request_connection_context = *connection_context;
            if !can_write_playlist
                || bulk_submission.is_submitting()
                || !bulk_scope.can_submit
                || !services.websocket.is_current_connection(request_connection_context)
            {
                return;
            }
            bulk_submission.dispatch(PlaylistUpdateBulkSubmissionAction::Begin(request_connection_context));
            let services = services.clone();
            let translate = translate.clone();
            let card_statuses = card_statuses.clone();
            let input_ids = bulk_scope.visible_input_ids.clone();
            let bulk_submission = bulk_submission.clone();
            spawn_local(async move {
                let Some(outcome) = current_bulk_outcome(
                    request_connection_context,
                    services.websocket.connection_context(),
                    classify_bulk_outcome(services.playlist.update_all_inputs().await, input_ids),
                ) else {
                    return;
                };
                match outcome {
                    PlaylistUpdateBulkOutcome::Accepted { run_id: Some(run_id), input_ids } => {
                        card_statuses.dispatch(PlaylistUpdateCardStatusAction::Accepted(PlaylistUpdateAcceptedScope {
                            connection_context: request_connection_context,
                            run_id,
                            input_ids,
                            refresh_policy: InputRefreshPolicy::NORMAL,
                        }));
                        services.toastr.success(translate.t("MESSAGES.PLAYLIST_UPDATE.SUCCESS"));
                    }
                    PlaylistUpdateBulkOutcome::Accepted { run_id: None, .. } => {
                        services.toastr.success(translate.t("MESSAGES.PLAYLIST_UPDATE.SUCCESS"));
                    }
                    PlaylistUpdateBulkOutcome::Conflict => {
                        services.toastr.warning(translate.t("MESSAGES.PLAYLIST_UPDATE.BULK_BUSY"));
                    }
                    PlaylistUpdateBulkOutcome::Failed => {
                        services.toastr.error(translate.t("MESSAGES.PLAYLIST_UPDATE.FAIL"));
                    }
                }
                bulk_submission.dispatch(PlaylistUpdateBulkSubmissionAction::Finish(request_connection_context));
            });
        })
    };

    let on_request_accepted = {
        let card_statuses = card_statuses.clone();
        Callback::from(move |accepted| {
            card_statuses.dispatch(PlaylistUpdateCardStatusAction::Accepted(accepted));
        })
    };

    let library_progress_view = visible_library_progress(library_progress.as_ref());

    html! {
      <div class="tp__playlist-update-view">
         <Breadcrumbs items={&*breadcrumbs}/>
         <div class="tp__playlist-update-view__header">
          <h1>{ translate.t("LABEL.UPDATE")}</h1>
        { html_if!(can_write_playlist, {
            <TextButton name="playlist_update"
                   icon="Refresh"
                   disabled={bulk_submission.is_submitting()
                       || !bulk_scope.can_submit
                       || !connection_context.is_connected()}
                   title={ translate.t("LABEL.UPDATE_ALL_INPUTS")}
                   onclick={handle_update}></TextButton>
        })}
        </div>
         {if let Some((message, is_error)) = library_progress_view {
             html! {
                 <section
                     class={classes!(
                         "tp__playlist-update-view__library-status",
                         is_error.then_some("tp__playlist-update-view__library-status--error"),
                     )}
                     role="status"
                     aria-live="polite"
                 >
                     {message}
                 </section>
             }
         } else {
             Html::default()
         }}
         <div class="tp__playlist-update-view__cards">
            {for input_update_run_views.into_iter().map(|view| view.with_library_catalog(library_catalog.as_ref())).filter_map(|view| {
                let input_id = view.input_id;
                input_update_cards_by_id.get(&input_id).map(|model| html! {
                    <InputUpdateCard
                        key={input_id}
                        model={Rc::clone(model)}
                        view={Rc::new(view)}
                        now={*now}
                        connection_context={*connection_context}
                        on_request_accepted={on_request_accepted.clone()}
                        can_submit={can_write_playlist && (model.capabilities != InputUpdateCapabilities::Rescan
                            || (can_write_library && library_enabled))}
                    />
                })
            })}
         </div>
      </div>
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn playlist_update_status_reload_rejects_older_responses_and_previous_socket_epochs() {
        let active = super::WebSocketConnectionContext::new(3, true);
        assert!(super::current_status_response(active, active, 5, 5));
        assert!(!super::current_status_response(active, active, 4, 5));
        assert!(!super::current_status_response(active, super::WebSocketConnectionContext::new(4, true), 5, 5));
        assert!(!super::current_status_response(active, super::WebSocketConnectionContext::new(3, false), 5, 5));
    }

    use super::{
        classify_bulk_outcome, current_bulk_outcome, playlist_update_card_status_action,
        reduce_playlist_update_bulk_submission, visible_library_progress, PlaylistUpdateBulkOutcome,
        PlaylistUpdateBulkSubmissionAction, PlaylistUpdateBulkSubmissionState,
    };
    use crate::{
        error::Error,
        model::{
            EventMessage, InputUpdateCardStatus, PlaylistUpdateAcceptedScope, PlaylistUpdateCardStatusAction,
            PlaylistUpdateCardStatuses,
        },
        services::WebSocketConnectionContext,
    };
    use shared::model::{
        InputRefreshPolicy, LibraryScanProgressEvent, LibraryScanSummary, LibraryScanSummaryStatus,
        OperationRunAccepted, PlaylistUpdateProgressEvent, PlaylistUpdateRunId, PlaylistUpdateRunOrder,
        PlaylistUpdateRunStateEvent, PlaylistUpdateState,
    };
    use std::rc::Rc;
    use yew::Reducible;

    #[test]
    fn playlist_update_action_library_card_uses_common_update_endpoint_not_header_scan() {
        let source = include_str!("input_update_card.rs");
        assert!(source.split_whitespace().collect::<String>().contains("services.playlist.update_input("));
        assert!(!source.contains("props.on_update_library"));
        assert!(source.contains("InputUpdateCapabilities::Rescan"));
    }

    #[test]
    fn playlist_update_view_header_has_only_bulk_action_and_keeps_library_read_and_progress() {
        let source = include_str!("playlist_update_view.rs").split("#[cfg(test)]").next().unwrap();
        let header = source
            .split_once("<div class=\"tp__playlist-update-view__header\">")
            .unwrap()
            .1
            .split_once("{if let Some((message, is_error)) = library_progress_view")
            .unwrap()
            .0;
        assert_eq!(header.matches("<TextButton").count(), 1, "only the existing bulk action belongs in the header");
        assert!(header.contains("LABEL.UPDATE_ALL_INPUTS"));
        assert!(header.contains("can_write_playlist"));
        assert!(header.contains("!connection_context.is_connected()"));
        assert!(!header.contains("tp__config-view__header-tools"));
        assert!(!source.contains("handle_update_content"));
        assert!(!source.contains("ACTION_UPDATE_LIBRARY"));
        assert!(!source.contains("library_updating"));
        assert!(source.contains("services.config.get_library_status().await"));
        assert!(source.contains("EventMessage::LibraryScanProgress(progress)"));
        assert!(source.contains("can_write_library && library_enabled"));

        let service = include_str!("../../../services/config_service.rs");
        assert!(!service.contains("pub async fn update_library("));
        assert!(service.contains("pub async fn get_library_status("));
    }

    fn reduce_event(
        state: Rc<PlaylistUpdateCardStatuses>,
        message: EventMessage,
        connection_context: WebSocketConnectionContext,
    ) -> Rc<PlaylistUpdateCardStatuses> {
        let action = playlist_update_card_status_action(&message, connection_context)
            .expect("playlist lifecycle event should map to a card action");
        state.reduce(action)
    }

    fn reduce_bulk_submission(
        state: PlaylistUpdateBulkSubmissionState,
        action: PlaylistUpdateBulkSubmissionAction,
    ) -> PlaylistUpdateBulkSubmissionState {
        reduce_playlist_update_bulk_submission(state, action)
    }

    const fn connection(socket_epoch: u64) -> WebSocketConnectionContext {
        WebSocketConnectionContext::new(socket_epoch, true)
    }

    #[test]
    fn playlist_update_bulk_conflict_and_failure_never_return_a_queued_card_scope() {
        let conflict = classify_bulk_outcome(Err(Error::Conflict("busy".to_string())), vec![1, 2]);
        let failure = classify_bulk_outcome(Err(Error::RequestError), vec![1, 2]);

        assert_eq!(conflict, PlaylistUpdateBulkOutcome::Conflict);
        assert_eq!(failure, PlaylistUpdateBulkOutcome::Failed);
    }

    #[test]
    fn playlist_update_bulk_accepted_request_preserves_the_complete_card_scope() {
        let run_id = PlaylistUpdateRunId::from("bulk-run");
        assert_eq!(
            classify_bulk_outcome(Ok(OperationRunAccepted::playlist_update(run_id.clone())), vec![30, 20]),
            PlaylistUpdateBulkOutcome::Accepted { run_id: Some(run_id), input_ids: vec![30, 20] }
        );
    }

    #[test]
    fn playlist_update_view_mixed_input_capabilities_keep_run_results_and_library_progress_separate() {
        use crate::model::{build_input_update_card_models, playlist_update_bulk_scope};
        use shared::{
            model::{ConfigInputDto, ConfigSourceDto, ConfigTargetDto, InputType, SourcesConfigDto},
            utils::Internable,
        };
        let sources = SourcesConfigDto {
            inputs: [
                (41, InputType::M3u),
                (7, InputType::Stalker),
                (3, InputType::XtreamBatch),
                (9, InputType::Library),
            ]
            .into_iter()
            .map(|(id, input_type)| ConfigInputDto {
                id,
                input_type,
                name: format!("input-{id}").intern(),
                ..ConfigInputDto::default()
            })
            .collect(),
            sources: vec![ConfigSourceDto {
                inputs: vec!["input-41".intern(), "input-7".intern(), "input-3".intern()],
                targets: vec![ConfigTargetDto { id: 20, name: "target".to_owned(), ..ConfigTargetDto::default() }],
            }],
            ..SourcesConfigDto::default()
        };
        let cards = build_input_update_card_models(&sources, &Default::default());
        let scope = playlist_update_bulk_scope(&sources, &cards);
        assert_eq!(scope.visible_input_ids, [41, 7, 3]);
        assert_eq!(cards[2].capabilities, shared::model::InputUpdateCapabilities::BulkOnly);
        let context = connection(1);
        let mut state = Rc::new(PlaylistUpdateCardStatuses::for_connection(context));
        state = state.reduce(PlaylistUpdateCardStatusAction::Accepted(PlaylistUpdateAcceptedScope {
            connection_context: context,
            run_id: "mixed".into(),
            input_ids: scope.visible_input_ids,
            refresh_policy: InputRefreshPolicy::NORMAL,
        }));
        for (input_id, outcome, message) in [
            (41, PlaylistUpdateState::Success, "M3U completed"),
            (7, PlaylistUpdateState::Partial, "cluster rejected"),
            (3, PlaylistUpdateState::Failure, "technical input error"),
        ] {
            state = reduce_event(
                state,
                EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent::input_completed(
                    "mixed".into(),
                    42.into(),
                    input_id,
                    outcome,
                    "same-display-name",
                    message,
                )),
                context,
            );
        }
        state = reduce_event(
            state,
            EventMessage::PlaylistUpdate(PlaylistUpdateRunStateEvent::correlated(
                "mixed".into(),
                42.into(),
                PlaylistUpdateState::Failure,
            )),
            context,
        );
        for (input_id, status, detail) in [
            (41, InputUpdateCardStatus::Success, "M3U completed"),
            (7, InputUpdateCardStatus::Partial, "cluster rejected"),
            (3, InputUpdateCardStatus::Failed, "technical input error"),
        ] {
            assert_eq!(state.for_input(input_id).status(), status);
            assert_eq!(state.for_input(input_id).details(), [detail]);
        }
        assert_eq!(state.for_input(9).status(), InputUpdateCardStatus::Ready);
        assert!(state.for_input(9).details().is_empty());
        let library = LibraryScanProgressEvent {
            summary: LibraryScanSummary {
                status: LibraryScanSummaryStatus::Success,
                message: "Library completed".to_owned(),
                result: None,
            },
        };
        assert!(
            playlist_update_card_status_action(&EventMessage::LibraryScanProgress(library.clone()), context).is_none()
        );
        assert_eq!(visible_library_progress(Some(&library)), Some(("Library completed", false)));
        assert_eq!(visible_library_progress(None), None);
    }

    #[test]
    fn playlist_update_view_follow_up_order_and_early_failure_do_not_depend_on_http_acceptance() {
        for progresses in [false, true] {
            let context = connection(1);
            let mut state = Rc::new(PlaylistUpdateCardStatuses::for_connection(context));
            state = state.reduce(PlaylistUpdateCardStatusAction::Accepted(PlaylistUpdateAcceptedScope {
                connection_context: context,
                run_id: "r2".into(),
                input_ids: vec![41],
                refresh_policy: InputRefreshPolicy::NORMAL,
            }));
            state = reduce_event(
                state,
                EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent::input_completed(
                    "r1".into(),
                    1.into(),
                    41,
                    PlaylistUpdateState::Success,
                    "M3U",
                    "r1 completed",
                )),
                context,
            );
            state = reduce_event(
                state,
                EventMessage::PlaylistUpdate(PlaylistUpdateRunStateEvent::correlated(
                    "r1".into(),
                    1.into(),
                    PlaylistUpdateState::Success,
                )),
                context,
            );
            assert_eq!(state.for_input(41).status(), InputUpdateCardStatus::Queued);
            if progresses {
                state = reduce_event(
                    state,
                    EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent::input_completed(
                        "r2".into(),
                        2.into(),
                        41,
                        PlaylistUpdateState::Failure,
                        "M3U",
                        "r2 failed",
                    )),
                    context,
                );
            }
            state = reduce_event(
                state,
                EventMessage::PlaylistUpdate(PlaylistUpdateRunStateEvent::correlated(
                    "r2".into(),
                    2.into(),
                    PlaylistUpdateState::Failure,
                )),
                context,
            );
            state = state.reduce(PlaylistUpdateCardStatusAction::Accepted(PlaylistUpdateAcceptedScope {
                connection_context: context,
                run_id: "r1".into(),
                input_ids: vec![41],
                refresh_policy: InputRefreshPolicy::NORMAL,
            }));
            state = reduce_event(
                state,
                EventMessage::PlaylistUpdate(PlaylistUpdateRunStateEvent::correlated(
                    "r1".into(),
                    1.into(),
                    PlaylistUpdateState::Success,
                )),
                context,
            );
            assert_eq!(state.for_input(41).status(), InputUpdateCardStatus::Failed);
            assert_eq!(state.for_input(41).details(), if progresses { vec!["r2 failed"] } else { vec![] });
        }
    }

    #[test]
    fn playlist_update_bulk_stale_callback_cannot_finish_new_connection_submission() {
        let connection_a = connection(1);
        let connection_b = connection(2);
        let state = reduce_bulk_submission(
            PlaylistUpdateBulkSubmissionState::default(),
            PlaylistUpdateBulkSubmissionAction::Begin(connection_a),
        );
        let state = reduce_bulk_submission(state, PlaylistUpdateBulkSubmissionAction::ConnectionChanged);
        let state = reduce_bulk_submission(state, PlaylistUpdateBulkSubmissionAction::Begin(connection_b));

        assert_eq!(
            current_bulk_outcome(
                connection_a,
                connection_b,
                PlaylistUpdateBulkOutcome::Accepted { run_id: Some("stale-a".into()), input_ids: vec![7] },
            ),
            None
        );
        let state = reduce_bulk_submission(state, PlaylistUpdateBulkSubmissionAction::Finish(connection_a));
        assert!(state.is_submitting_for(connection_b));

        assert!(matches!(
            current_bulk_outcome(
                connection_b,
                connection_b,
                PlaylistUpdateBulkOutcome::Accepted { run_id: Some("current-b".into()), input_ids: vec![7] },
            ),
            Some(PlaylistUpdateBulkOutcome::Accepted { .. })
        ));
        let state = reduce_bulk_submission(state, PlaylistUpdateBulkSubmissionAction::Finish(connection_b));
        assert!(!state.is_submitting());
    }

    #[test]
    fn playlist_update_status_view_lifecycle_resets_before_events_from_the_new_socket() {
        let runtime_a = WebSocketConnectionContext::new(1, true);
        let runtime_a_disconnected = WebSocketConnectionContext::new(1, false);
        let runtime_b = WebSocketConnectionContext::new(2, true);
        let r1 = PlaylistUpdateRunId::from("r1");
        let r2 = PlaylistUpdateRunId::from("r2");
        let state = Rc::new(PlaylistUpdateCardStatuses::for_connection(runtime_a));
        let state = reduce_event(
            state,
            EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent::input_completed(
                r1.clone(),
                PlaylistUpdateRunOrder::from(42),
                7,
                PlaylistUpdateState::Success,
                "input-7",
                "r1 success",
            )),
            runtime_a,
        );
        let state = reduce_event(
            state,
            EventMessage::PlaylistUpdate(PlaylistUpdateRunStateEvent::correlated(
                r1.clone(),
                PlaylistUpdateRunOrder::from(42),
                PlaylistUpdateState::Success,
            )),
            runtime_a,
        );
        assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Success);

        let state = reduce_event(state, EventMessage::WebSocketStatus(false), runtime_a_disconnected);
        let state = reduce_event(state, EventMessage::WebSocketStatus(true), runtime_b);
        let state = state.reduce(PlaylistUpdateCardStatusAction::Accepted(PlaylistUpdateAcceptedScope {
            connection_context: runtime_b,
            run_id: r2.clone(),
            input_ids: vec![7],
            refresh_policy: InputRefreshPolicy::NORMAL,
        }));
        let state = reduce_event(
            state,
            EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent::for_run_input(
                r2.clone(),
                PlaylistUpdateRunOrder::from(1),
                7,
                "input-7",
                "r2 started",
            )),
            runtime_b,
        );
        assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Updating);
        let state = reduce_event(
            state,
            EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent::input_completed(
                r2.clone(),
                PlaylistUpdateRunOrder::from(1),
                7,
                PlaylistUpdateState::Failure,
                "input-7",
                "r2 failed",
            )),
            runtime_b,
        );
        let state = reduce_event(
            state,
            EventMessage::PlaylistUpdate(PlaylistUpdateRunStateEvent::correlated(
                r2,
                PlaylistUpdateRunOrder::from(1),
                PlaylistUpdateState::Failure,
            )),
            runtime_b,
        );
        let state = state.reduce(PlaylistUpdateCardStatusAction::Accepted(PlaylistUpdateAcceptedScope {
            connection_context: runtime_a,
            run_id: r1.clone(),
            input_ids: vec![7],
            refresh_policy: InputRefreshPolicy::NORMAL,
        }));
        let state = reduce_event(
            state,
            EventMessage::PlaylistUpdate(PlaylistUpdateRunStateEvent::correlated(
                r1,
                PlaylistUpdateRunOrder::from(42),
                PlaylistUpdateState::Success,
            )),
            runtime_a,
        );

        assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Failed);
        assert_eq!(state.for_input(7).details(), ["r2 started", "r2 failed"]);
    }

    #[test]
    fn library_update_progress_is_absent_without_an_event_and_visible_with_content() {
        assert_eq!(visible_library_progress(None), None);

        let progress = LibraryScanProgressEvent {
            summary: LibraryScanSummary {
                status: LibraryScanSummaryStatus::Success,
                message: "Scan completed".to_string(),
                result: None,
            },
        };
        assert_eq!(visible_library_progress(Some(&progress)), Some(("Scan completed", false)));

        let empty = LibraryScanProgressEvent {
            summary: LibraryScanSummary {
                status: LibraryScanSummaryStatus::Error,
                message: "  ".to_string(),
                result: None,
            },
        };
        assert_eq!(visible_library_progress(Some(&empty)), None);
    }

    #[test]
    fn playlist_update_bulk_and_status_translations_exist_in_every_frontend_locale() -> Result<(), serde_json::Error> {
        let locales = [
            ("en", include_str!("../../../../public/assets/i18n/en.json")),
            ("ru", include_str!("../../../../public/assets/i18n/ru.json")),
            ("ar", include_str!("../../../../public/assets/i18n/ar.json")),
        ];
        let pointers = [
            "/LABEL/UPDATE_ALL_INPUTS",
            "/LABEL/UPDATE_STATUS_READY",
            "/LABEL/UPDATE_STATUS_QUEUED",
            "/LABEL/UPDATE_STATUS_UPDATING",
            "/LABEL/UPDATE_STATUS_SUCCESS",
            "/LABEL/UPDATE_STATUS_PARTIAL",
            "/LABEL/UPDATE_STATUS_FAILED",
            "/MESSAGES/PLAYLIST_UPDATE/BULK_BUSY",
        ];

        for (locale, source) in locales {
            let translations: serde_json::Value = serde_json::from_str(source)?;
            for pointer in pointers {
                assert!(
                    translations
                        .pointer(pointer)
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|value| !value.trim().is_empty()),
                    "missing {pointer} translation for {locale}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn playlist_update_view_responsive_layout_keeps_cards_as_the_only_scroll_region() {
        let styles = include_str!("../../../../scss/app/components/playlist/_playlist_update_view.scss");

        for declaration in [
            "grid-template-columns: minmax(0, 1fr) 20rem;",
            "@media (max-width: size.$mobile-breakpoint)",
            "grid-template-columns: minmax(0, 1fr);",
            "border-inline-start: 1px solid var(--border-color);",
            "border-block-start: 1px solid var(--border-color);",
            "flex-flow: row wrap;",
            "min-height: 0;",
            "overflow-y: auto;",
            "overflow-x: hidden;",
        ] {
            assert!(styles.contains(declaration), "missing responsive layout declaration: {declaration}");
        }
        assert!(!styles.contains("overflow-x: auto"));
    }

    #[test]
    fn playlist_update_view_uses_the_anchored_select_without_legacy_action_menus() {
        let card_source = include_str!("input_update_card.rs");

        assert!(card_source.contains("<Select"));
        assert!(card_source.contains("tp__playlist-update-view__policy-select"));
        assert!(card_source.contains("icon={Some(\"ChevronDown\".to_string())}"));
        assert!(card_source.contains("popup_width={PopupMenuWidth::MatchAnchor}"));
        assert!(card_source.contains("popup_placement={PopupMenuPlacement::BottomEnd}"));
        assert!(!card_source.contains("<PopupMenu"));
        assert!(!card_source.contains("<DropDownIconButton"));
    }

    #[test]
    fn input_update_card_capability_notes_follow_select_and_precede_action_button() {
        let source = include_str!("input_update_card.rs");
        let actions = source.split_once("<section class=\"tp__playlist-update-view__input-actions\">").unwrap().1;
        let select = actions.find("<Select").unwrap();
        let notes = actions.find("<TitledCard title={translate.t(\"LABEL.NOTES\")}").unwrap();
        let start = notes + actions[notes..].find("<TextButton").unwrap();
        assert!(select < notes && notes < start);
        assert!(actions.contains("aria_describedby={policy_description_id.clone()}"));
        assert!(actions.contains("<p id={policy_description_id}"));
        assert!(actions.contains("{policy_description}"));
        assert!(source.contains("translate.t(model.capabilities.description_key(effective_policy))"));
        assert!(!source.contains("tp__playlist-update-view__policy-option"));

        let styles = include_str!("../../../../scss/app/components/playlist/_playlist_update_view.scss");
        assert!(!styles.contains("&__policy-option"));
        assert!(styles.contains("padding-block-start: max(var(--padding-mini), 0.5rem);"));
    }

    #[test]
    fn playlist_update_view_markup_exposes_card_and_control_accessibility_contracts() {
        let card_source = include_str!("input_update_card.rs");

        for marker in [
            "<article aria-labelledby={heading_id.clone()}>",
            "<details class=\"tp__playlist-update-view__update-details\">",
            "<summary>{translate(\"MESSAGES.PLAYLIST_UPDATE.UPDATE_DETAILS\")}</summary>",
            "role=\"list\"",
            "role=\"listitem\"",
            "role=\"group\"",
            "aria-labelledby={target_group_heading_id}",
            "aria_pressed={Some(all_selection.aria_pressed().to_string())}",
            "aria-live=\"polite\"",
            "aria_describedby={policy_description_id.clone()}",
            "aria_label={Some(start_aria_label)}",
        ] {
            assert!(card_source.contains(marker), "missing accessibility marker: {marker}");
        }
    }

    #[test]
    fn playlist_update_view_reuses_global_buttons_without_chip_or_primary_overrides() {
        let card_source = include_str!("input_update_card.rs");
        let view_source = include_str!("playlist_update_view.rs").split("#[cfg(test)]").next().unwrap();
        let styles = include_str!("../../../../scss/app/components/playlist/_playlist_update_view.scss");

        assert!(!card_source.contains("<button"));
        assert!(!card_source.contains("tp__chip"));
        assert!(!card_source.contains("class=\"primary\""));
        assert!(card_source.contains("aria_pressed={Some(selection.aria_pressed().to_string())}"));
        assert!(!view_source.contains("class=\"primary\""));
        assert!(!styles.contains("min-height: 2.25rem"));
        assert!(!styles.contains("font: inherit"));
        assert!(!styles.contains("var(--tag-active-background-color)"));
    }

    #[test]
    fn playlist_update_view_target_buttons_use_active_state_without_confirmation_icons() {
        let card_source = include_str!("input_update_card.rs");
        let targets = card_source
            .split_once("<section class=\"tp__playlist-update-view__input-targets\">")
            .unwrap()
            .1
            .split_once("</section>")
            .unwrap()
            .0;

        assert_eq!(targets.matches("<TextButton").count(), 2);
        assert!(!targets.contains("icon="));
        assert!(targets.contains("(all_selection == TargetSelectionState::All).then_some(\"active\")"));
        assert!(!targets.contains("(all_selection != TargetSelectionState::Empty).then_some(\"active\")"));
        assert!(targets.contains("(all_selection == TargetSelectionState::Mixed).then_some(\"mixed\")"));
        assert!(targets.contains("selected.then_some(\"active\")"));
        assert!(targets.contains("aria_pressed={Some(all_selection.aria_pressed().to_string())}"));
        assert!(targets.contains("aria_pressed={Some(selection.aria_pressed().to_string())}"));
    }

    #[test]
    fn playlist_update_view_target_selection_keeps_standard_button_typography() {
        let buttons = include_str!("../../../../scss/app/components/_text_button.scss");
        let radio_buttons = include_str!("../../../../scss/app/components/_radio_button_group.scss");
        let cards = include_str!("../../../../scss/app/components/playlist/_playlist_update_view.scss");
        assert!(buttons.split_once(".tp__text-button.active,").unwrap().0.contains("font-weight: bold;"));
        assert!(!buttons.split_once(".tp__text-button.active,").unwrap().1.contains("font-weight"));
        assert!(!radio_buttons.contains("font-weight"));
        let target_styles = cards.split_once("&__target-toggle {").unwrap().1.split_once("&__empty-targets").unwrap().0;
        assert!(!target_styles.contains("font-weight"));
        assert!(!target_styles.contains("font:"));
    }

    #[test]
    fn playlist_update_view_spacing_separates_groups_controls_and_last_update() {
        let styles = include_str!("../../../../scss/app/components/playlist/_playlist_update_view.scss");

        for selector in
            ["&__cards", "&__input-card article", "&__input-card-body", "&__input-content", "&__input-actions"]
        {
            let rule = styles.split_once(&format!("{selector} {{")).unwrap().1.split('}').next().unwrap();
            assert!(rule.contains("gap: max(var(--gap-large), 1rem);"), "missing group spacing for {selector}");
        }
        for selector in ["&__input-targets", "&__target-toggles", "&__policy-control", "&__input-card-footer"] {
            let rule = styles.split_once(&format!("{selector} {{")).unwrap().1.split('}').next().unwrap();
            assert!(rule.contains("gap: max(var(--gap-default), 0.5rem);"), "missing control spacing for {selector}");
        }
        let actions = styles.split_once("&__input-actions {").unwrap().1.split('}').next().unwrap();
        // The card already supplies its default padding at the outer right edge.
        assert!(actions
            .contains("padding-inline: var(--padding-large) calc(var(--padding-large) - var(--padding-default));"));
        let card_styles = include_str!("../../../../scss/app/components/_card.scss");
        assert!(card_styles.contains("padding: var(--padding-default);"));
        let mobile = styles.split_once("@media").unwrap().1;
        let mobile_actions = mobile.split_once("&__input-actions {").unwrap().1.split('}').next().unwrap();
        assert!(mobile_actions.contains("padding-inline: 0;"));
        assert!(styles.contains("padding: max(var(--padding-default), 0.75rem);"));
        let card_source = include_str!("input_update_card.rs");
        let policy_control = card_source
            .split_once("<div class=\"tp__playlist-update-view__policy-control\">")
            .unwrap()
            .1
            .split_once("</div>")
            .unwrap()
            .0;
        assert!(policy_control.contains("tp__playlist-update-view__action-label"));
        assert!(policy_control.contains("<Select"));
        assert!(!policy_control.contains("<TitledCard"));
        // Only disabled inputs render the existing grey control. Every enabled input uses
        // the same Select, including update-only and separate Library workflows.
        assert_eq!(policy_control.matches("<TextButton").count(), 1);
        assert!(policy_control.contains("model.capabilities.rebuilds_targets()"));
        assert!(policy_control.contains("disabled={true}"));
        assert!(policy_control.contains("onclick={Callback::noop()}"));
        assert!(policy_control.contains("<Select"));
    }

    #[test]
    fn playlist_update_view_builds_and_passes_the_block_17_read_model() {
        let source = include_str!("playlist_update_view.rs").split("#[cfg(test)]").next().unwrap();
        let card_source = include_str!("input_update_card.rs").split("#[cfg(test)]").next().unwrap();

        assert!(
            source.contains("build_input_update_run_views(&config.sources, &cards, &card_statuses, &update_status)")
        );
        assert!(source.contains("(model.input_id, Rc::new(model))"));
        assert!(source.contains("input_update_cards_by_id.get(&input_id)"));
        assert!(source.contains("view={Rc::new(view)}"));
        assert!(card_source.contains("pub view: Rc<InputUpdateRunView>"));
        assert!(card_source.contains("{card_status_badge(&view, |key| translate.t(key))}"));
        assert!(card_source.contains("last_update_view_model(view.last_update_at, props.now)"));
        assert!(!card_source.contains("props.runtime"));
    }

    #[test]
    fn playlist_update_view_keeps_progress_inside_details_without_a_permanent_logbox() {
        let source = include_str!("input_update_card.rs").split("#[cfg(test)]").next().unwrap();
        let styles = include_str!("../../../../scss/app/components/playlist/_playlist_update_view.scss");
        let disclosure = source.split_once("<details class=\"tp__playlist-update-view__update-details\">").unwrap().1;

        assert!(disclosure.contains("if !view.progress_details.is_empty()"));
        assert!(disclosure.contains("tp__playlist-update-view__progress-details"));
        assert!(!source.contains("tp__playlist-update-view__input-details"));
        assert!(!styles.contains("&__input-details"));
        assert!(!styles.contains("font-family: ui-monospace"));
    }
}
