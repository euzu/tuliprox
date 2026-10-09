use super::{
    create_deferred_provider_open_future, create_timed_stream_context, resolve_grace_period_provisioning,
    should_use_direct_body_idle_timeout, stream_grace_period, wrap_timed_client_stream_if_needed, ActiveClientStream,
    ActiveClientStreamParams, CustomVideoBuffers, DeferredProviderOpenState, DirectBodyIdleTimeout,
    GraceProvisioningInfo, StreamAdmissionError, StreamMode, TimedStreamContext,
};
use crate::{
    api::model::{
        connection_manager::PROVIDER_END_NOT_SET, AppState, BoxedProviderStream, ConnectionManager, EventManager,
        MeteringStream, StreamDetails, StreamMeterHandle,
    },
    auth::Fingerprint,
    model::ProxyUserCredentials,
};
use axum::http::header::USER_AGENT;
use futures::{task::AtomicWaker, StreamExt};
use log::error;
use shared::{
    model::{UserConnectionPermission, VirtualId},
    utils::Internable,
};
use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU8},
        Arc,
    },
};
use tokio::sync::Notify;
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};
use tuliprox_session::{stream_options::StreamResponseMode, ConnectionRejectionReason};

pub(super) struct GracePeriodParams {
    pub(super) app_state: Arc<AppState>,
    pub(super) stream_details: StreamDetails,
    pub(super) user_grace_period: bool,
    pub(super) user: ProxyUserCredentials,
    pub(super) fingerprint: Fingerprint,
    pub(super) virtual_id: VirtualId,
    pub(super) session_token: Option<String>,
    pub(super) provisioning_info: Option<GraceProvisioningInfo>,
    pub(super) waker: Option<Arc<AtomicWaker>>,
    pub(super) hold_stream: bool,
    pub(super) capacity_notify: Arc<Notify>,
    pub(super) pending_provider_version: Option<u64>,
    // The `transition_version` of the session if it is in `GraceActive` lifecycle.
    // Used by the grace task to confirm the session is still in `GraceActive` before resolving.
    pub(super) grace_active_version: Option<u64>,
    // Set when the stream was admitted via a user-grace strategy.
    // On user-grace failure, remaining strategies are evaluated before final deny.
    pub(super) grace_resolution_context: Option<crate::api::model::GraceResolutionContext>,
    // The `ConnectionKind` from the original admission decision.
    // Preserved in `GraceResolutionContext.kind` and also passed directly here
    // so the grace task can use the runtime value when evaluating remaining strategies.
    pub(super) grace_kind: Option<crate::api::model::ConnectionKind>,
    // Whether the session is `socket_bound`. Used to construct the correct
    // `EvictionReentryGuard`.
    pub(super) socket_bound: bool,
    pub(super) shared_subscriber_id: Option<tuliprox_core::model::SharedSubscriberId>,
}

