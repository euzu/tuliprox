use crate::{hooks::use_service_context, model::EventMessage};
use shared::{
    model::{
        ActiveUserConnectionChange, DownloadsDelta, DownloadsResponse, FileDownloadDto, PlaylistItemType, StatusCheck,
        StreamChannel, StreamInfo, SystemInfo, TaskKindDto, TransferStatusDto, XtreamCluster,
    },
    utils::{current_time_secs, Internable},
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashSet},
    hash::{DefaultHasher, Hash, Hasher},
    net::{IpAddr, Ipv4Addr, SocketAddr},
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

fn is_running_download(download: &FileDownloadDto) -> bool { download.status == TransferStatusDto::Running }

fn download_stream_uid(id: &str) -> u32 {
    let mut hasher = DefaultHasher::new();
    id.hash(&mut hasher);
    let hash = hasher.finish();
    let mixed = (hash ^ (hash >> 32)) as u32;
    mixed.max(1)
}

fn download_stream_addr(uid: u32) -> SocketAddr {
    let octet3 = ((uid >> 8) & 0xff) as u8;
    let octet4 = (uid & 0xff) as u8;
    let port = ((uid >> 16) % u32::from(u16::MAX - 1) + 1) as u16;
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 254, octet3, octet4)), port)
}

fn download_task_to_stream(download: &FileDownloadDto) -> StreamInfo {
    let uid = download_stream_uid(&download.id);
    let (item_type, cluster, group) = match download.kind {
        TaskKindDto::Download => (PlaylistItemType::Video, XtreamCluster::Video, "Downloads"),
        TaskKindDto::Recording => (PlaylistItemType::Live, XtreamCluster::Live, "Recordings"),
    };
    StreamInfo {
        uid,
        meter_uid: 0,
        username: "background task".to_string(),
        channel: StreamChannel {
            target_id: 0,
            virtual_id: uid,
            provider_id: 0,
            input_name: "".intern(),
            item_type,
            cluster,
            group: group.intern(),
            title: download.title.clone().intern(),
            url: "".intern(),
            shared: false,
            shared_joined_existing: None,
            shared_stream_id: None,
            technical: None,
        },
        provider: "Download Manager".to_string(),
        addr: download_stream_addr(uid),
        client_ip: "background-task".to_string(),
        user_agent: "Tuliprox download worker".to_string(),
        ts: current_time_secs(),
        country_code: None,
        session_token: None,
        preserved: false,
        previous_session_id: None,
    }
}

fn merge_aux_streams(server_status: &mut StatusCheck, download_streams: &[StreamInfo]) {
    server_status.active_user_streams.retain(|stream| stream.client_ip != "background-task");
    server_status.active_user_streams.extend(download_streams.iter().cloned());
    dedupe_streams_by_identity(&mut server_status.active_user_streams);
}

fn rebuild_status_with_downloads(
    status_holder: &UseStateHandle<RefCell<Option<Rc<StatusCheck>>>>,
    status_signal: &UseStateHandle<Option<Rc<StatusCheck>>>,
    download_streams: &[StreamInfo],
) {
    let mut server_status =
        status_holder.borrow().as_ref().map_or_else(StatusCheck::default, |status| (**status).clone());
    merge_aux_streams(&mut server_status, download_streams);
    let new_status = Rc::new(server_status);
    *status_holder.borrow_mut() = Some(Rc::clone(&new_status));
    status_signal.set(Some(new_status));
}

fn apply_downloads_snapshot(download_streams: &mut Vec<StreamInfo>, response: &DownloadsResponse) {
    *download_streams =
        response.active.iter().filter(|download| is_running_download(download)).map(download_task_to_stream).collect();
}

