use crate::{
    api::{
        model::{
            AppState, BoxedProviderStream, CleanupEvent, ConnectionManager, CustomVideoStreamType, ProviderHandle,
            StreamDetails, StreamError, TimedClientStream, TransportStreamBuffer,
        },
        panel_api::{can_provision_on_exhausted, find_input_by_provider_name, run_panel_api_provisioning_probe},
    },
    auth::Fingerprint,
    model::{ConfigInput, ProxyUserCredentials},
    utils::debug_if_enabled,
};
use axum::http::{header::USER_AGENT, HeaderMap};
use bytes::Bytes;
use futures::{task::AtomicWaker, Future, Stream, StreamExt};
use log::{debug, error, info};
use shared::{
    model::{StreamChannel, UserConnectionPermission, VirtualId},
    utils::sanitize_sensitive_info,
};
use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
    task::{Context, Poll},
};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

const INNER_STREAM: u8 = 0_u8;
const USER_EXHAUSTED_STREAM: u8 = 1_u8;
const PROVIDER_EXHAUSTED_STREAM: u8 = 2_u8;
const CHANNEL_UNAVAILABLE_STREAM: u8 = 3_u8;
const PROVISIONING_STREAM: u8 = 4_u8;
const GRACE_PENDING: u8 = 255_u8; // Grace period check hasn't completed yet

struct GraceProvisioningInfo {
    input: Arc<ConfigInput>,
    stop_signal: CancellationToken,
}

struct ActiveClientStreamState {
    inner: BoxedProviderStream,
    send_custom_stream_flag: Option<Arc<AtomicU8>>,
    provider_handle: Option<ProviderHandle>,
    preempt_cancelled: Option<Pin<Box<WaitForCancellationFutureOwned>>>,
    grace_task_handle: Option<tokio::task::JoinHandle<()>>,
    provisionable: bool,
    custom_video: (
        Option<TransportStreamBuffer>,
        Option<TransportStreamBuffer>,
        Option<TransportStreamBuffer>,
        Option<TransportStreamBuffer>,
    ),
    waker: Option<Arc<AtomicWaker>>,
    connection_manager: Arc<ConnectionManager>,
    fingerprint: Arc<Fingerprint>,
    provider_stopped: bool,
    user_stream_released: bool,
}

impl ActiveClientStreamState {
    fn release_user_stream(&mut self) {
        if self.user_stream_released {
            return;
        }
        self.user_stream_released = true;
        self.connection_manager.send_cleanup(CleanupEvent::ReleaseStream {
            addr: self.fingerprint.addr,
        });
    }

    fn stop_grace_task(&mut self) {
        if let Some(task) = self.grace_task_handle.take() {
            task.abort();
        }
    }

    fn clear_finished_grace_task(&mut self) {
        if self
            .grace_task_handle
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            self.grace_task_handle = None;
        }
    }

    fn stop_provider_stream_preempted(&mut self) {
        self.provider_stopped = true;
        self.preempt_cancelled = None;
        self.stop_grace_task();
        self.release_user_stream();

        if self.provider_handle.is_some() {
            let handle = self.provider_handle.take();
            if let Some(flag) = &self.send_custom_stream_flag {
                flag.store(INNER_STREAM, Ordering::Release);
            }

            if let Some(waker) = &self.waker {
                waker.wake();
            }

            let addr = self.fingerprint.addr;
            self.inner = futures::stream::empty::<Result<Bytes, StreamError>>().boxed();

            debug_if_enabled!(
                "Provider stream preempted for {}; stopping client stream",
                sanitize_sensitive_info(&addr.to_string())
            );
            self.connection_manager.send_cleanup(CleanupEvent::ReleaseProviderHandle { handle });
        }
    }

    fn stop_provider_stream(&mut self, unavailable: bool) {
        self.provider_stopped = true;
        self.preempt_cancelled = None;
        self.stop_grace_task();
        self.release_user_stream();

        if self.provider_handle.is_some() {
            let handle = self.provider_handle.take();

            if unavailable {
                if let Some(flag) = &self.send_custom_stream_flag {
                    let _ = flag.compare_exchange(
                        INNER_STREAM,
                        CHANNEL_UNAVAILABLE_STREAM,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    );
                }
            }

            if let Some(waker) = &self.waker {
                waker.wake();
            }

            let addr = self.fingerprint.addr;
            self.inner = futures::stream::empty::<Result<Bytes, StreamError>>().boxed();

            let video_type = if unavailable {
                CustomVideoStreamType::ChannelUnavailable
            } else {
                CustomVideoStreamType::UserConnectionsExhausted
            };
            debug_if_enabled!(
                "Provider stream stopped due to grace period or unavailable provider channel for {}",
                sanitize_sensitive_info(&addr.to_string())
            );
            self.connection_manager.send_cleanup(CleanupEvent::UpdateDetailAndReleaseProvider {
                addr,
                video_type,
                handle,
            });
        }
    }

    fn poll_next_base(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, StreamError>>> {
        self.clear_finished_grace_task();
        if let Some(waker) = &self.waker {
            waker.register(cx.waker());
        }

        if let Some(fut) = self.preempt_cancelled.as_mut() {
            if fut.as_mut().poll(cx).is_ready() {
                self.stop_provider_stream_preempted();
                return Poll::Ready(None);
            }
        }

        let flag = match &self.send_custom_stream_flag {
            Some(flag) => flag.load(Ordering::Acquire),
            None => INNER_STREAM,
        };

        // When hold_stream_active is true and flag is GRACE_PENDING, we wait for the grace period
        // check to complete before starting to stream. The grace period task will update the flag.
        if flag == GRACE_PENDING {
            // Still waiting for grace period check to complete
            // The grace period task will wake us when done
            return Poll::Pending;
        }

        if flag == INNER_STREAM {
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Err(e))) => {
                    error!("Inner stream error: {e:?}");
                    self.stop_provider_stream(true);
                    return Poll::Ready(Some(Err(e)));
                }
                Poll::Ready(None) => {
                    self.stop_provider_stream(true);
                    return Poll::Ready(None);
                }
                healthy => return healthy,
            }
        }
        if !self.provider_stopped {
            self.stop_provider_stream(false);
        }
        Poll::Ready(None) // Fallback for subclasses to handle other flags
    }
}

