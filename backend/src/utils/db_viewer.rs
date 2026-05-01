use crate::api::model::{MetadataRetryDbKey, MetadataRetryDbValue};
use crate::repository::{BPlusTreeDiskIterator, BPlusTreeQuery, QosSnapshotRecord, VirtualIdRecord};
use base64::{engine::general_purpose, Engine as _};
use env_logger::{Builder, Target};
use lz4_flex::decompress_size_prepended;
use log::{error, LevelFilter};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use shared::model::{EpgChannel, M3uPlaylistItem, XtreamPlaylistItem};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

type DumpFn = fn(&Path) -> bool;

struct DumpRequest<'a> {
    filename: Option<&'a str>,
    label: &'static str,
    dump_fn: DumpFn,
}

pub struct DbViewerArgs<'a> {
    pub xtream_filename: Option<&'a str>,
    pub m3u_filename: Option<&'a str>,
    pub epg_filename: Option<&'a str>,
    pub tim_filename: Option<&'a str>,
    pub metadata_status_filename: Option<&'a str>,
    pub qos_snapshot_filename: Option<&'a str>,
}

impl<'a> DbViewerArgs<'a> {
    pub const fn new(
        xtream_filename: Option<&'a str>,
        m3u_filename: Option<&'a str>,
        epg_filename: Option<&'a str>,
        tim_filename: Option<&'a str>,
        metadata_status_filename: Option<&'a str>,
        qos_snapshot_filename: Option<&'a str>,
    ) -> Self {
        Self {
            xtream_filename,
            m3u_filename,
            epg_filename,
            tim_filename,
            metadata_status_filename,
            qos_snapshot_filename,
        }
    }
}

pub fn db_viewer(args: &DbViewerArgs<'_>) {
    let requests = [
        DumpRequest {
            filename: args.xtream_filename,
            label: "xtream",
            dump_fn: dump_xtream_db,
        },
        DumpRequest {
            filename: args.m3u_filename,
            label: "m3u",
            dump_fn: dump_m3u_db,
        },
        DumpRequest {
            filename: args.epg_filename,
            label: "epg",
            dump_fn: dump_epg_db,
        },
        DumpRequest {
            filename: args.tim_filename,
            label: "target_id_mapping",
            dump_fn: dump_target_mapping_db,
        },
        DumpRequest {
            filename: args.metadata_status_filename,
            label: "metadata_status",
            dump_fn: dump_metadata_status_db,
        },
        DumpRequest {
            filename: args.qos_snapshot_filename,
            label: "qos_snapshot",
            dump_fn: dump_qos_snapshot_db,
        },
    ];

    let any_requested = requests.iter().any(|request| request.filename.is_some());
    if !any_requested {
        return;
    }

    init_db_viewer_logger();

    let mut any_processed = false;
    for request in requests {
        if let Some(filename) = request.filename {
            any_processed = true;
            if !dump_db(filename, request.label, request.dump_fn) {
                exit_app(1);
            }
        }
    }

    if any_processed {
        exit_app(0);
    }
}

fn init_db_viewer_logger() {
    let mut log_builder = Builder::from_default_env();
    log_builder.target(Target::Stderr);
    log_builder.filter_level(LevelFilter::Info);
    let _ = log_builder.try_init();
}

fn try_dump_typed_db<K, V>(path: &Path) -> bool
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    if let Ok(mut query) = BPlusTreeQuery::<K, V>::try_new(path) {
        return print_json_from_iter(query.iter());
    }
    false
}

fn try_dump_m3u_with_key<K>(path: &Path) -> Result<bool, String>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
{
    let mut query = BPlusTreeQuery::<K, M3uPlaylistItem>::try_new(path).map_err(|err| err.to_string())?;
    query.len().map_err(|err| err.to_string())?;
    Ok(print_json_from_iter(query.iter()))
}

fn dump_xtream_db(path: &Path) -> bool { try_dump_typed_db::<u32, XtreamPlaylistItem>(path) }

