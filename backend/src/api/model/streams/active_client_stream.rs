use crate::{
    api::{
        model::{
            AppState, BoxedProviderStream, ConnectionManager, CustomVideoStreamType, ProviderHandle, StreamDetails,
            StreamError, TimedClientStream, TransportStreamBuffer,
        },
        panel_api::{can_provision_on_exhausted, find_input_by_provider_name, run_panel_api_provisioning_probe},
    },
    auth::Fingerprint,
    model::{ConfigInput, ProxyUserCredentials},
    utils::debug_if_enabled,
};
use axum::http::{header::USER_AGENT, HeaderMap};
use bytes::Bytes;
use futures::{task::AtomicWaker, Stream, StreamExt};
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
use tokio_util::sync::CancellationToken;

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
    preempt_watch_task: Option<tokio::task::JoinHandle<()>>,
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
}

impl ActiveClientStreamState {
    fn spawn_preempt_watch_task(
        provider_handle: Option<&ProviderHandle>,
        waker: &Arc<AtomicWaker>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let cancel_token = provider_handle.and_then(|handle| handle.cancel_token.as_ref()).cloned()?;
        let wake = Arc::clone(waker);
        Some(tokio::spawn(async move {
            cancel_token.cancelled().await;
            wake.wake();
        }))
    }

    fn stop_preempt_watch_task(&mut self) {
        if let Some(task) = self.preempt_watch_task.take() {
            task.abort();
        }
    }

    fn is_preempted(&self) -> bool {
        self.provider_handle
            .as_ref()
            .and_then(|handle| handle.cancel_token.as_ref())
            .is_some_and(CancellationToken::is_cancelled)
    }

    fn stop_provider_stream_preempted(&mut self) {
        self.provider_stopped = true;
        self.stop_preempt_watch_task();

        if self.provider_handle.is_some() {
            let mgr = Arc::clone(&self.connection_manager);
            let handle = self.provider_handle.take();

            if let Some(waker) = &self.waker {
                waker.wake();
            }

            let addr = self.fingerprint.addr;
            self.inner = futures::stream::empty::<Result<Bytes, StreamError>>().boxed();

            tokio::spawn(async move {
                debug_if_enabled!(
                    "Provider stream preempted for {}; stopping client stream",
                    sanitize_sensitive_info(&addr.to_string())
                );
                mgr.release_provider_handle(handle).await;
            });
        }
    }

    fn stop_provider_stream(&mut self, unavailable: bool) {
        self.provider_stopped = true;
        self.stop_preempt_watch_task();

        if self.provider_handle.is_some() {
            let mgr = Arc::clone(&self.connection_manager);
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

            let con_man = Arc::clone(&self.connection_manager);
            let addr = self.fingerprint.addr;
            self.inner = futures::stream::empty::<Result<Bytes, StreamError>>().boxed();

            tokio::spawn(async move {
                let stream_type = if unavailable {
                    CustomVideoStreamType::ChannelUnavailable
                } else {
                    CustomVideoStreamType::UserConnectionsExhausted
                };
                con_man.update_stream_detail(&addr, stream_type).await;
                debug_if_enabled!(
                    "Provider stream stopped due to grace period or unavailable provider channel for {}",
                    sanitize_sensitive_info(&addr.to_string())
                );
                mgr.release_provider_handle(handle).await;
            });
        }
    }

    fn poll_next_base(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, StreamError>>> {
        if let Some(waker) = &self.waker {
            waker.register(cx.waker());
        }

        if self.is_preempted() {
            self.stop_provider_stream_preempted();
            return Poll::Ready(None);
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

        if flag == INNER_STREAM {
            return Poll::Ready(None);
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
            if let Some(chunk) = buffer.next_chunk() {
                Poll::Ready(Some(Ok(chunk)))
            } else {
                // log::warn!("Custom video buffer empty/finished for {} (flag: {})", self.state.fingerprint.addr, flag);
                Poll::Ready(None)
            }
        } else {
            // log::warn!("No custom video buffer configured for flag {} for {}", flag, self.state.fingerprint.addr);
            // Poll inner again (will return None)
            Pin::new(&mut self.state.inner).poll_next(cx)
        }
    }
}

impl Drop for ActiveClientStream {
    fn drop(&mut self) {
        self.state.stop_preempt_watch_task();
        let mgr = Arc::clone(&self.state.connection_manager);
        let hndl = self.state.provider_handle.take();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                mgr.release_provider_handle(hndl).await;
            });
        }
    }
}

pub(in crate::api) struct ProvisionableActiveClientStream {
    state: ActiveClientStreamState,
}

impl Stream for ProvisionableActiveClientStream {
    type Item = Result<Bytes, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let flag = match &self.state.send_custom_stream_flag {
            Some(flag) => flag.load(Ordering::Acquire),
            None => INNER_STREAM,
        };

        let res = self.state.poll_next_base(cx);
        if !matches!(res, Poll::Ready(None)) || flag == INNER_STREAM {
            return res;
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
                None => Poll::Pending,
            }
        } else {
            Pin::new(&mut self.state.inner).poll_next(cx)
        }
    }
}

impl Drop for ProvisionableActiveClientStream {
    fn drop(&mut self) {
        self.state.stop_preempt_watch_task();
        let mgr = Arc::clone(&self.state.connection_manager);
        let hndl = self.state.provider_handle.take();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                mgr.release_provider_handle(hndl).await;
            });
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

    let provisioning_info = resolve_grace_period_provisioning(app_state, &stream_details);
    let has_provisioning = provisioning_info.is_some();
    let hold_stream = stream_details.grace_period.hold_stream;

    let waker = Arc::new(AtomicWaker::new());
    let grace_stop_flag =
        if grant_user_grace_period || (stream_details.has_grace_period() && stream_details.provider_name.is_some()) {
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

    let state = ActiveClientStreamState {
        inner: stream,
        preempt_watch_task: ActiveClientStreamState::spawn_preempt_watch_task(
            stream_details.provider_handle.as_ref(),
            &waker,
        ),
        provider_handle: stream_details.provider_handle,
        send_custom_stream_flag: grace_stop_flag,
        custom_video,
        waker: Some(waker),
        connection_manager: Arc::clone(&app_state.connection_manager),
        fingerprint: Arc::new(fingerprint.clone()),
        provider_stopped: false,
    };

    if has_provisioning {
        ProvisionableActiveClientStream { state }.boxed()
    } else {
        ActiveClientStream { state }.boxed()
    }
}

fn resolve_grace_period_provisioning(
    app_state: &Arc<AppState>,
    stream_details: &StreamDetails,
) -> Option<GraceProvisioningInfo> {
    if stream_details.disable_provider_grace {
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
) -> Option<Arc<AtomicU8>> {
    let active_users = Arc::clone(&app_state.active_users);
    let active_provider = Arc::clone(&app_state.active_provider);
    let connection_manager = Arc::clone(&app_state.connection_manager);

    let provider_grace_check = if stream_details.has_grace_period()
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
        tokio::spawn(async move {
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
        return Some(stream_strategy_flag);
    }
    None
}