pub(in crate::api) struct ActiveClientStream {
    state: ActiveClientStreamState,
}

impl Stream for ActiveClientStream {
    type Item = Result<Bytes, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let res = self.state.poll_next_base(cx);
        if !matches!(res, Poll::Ready(None)) {
            return res;
        }

        let flag = match &self.state.send_custom_stream_flag {
            Some(flag) => flag.load(Ordering::Acquire),
            None => INNER_STREAM,
        };

        // Only stay pending if provisionable AND the grace task is still running.
        // Once the grace task completes (handle is None), the decision is final —
        // if the stream ended, we must close to avoid hanging the client forever.
        let grace_active = self.state.provisionable && self.state.grace_task_handle.is_some();

        if flag == INNER_STREAM {
            return if grace_active { Poll::Pending } else { Poll::Ready(None) };
        }

        let buffer_opt = match flag {
            USER_EXHAUSTED_STREAM => self.state.custom_video.0.as_mut(),
            PROVIDER_EXHAUSTED_STREAM => self.state.custom_video.1.as_mut(),
            CHANNEL_UNAVAILABLE_STREAM => self.state.custom_video.2.as_mut(),
            PROVISIONING_STREAM => self.state.custom_video.3.as_mut(),
            _ => None,
        };

        if let Some(buffer) = buffer_opt {
            buffer.register_waker(cx.waker());
            match buffer.next_chunk() {
                Some(chunk) => Poll::Ready(Some(Ok(chunk))),
                None => if grace_active { Poll::Pending } else { Poll::Ready(None) },
            }
        } else {
            Pin::new(&mut self.state.inner).poll_next(cx)
        }
    }
}

