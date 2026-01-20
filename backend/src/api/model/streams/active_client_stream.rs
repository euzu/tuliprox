use crate::api::model::BoxedProviderStream;
use crate::api::model::StreamError;
use crate::api::model::TimedClientStream;
use crate::api::model::TransportStreamBuffer;
use crate::api::model::{AppState, ConnectionManager, CustomVideoStreamType, ProviderHandle, StreamDetails};
use crate::auth::Fingerprint;
use crate::model::ProxyUserCredentials;
use crate::utils::debug_if_enabled;
use axum::http::header::USER_AGENT;
use axum::http::HeaderMap;
use bytes::Bytes;
use futures::task::AtomicWaker;
use futures::Stream;
use futures::StreamExt;
use log::{error, info};
use shared::model::{StreamChannel, UserConnectionPermission};
use shared::utils::sanitize_sensitive_info;
use std::pin::Pin;
use std::sync::atomic::AtomicU8;
use std::sync::Arc;
use std::task::Poll;

const INNER_STREAM: u8 = 0_u8;
const USER_EXHAUSTED_STREAM: u8 = 1_u8;
const PROVIDER_EXHAUSTED_STREAM: u8 = 2_u8;
const CHANNEL_UNAVAILABLE_STREAM: u8 = 3_u8;

pub(in crate::api) struct ActiveClientStream {
    inner: BoxedProviderStream,
    send_custom_stream_flag: Option<Arc<AtomicU8>>,
    #[allow(dead_code)]
    provider_handle: Option<ProviderHandle>,
    custom_video: (Option<TransportStreamBuffer>, Option<TransportStreamBuffer>, Option<TransportStreamBuffer>),
    waker: Option<Arc<AtomicWaker>>,
    connection_manager: Arc<ConnectionManager>,
    fingerprint: Arc<Fingerprint>,
    provider_stopped: bool,
}

impl ActiveClientStream {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn new(mut stream_details: StreamDetails,
                            app_state: &Arc<AppState>,
                            user: &ProxyUserCredentials,
                            connection_permission: UserConnectionPermission,
                            fingerprint: &Fingerprint,
                            stream_channel: StreamChannel,
                            session_token: Option<&str>,
                            req_headers: &HeaderMap) -> Self {
        if connection_permission == UserConnectionPermission::Exhausted {
            error!("Something is wrong this should not happen");
        }
        let grant_user_grace_period = connection_permission == UserConnectionPermission::GracePeriod;
        let username = user.username.as_str();
        let provider_name = stream_details.provider_name.as_ref().map_or_else(String::new, ToString::to_string);

        let user_agent = req_headers.get(USER_AGENT).map(|h| String::from_utf8_lossy(h.as_bytes())).unwrap_or_default();

        let virtual_id = stream_channel.virtual_id;
        app_state.connection_manager.update_connection(username, user.max_connections, fingerprint, &provider_name, stream_channel, user_agent, session_token).await;
        if let Some((_, _, _m_, Some(cvt))) = stream_details.stream_info.as_ref() {
            app_state.connection_manager.update_stream_detail(&fingerprint.addr, *cvt).await;
        }
        let cfg = &app_state.app_config;
        let (grace_stop_flag, waker) = if grant_user_grace_period || (stream_details.has_grace_period() && stream_details.provider_name.is_some()) {
            let waker = Arc::new(AtomicWaker::new());
            let flag = Self::stream_grace_period(app_state, &stream_details, grant_user_grace_period, user, fingerprint, Some(Arc::clone(&waker)));
            let maybe_waker = flag.as_ref().map(|_| waker);
            (flag, maybe_waker)
        } else {
            (Self::stream_grace_period(app_state, &stream_details, grant_user_grace_period, user, fingerprint, None), None)
        };

        let custom_response = cfg.custom_stream_response.load();
        let custom_video = custom_response.as_ref()
            .map_or((None, None, None), |c|
                (
                    c.user_connections_exhausted.clone(),
                    c.provider_connections_exhausted.clone(),
                    c.channel_unavailable.clone(),
                ));

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

        Self {
            inner: stream,
            provider_handle: stream_details.provider_handle,
            send_custom_stream_flag: grace_stop_flag,
            custom_video,
            waker,
            connection_manager: Arc::clone(&app_state.connection_manager),
            fingerprint: Arc::new(fingerprint.clone()),
            provider_stopped: false,
        }
    }

