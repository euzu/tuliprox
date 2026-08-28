use crate::{
    hooks::use_service_context,
    model::{EventMessage, BACKGROUND_TRANSFER_CLIENT_IP, BACKGROUND_TRANSFER_PROVIDER},
    utils::is_shared_hls_stream,
};
use shared::{
    model::{
        permission::Permission, ActiveUserConnectionChange, DownloadsDelta, DownloadsResponse, FileDownloadDto,
        PlaylistItemType, ProtocolMessage, StatusCheck, StreamChannel, StreamInfo, SystemInfo, TaskKindDto,
        TransferStatusDto, XtreamCluster,
    },
    utils::{contains_ascii_case_insensitive, current_time_secs, is_catchup_session_token, Internable},
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

fn stream_url_looks_adaptive(url: &str) -> bool {
    contains_ascii_case_insensitive(url, b".m3u8") || contains_ascii_case_insensitive(url, b".mpd")
}

fn is_sticky_session_stream(stream: &StreamInfo) -> bool {
    // Match backend `is_stable_session_stream`, plus Catchup without a token so soft-preserve
    // still wins when archive segments briefly lose session metadata.
    if stream.channel.item_type == PlaylistItemType::Catchup {
        return true;
    }
    if stream.channel.item_type.is_live_adaptive() || is_shared_hls_stream(stream) {
        return true;
    }
    if stream_url_looks_adaptive(stream.channel.url.as_ref()) {
        return true;
    }
    stream.session_token.as_deref().is_some_and(is_catchup_session_token)
}

fn find_stream_update_index(streams: &[StreamInfo], updated_stream: &StreamInfo) -> Option<usize> {
    let updated_key = stream_identity_key(updated_stream);
    if let Some(index) = streams.iter().position(|stream| stream_identity_key(stream) == updated_key) {
        return Some(index);
    }

    if let Some(session_token) = updated_stream.session_token.as_deref() {
        if is_sticky_session_stream(updated_stream) {
            if let Some(index) =
                streams.iter().position(|stream| stream.session_token.as_deref() == Some(session_token))
            {
                return Some(index);
            }
        }
    }

    // Catchup without a stable token still remaps by channel identity so Streams does not accumulate
    // one row per HLS segment addr/uid churn.
    if updated_stream.channel.item_type == PlaylistItemType::Catchup {
        if let Some(index) = streams.iter().position(|stream| {
            stream.channel.item_type == PlaylistItemType::Catchup
                && stream.username == updated_stream.username
                && stream.channel.virtual_id == updated_stream.channel.virtual_id
        }) {
            return Some(index);
        }
    }

    if updated_stream.session_token.is_none() {
        return streams.iter().position(|stream| stream.addr == updated_stream.addr && stream.session_token.is_none());
    }

    None
}

fn dedupe_streams_by_identity(streams: &mut Vec<StreamInfo>) {
    let mut seen = HashSet::new();
    streams.retain(|stream| seen.insert(stream_identity_key(stream)));
}

/// Drop preserved sticky rows once the same user+IP has an active replacement row.
/// Keeps soft-preserve during HLS segment gaps (no active row yet); clears zapped channels immediately
/// instead of waiting for `hls_session_ttl` (~15s).
fn prune_zapped_preserved_streams(streams: &mut Vec<StreamInfo>) {
    let users_with_active: HashSet<(String, String)> = streams
        .iter()
        .filter(|stream| !stream.preserved && stream.client_ip != BACKGROUND_TRANSFER_CLIENT_IP)
        .map(|stream| (stream.username.clone(), stream.client_ip.clone()))
        .collect();
    if users_with_active.is_empty() {
        return;
    }

    streams.retain(|stream| {
        if stream.client_ip == BACKGROUND_TRANSFER_CLIENT_IP {
            return true;
        }
        if !stream.preserved || !is_sticky_session_stream(stream) {
            return true;
        }
        let user_ip = (stream.username.clone(), stream.client_ip.clone());
        if !users_with_active.contains(&user_ip) {
            return true;
        }
        false
    });
}

fn mark_sticky_session_preserved(stream: &mut StreamInfo) -> bool {
    if is_sticky_session_stream(stream) {
        stream.preserved = true;
        true
    } else {
        false
    }
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

fn download_task_to_stream_with_ts(download: &FileDownloadDto, ts: u64) -> StreamInfo {
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
            epg_channel_id: None,
            epg_reference_ts: None,
            upstream_user_agent: None,
        },
        provider: BACKGROUND_TRANSFER_PROVIDER.intern(),
        addr: download_stream_addr(uid),
        client_ip: BACKGROUND_TRANSFER_CLIENT_IP.to_string(),
        user_agent: "Tuliprox download worker".to_string(),
        ts,
        started_at: ts,
        country_code: None,
        session_token: None,
        preserved: false,
        previous_session_id: None,
    }
}