fn dump_m3u_db(path: &Path) -> bool {
    // M3U DB keys can be u32 (target playlists) or Arc<str> (input playlists).
    let err_u32 = match try_dump_m3u_with_key::<u32>(path) {
        Ok(result) => return result,
        Err(err) => Some(err),
    };
    let err_str = match try_dump_m3u_with_key::<Arc<str>>(path) {
        Ok(result) => return result,
        Err(err) => Some(err),
    };

    error!(
        "Failed to open M3U DB with any known key type at {}: u32_err={:?}, string_err={:?}",
        path.display(),
        err_u32,
        err_str
    );
    false
}

fn dump_epg_db(path: &Path) -> bool { try_dump_typed_db::<Arc<str>, EpgChannel>(path) }

fn dump_target_mapping_db(path: &Path) -> bool { try_dump_typed_db::<u32, VirtualIdRecord>(path) }

fn dump_metadata_status_db(path: &Path) -> bool {
    try_dump_typed_db::<MetadataRetryDbKey, MetadataRetryDbValue>(path)
}

fn dump_qos_snapshot_db(path: &Path) -> bool { try_dump_typed_db::<String, QosSnapshotRecord>(path) }

fn dump_db(filename: &str, label: &str, dump_fn: DumpFn) -> bool {
    match PathBuf::from(filename).canonicalize() {
        Ok(path) => {
            if !dump_fn(&path) {
                error!("Failed to dump {label} DB at {}", path.display());
                return false;
            }
            true
        }
        Err(err) => {
            error!("Invalid file path for {label} DB: {err}");
            false
        }
    }
}

fn print_json_from_iter<K, P>(iterator: BPlusTreeDiskIterator<K, P>) -> bool
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    P: Serialize + for<'de> Deserialize<'de> + Clone,
{
    let mut error_count = 0;

    println!("[");
    let mut first = true;
    for (_, entry) in iterator {
        match to_human_readable_json_value(&entry).and_then(|value| serde_json::to_string(&value)) {
            Ok(json) => {
                if !first {
                    println!(",");
                }
                println!("{json}");
                first = false;
            }
            Err(err) => {
                error!("Failed: {err}");
                error_count += 1;
            }
        }
    }
    println!("]");

    error_count == 0
}

fn to_human_readable_json_value<T: Serialize>(entry: &T) -> Result<Value, serde_json::Error> {
    let mut value = serde_json::to_value(entry)?;
    humanize_dump_value(&mut value);
    Ok(value)
}

fn humanize_dump_value(value: &mut Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                humanize_dump_value(item);
            }
        }
        Value::Object(fields) => {
            for (key, field_value) in fields {
                if matches!(key.as_str(), "video" | "audio") {
                    decode_storage_json_field(field_value);
                } else {
                    humanize_dump_value(field_value);
                }
            }
        }
        _ => {}
    }
}

fn decode_storage_json_field(value: &mut Value) {
    let Value::String(encoded) = value else {
        return;
    };

    let Ok(compressed) = general_purpose::STANDARD_NO_PAD.decode(encoded) else {
        return;
    };
    let Ok(decompressed) = decompress_size_prepended(&compressed) else {
        return;
    };
    let Ok(text) = String::from_utf8(decompressed) else {
        return;
    };

    *value = serde_json::from_str(&text).unwrap_or(Value::String(text));
    humanize_dump_value(value);
}

