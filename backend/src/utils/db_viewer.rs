use crate::api::model::{MetadataRetryDbKey, MetadataRetryDbValue};
use crate::repository::{BPlusTreeDiskIterator, BPlusTreeQuery, VirtualIdRecord};
use log::error;
use serde::{Deserialize, Serialize};
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
}

impl<'a> DbViewerArgs<'a> {
    pub const fn new(
        xtream_filename: Option<&'a str>,
        m3u_filename: Option<&'a str>,
        epg_filename: Option<&'a str>,
        tim_filename: Option<&'a str>,
        metadata_status_filename: Option<&'a str>,
    ) -> Self {
        Self {
            xtream_filename,
            m3u_filename,
            epg_filename,
            tim_filename,
            metadata_status_filename,
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
    ];

    let any_requested = requests.iter().any(|request| request.filename.is_some());
    if !any_requested {
        return;
    }

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
        match serde_json::to_string(&entry) {
            Ok(text) => {
                if !first {
                    println!(",");
                }
                println!("{text}");
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

fn exit_app(code: i32) {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code);
}