    fn stream_grace_period(app_state: &AppState,
                           stream_details: &StreamDetails,
                           user_grace_period: bool,
                           user: &ProxyUserCredentials,
                           fingerprint: &Fingerprint,
                           waker: Option<Arc<AtomicWaker>>) -> Option<Arc<AtomicU8>> {
        let active_users = Arc::clone(&app_state.active_users);
        let active_provider = Arc::clone(&app_state.active_provider);
        let connection_manager = Arc::clone(&app_state.connection_manager);

        let provider_grace_check = if stream_details.has_grace_period() && stream_details.provider_name.is_some() {
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

        if provider_grace_check.is_some() || user_grace_check.is_some() {
            let stream_strategy_flag = Arc::new(AtomicU8::new(INNER_STREAM));
            let stream_strategy_flag_copy = Arc::clone(&stream_strategy_flag);
            let grace_period_millis = stream_details.grace_period_millis;

            let user_manager = Arc::clone(&active_users);
            let provider_manager = Arc::clone(&active_provider);
            let connection_manager = Arc::clone(&connection_manager);
            let reconnect_flag = stream_details.reconnect_flag.clone();
            let fingerprint = fingerprint.clone();
            tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_millis(grace_period_millis)).await;

                let mut updated = false;
                if let Some((username, max_connections)) = user_grace_check {
                    let active_connections = user_manager.user_connections(&username).await;
                    if active_connections > max_connections {
                        stream_strategy_flag_copy.store(USER_EXHAUSTED_STREAM, std::sync::atomic::Ordering::Release);
                        connection_manager.update_stream_detail(&fingerprint.addr, CustomVideoStreamType::UserConnectionsExhausted).await;
                        info!("User connections exhausted for active clients: {username}");
                        updated = true;
                    }
                }

                if !updated {
                    if let Some(provider_name) = provider_grace_check {
                        if provider_manager.is_over_limit(&provider_name).await {
                            stream_strategy_flag_copy.store(PROVIDER_EXHAUSTED_STREAM, std::sync::atomic::Ordering::Release);
                            connection_manager.update_stream_detail(&fingerprint.addr, CustomVideoStreamType::ProviderConnectionsExhausted).await;
                            info!("Provider connections exhausted for active clients: {provider_name}");
                            updated = true;
                        }
                    }
                }

                if !updated {
                    stream_strategy_flag_copy.store(INNER_STREAM, std::sync::atomic::Ordering::Release);
                }

                if updated {
                    if let Some(flag) = reconnect_flag {
                        flag.notify();
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

    fn stop_provider_stream(&mut self, unavailable: bool) {
        self.provider_stopped = true;

        if self.provider_handle.is_some() {
            let mgr = Arc::clone(&self.connection_manager);
            let handle = self.provider_handle.take();

            if unavailable {
                if let Some(flag) = &self.send_custom_stream_flag {
                    flag.store(CHANNEL_UNAVAILABLE_STREAM, std::sync::atomic::Ordering::Release);
                }
            }

            // Ensure the stream is re-polled after state change
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
                debug_if_enabled!( "Provider stream stopped due to grace period or unavailable provider channel for {}", sanitize_sensitive_info(&addr.to_string())
            );

                mgr.release_provider_handle(handle).await;
            });
        }
    }
}
impl Stream for ActiveClientStream {
    type Item = Result<Bytes, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(waker) = &self.waker {
            waker.register(cx.waker());
        }
        let flag = {
            match &self.send_custom_stream_flag {
                Some(flag) => flag.load(std::sync::atomic::Ordering::Acquire),
                None => INNER_STREAM,
            }
        };

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

        let buffer_opt = match flag {
            USER_EXHAUSTED_STREAM => {
                self.custom_video.0.as_mut()
            }
            PROVIDER_EXHAUSTED_STREAM => {
                self.custom_video.1.as_mut()
            }
            CHANNEL_UNAVAILABLE_STREAM => {
                self.custom_video.2.as_mut()
            }
            _ => None,
        };

        if let Some(buffer) = buffer_opt {
            Poll::Ready(Some(Ok(buffer.next_chunk())))
        } else {
            // At this point it should be the empty stream
            Pin::new(&mut self.inner).poll_next(cx)
        }
    }
}

impl Drop for ActiveClientStream {
    fn drop(&mut self) {
        let mgr = Arc::clone(&self.connection_manager);
        let hndl = self.provider_handle.take();
        tokio::spawn(async move {
            mgr.release_provider_handle(hndl).await;
        });
    }
}
