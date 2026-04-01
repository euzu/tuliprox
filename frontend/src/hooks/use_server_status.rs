use crate::{hooks::use_service_context, model::EventMessage};
use gloo_timers::callback::Interval;
use shared::model::{ActiveUserConnectionChange, StatusCheck, StreamInfo, SystemInfo};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashSet},
    net::SocketAddr,
    rc::Rc,
};
use yew::{platform::spawn_local, prelude::*};

type ServerStatusState =
    (UseStateHandle<RefCell<Option<Rc<StatusCheck>>>>, UseStateHandle<RefCell<Option<Rc<SystemInfo>>>>);

fn stream_identity_key(stream: &StreamInfo) -> (SocketAddr, u32) { (stream.addr, stream.uid) }

fn find_stream_update_index(streams: &[StreamInfo], updated_stream: &StreamInfo) -> Option<usize> {
    let updated_key = stream_identity_key(updated_stream);
    if let Some(index) = streams.iter().position(|stream| stream_identity_key(stream) == updated_key) {
        return Some(index);
    }

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

fn dedupe_streams_by_identity(streams: &mut Vec<StreamInfo>) {
    let mut seen = HashSet::new();
    streams.retain(|stream| seen.insert(stream_identity_key(stream)));
}

fn apply_active_user_change(server_status: &mut StatusCheck, event: ActiveUserConnectionChange) {
    match event {
        ActiveUserConnectionChange::Updated(stream_info) => {
            if let Some(pos) = find_stream_update_index(&server_status.active_user_streams, &stream_info) {
                server_status.active_user_streams[pos] = stream_info;
            } else {
                server_status.active_user_streams.push(stream_info);
            }
            dedupe_streams_by_identity(&mut server_status.active_user_streams);
        }
        ActiveUserConnectionChange::Disconnected(addr) => {
            server_status.active_user_streams.retain(|stream_info| stream_info.addr != addr);
        }
        ActiveUserConnectionChange::Connections(user_count, connections) => {
            server_status.active_users = user_count;
            server_status.active_user_connections = connections;
            if connections == 0 {
                server_status.active_user_streams.clear();
            }
        }
    }
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
                        let mut server_status = (*server_status).clone();
                        dedupe_streams_by_identity(&mut server_status.active_user_streams);
                        let server_status = Rc::new(server_status);
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
                        apply_active_user_change(&mut server_status, event);

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
    use super::{apply_active_user_change, dedupe_streams_by_identity, find_stream_update_index};
    use shared::{
        model::{ActiveUserConnectionChange, PlaylistItemType, StatusCheck, StreamChannel, StreamInfo, XtreamCluster},
        utils::Internable,
    };
    use std::net::SocketAddr;

    fn test_stream(uid: u32, addr: &str, session_token: Option<&str>, item_type: PlaylistItemType) -> StreamInfo {
        StreamInfo {
            uid,
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
            country_code: None,
            session_token: session_token.map(ToOwned::to_owned),
            preserved: false,
            previous_session_id: None,
        }
    }

    #[test]
    fn test_find_stream_update_index_prefers_adaptive_session_token_over_addr() {
        let existing = test_stream(1, "127.0.0.1:1234", Some("tok-hls"), PlaylistItemType::LiveHls);
        let updated = test_stream(1, "127.0.0.1:5678", Some("tok-hls"), PlaylistItemType::LiveDash);

        assert_eq!(find_stream_update_index(&[existing], &updated), Some(0));
    }

    #[test]
    fn test_find_stream_update_index_matches_exact_render_key_first() {
        let existing = test_stream(2, "127.0.0.1:1234", Some("tok-a"), PlaylistItemType::LiveHls);
        let updated = test_stream(2, "127.0.0.1:1234", Some("tok-b"), PlaylistItemType::LiveDash);

        assert_eq!(find_stream_update_index(&[existing], &updated), Some(0));
    }

    #[test]
    fn test_dedupe_streams_by_identity_removes_duplicate_render_keys() {
        let mut streams = vec![
            test_stream(2, "127.0.0.1:1234", Some("tok-a"), PlaylistItemType::LiveHls),
            test_stream(2, "127.0.0.1:1234", Some("tok-b"), PlaylistItemType::LiveDash),
            test_stream(2, "127.0.0.1:5678", Some("tok-c"), PlaylistItemType::LiveDash),
        ];

        dedupe_streams_by_identity(&mut streams);

        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].addr, "127.0.0.1:1234".parse::<SocketAddr>().unwrap_or_else(|_| unreachable!()));
        assert_eq!(streams[1].addr, "127.0.0.1:5678".parse::<SocketAddr>().unwrap_or_else(|_| unreachable!()));
    }

    #[test]
    fn test_connections_zero_clears_stale_stream_rows() {
        let mut status = StatusCheck::default();
        status.active_users = 1;
        status.active_user_connections = 1;
        status.active_user_streams = vec![
            test_stream(1, "127.0.0.1:1234", Some("tok-a"), PlaylistItemType::Video),
            test_stream(2, "127.0.0.1:5678", Some("tok-b"), PlaylistItemType::Series),
        ];

        apply_active_user_change(&mut status, ActiveUserConnectionChange::Connections(0, 0));

        assert_eq!(status.active_users, 0);
        assert_eq!(status.active_user_connections, 0);
        assert!(status.active_user_streams.is_empty());
    }
}
