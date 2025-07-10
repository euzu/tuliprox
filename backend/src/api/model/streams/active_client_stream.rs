use crate::api::api_utils::StreamDetails;
use crate::api::model::active_provider_manager::{ActiveProviderManager, ProviderConnectionGuard};
use crate::api::model::active_user_manager::ActiveUserManager;
use crate::api::model::active_user_manager::UserConnectionGuard;
use crate::api::model::app_state::AppState;
use crate::api::model::stream::BoxedProviderStream;
use crate::api::model::stream_error::StreamError;
use crate::api::model::streams::transport_stream_buffer::TransportStreamBuffer;
use crate::model::{ProxyUserCredentials};
use bytes::Bytes;
use futures::Stream;
use log::{error, info};
use std::pin::Pin;
use std::sync::atomic::AtomicU8;
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};
use crate::api::model::streams::timed_client_stream::TimedClientStream;
use futures::{StreamExt};
use shared::model::UserConnectionPermission;

const INNER_STREAM: u8 = 0_u8;
const GRACE_BLOCK_STREAM: u8 = 1_u8;
const USER_EXHAUSTED_STREAM: u8 = 2_u8;
const PROVIDER_EXHAUSTED_STREAM: u8 = 3_u8;

pub(in crate::api) struct ActiveClientStream {
    inner: BoxedProviderStream,
    send_custom_stream_flag: Option<Arc<AtomicU8>>,
    #[allow(unused)]
    user_connection_guard: Option<UserConnectionGuard>,
    #[allow(dead_code)]
    provider_connection_guard: Option<ProviderConnectionGuard>,
    custom_video: (Option<TransportStreamBuffer>, Option<TransportStreamBuffer>),
    waker: Arc<Mutex<Option<Waker>>>,
}

impl ActiveClientStream {
    pub(crate) async fn new(mut stream_details: StreamDetails,
                            app_state: &AppState,
                            user: &ProxyUserCredentials,
                            connection_permission: UserConnectionPermission) -> Self {
        let active_user = app_state.active_users.clone();
        let active_provider = app_state.active_provider.clone();
        if connection_permission == UserConnectionPermission::Exhausted {
            error!("Something is wrong this should not happen");
        }
        let grant_user_grace_period = connection_permission == UserConnectionPermission::GracePeriod;
        let username = user.username.as_str();
        let user_connection_guard = Some(active_user.add_connection(username, user.max_connections).await);
        let cfg = &app_state.app_config;
        let waker = Arc::new(Mutex::new(None));
        let waker_clone = Arc::clone(&waker);
        let grace_stop_flag = Self::stream_grace_period(&stream_details, grant_user_grace_period, user, &active_user, &active_provider, &waker_clone);
        let custom_response = cfg.custom_stream_response.load();
        let custom_video = custom_response.as_ref()
            .map_or((None, None), |c|
                (
                    c.user_connections_exhausted.clone(),
                    c.provider_connections_exhausted.clone()
                ));

        let stream = stream_details.stream.take().unwrap();
        let stream = match app_state.app_config.config.load().sleep_timer_mins {
            None => stream,
            Some(mins) => {
                let secs = u32::try_from((u64::from(mins) * 60).min(u64::from(u32::MAX))).unwrap_or(0);
                if secs > 0 {
                    TimedClientStream::new(stream,  secs).boxed()
                } else {
                    stream
                }
            }
        };

        Self {
            inner: stream,
            user_connection_guard,
            provider_connection_guard: stream_details.provider_connection_guard,
            send_custom_stream_flag: grace_stop_flag,
            custom_video,
            waker,
        }
    }