impl Drop for ActiveClientStream {
    fn drop(&mut self) {
        self.state.stop_grace_task();
        let addr = self.state.fingerprint.addr;
        let handle = self.state.provider_handle.take();
        if self.state.user_stream_released {
            self.state.connection_manager.send_cleanup(CleanupEvent::ReleaseProviderHandle { handle });
        } else {
            self.state.user_stream_released = true;
            self.state.connection_manager.send_cleanup(CleanupEvent::ReleaseStreamAndProviderHandle { addr, handle });
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) async fn create_active_client_stream(
    mut stream_details: StreamDetails,
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    connection_permission: UserConnectionPermission,
    fingerprint: &Fingerprint,
    stream_channel: StreamChannel,
    session_token: Option<&str>,
    req_headers: &HeaderMap,
) -> BoxedProviderStream {
    if connection_permission == UserConnectionPermission::Exhausted {
        error!("Something is wrong this should not happen");
    }
    let grant_user_grace_period = connection_permission == UserConnectionPermission::GracePeriod;
    let username = user.username.as_str();
    let provider_name = stream_details.provider_name.as_ref().map_or_else(String::new, ToString::to_string);

    let user_agent = req_headers.get(USER_AGENT).map(|h| String::from_utf8_lossy(h.as_bytes())).unwrap_or_default();

    let virtual_id = stream_channel.virtual_id;
    let is_shared_source_stream = stream_channel.shared && stream_details.request_url.is_some();
    app_state
        .connection_manager
        .update_connection(
            username,
            user.max_connections,
            fingerprint,
            &provider_name,
            stream_channel,
            user_agent,
            session_token,
        )
        .await;
    if let Some((_, _, _m_, Some(cvt))) = stream_details.stream_info.as_ref() {
        app_state.connection_manager.update_stream_detail(&fingerprint.addr, *cvt).await;
    }

    // Shared broadcaster source (first subscriber path): feed provider bytes directly.
    // Grace/custom handling is not needed here because this stream is only the fan-out source.
    if is_shared_source_stream {
        let stream = match stream_details.stream.take() {
            None => {
                let provider_handle = stream_details.provider_handle.take();
                app_state.connection_manager.release_provider_handle(provider_handle).await;
                futures::stream::empty::<Result<Bytes, StreamError>>().boxed()
            }
            Some(stream) => {
                let config = app_state.app_config.config.load();
                match config.sleep_timer_mins {
                    None => stream,
                    Some(mins) => {
                        let secs = u32::try_from((u64::from(mins) * 60).min(u64::from(u32::MAX))).unwrap_or(0);
                        if secs > 0 {
                            TimedClientStream::new(app_state, stream, secs, fingerprint.addr, virtual_id).boxed()
                        } else {
                            stream
                        }
                    }
                }
            }
        };
        return stream;
    }

    let provisioning_info = resolve_grace_period_provisioning(app_state, &stream_details);
    let has_provisioning = provisioning_info.is_some();
    let hold_stream = stream_details.grace_period.hold_stream;

    let waker = Arc::new(AtomicWaker::new());
    let (grace_stop_flag, grace_task_handle) = if grant_user_grace_period || stream_details.provider_grace_active {
        stream_grace_period(
            app_state,
            &stream_details,
            grant_user_grace_period,
            user,
            fingerprint,
            virtual_id,
            provisioning_info,
            Some(Arc::clone(&waker)),
            hold_stream,
        )
    } else {
        stream_grace_period(
            app_state,
            &stream_details,
            grant_user_grace_period,
            user,
            fingerprint,
            virtual_id,
            provisioning_info,
            None,
            hold_stream,
        )
    };

    let cfg = &app_state.app_config;
    let custom_response = cfg.custom_stream_response.load();
    let custom_video = custom_response.as_ref().map_or((None, None, None, None), |c| {
        (
            c.user_connections_exhausted.clone(),
            c.provider_connections_exhausted.clone(),
            c.channel_unavailable.clone(),
            c.panel_api_provisioning.clone(),
        )
    });

    let stream = match stream_details.stream.take() {
        None => {
            let provider_handle = stream_details.provider_handle.take();
            app_state.connection_manager.release_provider_handle(provider_handle).await;
            futures::stream::empty::<Result<Bytes, StreamError>>().boxed()
        }
        Some(stream) => {
            let config = app_state.app_config.config.load();
            match config.sleep_timer_mins {
                None => stream,
                Some(mins) => {
                    let secs = u32::try_from((u64::from(mins) * 60).min(u64::from(u32::MAX))).unwrap_or(0);
                    if secs > 0 {
                        TimedClientStream::new(app_state, stream, secs, fingerprint.addr, virtual_id).boxed()
                    } else {
                        stream
                    }
                }
            }
        }
    };

    let preempt_cancelled = stream_details.provider_handle
        .as_ref()
        .and_then(|h| h.cancel_token.as_ref())
        .map(|token| Box::pin(token.clone().cancelled_owned()));

    let state = ActiveClientStreamState {
        inner: stream,
        preempt_cancelled,
        grace_task_handle,
        provider_handle: stream_details.provider_handle,
        send_custom_stream_flag: grace_stop_flag,
        custom_video,
        provisionable: has_provisioning,
        waker: Some(waker),
        connection_manager: Arc::clone(&app_state.connection_manager),
        fingerprint: Arc::new(fingerprint.clone()),
        provider_stopped: false,
        user_stream_released: false,
    };

    ActiveClientStream { state }.boxed()
}

fn resolve_grace_period_provisioning(
    app_state: &Arc<AppState>,
    stream_details: &StreamDetails,
) -> Option<GraceProvisioningInfo> {
    if stream_details.disable_provider_grace || !stream_details.provider_grace_active {
        return None;
    }
    let provider_name = stream_details.provider_name.as_deref();
    let input = provider_name.and_then(|name| find_input_by_provider_name(app_state.as_ref(), name))?;
    if !can_provision_on_exhausted(app_state, &input) {
        return None;
    }

    let stop_signal = CancellationToken::new();
    Some(GraceProvisioningInfo { input, stop_signal })
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn stream_grace_period(
    app_state: &Arc<AppState>,
    stream_details: &StreamDetails,
    user_grace_period: bool,
    user: &ProxyUserCredentials,
    fingerprint: &Fingerprint,
    virtual_id: VirtualId,
    provisioning_info: Option<GraceProvisioningInfo>,
    waker: Option<Arc<AtomicWaker>>,
    hold_stream: bool,
) -> (Option<Arc<AtomicU8>>, Option<tokio::task::JoinHandle<()>>) {
    let active_users = Arc::clone(&app_state.active_users);
    let active_provider = Arc::clone(&app_state.active_provider);
    let connection_manager = Arc::clone(&app_state.connection_manager);

    let provider_grace_check = if stream_details.provider_grace_active
        && stream_details.provider_name.is_some()
        && !stream_details.disable_provider_grace
    {
        stream_details.provider_name.clone()
    } else {
        None
    };

    let user_max_connections = user.max_connections;
    let user_grace_check = if user_grace_period && user_max_connections > 0 {
        let user_name = user.username.clone();
        Some((user_name, user_max_connections))
    } else {
        None
    };

    debug!("hold stream {hold_stream}");

    if provider_grace_check.is_some() || user_grace_check.is_some() {
        let stream_strategy_flag = Arc::new(AtomicU8::new(if hold_stream { GRACE_PENDING } else { INNER_STREAM }));
        let stream_strategy_flag_copy = Arc::clone(&stream_strategy_flag);
        let grace_period_millis = stream_details.grace_period.period_millis;

        let user_manager = Arc::clone(&active_users);
        let provider_manager = Arc::clone(&active_provider);
        let connection_manager = Arc::clone(&connection_manager);
        let reconnect_flag = stream_details.reconnect_flag.clone();
        let fingerprint = fingerprint.clone();
        let app_state = Arc::clone(app_state);
        let grace_task_handle = tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(grace_period_millis)).await;

            let mut updated = false;
            if let Some((username, max_connections)) = user_grace_check {
                let active_connections = user_manager.user_connections(&username).await;
                if active_connections > max_connections {
                    stream_strategy_flag_copy.store(USER_EXHAUSTED_STREAM, Ordering::Release);
                    connection_manager
                        .update_stream_detail(&fingerprint.addr, CustomVideoStreamType::UserConnectionsExhausted)
                        .await;
                    // Release the shared stream subscription to stop the subscriber loop
                    connection_manager.shared_stream_manager.release_connection(&fingerprint.addr, true).await;
                    info!("User connections exhausted for active clients: {username}");
                    updated = true;
                }
            }

            if !updated {
                if let Some(provider_name) = provider_grace_check {
                    if provider_manager.is_over_limit(&provider_name).await {
                        if let Some(provisioning_info) = provisioning_info {
                            stream_strategy_flag_copy.store(PROVISIONING_STREAM, Ordering::Release);
                            connection_manager
                                .update_stream_detail(&fingerprint.addr, CustomVideoStreamType::Provisioning)
                                .await;
                            debug_if_enabled!(
                                "Provider grace period exhausted; provisioning for active clients: {provider_name}"
                            );
                            let app_state = Arc::clone(&app_state);
                            let input = (*provisioning_info.input).clone();
                            let stop_signal = provisioning_info.stop_signal;
                            let addr = fingerprint.addr;
                            tokio::spawn(async move {
                                if let Err(err) =
                                    run_panel_api_provisioning_probe(app_state, input, stop_signal, addr, virtual_id)
                                        .await
                                {
                                    error!("Error running Probe: {err:?}");
                                }
                            });
                        } else {
                            stream_strategy_flag_copy.store(PROVIDER_EXHAUSTED_STREAM, Ordering::Release);
                            connection_manager
                                .update_stream_detail(
                                    &fingerprint.addr,
                                    CustomVideoStreamType::ProviderConnectionsExhausted,
                                )
                                .await;
                            // Release the shared stream subscription to stop the subscriber loop
                            connection_manager.shared_stream_manager.release_connection(&fingerprint.addr, true).await;
                            info!("Provider connections exhausted for active clients: {provider_name}");
                        }
                        updated = true;
                    }
                }
            }

            if !updated {
                stream_strategy_flag_copy.store(INNER_STREAM, Ordering::Release);
            }

            if updated {
                if let Some(flag) = reconnect_flag {
                    flag.cancel();
                }
            }

            if let Some(w) = waker.as_ref() {
                w.wake();
            }
        });
        return (Some(stream_strategy_flag), Some(grace_task_handle));
    }
    (None, None)
}
