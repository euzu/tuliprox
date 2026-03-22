use crate::{hooks::use_service_context, model::EventMessage};
use gloo_timers::callback::Interval;
use shared::model::{ActiveUserConnectionChange, StatusCheck, StreamInfo, SystemInfo};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
use yew::{platform::spawn_local, prelude::*};

type ServerStatusState =
    (UseStateHandle<RefCell<Option<Rc<StatusCheck>>>>, UseStateHandle<RefCell<Option<Rc<SystemInfo>>>>);

fn find_stream_update_index(streams: &[StreamInfo], updated_stream: &StreamInfo) -> Option<usize> {
    if updated_stream.channel.item_type.is_live_adaptive() {
        if let Some(session_token) = updated_stream.session_token.as_deref() {
            if let Some(index) =
                streams.iter().position(|stream| stream.session_token.as_deref() == Some(session_token))
            {
                return Some(index);
            }
        }
    }

    streams.iter().position(|stream| stream.addr == updated_stream.addr)
}

#[hook]
pub fn use_server_status(
    status: UseStateHandle<Option<Rc<StatusCheck>>>,
    system_info: UseStateHandle<Option<Rc<SystemInfo>>>,
    enabled: bool,
) -> ServerStatusState {
    let services = use_service_context();
    let status_holder = use_state(|| RefCell::new(None::<Rc<StatusCheck>>));
    let system_info_holder = use_state(|| RefCell::new(None::<Rc<SystemInfo>>));

    {
        let services_ctx = services.clone();
        let status_signal = status.clone();
        let status_holder_signal = status_holder.clone();
        let system_info_signal = system_info.clone();
        let system_info_holder_signal = system_info_holder.clone();

        use_effect_with(enabled, move |enabled| {
            let mut subid: Option<usize> = None;
            let mut interval: Option<Interval> = None;

            if *enabled {
                subid = Some(services_ctx.event.subscribe(move |msg| match msg {
                    EventMessage::ServerStatus(server_status) => {
                        *status_holder_signal.borrow_mut() = Some(Rc::clone(&server_status));
                        status_signal.set(Some(server_status));
                    }
                    EventMessage::ActiveUser(event) => {
                        let mut server_status = {
                            if let Some(old_status) = status_holder_signal.borrow().as_ref() {
                                (**old_status).clone()
                            } else {
                                StatusCheck::default()
                            }
                        };

                        match event {
                            ActiveUserConnectionChange::Updated(stream_info) => {
                                if let Some(pos) =
                                    find_stream_update_index(&server_status.active_user_streams, &stream_info)
                                {
                                    server_status.active_user_streams[pos] = stream_info;
                                } else {
                                    server_status.active_user_streams.push(stream_info);
                                }
                            }
                            ActiveUserConnectionChange::Disconnected(addr) => {
                                server_status.active_user_streams.retain(|stream_info| stream_info.addr != addr);
                            }
                            ActiveUserConnectionChange::Connections(user_count, connections) => {
                                server_status.active_users = user_count;
                                server_status.active_user_connections = connections;
                            }
                        }

                        let new_status = Rc::new(server_status);
                        *status_holder_signal.borrow_mut() = Some(Rc::clone(&new_status));
                        status_signal.set(Some(new_status));
                    }
                    EventMessage::ActiveProvider(provider, connections) => {
                        let mut server_status = {
                            if let Some(old_status) = status_holder_signal.borrow().as_ref() {
                                (**old_status).clone()
                            } else {
                                StatusCheck::default()
                            }
                        };
                        if let Some(treemap) = server_status.active_provider_connections.as_mut() {
                            treemap.insert(provider, connections);
                        } else {
                            let mut treemap = BTreeMap::new();
                            treemap.insert(provider, connections);
                            server_status.active_provider_connections = Some(treemap);
                        }
                        let new_status = Rc::new(server_status);
                        *status_holder_signal.borrow_mut() = Some(Rc::clone(&new_status));
                        status_signal.set(Some(new_status));
                    }
                    EventMessage::SystemInfoUpdate(system_info) => {
                        let info = Rc::new(system_info);
                        *system_info_holder_signal.borrow_mut() = Some(Rc::clone(&info));
                        system_info_signal.set(Some(info));
                    }
                    _ => {}
                }));

                let fetch_status = {
                    let services_clone = services_ctx.clone();
                    move || {
                        let services_clone = services_clone.clone();
                        spawn_local(async move {
                            services_clone.websocket.get_server_status().await;
                        });
                    }
                };

                fetch_status();
                interval = Some(Interval::new(60 * 1000, move || {
                    fetch_status();
                }));
            }

            let services_clone = services_ctx.clone();
            move || {
                drop(interval);
                if let Some(subid) = subid {
                    services_clone.event.unsubscribe(subid);
                }
            }
        });
    }
    (status_holder, system_info_holder)
}

#[cfg(test)]
mod tests {
    use super::find_stream_update_index;
    use shared::{
        model::{PlaylistItemType, StreamChannel, StreamInfo, XtreamCluster},
        utils::Internable,
    };
    use std::net::SocketAddr;

    fn test_stream(addr: &str, session_token: Option<&str>, item_type: PlaylistItemType) -> StreamInfo {
        StreamInfo {
            uid: 1,
            meter_uid: 2,
            username: "user".to_string(),
            channel: StreamChannel {
                target_id: 1,
                virtual_id: 1,
                provider_id: 1,
                item_type,
                cluster: XtreamCluster::Live,
                group: "group".intern(),
                title: "title".intern(),
                url: "http://localhost/live.m3u8".intern(),
                shared: false,
                technical: None,
            },
            provider: "provider".to_string(),
            addr: addr.parse::<SocketAddr>().unwrap_or_else(|_| unreachable!()),
            client_ip: "127.0.0.1".to_string(),
            user_agent: "ua".to_string(),
            ts: 1,
            country: None,
            session_token: session_token.map(ToOwned::to_owned),
            preserved: false,
        }
    }

    #[test]
    fn test_find_stream_update_index_prefers_adaptive_session_token_over_addr() {
        let existing = test_stream("127.0.0.1:1234", Some("tok-hls"), PlaylistItemType::LiveHls);
        let updated = test_stream("127.0.0.1:5678", Some("tok-hls"), PlaylistItemType::LiveDash);

        assert_eq!(find_stream_update_index(&[existing], &updated), Some(0));
    }
}