    fn stream_grace_period(stream_details: &StreamDetails,
                           user_grace_period: bool,
                           user: &ProxyUserCredentials,
                           active_user: &Arc<ActiveUserManager>,
                           active_provider: &Arc<ActiveProviderManager>,
                           waker: &Arc<Mutex<Option<Waker>>>) -> Option<Arc<AtomicU8>> {
        let provider_grace_check = if stream_details.has_grace_period() && stream_details.input_name.is_some() {
            let provider_name = stream_details.input_name.as_deref().unwrap_or_default().to_string();
            let provider_manager = Arc::clone(active_provider);
            let reconnect_flag = stream_details.reconnect_flag.clone();
            Some((provider_name, provider_manager, reconnect_flag))
        } else {
            None
        };
        let user_max_connections = user.max_connections;
        let user_grace_check = if user_grace_period && user_max_connections > 0 {
            let user_name = user.username.clone();
            let user_manager = Arc::clone(active_user);
            let reconnect_flag = stream_details.reconnect_flag.clone();
            Some((user_name, user_manager, user_max_connections, reconnect_flag))
        } else {
            None
        };

        if provider_grace_check.is_some() || user_grace_check.is_some() {
            let stream_strategy_flag = Arc::new(AtomicU8::new(GRACE_BLOCK_STREAM));
            let stream_strategy_flag_copy = Arc::clone(&stream_strategy_flag);
            let waker_copy = Arc::clone(waker);
            let grace_period_millis = stream_details.grace_period_millis;

            tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_millis(grace_period_millis)).await;

                let mut updated = false;

                if let Some((username, user_manager, max_connections, reconnect_flag)) = user_grace_check {
                    let active_connections = user_manager.user_connections(&username).await;
                    if active_connections > max_connections {
                        info!("User connections exhausted for active clients: {username}");
                        stream_strategy_flag_copy.store(USER_EXHAUSTED_STREAM, std::sync::atomic::Ordering::SeqCst);
                        if let Some(flag) = reconnect_flag {
                            info!("Stopped reconnecting, user connections exhausted");
                            flag.notify();
                        }
                        updated = true;
                    }
                }

                if !updated {
                    if let Some((provider_name, provider_manager, reconnect_flag)) = provider_grace_check {
                        if provider_manager.is_over_limit(&provider_name).await {
                            info!("Provider connections exhausted for active clients: {provider_name}");
                            stream_strategy_flag_copy.store(PROVIDER_EXHAUSTED_STREAM, std::sync::atomic::Ordering::SeqCst);
                            if let Some(flag) = reconnect_flag {
                                info!("Stopped reconnecting, provider connections exhausted");
                                flag.notify();
                            }
                            updated = true;
                        }
                    }
                }

                if !updated {
                    stream_strategy_flag_copy.store(INNER_STREAM, std::sync::atomic::Ordering::SeqCst);
                }
                if let Ok(mut waker_guard) = waker_copy.lock() {
                    if let Some(w) = waker_guard.take() {
                        w.wake();
                    }
                } else {
                    error!("Failed to acquire waker lock - mutex poisoned");
                }
            });
            return Some(stream_strategy_flag);
        }
        None
    }
}
impl Stream for ActiveClientStream {
    type Item = Result<Bytes, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Option<Self::Item>> {
        let flag = match &self.send_custom_stream_flag {
            Some(flag) => flag.load(std::sync::atomic::Ordering::SeqCst),
            None => INNER_STREAM,
        };

        if flag == INNER_STREAM {
            return Pin::new(&mut self.inner).poll_next(cx);
        }

        if flag == GRACE_BLOCK_STREAM {
            if let Ok(mut waker_lock) = self.waker.lock() {
                if waker_lock.is_none() {
                    *waker_lock = Some(cx.waker().clone());
                }
                return Poll::Pending;
            }
            return Poll::Ready(Some(Ok(Bytes::new())));
        }

        let buffer_opt = match flag {
            USER_EXHAUSTED_STREAM => self.custom_video.0.as_mut(),
            PROVIDER_EXHAUSTED_STREAM => self.custom_video.1.as_mut(),
            _ => None,
        };

        if let Some(buffer) = buffer_opt {
           return Poll::Ready(Some(Ok(buffer.next_chunk())));
        }

        Poll::Ready(None)
    }
}