fn preserved_download_stream_ts(existing_streams: &[StreamInfo], download: &FileDownloadDto) -> u64 {
    let uid = download_stream_uid(&download.id);
    existing_streams
        .iter()
        .find(|stream| stream.uid == uid && stream.client_ip == BACKGROUND_TRANSFER_CLIENT_IP)
        .map_or_else(current_time_secs, |stream| stream.ts)
}

fn merge_aux_streams(server_status: &mut StatusCheck, download_streams: &[StreamInfo]) {
    server_status.active_user_streams.retain(|stream| stream.client_ip != BACKGROUND_TRANSFER_CLIENT_IP);
    server_status.active_user_streams.extend(download_streams.iter().cloned());
    dedupe_streams_by_identity(&mut server_status.active_user_streams);
}

fn replace_server_status_snapshot(
    status_holder: &mut Option<Rc<StatusCheck>>,
    mut server_status: StatusCheck,
    download_streams: &[StreamInfo],
) -> Rc<StatusCheck> {
    // Trust backend `panel_streams` as authoritative. Re-merging sticky FE ghosts resurrected
    // ended archive/HLS rows until a full page reload.
    dedupe_streams_by_identity(&mut server_status.active_user_streams);
    prune_zapped_preserved_streams(&mut server_status.active_user_streams);
    merge_aux_streams(&mut server_status, download_streams);
    let server_status = Rc::new(server_status);
    *status_holder = Some(Rc::clone(&server_status));
    server_status
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
    let previous_streams = download_streams.clone();
    *download_streams = response
        .active
        .iter()
        .filter(|download| is_running_download(download))
        .map(|download| {
            let ts = preserved_download_stream_ts(&previous_streams, download);
            download_task_to_stream_with_ts(download, ts)
        })
        .collect();
}

