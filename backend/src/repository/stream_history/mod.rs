mod archive;
mod writer;
mod storage;
mod reader;
mod async_iterator;

pub use writer::*;
pub use archive::*;
pub use storage::*;
pub use reader::*;
pub use async_iterator::*;

#[cfg(test)]
pub(crate) mod tests {
    use crate::model::{StreamHistoryRecord, RECORD_SCHEMA_VERSION};
    use shared::model::StreamHistoryEventType;

    pub fn make_base_test_record() -> StreamHistoryRecord {
        StreamHistoryRecord {
            schema_version: RECORD_SCHEMA_VERSION,
            event_type: StreamHistoryEventType::Connect,
            event_ts_utc: 1_742_600_001,
            partition_day_utc: "2025-03-21".to_string(),
            session_id: 1,
            source_addr: None,
            api_username: None,
            provider_name: None,
            provider_username: None,
            input_name: None,
            virtual_id: None,
            item_type: None,
            title: None,
            group: None,
            country: None,
            user_agent: None,
            shared: None,
            shared_joined_existing: None,
            shared_stream_id: None,
            provider_id: None,
            cluster: None,
            container: None,
            stream_url_hash: None,
            stream_identity_key: None,
            video_codec: None,
            audio_codec: None,
            audio_channels: None,
            resolution: None,
            fps: None,
            connect_ts_utc: None,
            disconnect_ts_utc: None,
            session_duration: None,
            bytes_sent: None,
            first_byte_latency_ms: None,
            provider_reconnect_count: None,
            failure_stage: None,
            provider_http_status: None,
            provider_error_class: None,
            connect_failure_reason: None,
            disconnect_reason: None,
            previous_session_id: None,
            target_name: None,
        }
    }

    pub fn sample_connect_record() -> StreamHistoryRecord {
        let mut r = make_base_test_record();
        r.event_ts_utc = 1_742_600_001;
        r.session_id = 999;
        r.source_addr = Some("192.0.2.1:12345".to_string());
        r.api_username = Some("alice".to_string());
        r.provider_name = Some(shared::utils::Internable::intern("acme-tv"));
        r.provider_username = Some("acme_user".to_string());
        r.input_name = Some(shared::utils::Internable::intern("provider-input"));
        r.virtual_id = Some(1234);
        r.item_type = Some(shared::model::PlaylistItemType::Live);
        r.title = Some("News Channel".to_string());
        r.group = Some("News".to_string());
        r.country = Some("DE".to_string());
        r.user_agent = Some("VLC/3.0".to_string());
        r.shared = Some(false);
        r.provider_id = Some(1);
        r.cluster = Some("live".to_string());
        r.container = Some("mpegts".to_string());
        r.stream_url_hash = Some("abc123".to_string());
        r.stream_identity_key = Some("identity123".to_string());
        r.video_codec = Some("H.264".to_string());
        r.audio_codec = Some("AAC".to_string());
        r.audio_channels = Some("STEREO".to_string());
        r.resolution = Some("1920x1080".to_string());
        r.fps = Some("50".to_string());
        r.connect_ts_utc = Some(1_742_600_001);
        r
    }

    pub fn sample_disconnect_record(session_id: u64) -> StreamHistoryRecord {
        let mut r = sample_connect_record();
        r.event_type = StreamHistoryEventType::Disconnect;
        r.event_ts_utc = 1_742_603_601;
        r.partition_day_utc = "2025-03-22".to_string();
        r.session_id = session_id;
        r.disconnect_ts_utc = Some(1_742_603_601);
        r.session_duration = Some(3600);
        r.bytes_sent = Some(1_234_567_890);
        r.first_byte_latency_ms = Some(150);
        r.provider_reconnect_count = Some(0);
        r.disconnect_reason = Some(shared::model::DisconnectReason::ClientClosed);
        r
    }

    pub fn sample_stream_info() -> shared::model::StreamInfo {
        let addr: std::net::SocketAddr = "192.0.2.1:12345".parse().unwrap();
        let mut info = shared::model::StreamInfo::new(shared::model::StreamInfoParams {
            uid: 999,
            meter_uid: 1001,
            username: "alice",
            addr: &addr,
            client_ip: "192.0.2.1",
            provider: shared::utils::Internable::intern("acme-tv"),
            stream_channel: shared::model::StreamChannel {
                target_id: 1,
                virtual_id: 1234,
                provider_id: 1,
                input_name: shared::utils::Internable::intern("provider-input"),
                item_type: shared::model::PlaylistItemType::Live,
                cluster: shared::model::XtreamCluster::Live,
                group: shared::utils::Internable::intern("News"),
                title: shared::utils::Internable::intern("News Channel"),
                url: shared::utils::Internable::intern("http://localhost/stream.ts"),
                shared: false,
                shared_joined_existing: None,
                shared_stream_id: None,
                technical: Some(shared::model::StreamTechnicalInfo {
                    container: "mpegts".to_string(),
                    resolution: "1920x1080".to_string(),
                    fps: "50".to_string(),
                    video_codec: "H.264".to_string(),
                    audio_codec: "AAC".to_string(),
                    audio_channels: "STEREO".to_string(),
                }),
                epg_channel_id: None,
                epg_reference_ts: None,
                source_user_agent: None,
            },
            user_agent: String::from("VLC/3.0"),
            country_code: Some(String::from("DE")),
            session_token: None,
        });
        info.ts = 1_742_600_001;
        info
    }
}