fn apply_downloads_delta(download_streams: &mut Vec<StreamInfo>, delta: &DownloadsDelta) {
    match delta {
        DownloadsDelta::SnapshotReset(response) => apply_downloads_snapshot(download_streams, response),
        DownloadsDelta::ActivePatched(download) => {
            if !is_running_download(download) {
                download_streams.clear();
                return;
            }
            let stream = download_task_to_stream(download);
            if let Some(existing) = download_streams.iter_mut().find(|current| current.uid == stream.uid) {
                *existing = stream;
            } else {
                download_streams.clear();
                download_streams.push(stream);
            }
        }
        DownloadsDelta::ActiveCleared => {
            download_streams.clear();
        }
        DownloadsDelta::QueueReplaced { .. } | DownloadsDelta::FinishedReplaced { .. } => {}
    }
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
    let download_streams_holder = use_state(|| RefCell::new(Vec::<StreamInfo>::new()));

    {
        let services_ctx = services.clone();
        let status_signal = status.clone();
        let status_holder_signal = status_holder.clone();
        let download_streams_holder_signal = download_streams_holder.clone();
        let system_info_signal = system_info.clone();
        let system_info_holder_signal = system_info_holder.clone();

        use_effect_with(enabled, move |enabled| {
            let mut subid: Option<usize> = None;

            if *enabled {
                let fetch_status: Rc<dyn Fn()> = Rc::new({
                    let services_clone = services_ctx.clone();
                    move || {
                        let services_clone = services_clone.clone();
                        spawn_local(async move {
                            services_clone.websocket.get_server_status().await;
                        });
                    }
                });
                let fetch_status_on_ws = Rc::clone(&fetch_status);

                subid = Some(services_ctx.event.subscribe(move |msg| match msg {
                    EventMessage::ServerStatus(server_status) => {
                        let mut server_status = (*server_status).clone();
                        dedupe_streams_by_identity(&mut server_status.active_user_streams);
                        merge_aux_streams(&mut server_status, download_streams_holder_signal.borrow().as_slice());
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
                        merge_aux_streams(&mut server_status, download_streams_holder_signal.borrow().as_slice());

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
                    EventMessage::DownloadsUpdate(downloads) => {
                        let mut next_download_streams = (*download_streams_holder_signal.borrow()).clone();
                        apply_downloads_snapshot(&mut next_download_streams, &downloads);
                        *download_streams_holder_signal.borrow_mut() = next_download_streams.clone();
                        rebuild_status_with_downloads(&status_holder_signal, &status_signal, &next_download_streams);
                    }
                    EventMessage::DownloadsDeltaUpdate(delta) => {
                        let mut next_download_streams = (*download_streams_holder_signal.borrow()).clone();
                        apply_downloads_delta(&mut next_download_streams, &delta);
                        *download_streams_holder_signal.borrow_mut() = next_download_streams.clone();
                        rebuild_status_with_downloads(&status_holder_signal, &status_signal, &next_download_streams);
                    }
                    EventMessage::SystemInfoUpdate(system_info) => {
                        let info = Rc::new(system_info);
                        *system_info_holder_signal.borrow_mut() = Some(Rc::clone(&info));
                        system_info_signal.set(Some(info));
                    }
                    EventMessage::WebSocketStatus(true) => {
                        fetch_status_on_ws();
                    }
                    _ => {}
                }));

                fetch_status();
            }

            let services_clone = services_ctx.clone();
            move || {
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
    use super::{
        apply_active_user_change, apply_downloads_delta, apply_downloads_snapshot, dedupe_streams_by_identity,
        find_stream_update_index,
    };
    use shared::{
        model::{
            ActiveUserConnectionChange, DownloadsDelta, DownloadsResponse, FileDownloadDto, PlaylistItemType,
            StatusCheck, StreamChannel, StreamInfo, TaskKindDto, TaskPriorityDto, TransferStatusDto, XtreamCluster,
        },
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
                input_name: "input".intern(),
                shared: false,
                shared_joined_existing: None,
                shared_stream_id: None,
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

    fn test_download(id: &str, status: TransferStatusDto, kind: TaskKindDto) -> FileDownloadDto {
        FileDownloadDto {
            id: id.to_string(),
            title: format!("{id}.ts"),
            kind,
            priority: TaskPriorityDto::Background,
            status,
            retry_attempts: 0,
            downloaded_bytes: 128,
            total_bytes: Some(1024),
            next_retry_at: None,
            scheduled_start_at: None,
            duration_secs: None,
            error: None,
        }
    }

    #[test]
    fn downloads_snapshot_creates_running_pseudo_streams_only() {
        let response = DownloadsResponse {
            queue: Vec::new(),
            finished: Vec::new(),
            active: vec![
                test_download("running", TransferStatusDto::Running, TaskKindDto::Download),
                test_download("paused", TransferStatusDto::Paused, TaskKindDto::Recording),
            ],
        };
        let mut streams = Vec::new();

        apply_downloads_snapshot(&mut streams, &response);

        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].provider, "Download Manager");
        assert_eq!(streams[0].client_ip, "background-task");
        assert_eq!(streams[0].channel.item_type, PlaylistItemType::Video);
    }

    #[test]
    fn downloads_delta_clears_pseudo_stream_when_active_stops_running() {
        let mut streams = Vec::new();
        apply_downloads_snapshot(
            &mut streams,
            &DownloadsResponse {
                queue: Vec::new(),
                finished: Vec::new(),
                active: vec![test_download("running", TransferStatusDto::Running, TaskKindDto::Recording)],
            },
        );

        apply_downloads_delta(
            &mut streams,
            &DownloadsDelta::ActivePatched(test_download(
                "running",
                TransferStatusDto::Completed,
                TaskKindDto::Recording,
            )),
        );

        assert!(streams.is_empty());
    }
}