fn apply_downloads_delta(download_streams: &mut Vec<StreamInfo>, delta: &DownloadsDelta) {
    match delta {
        DownloadsDelta::SnapshotReset(response) => apply_downloads_snapshot(download_streams, response),
        DownloadsDelta::ActivePatched(download) => {
            let uid = download_stream_uid(&download.id);
            if !is_running_download(download) {
                download_streams.retain(|stream| stream.uid != uid);
                return;
            }
            let ts = preserved_download_stream_ts(download_streams, download);
            let stream = download_task_to_stream_with_ts(download, ts);
            if let Some(existing) = download_streams.iter_mut().find(|current| current.uid == stream.uid) {
                *existing = stream;
            } else {
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
            if stream_info.preserved {
                // Never drop on Updated(preserved): backend already decided the session row should
                // linger between HLS segments. Removing here blanked archive Streams when sticky
                // detection lagged behind backend `is_stable_session_stream`.
                if is_sticky_session_stream(&stream_info) {
                    if let Some(pos) = find_stream_update_index(&server_status.active_user_streams, &stream_info) {
                        server_status.active_user_streams[pos] = stream_info;
                    } else {
                        server_status.active_user_streams.push(stream_info);
                    }
                    dedupe_streams_by_identity(&mut server_status.active_user_streams);
                }
                return;
            }
            if let Some(pos) = find_stream_update_index(&server_status.active_user_streams, &stream_info) {
                server_status.active_user_streams[pos] = stream_info;
            } else {
                server_status.active_user_streams.push(stream_info);
            }
            dedupe_streams_by_identity(&mut server_status.active_user_streams);
            // Channel zap: new active channel replaces preserved soft-kept previous channel immediately.
            prune_zapped_preserved_streams(&mut server_status.active_user_streams);
        }
        ActiveUserConnectionChange::Disconnected(addr) => {
            let mut retained = Vec::with_capacity(server_status.active_user_streams.len());
            for mut stream in server_status.active_user_streams.drain(..) {
                if stream.addr != addr {
                    retained.push(stream);
                } else if stream.preserved && is_sticky_session_stream(&stream) {
                    continue;
                } else if mark_sticky_session_preserved(&mut stream) {
                    retained.push(stream);
                }
            }
            server_status.active_user_streams = retained;
        }
        ActiveUserConnectionChange::DisconnectedStream { addr, uid } => {
            let already_preserved = server_status
                .active_user_streams
                .iter()
                .any(|stream_info| stream_info.addr == addr && stream_info.uid == uid && stream_info.preserved);
            if already_preserved {
                // Backend session TTL expiry — do not soft-preserve again (that left ghosts until reload).
                server_status
                    .active_user_streams
                    .retain(|stream_info| stream_info.addr != addr || stream_info.uid != uid);
                return;
            }
            if let Some(stream) = server_status
                .active_user_streams
                .iter_mut()
                .find(|stream_info| stream_info.addr == addr && stream_info.uid == uid)
            {
                if mark_sticky_session_preserved(stream) {
                    return;
                }
            }
            server_status.active_user_streams.retain(|stream_info| stream_info.addr != addr || stream_info.uid != uid);
        }
        ActiveUserConnectionChange::Connections(user_count, connections) => {
            server_status.active_users = user_count;
            server_status.active_user_connections = connections;
            // Connections(0) often arrives before Disconnected* between HLS segments. If we leave
            // sticky rows as active (preserved=false), adaptive last_seen keeps refreshing and the
            // Streams TTL never hides them — ghosts until page reload.
            if connections == 0 {
                for stream in &mut server_status.active_user_streams {
                    mark_sticky_session_preserved(stream);
                }
                server_status.active_user_streams.retain(|stream| {
                    stream.client_ip == BACKGROUND_TRANSFER_CLIENT_IP
                        || (stream.preserved && is_sticky_session_stream(stream))
                });
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
                            if services_clone.auth.has_permission(Permission::RecordingRead) {
                                if services_clone.websocket.send_message(ProtocolMessage::DownloadsRequest) {
                                    return;
                                }
                                if let Ok(downloads) = services_clone.downloads.get_downloads().await {
                                    services_clone.event.broadcast(EventMessage::DownloadsUpdate(Rc::new(downloads)));
                                }
                            }
                        });
                    }
                });
                let fetch_status_on_ws = Rc::clone(&fetch_status);

                subid = Some(services_ctx.event.subscribe(move |msg| match msg {
                    EventMessage::ServerStatus(server_status) => {
                        let server_status = replace_server_status_snapshot(
                            &mut status_holder_signal.borrow_mut(),
                            (*server_status).clone(),
                            download_streams_holder_signal.borrow().as_slice(),
                        );
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
        download_task_to_stream_with_ts, find_stream_update_index, replace_server_status_snapshot,
    };
    use shared::{
        model::{
            ActiveUserConnectionChange, DownloadsDelta, DownloadsResponse, FileDownloadDto, PlaylistItemType,
            StreamChannel, StreamInfo, TaskKindDto, TaskPriorityDto, TransferStatusDto, XtreamCluster,
        },
        utils::Internable,
    };
    use std::{net::SocketAddr, rc::Rc};

    fn test_stream(uid: u32, addr: &str, session_token: Option<&str>, item_type: PlaylistItemType) -> StreamInfo {
        let url = match item_type {
            PlaylistItemType::LiveHls => "http://localhost/live.m3u8",
            PlaylistItemType::LiveDash => "http://localhost/live.mpd",
            _ => "http://localhost/live.ts",
        };
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
                url: url.intern(),
                input_name: "input".intern(),
                shared: false,
                shared_joined_existing: None,
                shared_stream_id: None,
                technical: None,
                epg_channel_id: None,
                epg_reference_ts: None,
                upstream_user_agent: None,
            },
            provider: "provider".intern(),
            addr: addr.parse::<SocketAddr>().unwrap_or_else(|_| unreachable!()),
            client_ip: "127.0.0.1".to_string(),
            user_agent: "ua".to_string(),
            ts: 1,
            started_at: 1,
            country_code: None,
            session_token: session_token.map(ToOwned::to_owned),
            preserved: false,
            previous_session_id: None,
        }
    }

    fn test_shared_hls_stream(uid: u32, addr: &str, session_token: Option<&str>) -> StreamInfo {
        let mut stream = test_stream(uid, addr, session_token, PlaylistItemType::LiveHls);
        stream.channel.shared = true;
        stream
    }

    #[test]
    fn test_find_stream_update_index_prefers_adaptive_session_token_over_addr() {
        let existing = test_stream(1, "127.0.0.1:1234", Some("tok-hls"), PlaylistItemType::LiveHls);
        let updated = test_stream(1, "127.0.0.1:5678", Some("tok-hls"), PlaylistItemType::LiveDash);

        assert_eq!(find_stream_update_index(&[existing], &updated), Some(0));
    }

    #[test]
    fn test_find_stream_update_index_prefers_catchup_session_token_over_addr() {
        let existing = test_stream(3, "127.0.0.1:1234", Some("tok-catchup"), PlaylistItemType::Catchup);
        let updated = test_stream(3, "127.0.0.1:5678", Some("tok-catchup"), PlaylistItemType::Catchup);

        assert_eq!(find_stream_update_index(&[existing], &updated), Some(0));
    }

    #[test]
    fn test_find_stream_update_index_matches_exact_render_key_first() {
        let existing = test_stream(2, "127.0.0.1:1234", Some("tok-a"), PlaylistItemType::LiveHls);
        let updated = test_stream(2, "127.0.0.1:1234", Some("tok-b"), PlaylistItemType::LiveDash);

        assert_eq!(find_stream_update_index(&[existing], &updated), Some(0));
    }

    #[test]
    fn test_find_stream_update_index_keeps_socket_bound_streams_with_same_addr_separate() {
        let existing = test_stream(1, "127.0.0.1:1234", Some("tok-live"), PlaylistItemType::Live);
        let updated = test_stream(2, "127.0.0.1:1234", Some("tok-live"), PlaylistItemType::Live);

        assert_eq!(find_stream_update_index(&[existing], &updated), None);
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
    fn test_connections_zero_drops_plain_rows_and_soft_preserves_sticky_with_zero_users() {
        let mut plain_a = test_stream(1, "127.0.0.1:1234", Some("tok-a"), PlaylistItemType::Video);
        plain_a.channel.url = "http://localhost/movie.ts".intern();
        let mut plain_b = test_stream(2, "127.0.0.1:5678", Some("tok-b"), PlaylistItemType::Series);
        plain_b.channel.url = "http://localhost/series.ts".intern();
        let sticky = test_stream(3, "127.0.0.1:9999", Some("tok-catchup"), PlaylistItemType::Catchup);
        let mut status = shared::model::StatusCheck {
            active_users: 1,
            active_user_connections: 1,
            active_user_streams: vec![plain_a, plain_b, sticky.clone()],
            ..Default::default()
        };

        apply_active_user_change(&mut status, ActiveUserConnectionChange::Connections(0, 0));

        assert_eq!(status.active_users, 0);
        assert_eq!(status.active_user_connections, 0);
        assert_eq!(status.active_user_streams.len(), 1);
        assert_eq!(status.active_user_streams[0].uid, sticky.uid);
        assert!(status.active_user_streams[0].preserved);
    }

    #[test]
    fn test_connections_zero_soft_preserves_sticky_and_drops_plain_rows_keeps_already_preserved_row() {
        let mut preserved = test_stream(1, "127.0.0.1:1234", Some("tok-hls"), PlaylistItemType::LiveHls);
        preserved.preserved = true;
        let mut plain = test_stream(2, "127.0.0.1:5678", Some("tok-vod"), PlaylistItemType::Video);
        plain.channel.url = "http://localhost/movie.ts".intern();
        let active_catchup = test_stream(3, "127.0.0.1:9999", Some("tok-catchup"), PlaylistItemType::Catchup);
        let mut status = shared::model::StatusCheck {
            active_users: 1,
            active_user_connections: 1,
            active_user_streams: vec![preserved.clone(), plain, active_catchup.clone()],
            ..Default::default()
        };

        apply_active_user_change(&mut status, ActiveUserConnectionChange::Connections(1, 0));

        assert_eq!(status.active_users, 1);
        assert_eq!(status.active_user_connections, 0);
        assert_eq!(status.active_user_streams.len(), 2);
        assert!(status.active_user_streams.iter().all(|stream| stream.preserved));
        assert!(status.active_user_streams.iter().any(|stream| stream.uid == preserved.uid));
        assert!(status.active_user_streams.iter().any(|stream| stream.uid == active_catchup.uid));
    }

    #[test]
    fn test_preserved_adaptive_update_stays_visible_for_frontend_ttl_cleanup() {
        let mut preserved = test_stream(1, "127.0.0.1:1234", Some("tok-hls"), PlaylistItemType::LiveHls);
        preserved.preserved = true;
        let mut other = test_stream(2, "127.0.0.1:5678", Some("tok-other"), PlaylistItemType::LiveHls);
        other.channel.virtual_id = 2;
        let mut status = shared::model::StatusCheck {
            active_users: 2,
            active_user_connections: 2,
            active_user_streams: vec![
                test_stream(1, "127.0.0.1:1234", Some("tok-hls"), PlaylistItemType::LiveHls),
                other.clone(),
            ],
            ..Default::default()
        };

        apply_active_user_change(&mut status, ActiveUserConnectionChange::Updated(preserved));

        assert_eq!(status.active_user_streams.len(), 2);
        assert!(status
            .active_user_streams
            .iter()
            .any(|stream| stream.addr == "127.0.0.1:1234".parse::<SocketAddr>().unwrap_or_else(|_| unreachable!())
                && stream.preserved));
        assert!(status.active_user_streams.iter().any(|stream| stream == &other));
        assert_eq!(status.active_users, 2);
        assert_eq!(status.active_user_connections, 2);
    }

    #[test]
    fn test_preserved_non_adaptive_update_is_ignored_without_clearing_other_rows() {
        let mut preserved = test_stream(1, "127.0.0.1:1234", Some("tok-vod"), PlaylistItemType::Video);
        preserved.preserved = true;
        let other = test_stream(2, "127.0.0.1:5678", Some("tok-other"), PlaylistItemType::LiveHls);
        let mut status = shared::model::StatusCheck {
            active_users: 2,
            active_user_connections: 2,
            active_user_streams: vec![
                test_stream(1, "127.0.0.1:1234", Some("tok-vod"), PlaylistItemType::Video),
                other.clone(),
            ],
            ..Default::default()
        };
        let before = status.active_user_streams.clone();

        apply_active_user_change(&mut status, ActiveUserConnectionChange::Updated(preserved));

        // Non-sticky preserved updates must not delete panel rows (that path blanked archive Streams).
        assert_eq!(status.active_user_streams, before);
        assert_eq!(status.active_users, 2);
        assert_eq!(status.active_user_connections, 2);
    }

    #[test]
    fn test_preserved_shared_hls_update_keeps_stream_visible() {
        let mut preserved = test_shared_hls_stream(1, "127.0.0.1:1234", Some("tok-hls"));
        preserved.preserved = true;
        let mut status = shared::model::StatusCheck {
            active_users: 1,
            active_user_connections: 1,
            active_user_streams: vec![test_shared_hls_stream(1, "127.0.0.1:1234", Some("tok-hls"))],
            ..Default::default()
        };

        apply_active_user_change(&mut status, ActiveUserConnectionChange::Updated(preserved.clone()));

        assert_eq!(status.active_user_streams, vec![preserved]);
    }

    #[test]
    fn test_preserved_catchup_update_stays_visible_between_archive_segments() {
        let mut preserved = test_stream(1, "127.0.0.1:1234", Some("tok-catchup"), PlaylistItemType::Catchup);
        preserved.preserved = true;
        let mut other = test_stream(2, "127.0.0.1:5678", Some("tok-live"), PlaylistItemType::LiveHls);
        other.channel.virtual_id = 2;
        let mut status = shared::model::StatusCheck {
            active_users: 2,
            active_user_connections: 2,
            active_user_streams: vec![
                test_stream(1, "127.0.0.1:1234", Some("tok-catchup"), PlaylistItemType::Catchup),
                other.clone(),
            ],
            ..Default::default()
        };

        apply_active_user_change(&mut status, ActiveUserConnectionChange::Updated(preserved.clone()));

        assert_eq!(status.active_user_streams.len(), 2);
        assert!(status.active_user_streams.iter().any(|stream| stream.uid == preserved.uid
            && stream.preserved
            && stream.channel.item_type == PlaylistItemType::Catchup));
        assert!(status.active_user_streams.iter().any(|stream| stream == &other));
    }

    #[test]
    fn test_connections_zero_keeps_preserved_catchup_rows_for_ttl_cleanup() {
        let mut preserved = test_stream(1, "127.0.0.1:1234", Some("tok-catchup"), PlaylistItemType::Catchup);
        preserved.preserved = true;
        let mut non_session = test_stream(2, "127.0.0.1:5678", Some("tok-vod"), PlaylistItemType::Video);
        non_session.channel.virtual_id = 2;
        let mut status = shared::model::StatusCheck {
            active_users: 1,
            active_user_connections: 1,
            active_user_streams: vec![preserved.clone(), non_session],
            ..Default::default()
        };

        apply_active_user_change(&mut status, ActiveUserConnectionChange::Connections(1, 0));

        assert_eq!(status.active_user_streams.len(), 1);
        assert!(status.active_user_streams.iter().any(|s| s.uid == preserved.uid));
    }

    #[test]
    fn test_connections_zero_soft_preserves_active_catchup_before_disconnect_event() {
        // Reproduce backend ordering: Connections(0) can arrive before DisconnectedStream.
        let active = test_stream(1, "127.0.0.1:1234", Some("m3u-catchup|archive|100|3600"), PlaylistItemType::Catchup);
        let mut vod = test_stream(2, "127.0.0.1:5678", Some("tok-vod"), PlaylistItemType::Video);
        vod.channel.url = "http://localhost/movie.ts".intern();
        let mut status = shared::model::StatusCheck {
            active_users: 1,
            active_user_connections: 1,
            active_user_streams: vec![active.clone(), vod],
            ..Default::default()
        };

        apply_active_user_change(&mut status, ActiveUserConnectionChange::Connections(1, 0));

        assert_eq!(status.active_user_streams.len(), 1);
        assert_eq!(status.active_user_connections, 0);
        assert!(status.active_user_streams.iter().any(|s| s.uid == active.uid && s.preserved));
    }

    #[test]
    fn test_connections_zero_soft_preserves_live_typed_catchup_session_token() {
        let active = test_stream(1, "127.0.0.1:1234", Some("m3u-catchup|x|archive|100|3600"), PlaylistItemType::Live);
        let mut status = shared::model::StatusCheck {
            active_users: 1,
            active_user_connections: 1,
            active_user_streams: vec![active.clone()],
            ..Default::default()
        };

        apply_active_user_change(&mut status, ActiveUserConnectionChange::Connections(1, 0));

        assert_eq!(status.active_user_streams.len(), 1);
        assert!(status.active_user_streams[0].preserved);
        assert_eq!(status.active_user_streams[0].uid, active.uid);
        assert_eq!(status.active_user_connections, 0);
    }

    #[test]
    fn test_server_status_snapshot_does_not_resurrect_sticky_omitted_by_backend() {
        let mut preserved = test_stream(1, "127.0.0.1:1234", Some("tok-catchup"), PlaylistItemType::Catchup);
        preserved.preserved = true;
        let mut status_holder = Some(Rc::new(shared::model::StatusCheck {
            active_users: 1,
            active_user_connections: 0,
            active_user_streams: vec![preserved],
            ..Default::default()
        }));
        let empty_backend_snapshot = shared::model::StatusCheck {
            active_users: 0,
            active_user_connections: 0,
            active_user_streams: vec![],
            ..Default::default()
        };

        let status = replace_server_status_snapshot(&mut status_holder, empty_backend_snapshot, &[]);

        assert!(status.active_user_streams.is_empty());
    }

    #[test]
    fn test_connections_zero_soft_preserves_catchup_without_session_token() {
        let active = test_stream(1, "127.0.0.1:1234", None, PlaylistItemType::Catchup);
        let mut status = shared::model::StatusCheck {
            active_users: 1,
            active_user_connections: 1,
            active_user_streams: vec![active.clone()],
            ..Default::default()
        };

        apply_active_user_change(&mut status, ActiveUserConnectionChange::Connections(1, 0));

        assert_eq!(status.active_user_streams.len(), 1);
        assert!(status.active_user_streams[0].preserved);
        assert_eq!(status.active_user_streams[0].uid, active.uid);
        assert_eq!(status.active_user_connections, 0);
    }

    #[test]
    fn test_catchup_update_remaps_by_virtual_id_across_segment_addr_churn() {
        let previous = test_stream(1, "127.0.0.1:1111", None, PlaylistItemType::Catchup);
        let mut next = test_stream(99, "127.0.0.1:2222", None, PlaylistItemType::Catchup);
        next.channel.virtual_id = previous.channel.virtual_id;
        next.username = previous.username.clone();
        let mut status = shared::model::StatusCheck {
            active_users: 1,
            active_user_connections: 1,
            active_user_streams: vec![previous],
            ..Default::default()
        };

        apply_active_user_change(&mut status, ActiveUserConnectionChange::Updated(next.clone()));

        assert_eq!(status.active_user_streams.len(), 1);
        assert_eq!(status.active_user_streams[0].uid, next.uid);
        assert_eq!(status.active_user_streams[0].addr, next.addr);
    }

    #[test]
    fn test_active_update_drops_preserved_previous_channel_on_zap() {
        let mut previous = test_stream(1, "127.0.0.1:1111", Some("tok-old"), PlaylistItemType::LiveHls);
        previous.preserved = true;
        previous.channel.virtual_id = 100;
        previous.channel.title = "Old Channel".intern();
        let mut next = test_stream(2, "127.0.0.1:2222", Some("tok-new"), PlaylistItemType::LiveHls);
        next.channel.virtual_id = 200;
        next.channel.title = "New Channel".intern();
        let mut status = shared::model::StatusCheck {
            active_users: 1,
            active_user_connections: 1,
            active_user_streams: vec![previous],
            ..Default::default()
        };

        apply_active_user_change(&mut status, ActiveUserConnectionChange::Updated(next.clone()));

        assert_eq!(status.active_user_streams.len(), 1);
        assert_eq!(status.active_user_streams[0].uid, next.uid);
        assert_eq!(status.active_user_streams[0].channel.virtual_id, 200);
        assert!(!status.active_user_streams[0].preserved);
    }

    #[test]
    fn test_active_update_keeps_preserved_same_channel_during_segment_gap() {
        let mut previous = test_stream(1, "127.0.0.1:1111", Some("tok-hls"), PlaylistItemType::LiveHls);
        previous.preserved = true;
        previous.channel.virtual_id = 100;
        // Another user's preserved row must stay.
        let mut other_user = test_stream(9, "127.0.0.1:9999", Some("tok-other"), PlaylistItemType::LiveHls);
        other_user.preserved = true;
        other_user.username = "other".to_string();
        other_user.channel.virtual_id = 999;
        let mut next = test_stream(2, "127.0.0.1:2222", Some("tok-hls-next"), PlaylistItemType::LiveHls);
        next.channel.virtual_id = 100;
        let mut status = shared::model::StatusCheck {
            active_users: 2,
            active_user_connections: 1,
            active_user_streams: vec![previous.clone(), other_user.clone()],
            ..Default::default()
        };

        apply_active_user_change(&mut status, ActiveUserConnectionChange::Updated(next.clone()));

        assert_eq!(status.active_user_streams.len(), 2);
        assert!(status.active_user_streams.iter().any(|s| s.uid == next.uid && s.channel.virtual_id == 100));
        assert!(status.active_user_streams.iter().any(|s| s.uid == other_user.uid && s.preserved));
    }

    #[test]
    fn test_disconnected_stream_soft_preserves_catchup_between_segments() {
        let active = test_stream(1, "127.0.0.1:1234", Some("tok-catchup"), PlaylistItemType::Catchup);
        let other = test_stream(2, "127.0.0.1:5678", Some("tok-live"), PlaylistItemType::LiveHls);
        let mut status = shared::model::StatusCheck {
            active_users: 2,
            active_user_connections: 2,
            active_user_streams: vec![active.clone(), other.clone()],
            ..Default::default()
        };

        apply_active_user_change(
            &mut status,
            ActiveUserConnectionChange::DisconnectedStream { addr: active.addr, uid: active.uid },
        );

        assert_eq!(status.active_user_streams.len(), 2);
        assert!(status.active_user_streams.iter().any(|stream| {
            stream.uid == active.uid && stream.preserved && stream.channel.item_type == PlaylistItemType::Catchup
        }));
        assert!(status.active_user_streams.iter().any(|stream| stream == &other));
    }

    #[test]
    fn test_disconnected_stream_hard_removes_already_preserved_on_session_expiry() {
        let mut preserved = test_stream(1, "127.0.0.1:1234", Some("tok-catchup"), PlaylistItemType::Catchup);
        preserved.preserved = true;
        let other = test_stream(2, "127.0.0.1:5678", Some("tok-live"), PlaylistItemType::LiveHls);
        let mut status = shared::model::StatusCheck {
            active_users: 1,
            active_user_connections: 0,
            active_user_streams: vec![preserved.clone(), other.clone()],
            ..Default::default()
        };

        apply_active_user_change(
            &mut status,
            ActiveUserConnectionChange::DisconnectedStream { addr: preserved.addr, uid: preserved.uid },
        );

        assert_eq!(status.active_user_streams, vec![other]);
    }

    #[test]
    fn test_disconnected_addr_soft_preserves_catchup_and_drops_plain_live() {
        let catchup = test_stream(1, "127.0.0.1:1234", Some("tok-catchup"), PlaylistItemType::Catchup);
        let mut live = test_stream(2, "127.0.0.1:1234", Some("tok-live"), PlaylistItemType::Live);
        live.channel.url = "http://localhost/live.ts".intern();
        let mut status = shared::model::StatusCheck {
            active_users: 1,
            active_user_connections: 2,
            active_user_streams: vec![catchup.clone(), live],
            ..Default::default()
        };

        apply_active_user_change(&mut status, ActiveUserConnectionChange::Disconnected(catchup.addr));

        assert_eq!(status.active_user_streams.len(), 1);
        assert!(status.active_user_streams[0].preserved);
        assert_eq!(status.active_user_streams[0].channel.item_type, PlaylistItemType::Catchup);
    }

    #[test]
    fn test_disconnected_addr_hard_removes_already_preserved_sticky() {
        let mut preserved = test_stream(1, "127.0.0.1:1234", Some("tok-catchup"), PlaylistItemType::Catchup);
        preserved.preserved = true;
        let other = test_stream(2, "127.0.0.1:5678", Some("tok-live"), PlaylistItemType::LiveHls);
        let mut status = shared::model::StatusCheck {
            active_users: 1,
            active_user_connections: 0,
            active_user_streams: vec![preserved.clone(), other.clone()],
            ..Default::default()
        };

        apply_active_user_change(&mut status, ActiveUserConnectionChange::Disconnected(preserved.addr));

        assert_eq!(status.active_user_streams, vec![other]);
    }

    #[test]
    fn test_disconnected_addr_evaluates_matching_streams_independently() {
        let mut expired = test_stream(1, "127.0.0.1:1234", Some("tok-old"), PlaylistItemType::Catchup);
        expired.preserved = true;
        let active = test_stream(2, "127.0.0.1:1234", Some("tok-new"), PlaylistItemType::LiveHls);
        let mut status = shared::model::StatusCheck {
            active_users: 1,
            active_user_connections: 2,
            active_user_streams: vec![expired, active.clone()],
            ..Default::default()
        };

        apply_active_user_change(&mut status, ActiveUserConnectionChange::Disconnected(active.addr));

        assert_eq!(status.active_user_streams.len(), 1);
        assert_eq!(status.active_user_streams[0].uid, active.uid);
        assert!(status.active_user_streams[0].preserved);
    }

    #[test]
    fn test_disconnected_stream_removes_only_matching_stream_identity() {
        let removed = test_stream(1, "127.0.0.1:1234", Some("tok-live-a"), PlaylistItemType::Live);
        let kept = test_stream(2, "127.0.0.1:1234", Some("tok-live-b"), PlaylistItemType::Live);
        let mut status = shared::model::StatusCheck {
            active_users: 1,
            active_user_connections: 2,
            active_user_streams: vec![removed.clone(), kept.clone()],
            ..Default::default()
        };

        apply_active_user_change(
            &mut status,
            ActiveUserConnectionChange::DisconnectedStream { addr: removed.addr, uid: removed.uid },
        );

        assert_eq!(status.active_user_streams, vec![kept]);
    }

    #[test]
    fn server_status_snapshot_replaces_stale_backend_streams_and_readds_current_downloads() {
        let stale_stream = test_stream(1, "127.0.0.1:1234", Some("tok-series"), PlaylistItemType::Series);
        let download_stream = download_task_to_stream_with_ts(
            &test_download("running", TransferStatusDto::Running, TaskKindDto::Download),
            123,
        );
        let mut status_holder = Some(Rc::new(shared::model::StatusCheck {
            active_users: 1,
            active_user_connections: 1,
            active_user_streams: vec![stale_stream],
            ..Default::default()
        }));
        let clean_backend_snapshot = shared::model::StatusCheck::default();

        let status = replace_server_status_snapshot(
            &mut status_holder,
            clean_backend_snapshot,
            std::slice::from_ref(&download_stream),
        );

        assert_eq!(status.active_users, 0);
        assert_eq!(status.active_user_connections, 0);
        assert_eq!(status.active_user_streams, vec![download_stream]);
        assert_eq!(status_holder.as_deref(), Some(status.as_ref()));
    }

    fn test_download(id: &str, status: TransferStatusDto, kind: TaskKindDto) -> FileDownloadDto {
        FileDownloadDto {
            id: id.to_string(),
            title: format!("{id}.ts"),
            kind,
            recording_type: if kind == TaskKindDto::Recording {
                shared::model::RecordingTypeDto::Live
            } else {
                shared::model::RecordingTypeDto::Vod
            },
            priority: TaskPriorityDto::Background,
            status,
            retry_attempts: 0,
            downloaded_bytes: 128,
            total_bytes: Some(1024),
            next_retry_at: None,
            scheduled_start_at: None,
            duration_secs: None,
            error: None,
            recording: None,
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
        assert_eq!(streams[0].provider.as_ref(), "Download Manager");
        assert_eq!(streams[0].client_ip, "background-task");
        assert_eq!(streams[0].channel.item_type, PlaylistItemType::Video);
    }

    #[test]
    fn downloads_delta_updates_only_matching_pseudo_stream() {
        let mut streams = Vec::new();
        apply_downloads_snapshot(
            &mut streams,
            &DownloadsResponse {
                queue: Vec::new(),
                finished: Vec::new(),
                active: vec![
                    test_download("one", TransferStatusDto::Running, TaskKindDto::Download),
                    test_download("two", TransferStatusDto::Running, TaskKindDto::Recording),
                ],
            },
        );

        apply_downloads_delta(
            &mut streams,
            &DownloadsDelta::ActivePatched(test_download("one", TransferStatusDto::Paused, TaskKindDto::Download)),
        );

        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].channel.title.as_ref(), "two.ts");
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

    #[test]
    fn downloads_delta_preserves_pseudo_stream_start_timestamp() {
        let mut streams = vec![download_task_to_stream_with_ts(
            &test_download("running", TransferStatusDto::Running, TaskKindDto::Download),
            123,
        )];

        apply_downloads_delta(
            &mut streams,
            &DownloadsDelta::ActivePatched(test_download("running", TransferStatusDto::Running, TaskKindDto::Download)),
        );

        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].ts, 123);
    }

    #[test]
    fn downloads_snapshot_preserves_pseudo_stream_start_timestamp() {
        let mut streams = vec![download_task_to_stream_with_ts(
            &test_download("running", TransferStatusDto::Running, TaskKindDto::Recording),
            456,
        )];

        apply_downloads_snapshot(
            &mut streams,
            &DownloadsResponse {
                queue: Vec::new(),
                finished: Vec::new(),
                active: vec![test_download("running", TransferStatusDto::Running, TaskKindDto::Recording)],
            },
        );

        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].ts, 456);
    }
}