#[allow(clippy::struct_excessive_bools)]
pub(super) struct ActiveClientStreamState {
    pub(super) response_mode: StreamResponseMode,
    pub(super) inner: Option<BoxedProviderStream>,
    pub(super) send_custom_stream_flag: Option<Arc<AtomicU8>>,
    pub(super) provider_handle: Option<tuliprox_session::ManagedProviderHandle>,
    pub(super) deferred_provider_open: Option<DeferredProviderOpenState>,
    pub(super) timed_stream_context: Option<TimedStreamContext>,
    pub(super) preempt_cancelled: Option<Pin<Box<WaitForCancellationFutureOwned>>>,
    pub(super) grace_task_handle: Option<tokio::task::JoinHandle<()>>,
    /// Cancels a panel-api provisioning probe spawned by the grace task; aborting the
    /// grace task alone would leave the probe running to its own timeout.
    pub(super) provisioning_stop_signal: Option<CancellationToken>,
    pub(super) provisionable: bool,
    pub(super) custom_video: CustomVideoBuffers,
    pub(super) meter: Option<Arc<StreamMeterHandle>>,
    pub(super) event_manager: Arc<EventManager>,
    pub(super) waker: Option<Arc<AtomicWaker>>,
    pub(super) connection_manager: Arc<ConnectionManager>,
    pub(super) fingerprint: Arc<Fingerprint>,
    pub(super) stream_uid: Option<u32>,
    pub(super) provider_stopped: bool,
    pub(super) user_stream_released: bool,
    /// Mirrors `user_stream_released` for the provider handle to guard against double-release
    /// when preemption and Drop race.
    pub(super) provider_handle_released: bool,
    pub(super) custom_video_timeout_secs: u32,
    pub(super) custom_video_timeout_mode: Option<StreamMode>,
    pub(super) custom_video_timeout_sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    pub(super) direct_body_idle_timeout: DirectBodyIdleTimeout,
    /// Set once when the provider stream ends. Read once in Drop. Never queried during streaming.
    /// Separate from `send_custom_stream_flag` (`StreamMode`). Uses `PROVIDER_END_*` constants.
    pub(super) provider_end_reason: AtomicU8,
    pub(super) provider_error_class: Option<&'static str>,
    pub(super) provider_http_status: Option<u16>,
    /// Count of successful provider reconnections during this session (grace period / deferred open).
    pub(super) provider_reconnect_count: AtomicU8,
    /// Playback lease owner (session token) whose provider slot is confirmed once real
    /// media bytes reach the client. `None` when this stream has no provider lease.
    pub(super) lease_owner: Option<Arc<str>>,
    pub(super) media_started: Option<Arc<AtomicBool>>,
    /// Guards against emitting the confirmation more than once per stream.
    pub(super) lease_confirmed: bool,
    pub(super) lease_request_id: Option<tuliprox_core::model::PlaybackRequestId>,
    /// Guaranteed final cleanup for the registered request claim. The body owns this and
    /// finishes it exactly once with the real provider outcome, so a full cleanup queue can
    /// never leak the user request or provider lease.
    pub(super) request_cleanup: Option<tuliprox_session::OwnedRequestCleanup>,
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn create_active_client_stream(
    request: ActiveClientStreamParams<'_>,
) -> Result<BoxedProviderStream, StreamAdmissionError> {
    let ActiveClientStreamParams {
        mut stream_details,
        app_state,
        user,
        connection_permission,
        connection_kind,
        fingerprint,
        stream_channel,
        socket_bound,
        session_token,
        req_headers,
        meter_uid,
        meter_stream,
    } = request;
    if connection_permission == UserConnectionPermission::Exhausted {
        error!("Something is wrong this should not happen");
    }
    let response_mode = stream_details.response_mode;
    let grant_user_grace_period = connection_permission == UserConnectionPermission::GracePeriod;
    let username = user.username.as_str();
    let provider_name = stream_details.provider_name.clone().unwrap_or_else(|| "unknown".intern());

    let user_agent = req_headers.get(USER_AGENT).map(|h| String::from_utf8_lossy(h.as_bytes())).unwrap_or_default();

    let virtual_id = stream_channel.virtual_id;
    let is_shared_source_stream = stream_channel.shared && stream_details.stream.is_some();
    let direct_body_idle_timeout = if should_use_direct_body_idle_timeout(&stream_channel) {
        DirectBodyIdleTimeout::enabled()
    } else {
        DirectBodyIdleTimeout::disabled()
    };
    // A shared response already owns a guaranteed ReleaseSharedSubscriber permit
    // before this registration starts. That terminal event also removes this exact user request, so reserving
    // another permit from the same bounded queue would create a circular admission
    // dependency under saturation.
    let shared_cleanup_capability = stream_details.shared_subscriber_id.take();
    let shared_subscriber_id =
        shared_cleanup_capability.as_ref().map(tuliprox_session::SharedCleanupCapability::subscriber_id);
    let request_uid = shared_subscriber_id.map_or_else(
        || app_state.connection_manager.next_stream_uid(),
        tuliprox_core::model::SharedSubscriberId::stream_uid,
    );
    let provider_request_id = stream_details
        .provider_handle
        .as_ref()
        .and_then(|managed| managed.handle())
        .and_then(|handle| handle.playback_request_id);
    let connection = crate::api::model::ConnectionParams {
        meter_uid,
        username,
        max_connections: user.max_connections,
        soft_connections: user.soft_connections,
        connection_kind,
        priority: user.priority,
        soft_priority: user.soft_priority,
        fingerprint,
        provider: provider_name,
        stream_channel: &stream_channel,
        user_agent,
        session_token,
    };
    let mut registered_request = if let Some(capability) = shared_cleanup_capability.filter(|_| stream_channel.shared) {
        app_state
            .connection_manager
            .update_connection_with_uid_using_shared_cleanup(
                connection,
                tuliprox_session::ConnectionHistoryMode::EmitConnect,
                capability,
                provider_request_id,
            )
            .await
    } else {
        app_state
            .connection_manager
            .update_connection_with_uid_and_session_registration(
                connection,
                tuliprox_session::ConnectionHistoryMode::EmitConnect,
                request_uid,
                provider_request_id,
                stream_details.session_registration.as_ref(),
            )
            .await
    };
    let request_uid = registered_request.request_uid;
    let display_stream_uid = registered_request.display_uid();
    let stream_uid = Some(request_uid);
    if registered_request.display_stream.is_none() {
        // Admission was rejected (closed cleanup receiver, bounded timeout, or failed
        // registration). No provider bytes may be delivered; the acquired provider
        // handle and any open provider stream are released when `request` (and its
        // `stream_details`) is dropped on return.
        let reason = registered_request.rejection_reason().unwrap_or(ConnectionRejectionReason::RegistrationFailed);
        error!("Stream admission rejected for user {username}: {reason:?}; dropping provider handle");
        return Err(reason.into());
    }
    if let Some((_, _, _m_, Some(cvt))) = stream_details.stream_info.as_ref() {
        app_state.connection_manager.update_stream_detail_by_uid(request_uid, *cvt).await;
    }

    let meter = if meter_stream && meter_uid != 0 {
        let meter = Arc::new(StreamMeterHandle::new(meter_uid, Arc::downgrade(&app_state.event_manager)));
        app_state.event_manager.register_meter(Arc::clone(&meter)).await;
        Some(meter)
    } else {
        None
    };

    // Shared broadcaster source (first subscriber path): feed provider bytes directly.
    // Grace/custom handling is not needed here because this stream is only the fan-out source.
    if is_shared_source_stream {
        if let Some(stream) = stream_details.stream.take() {
            let stream = if let Some(meter) = &meter {
                MeteringStream::new(stream, Arc::clone(meter), Arc::clone(&app_state.event_manager)).boxed()
            } else {
                stream
            };
            // The shared source owns the claim from here; disarm the registration rollback.
            registered_request.commit();
            return Ok(stream);
        }
    }

    let provisioning_info = resolve_grace_period_provisioning(app_state, &stream_details);
    let has_provisioning = provisioning_info.is_some();
    let provisioning_stop_signal = provisioning_info.as_ref().map(|info| info.stop_signal.clone());
    let hold_stream = stream_details.grace_period.hold_stream;
    let capacity_notify = app_state.connection_manager.capacity_notified();
    let pending_provider_version = if hold_stream {
        if let Some(token) = session_token {
            app_state.active_users.pending_provider_version(&user.username, token).await
        } else {
            None
        }
    } else {
        None
    };
    // `GraceActive` version: populated when the session is in `GraceActive` lifecycle (GraceMode::Instant).
    let grace_active_version = if let Some(token) = session_token {
        app_state.active_users.grace_active_version(&user.username, token).await
    } else {
        None
    };

    let waker = Arc::new(AtomicWaker::new());
    let owned_session_token: Option<String> = session_token.map(str::to_string);
    // A provider-backed playback confirms its lease owner once media flows. Local
    // media and streams without a provider handle carry no lease to confirm.
    let lease_owner: Option<Arc<str>> = session_token
        .filter(|_| stream_details.provider_handle.is_some() || stream_details.has_deferred_provider_open())
        .map(Arc::<str>::from);
    let media_started = if let Some(owner) = lease_owner.as_deref() {
        app_state.active_users.media_started_flag(&user.username, owner).await
    } else {
        None
    };
    let owned_grace_ctx = stream_details.grace_resolution_context.clone();
    let lease_request_id = stream_details
        .provider_handle
        .as_ref()
        .and_then(|managed| managed.handle())
        .and_then(|handle| handle.playback_request_id);
    // Compute deferred-open metadata before taking the handle: the grace snapshot only
    // needs the provider identity, while the body owns the actual release.
    let has_deferred_open = stream_details.has_deferred_provider_open();
    let deferred_provider_open =
        create_deferred_provider_open_future(app_state, &stream_details, fingerprint, &stream_channel, req_headers);
    let timed_stream_context = deferred_provider_open
        .as_ref()
        .and_then(|_| create_timed_stream_context(app_state, VirtualId::new(virtual_id)));
    // The body owns the provider slot from here on; the managed owner releases it
    // synchronously on drop, never through a lossy cleanup message.
    let mut provider_handle_preserved = stream_details.provider_handle.take();
    let stream_taken = stream_details.stream.take();
    let grace_waker =
        if grant_user_grace_period || stream_details.provider_grace_active { Some(Arc::clone(&waker)) } else { None };
    let (grace_stop_flag, grace_task_handle) = stream_grace_period(GracePeriodParams {
        app_state: Arc::clone(app_state),
        stream_details,
        user_grace_period: grant_user_grace_period,
        user: user.clone(),
        fingerprint: fingerprint.clone(),
        virtual_id: VirtualId::new(virtual_id),
        session_token: owned_session_token,
        provisioning_info,
        waker: grace_waker,
        hold_stream,
        capacity_notify,
        pending_provider_version,
        grace_active_version,
        grace_resolution_context: owned_grace_ctx,
        grace_kind: Some(connection_kind),
        socket_bound,
        shared_subscriber_id,
    });

    let cfg = &app_state.app_config;
    let custom_response = cfg.custom_stream_response.load();
    let custom_video_timeout_secs = cfg.config.load().custom_stream_response_timeout_secs;
    let custom_video = custom_response.as_ref().filter(|_| response_mode == StreamResponseMode::Stream).map_or(
        CustomVideoBuffers {
            user_exhausted: None,
            provider_exhausted: None,
            unavailable: None,
            provisioning: None,
            low_priority_preempted: None,
        },
        |c| CustomVideoBuffers {
            user_exhausted: c.user_connections_exhausted.clone(),
            provider_exhausted: c.provider_connections_exhausted.clone(),
            unavailable: c.channel_unavailable.clone(),
            provisioning: c.panel_api_provisioning.clone(),
            low_priority_preempted: c.low_priority_preempted.clone(),
        },
    );

    let stream: Option<BoxedProviderStream> = match stream_taken {
        None => {
            if !has_deferred_open {
                drop(provider_handle_preserved.take());
            }
            None
        }
        Some(stream) => {
            let stream = if let Some(meter) = &meter {
                MeteringStream::new(stream, Arc::clone(meter), Arc::clone(&app_state.event_manager)).boxed()
            } else {
                stream
            };
            Some(wrap_timed_client_stream_if_needed(
                app_state,
                stream,
                fingerprint.addr,
                VirtualId::new(virtual_id),
                display_stream_uid,
            ))
        }
    };

    let preempt_cancelled = provider_handle_preserved
        .as_ref()
        .and_then(|managed| managed.handle())
        .and_then(|h| h.cancel_token.as_ref())
        .map(|token| Box::pin(token.clone().cancelled_owned()));

    let mut send_custom_stream_flag = grace_stop_flag;
    if send_custom_stream_flag.is_none() && preempt_cancelled.is_some() && custom_video.low_priority_preempted.is_some()
    {
        send_custom_stream_flag = Some(Arc::new(AtomicU8::new(StreamMode::Inner as u8)));
    }

    let state = ActiveClientStreamState {
        response_mode,
        inner: stream,
        deferred_provider_open,
        timed_stream_context,
        preempt_cancelled,
        grace_task_handle,
        provisioning_stop_signal,
        provider_handle: provider_handle_preserved,
        send_custom_stream_flag,
        provisionable: has_provisioning,
        custom_video,
        meter,
        event_manager: Arc::clone(&app_state.event_manager),
        waker: Some(waker),
        connection_manager: Arc::clone(&app_state.connection_manager),
        fingerprint: Arc::new(fingerprint.clone()),
        stream_uid,
        provider_stopped: false,
        user_stream_released: false,
        provider_handle_released: false,
        custom_video_timeout_secs,
        custom_video_timeout_mode: None,
        custom_video_timeout_sleep: None,
        direct_body_idle_timeout,
        provider_end_reason: AtomicU8::new(PROVIDER_END_NOT_SET),
        provider_error_class: None,
        provider_http_status: None,
        provider_reconnect_count: AtomicU8::new(0),
        lease_owner,
        media_started,
        lease_confirmed: false,
        lease_request_id,
        request_cleanup: registered_request.into_body_cleanup(),
    };

    Ok(ActiveClientStream { state }.boxed())
}