fn exit_app(code: i32) {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::{dump_qos_snapshot_db, to_human_readable_json_value};
    use crate::repository::{BPlusTree, QosSnapshotDailyBucket, QosSnapshotRecord, QosSnapshotWindow};
    use base64::{engine::general_purpose, Engine as _};
    use lz4_flex::compress_prepend_size;
    use serde_json::{json, Value};
    use shared::{
        model::{LiveStreamProperties, PlaylistItemType, StreamProperties, XtreamCluster, XtreamPlaylistItem},
        utils::Internable,
    };
    use tempfile::tempdir;

    fn encode_storage_json(text: &str) -> String {
        general_purpose::STANDARD_NO_PAD.encode(compress_prepend_size(text.as_bytes()))
    }

    #[test]
    fn dump_qos_snapshot_db_reads_bplustree_records() {
        let temp = tempdir().expect("tempdir should succeed");
        let path = temp.path().join("qos_snapshot.db");

        let mut tree = BPlusTree::<String, QosSnapshotRecord>::new();
        let record = QosSnapshotRecord {
            stream_identity_key: "stream-a".to_string(),
            input_name: "input-a".intern(),
            target_name: "target-a".intern(),
            provider_name: "provider-a".intern(),
            provider_id: 22,
            virtual_id: 33,
            item_type: PlaylistItemType::Live,
            updated_at: 1_700_000_000,
            last_event_at: 1_700_000_001,
            window_24h: QosSnapshotWindow {
                score: 87,
                confidence: 70,
                ..QosSnapshotWindow::default()
            },
            window_7d: QosSnapshotWindow::default(),
            window_30d: QosSnapshotWindow::default(),
            daily_buckets: std::collections::BTreeMap::from([(
                "2026-04-02".to_string(),
                QosSnapshotDailyBucket::default(),
            )]),
        };
        tree.insert(record.stream_identity_key.clone(), record);
        tree.store(&path).expect("tree should store");

        assert!(dump_qos_snapshot_db(&path));
    }

    #[test]
    fn human_readable_dump_decodes_compressed_video_and_audio_fields() {
        let item = XtreamPlaylistItem {
            virtual_id: 42,
            provider_id: 52_568,
            name: "Example".intern(),
            logo: "".intern(),
            logo_small: "".intern(),
            group: "Movies".intern(),
            title: "Example".intern(),
            parent_code: "".intern(),
            rec: "".intern(),
            url: "https://example.invalid/movie.mkv".intern(),
            epg_channel_id: None,
            xtream_cluster: XtreamCluster::Live,
            additional_properties: Some(StreamProperties::Live(Box::new(LiveStreamProperties {
                video: Some(r#"{"codec_name":"h264","width":1920,"height":1080}"#.intern()),
                audio: Some(r#"{"codec_name":"aac","channels":2}"#.intern()),
                ..LiveStreamProperties::default()
            }))),
            item_type: PlaylistItemType::Live,
            category_id: 7,
            input_name: "input".intern(),
            channel_no: 0,
            source_ordinal: 0,
        };

        let value = to_human_readable_json_value(&item).expect("dump value should serialize");
        let props = value
            .get("additional_properties")
            .and_then(|value| value.get("Live"))
            .expect("live properties should be present");

        assert_eq!(value.get("virtual_id").and_then(Value::as_u64), Some(42));
        assert_eq!(value.get("provider_id").and_then(Value::as_u64), Some(52_568));
        assert_eq!(value.get("name").and_then(Value::as_str), Some("Example"));
        assert_eq!(value.get("group").and_then(Value::as_str), Some("Movies"));
        assert_eq!(props.get("video").and_then(|value| value.get("codec_name")).and_then(Value::as_str), Some("h264"));
        assert_eq!(props.get("audio").and_then(|value| value.get("codec_name")).and_then(Value::as_str), Some("aac"));
    }

    #[test]
    fn human_readable_dump_recurses_after_decoding_storage_json_fields() {
        let nested_audio = encode_storage_json(r#"{"codec_name":"aac","channels":2}"#);
        let video = encode_storage_json(&format!(r#"{{"codec_name":"h264","audio":"{nested_audio}"}}"#));
        let value = json!({ "video": video });

        let value = to_human_readable_json_value(&value).expect("dump value should serialize");

        assert_eq!(value["video"]["codec_name"], "h264");
        assert_eq!(value["video"]["audio"]["codec_name"], "aac");
    }
}
