use shared::error::{create_tuliprox_error_result, info_err};
use shared::error::{TuliproxError, TuliproxErrorKind};
use crate::model::{ApiProxyServerInfo, AppConfig, ProxyUserCredentials};
use crate::model::{ConfigTarget, StrmTargetOutput};
use crate::model::XtreamSeriesEpisode;
use crate::repository::bplustree::BPlusTree;
use crate::repository::storage::{ensure_target_storage_path, get_input_storage_path};
use crate::repository::storage_const;
use crate::repository::xtream_repository::{xtream_get_record_file_path, InputVodInfoRecord};
use shared::utils::{extract_extension_from_url, hash_bytes, ExportStyleConfig, CONSTANTS};
use crate::utils::FileReadGuard;
use chrono::Datelike;
use filetime::{set_file_times, FileTime};
use log::{error, trace};
use regex::Regex;
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::{create_dir_all, remove_dir, remove_file, File};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use shared::model::{ClusterFlags, FieldGetAccessor, PlaylistGroup, PlaylistItem, PlaylistItemType, StrmExportStyle, UUIDType};
use crate::utils;

/// Sanitizes a string to be safe for use as a file or directory name by
/// following a strict "allow-list" approach and discarding invalid characters.
fn sanitize_for_filename(text: &str, underscore_whitespace: bool) -> String {
    // A default placeholder for filenames that become empty after sanitization.
    const EMPTY_FILENAME_REPLACEMENT: &str = "unnamed";

    // 1. Trim leading/trailing whitespace.
    let trimmed = text.trim();

    // 2. Build the sanitized string by filtering and mapping characters.
    let mut sanitized: String = trimmed
        .chars()
        .filter_map(|c| {
            // Decide which characters to keep or transform.
            if c.is_alphanumeric() {
                Some(c)
            } else if "+=,._-@#()".contains(c) { // <-- Allow list of safe punctuation.
                Some(c)
            } else if c.is_whitespace() {
                if underscore_whitespace {
                    Some('_')
                } else {
                    Some(' ')
                }
            } else {
                // Discard all other characters.
                None
            }
        })
        .collect();

    // 3. Remove any leading periods to prevent creating hidden files/directories.
    while sanitized.starts_with('.') {
        sanitized.remove(0);
    }

    // 4. Final check: If sanitization resulted in an empty string, return a default.
    if sanitized.is_empty() {
        EMPTY_FILENAME_REPLACEMENT.to_string()
    } else {
        sanitized
    }
}

/// Finds and removes the first regex match, returning the modified string and the match.
fn extract_match(name: &str, pattern: &Regex) -> (String, Option<String>) {
    pattern.find(name).map_or_else(
        || (name.to_string(), None),
        |m| {
            let matched = String::from(&name[m.start()..m.end()]);
            let new_name = format!("{}{}", &name[0..m.start()], &name[m.end()..]);
            (new_name, Some(matched))
        },
    )
}

/// Extracts a number from a pattern (e.g., "S01" -> 1), falling back to a default value.
fn extract_season_or_episode_match_with_default(
    name: &str,
    pattern: &Regex,
    default_value: Option<&String>,
) -> (String, Option<u32>) {
    let (new_name, value) = extract_match(name, pattern);
    let new_value = match value {
        // we skip the prefix S or E to parse the num
        Some(num_value) => num_value[1..].parse::<u32>().ok(),
        None => default_value.and_then(|val| val.parse::<u32>().ok()),
    };
    (new_name, new_value)
}

// Extracts and removes an episode number (e.g., "E01") based on style patterns.
fn style_rename_season(
    name: &str,
    style: &ExportStyleConfig,
    series_season: Option<&String>,
) -> (String, Option<u32>) {
    extract_season_or_episode_match_with_default(name, &style.season, series_season)
}

/// Renames episode numbers in media titles according to style patterns
fn style_rename_episode(
    name: &str,
    style: &ExportStyleConfig,
    series_episode: Option<&String>,
) -> (String, Option<u32>) {
    extract_season_or_episode_match_with_default(name, &style.episode, series_episode)
}

/// Extracts and formats year information from media titles
fn style_rename_year<'a>(
    name: &'a str,
    style: &ExportStyleConfig,
    release_date: Option<&'a String>,
) -> (&'a str, Option<u32>) {
    let mut years = Vec::new();

    let cur_year = u32::try_from(chrono::Utc::now().year()).unwrap_or(0);
    let mut new_name = String::with_capacity(name.len());
    let mut last_index = 0;

    for caps in style.year.captures_iter(name) {
        if let Some(year_match) = caps.get(1) {
            if let Ok(year) = year_match.as_str().parse::<u32>() {
                if (1900..=cur_year).contains(&year) {
                    years.push(year);
                    let match_start = caps.get(0).unwrap().start();
                    let match_end = caps.get(0).unwrap().end();
                    new_name.push_str(&name[last_index..match_start]);
                    last_index = match_end;
                }
            }
        }
    }
    new_name.push_str(&name[last_index..]);

    let smallest_year = years.into_iter().min();
    if smallest_year.is_none() {
        if let Some(rel_date) = release_date {
            if let Some(year) = extract_match(rel_date, &style.year)
                .1
                .and_then(|y| y.parse::<u32>().ok())
            {
                return (name, Some(year));
            }
        }
    }

    (Box::leak(new_name.into_boxed_str()), smallest_year)
}

#[derive(Clone)]
enum InputTmdbIndexTree {
    Video(BPlusTree<u32, InputVodInfoRecord>),
    Series(BPlusTree<u32, XtreamSeriesEpisode>),
}

#[derive(Clone)]
enum InputTmdbIndexValue {
    Video(InputVodInfoRecord),
    Series(XtreamSeriesEpisode),
}

type InputTmdbIndexMap = HashMap<String, Option<(FileReadGuard, InputTmdbIndexTree)>>;
async fn get_tmdb_value(
    cfg: &AppConfig,
    provider_id: Option<u32>,
    input_name: &str,
    input_indexes: &mut InputTmdbIndexMap,
    item_type: PlaylistItemType,
) -> Option<InputTmdbIndexValue> {
    // the tmdb_ids are stored inside record files for xtream input.
    // we load this record files on request for each input and item_type.
    let pid = provider_id?;
    match input_indexes.entry(input_name.to_string()) {
        std::collections::hash_map::Entry::Occupied(entry) => {
            if let Some((_, tree_value)) = entry.get() {
                match tree_value {
                    InputTmdbIndexTree::Video(tree) => tree
                        .query(&pid)
                        .map(|vod_record| InputTmdbIndexValue::Video(vod_record.clone())),
                    InputTmdbIndexTree::Series(tree) => tree
                        .query(&pid)
                        .map(|episode| InputTmdbIndexValue::Series(episode.clone())),
                }
            } else {
                None
            }
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
            if let Ok(Some(tmdb_path)) = get_input_storage_path(input_name, &cfg.config.load().working_dir)
                .map(|storage_path| xtream_get_record_file_path(&storage_path, item_type))
            {
                {
                    let file_lock = cfg.file_locks.read_lock(&tmdb_path).await;
                    match item_type {
                        PlaylistItemType::Series => {
                            if let Ok(tree) =
                                BPlusTree::<u32, XtreamSeriesEpisode>::load(&tmdb_path)
                            {
                                let tmdb_id = tree.query(&pid).map(|episode| {
                                    InputTmdbIndexValue::Series(episode.clone())
                                });
                                entry.insert(Some((
                                    file_lock,
                                    InputTmdbIndexTree::Series(tree),
                                )));
                                return tmdb_id;
                            }
                        }
                        PlaylistItemType::Video => {
                            if let Ok(tree) =
                                BPlusTree::<u32, InputVodInfoRecord>::load(&tmdb_path)
                            {
                                let tmdb_id = tree.query(&pid).map(|vod_record| {
                                    InputTmdbIndexValue::Video(vod_record.clone())
                                });
                                entry.insert(Some((
                                    file_lock,
                                    InputTmdbIndexTree::Video(tree),
                                )));
                                return tmdb_id;
                            }
                        }
                        _ => {}
                    }
                };
            }
            entry.insert(None);
            None
        }
    }
}

pub fn strm_get_file_paths(target_path: &Path) -> PathBuf {
    target_path.join(PathBuf::from(format!("{}.{}", storage_const::FILE_STRM, storage_const::FILE_SUFFIX_DB)))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StrmItemInfo {
    group: String,
    title: String,
    item_type: PlaylistItemType,
    provider_id: Option<u32>,
    virtual_id: u32,
    input_name: String,
    url: String,
    series_name: Option<String>,
    release_date: Option<String>,
    season: Option<String>,
    episode: Option<String>,
    added: Option<u64>,
}

impl StrmItemInfo {
    pub(crate) fn get_file_ts(&self) -> Option<u64> {
        self.added
    }
}

fn extract_item_info(pli: &mut PlaylistItem) -> StrmItemInfo {
    let header = &mut pli.header;
    let group = header.group.to_string();
    let title = header.title.to_string();
    let item_type = header.item_type;
    let provider_id = header.get_provider_id();
    let virtual_id = header.virtual_id;
    let input_name = header.input_name.to_string();
    let url = header.url.to_string();
    let (series_name, release_date, added, season, episode) = match header.item_type {
        PlaylistItemType::Series => {
            let series_name = match header.get_field("name") {
                Some(name) if !name.is_empty() => Some(name.to_string()),
                _ => header.get_additional_property_as_str("series_name"),
            };
            let release_date = header
                .get_additional_property_as_str("series_release_date")
                .or_else(|| header.get_additional_property_as_str("release_date"));
            let season = header.get_additional_property_as_str("season");
            let episode = header.get_additional_property_as_str("episode");
            let added = header.get_additional_property_as_u64("added");
            (series_name, release_date, added, season, episode)
        }
        PlaylistItemType::Video => {
            let name = header.get_field("name").map(|v| v.to_string());
            let release_date = header.get_additional_property_as_str("release_date");
            let added = header.get_additional_property_as_u64("added");
            (name, release_date, added, None, None)
        }
        _ => (None, None, None, None, None),
    };
    StrmItemInfo {
        group,
        title,
        item_type,
        provider_id,
        virtual_id,
        input_name,
        url,
        series_name,
        release_date,
        season,
        episode,
        added,
    }
}

async fn prepare_strm_output_directory(path: &Path) -> Result<(), TuliproxError> {
    // Ensure the directory exists
    if let Err(e) = tokio::fs::create_dir_all(path).await {
        error!("Failed to create directory {}: {e}", path.display());
        return create_tuliprox_error_result!(
            TuliproxErrorKind::Notify,
            "Error creating STRM directory: {e}"
        );
    }
    Ok(())
}

async fn read_files_non_recursive(path: &Path) -> tokio::io::Result<Vec<PathBuf>> {
    let mut stack = vec![PathBuf::from(path)]; // Initialize the stack with the starting directory
    let mut files = vec![]; // To store all the found files

    while let Some(current_dir) = stack.pop() {
        // Read the directory
        let mut dir_read = tokio::fs::read_dir(&current_dir).await?;
        // Iterate over the entries in the current directory
        while let Some(entry) = dir_read.next_entry().await? {
            let entry_path = entry.path();
            // If it's a directory, push it onto the stack for later processing
            if entry_path.is_dir() {
                stack.push(entry_path.clone());
            } else {
                // If it's a file, add it to the entries list
                files.push(entry_path);
            }
        }
    }
    Ok(files)
}

async fn cleanup_strm_output_directory(
    cleanup: bool,
    root_path: &Path,
    existing: &HashSet<String>,
    processed: &HashSet<String>,
) -> Result<(), String> {
    if !(root_path.exists() && root_path.is_dir()) {
        return Err(format!(
            "Error: STRM directory does not exist: {}", root_path.display()
        ));
    }

    let to_remove: HashSet<String> = if cleanup {
        // Remove al files which are not in `processed`
        let mut found_files = HashSet::new();
        let files = read_files_non_recursive(root_path).await.map_err(|err| err.to_string())?;
        for file_path in files {
            if let Some(file_name) = file_path
                .strip_prefix(root_path)
                .ok()
                .and_then(|p| p.to_str()) {
                found_files.insert(file_name.to_string());
            }
        }
        &found_files - processed
    } else {
        // Remove all files from `existing`, which are not in `processed`
        existing - processed
    };

    for file in &to_remove {
        let file_path = root_path.join(file);
        if let Err(err) = remove_file(&file_path).await {
            error!("Failed to remove file {}: {err}", file_path.display());
        }
    }

    // TODO should we delete all empty directories if cleanup=false ?
    remove_empty_dirs(root_path.into()).await;
    Ok(())
}

fn filter_strm_item(pli: &PlaylistItem) -> bool {
    let item_type = pli.header.item_type;
    item_type == PlaylistItemType::Live
        || item_type == PlaylistItemType::Video
        || item_type == PlaylistItemType::Series
}

fn get_relative_path_str(full_path: &Path, root_path: &Path) -> String {
    full_path
        .strip_prefix(root_path)
        .map_or_else(
            |_| full_path.to_string_lossy(),
            |relative| relative.to_string_lossy(),
        )
        .to_string()
}

struct StrmFile {
    file_name: Arc<String>,
    dir_path: PathBuf,
    strm_info: StrmItemInfo,
}

/// Formats names according to the official Kodi documentation, with `TMDb` ID for better matching.
/// Movie: /Movie Name (Year) {tmdb=XXXXX}/Movie Name (Year).strm
/// Series: /Show Name (Year) {tmdb=XXXXX}/Season 01/Show Name S01E01.strm
fn format_for_kodi(
    strm_item_info: &StrmItemInfo,
    tmdb_id: u32,
    separator: &str,
    flat: bool,
) -> (PathBuf, String) {
    let mut dir_path = PathBuf::new();
    let category = sanitize_for_filename(&strm_item_info.group, false);

    match strm_item_info.item_type {
        PlaylistItemType::Video => {
            let id_string = if tmdb_id > 0 { format!("{separator}{{tmdb={tmdb_id}}}") } else { String::new() };
            let (name, year) = style_rename_year(&strm_item_info.title, &CONSTANTS.export_style_config, strm_item_info.release_date.as_ref());
            let sanitized_title = sanitize_for_filename(name.trim(), false);
            let year_string = year.map_or(String::new(), |y| format!("{separator}({y})"));

            let base_name = format!("{sanitized_title}{year_string}");
            let folder_name = format!("{base_name}{id_string}");
            let final_filename = base_name; // Kodi is flexible, filename without ID is clean. The folder has the ID.

            if flat {
                dir_path.push(format!("{folder_name}{separator}[{category}]"));
            } else {
                dir_path.push(category);
                dir_path.push(folder_name);
            }
            (dir_path, final_filename)
        }
        PlaylistItemType::Series => {
            let id_string = if tmdb_id > 0 { format!("{separator}{{tmdb={tmdb_id}}}") } else { String::new() };
            let series_name_raw = strm_item_info.series_name.as_ref().unwrap_or(&strm_item_info.title);
            let (name, year) = style_rename_year(series_name_raw, &CONSTANTS.export_style_config, strm_item_info.release_date.as_ref());
            let sanitized_series_name = sanitize_for_filename(name.trim(), false);
            let year_string = year.map_or(String::new(), |y| format!("{separator}({y})"));

            let series_folder_name = format!("{sanitized_series_name}{year_string}{id_string}");

            let (_, season) = style_rename_season(&strm_item_info.title, &CONSTANTS.export_style_config, strm_item_info.season.as_ref());
            let (_, episode) = style_rename_episode(&strm_item_info.title, &CONSTANTS.export_style_config, strm_item_info.episode.as_ref());
            let season_num = season.unwrap_or(1);
            let episode_num = episode.unwrap_or(1);

            let final_filename = format!("{sanitized_series_name}{separator}S{season_num:02}E{episode_num:02}");
            let season_folder = format!("Season{separator}{season_num:02}");

            if flat {
                dir_path.push(format!("{series_folder_name}{separator}[{category}]"));
                dir_path.push(season_folder);
            } else {
                dir_path.push(category);
                dir_path.push(series_folder_name);
                dir_path.push(season_folder);
            }
            (dir_path, final_filename)
        }
        _ => (PathBuf::new(), sanitize_for_filename(&strm_item_info.title, separator == "_")),
    }
}

/// Formats names according to the official Plex documentation.
/// Movie: /Movie Name (Year) {tmdb-XXXXX}/Movie Name (Year).strm
/// Series: /Show Name (Year) {tmdb-XXXXX}/Season 01/Show Name - s01e01.strm
fn format_for_plex(
    strm_item_info: &StrmItemInfo,
    tmdb_id: u32,
    separator: &str,
    flat: bool,
) -> (PathBuf, String) {
    let mut dir_path = PathBuf::new();
    let category = sanitize_for_filename(&strm_item_info.group, false);

    match strm_item_info.item_type {
        PlaylistItemType::Video => {
            let id_string = if tmdb_id > 0 { format!("{separator}{{tmdb-{tmdb_id}}}") } else { String::new() };
            let (name, year) = style_rename_year(&strm_item_info.title, &CONSTANTS.export_style_config, strm_item_info.release_date.as_ref());
            let sanitized_title = sanitize_for_filename(name.trim(), false);
            let year_string = year.map_or(String::new(), |y| format!("{separator}({y})"));

            let base_name = format!("{sanitized_title}{year_string}");
            let folder_name = format!("{base_name}{id_string}");
            let final_filename = base_name; // Plex: ID is only in the folder name.

            if flat {
                dir_path.push(format!("{folder_name}{separator}[{category}]"));
            } else {
                dir_path.push(category);
                dir_path.push(folder_name);
            }
            (dir_path, final_filename)
        }
        PlaylistItemType::Series => {
            let id_string = if tmdb_id > 0 { format!("{separator}{{tmdb-{tmdb_id}}}") } else { String::new() };
            let series_name_raw = strm_item_info.series_name.as_ref().unwrap_or(&strm_item_info.title);
            let (name, year) = style_rename_year(series_name_raw, &CONSTANTS.export_style_config, strm_item_info.release_date.as_ref());
            let sanitized_series_name = sanitize_for_filename(name.trim(), false);
            let year_string = year.map_or(String::new(), |y| format!("{separator}({y})"));

            let series_folder_name = format!("{sanitized_series_name}{year_string}{id_string}");

            let (_, season) = style_rename_season(&strm_item_info.title, &CONSTANTS.export_style_config, strm_item_info.season.as_ref());
            let (_, episode) = style_rename_episode(&strm_item_info.title, &CONSTANTS.export_style_config, strm_item_info.episode.as_ref());
            let season_num = season.unwrap_or(1);
            let episode_num = episode.unwrap_or(1);

            // Plex standard: lowercase 's' and hyphens as separators.
            let final_filename = format!("{sanitized_series_name} - s{season_num:02}e{episode_num:02}");
            let season_folder = format!("Season{separator}{season_num:02}");

            if flat {
                dir_path.push(format!("{series_folder_name}{separator}[{category}]"));
                dir_path.push(season_folder);
            } else {
                dir_path.push(category);
                dir_path.push(series_folder_name);
                dir_path.push(season_folder);
            }
            (dir_path, final_filename)
        }
        _ => (PathBuf::new(), sanitize_for_filename(&strm_item_info.title, separator == "_")),
    }
}

/// Formats names according to the official Emby documentation.
/// Movie: /Movie Name (Year)/Movie Name (Year) [tmdbid=XXXXX].strm
/// Series: /Show Name (Year) [tmdbid=XXXXX]/Season 01/Show Name - S01E01.strm
fn format_for_emby(
    strm_item_info: &StrmItemInfo,
    tmdb_id: u32,
    separator: &str,
    flat: bool,
) -> (PathBuf, String) {
    let mut dir_path = PathBuf::new();
    let category = sanitize_for_filename(&strm_item_info.group, false);

    match strm_item_info.item_type {
        PlaylistItemType::Video => {
            // Emby prefers the ID in the filename.
            let id_string = if tmdb_id > 0 { format!("{separator}[tmdbid={tmdb_id}]") } else { String::new() };
            let (name, year) = style_rename_year(&strm_item_info.title, &CONSTANTS.export_style_config, strm_item_info.release_date.as_ref());
            let sanitized_title = sanitize_for_filename(name.trim(), false);
            let year_string = year.map_or(String::new(), |y| format!("{separator}({y})"));

            let base_name = format!("{sanitized_title}{year_string}");
            let folder_name = base_name.clone(); // Folder name does not contain the ID.
            let final_filename = format!("{base_name}{id_string}");

            if flat {
                dir_path.push(format!("{folder_name}{separator}[{category}]"));
            } else {
                dir_path.push(category);
                dir_path.push(folder_name);
            }
            (dir_path, final_filename)
        }
        PlaylistItemType::Series => {
            // For series, the ID goes in the folder name.
            let id_string = if tmdb_id > 0 { format!("{separator}[tmdbid={tmdb_id}]") } else { String::new() };
            let series_name_raw = strm_item_info.series_name.as_ref().unwrap_or(&strm_item_info.title);
            let (name, year) = style_rename_year(series_name_raw, &CONSTANTS.export_style_config, strm_item_info.release_date.as_ref());
            let sanitized_series_name = sanitize_for_filename(name.trim(), false);
            let year_string = year.map_or(String::new(), |y| format!("{separator}({y})"));

            let series_folder_name = format!("{sanitized_series_name}{year_string}{id_string}");

            let (_, season) = style_rename_season(&strm_item_info.title, &CONSTANTS.export_style_config, strm_item_info.season.as_ref());
            let (_, episode) = style_rename_episode(&strm_item_info.title, &CONSTANTS.export_style_config, strm_item_info.episode.as_ref());
            let season_num = season.unwrap_or(1);
            let episode_num = episode.unwrap_or(1);

            // Emby/Jellyfin standard: uppercase 'S' and hyphens.
            let final_filename = format!("{sanitized_series_name} - S{season_num:02}E{episode_num:02}");
            let season_folder = format!("Season{separator}{season_num:02}");

            if flat {
                dir_path.push(format!("{series_folder_name}{separator}[{category}]"));
                dir_path.push(season_folder);
            } else {
                dir_path.push(category);
                dir_path.push(series_folder_name);
                dir_path.push(season_folder);
            }
            (dir_path, final_filename)
        }
        _ => (PathBuf::new(), sanitize_for_filename(&strm_item_info.title, separator == "_")),
    }
}

/// Formats names according to the official Jellyfin documentation.
/// Movie: /Movie Name (Year) [tmdbid-XXXXX]/Movie Name (Year).strm
/// Series: /Show Name (Year) [tmdbid-XXXXX]/Season 01/Show Name - S01E01.strm
fn format_for_jellyfin(
    strm_item_info: &StrmItemInfo,
    tmdb_id: u32,
    separator: &str,
    flat: bool,
) -> (PathBuf, String) {
    let mut dir_path = PathBuf::new();
    let category = sanitize_for_filename(&strm_item_info.group, false);

    match strm_item_info.item_type {
        PlaylistItemType::Video => {
            let id_string = if tmdb_id > 0 { format!("{separator}[tmdbid-{tmdb_id}]") } else { String::new() };
            let (name, year) = style_rename_year(&strm_item_info.title, &CONSTANTS.export_style_config, strm_item_info.release_date.as_ref());
            let sanitized_title = sanitize_for_filename(name.trim(), false);
            let year_string = year.map_or(String::new(), |y| format!("{separator}({y})"));

            let base_name = format!("{sanitized_title}{year_string}");
            let folder_name = format!("{base_name}{id_string}");
            let final_filename = base_name; // Jellyfin: ID is only in the folder name.

            if flat {
                dir_path.push(format!("{folder_name}{separator}[{category}]"));
            } else {
                dir_path.push(category);
                dir_path.push(folder_name);
            }
            (dir_path, final_filename)
        }
        PlaylistItemType::Series => {
            let id_string = if tmdb_id > 0 { format!("{separator}[tmdbid-{tmdb_id}]") } else { String::new() };
            let series_name_raw = strm_item_info.series_name.as_ref().unwrap_or(&strm_item_info.title);
            let (name, year) = style_rename_year(series_name_raw, &CONSTANTS.export_style_config, strm_item_info.release_date.as_ref());
            let sanitized_series_name = sanitize_for_filename(name.trim(), false);
            let year_string = year.map_or(String::new(), |y| format!("{separator}({y})"));

            let series_folder_name = format!("{sanitized_series_name}{year_string}{id_string}");

            let (_, season) = style_rename_season(&strm_item_info.title, &CONSTANTS.export_style_config, strm_item_info.season.as_ref());
            let (_, episode) = style_rename_episode(&strm_item_info.title, &CONSTANTS.export_style_config, strm_item_info.episode.as_ref());
            let season_num = season.unwrap_or(1);
            let episode_num = episode.unwrap_or(1);

            // Emby/Jellyfin standard: uppercase 'S' and hyphens.
            let final_filename = format!("{sanitized_series_name} - S{season_num:02}E{episode_num:02}");
            let season_folder = format!("Season{separator}{season_num:02}");

            if flat {
                dir_path.push(format!("{series_folder_name}{separator}[{category}]"));
                dir_path.push(season_folder);
            } else {
                dir_path.push(category);
                dir_path.push(series_folder_name);
                dir_path.push(season_folder);
            }
            (dir_path, final_filename)
        }
        _ => (PathBuf::new(), sanitize_for_filename(&strm_item_info.title, separator == "_")),
    }
}


/// Generates style-compliant directory and file names by dispatching
/// the call to a dedicated formatting function for the respective style.
async fn style_based_rename(
    cfg: &AppConfig,
    strm_item_info: &StrmItemInfo,
    input_tmdb_indexes: &mut InputTmdbIndexMap,
    style: &StrmExportStyle,
    underscore_whitespace: bool,
    flat: bool,
) -> (PathBuf, String) {
    let separator = if underscore_whitespace { "_" } else { " " };

    let tmdb_id_val = get_tmdb_value(
        cfg,
        strm_item_info.provider_id,
        strm_item_info.input_name.as_str(),
        input_tmdb_indexes,
        strm_item_info.item_type,
    ).await;

    let tmdb_id = tmdb_id_val.map_or(0, |v| match v {
        InputTmdbIndexValue::Video(r) => r.tmdb_id,
        InputTmdbIndexValue::Series(e) => e.tmdb_id,
    });

    // Dispatch the call to the responsible function based on the style.
    match style {
        StrmExportStyle::Kodi => format_for_kodi(strm_item_info, tmdb_id, separator, flat),
        StrmExportStyle::Plex => format_for_plex(strm_item_info, tmdb_id, separator, flat),
        StrmExportStyle::Emby => format_for_emby(strm_item_info, tmdb_id, separator, flat),
        StrmExportStyle::Jellyfin => format_for_jellyfin(strm_item_info, tmdb_id, separator, flat),
    }
}

async fn prepare_strm_files(
    cfg: &AppConfig,
    new_playlist: &mut [PlaylistGroup],
    _root_path: &Path,
    underscore_whitespace: bool,
    style: &StrmExportStyle,
    flat: bool,
) -> Vec<StrmFile> {
    let channel_count = new_playlist
        .iter()
        .map(|g| g.filter_count(filter_strm_item))
        .sum();
    // contains all filenames to detect collisions
    let mut all_filenames = HashSet::with_capacity(channel_count);
    // contains only collision filenames
    let mut collisions: HashSet<Arc<String>> = HashSet::new();
    let mut input_tmdb_indexes: InputTmdbIndexMap = HashMap::with_capacity(channel_count);
    let mut result = Vec::with_capacity(channel_count);

    // first we create the names to identify name collisions
    for pg in new_playlist.iter_mut() {
        for pli in pg.channels.iter_mut().filter(|c| filter_strm_item(c)) {
            let strm_item_info = extract_item_info(pli);

            let (dir_path, strm_file_name) = style_based_rename(
                cfg,
                &strm_item_info,
                &mut input_tmdb_indexes,
                style,
                underscore_whitespace,
                flat,
            ).await;

            let filename = Arc::new(strm_file_name);
            if all_filenames.contains(&filename) {
                collisions.insert(Arc::clone(&filename));
            }
            all_filenames.insert(Arc::clone(&filename));
            result.push(StrmFile {
                file_name: Arc::clone(&filename),
                dir_path,
                strm_info: strm_item_info,
            });
        }
    }

    if !collisions.is_empty() {
        // This separator is specifically for the multi-version naming convention.
        // According to the docs (Plex, Jellyfin), this should be " - " (space-hyphen-space).
        // The user's `underscore_whitespace` setting should not apply to this structural separator.
        let version_separator = " - ";
        let separator = if underscore_whitespace { "_" } else { " " };
        result
            .iter_mut()
            .filter(|s| collisions.contains(&s.file_name))
            .for_each(|s| {
                // Create a descriptive and unique identifier for this version.
                // This will become the "ArbitraryText" or "Label".
                let version_label = format!("Version{}id#{}", separator, s.strm_info.virtual_id);

                // The base filename is the part that is identical for all versions.
                let base_filename = &s.file_name;

                // Apply the specific multi-version naming convention for the selected style.
                // NOTE: For multi-versioning, all four platforms use the same " - Suffix" logic.
                // A style-specific match is not strictly necessary here but is good practice for future flexibility.
                let new_filename = match style {
                    // Plex, Emby, and Kodi all follow the `Filename - Suffix` pattern.
                    StrmExportStyle::Plex | StrmExportStyle::Emby | StrmExportStyle::Kodi => {
                        format!("{base_filename}{version_separator}{version_label}")
                    }

                    // Jellyfin also follows this pattern, but explicitly shows an option for " - [Label]".
                    // Using brackets makes the version distinct and is a clean implementation.
                    StrmExportStyle::Jellyfin => {
                        format!("{base_filename}{version_separator}[{version_label}]")
                    }
                };

                s.file_name = Arc::new(new_filename);
            });
    }
    result
}

pub async fn write_strm_playlist(
    app_config: &AppConfig,
    target: &ConfigTarget,
    target_output: &StrmTargetOutput,
    new_playlist: &mut [PlaylistGroup],
) -> Result<(), TuliproxError> {
    if new_playlist.is_empty() {
        return Ok(());
    }

    let config = app_config.config.load();
    let Some(root_path) = utils::get_file_path(
        &config.working_dir,
        Some(std::path::PathBuf::from(&target_output.directory)),
    ) else {
        return Err(info_err!(format!(
            "Failed to get file path for {}",
            target_output.directory
        )));
    };

    let user_and_server_info = get_credentials_and_server_info(app_config, target_output.username.as_ref());
    let strm_index_path =
        strm_get_file_paths(&ensure_target_storage_path(&config, target.name.as_str())?);
    let existing_strm = {
        let _file_lock = app_config
            .file_locks
            .read_lock(&strm_index_path);
        read_strm_file_index(&strm_index_path)
            .await
            .unwrap_or_else(|_| HashSet::with_capacity(4096))
    };
    let mut processed_strm: HashSet<String> = HashSet::with_capacity(existing_strm.len());

    let mut failed = vec![];

    prepare_strm_output_directory(&root_path).await?;

    let target_force_redirect = target.options.as_ref().and_then(|o| o.force_redirect.as_ref());

    // we need to consider
    // - Live streams
    // - Xtream Series Episode (has series_name and release_date)
    // - Xtream VOD (should have year or release_date)
    // - M3u Series (TODO we dont have this currently, should be guessed through m3u parser)
    // - M3u Vod (no additional infos, need to extract from title)
    let strm_files = prepare_strm_files(
        app_config,
        new_playlist,
        &root_path,
        target_output.underscore_whitespace,
        &target_output.style,
        target_output.flat,
    ).await;
    for strm_file in strm_files {
        // file paths
        let output_path = root_path.join(&strm_file.dir_path);
        let file_path = output_path.join(format!("{}.strm", strm_file.file_name));
        let file_exists = file_path.exists();
        let relative_file_path = get_relative_path_str(&file_path, &root_path);

        // create content
        let url = get_strm_url(target_force_redirect, user_and_server_info.as_ref(), &strm_file.strm_info);
        let mut content = target_output.strm_props.as_ref().map_or_else(Vec::new, std::clone::Clone::clone);
        content.push(url);
        let content_text = content.join("\r\n");
        let content_as_bytes = content_text.as_bytes();
        let content_hash = hash_bytes(content_as_bytes);

        // check if file exists and has same hash
        if file_exists && has_strm_file_same_hash(&file_path, content_hash).await {
            processed_strm.insert(relative_file_path);
            continue; // skip creation
        }

        // if we cant create the directory skip this entry
        if !ensure_strm_file_directory(&mut failed, &output_path).await {
            continue;
        }

        match write_strm_file(
            &file_path,
            content_as_bytes,
            strm_file.strm_info.get_file_ts(),
        ).await
        {
            Ok(()) => {
                processed_strm.insert(relative_file_path);
            }
            Err(err) => {
                failed.push(err);
            }
        };
    }

    if let Err(err) = write_strm_index_file(app_config, &processed_strm, &strm_index_path).await {
        failed.push(err);
    }

    if let Err(err) =
        cleanup_strm_output_directory(target_output.cleanup, &root_path, &existing_strm, &processed_strm).await
    {
        failed.push(err);
    }

    if failed.is_empty() {
        Ok(())
    } else {
        Err(info_err!(failed.join(", ")))
    }
}
async fn write_strm_index_file(
    cfg: &AppConfig,
    entries: &HashSet<String>,
    index_file_path: &PathBuf,
) -> Result<(), String> {
    let _file_lock = cfg
        .file_locks
        .write_lock(index_file_path);
    let file = File::create(index_file_path)
        .await
        .map_err(|err| format!("Failed to create strm index file: {} {err}", index_file_path.display()))?;
    let mut writer = BufWriter::new(file);
    let new_line = "\n".as_bytes();
    for entry in entries {
        writer
            .write_all(entry.as_bytes())
            .await
            .map_err(|err| format!("Failed to write strm index entry: {err}"))?;
        writer
            .write(new_line)
            .await
            .map_err(|err| format!("Failed to write strm index entry: {err}"))?;
    }
    writer
        .flush()
        .await
        .map_err(|err| format!("failed to write strm index entry: {err}"))?;
    Ok(())
}

async fn ensure_strm_file_directory(failed: &mut Vec<String>, output_path: &Path) -> bool {
    if !output_path.exists() {
        if let Err(e) = create_dir_all(output_path).await {
            let err_msg =
                format!("Failed to create directory for strm playlist: {} {e}", output_path.display());
            error!("{err_msg}");
            failed.push(err_msg);
            return false; // skip creation, could not create directory
        };
    }
    true
}

async fn write_strm_file(
    file_path: &Path,
    content_as_bytes: &[u8],
    timestamp: Option<u64>,
) -> Result<(), String> {
    File::create(file_path)
        .await
        .map_err(|err| format!("Failed to create strm file: {err}"))?
        .write_all(content_as_bytes)
        .await
        .map_err(|err| format!("Failed to write strm playlist: {err}"))?;

    if let Some(ts) = timestamp {
        #[allow(clippy::cast_possible_wrap)]
        let mtime = FileTime::from_unix_time(ts as i64, 0); // Unix-Timestamp: 01.01.2023 00:00:00 UTC
        #[allow(clippy::cast_possible_wrap)]
        let atime = FileTime::from_unix_time(ts as i64, 0); // access time
        let _ = set_file_times(file_path, mtime, atime);
    }

    Ok(())
}

async fn has_strm_file_same_hash(file_path: &PathBuf, content_hash: UUIDType) -> bool {
    if let Ok(file) = File::open(&file_path).await {
        let mut reader = BufReader::new(file);
        let mut buffer = Vec::new();
        match reader.read_to_end(&mut buffer).await {
            Ok(_) => {
                let file_hash = hash_bytes(&buffer);
                if content_hash == file_hash {
                    return true;
                }
            }
            Err(err) => {
                error!("Could not read existing strm file {} {err}", file_path.display());
            }
        }
    }
    false
}

fn get_credentials_and_server_info(
    cfg: &AppConfig,
    username: Option<&String>,
) -> Option<(ProxyUserCredentials, ApiProxyServerInfo)> {
    let username = username?;
    let credentials = cfg.get_user_credentials(username)?;
    let server_info = cfg.get_user_server_info(&credentials);
    Some((credentials, server_info))
}

async fn read_strm_file_index(strm_file_index_path: &Path) -> std::io::Result<HashSet<String>> {
    let file = File::open(strm_file_index_path).await?;
    let reader = BufReader::new(file);
    let mut result = HashSet::new();
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        result.insert(line);
    }
    Ok(result)
}

fn get_strm_url(
    target_force_redirect: Option<&ClusterFlags>,
    user_and_server_info: Option<&(ProxyUserCredentials, ApiProxyServerInfo)>,
    str_item_info: &StrmItemInfo,
) -> String {
    let Some((user, server_info)) = user_and_server_info else { return str_item_info.url.to_string(); };

    let redirect = user.proxy.is_redirect(str_item_info.item_type) || target_force_redirect.is_some_and(|f| f.has_cluster(str_item_info.item_type));
    if redirect {
        return str_item_info.url.to_string();
    }

    if let Some(stream_type) = match str_item_info.item_type {
        PlaylistItemType::Series => Some("series"),
        PlaylistItemType::Live => Some("live"),
        PlaylistItemType::Video => Some("movie"),
        _ => None,
    } {
        let url = str_item_info.url.as_str();
        let ext = extract_extension_from_url(url)
            .map_or_else(String::new, std::string::ToString::to_string);
        format!(
            "{}/{stream_type}/{}/{}/{}{ext}",
            server_info.get_base_url(),
            user.username,
            user.password,
            str_item_info.virtual_id
        )
    } else {
        str_item_info.url.to_string()
    }
}

// /////////////////////////////////////////////
// - Cleanup -
// We first build a Directory Tree to
// identifiy the deletable files and directories
// /////////////////////////////////////////////
#[derive(Debug, Clone)]
struct DirNode {
    path: PathBuf,
    is_root: bool, // is root -> not delete!
    has_files: bool, //  has content -> do not delete!
    children: HashSet<PathBuf>,
    parent: Option<PathBuf>,
}

impl DirNode {
    fn new(path: PathBuf, parent: Option<PathBuf>) -> Self {
        Self::new_with_flag(path, parent, false)
    }

    fn new_root(path: PathBuf) -> Self {
        Self::new_with_flag(path, None, true)
    }

    fn new_with_flag(path: PathBuf, parent: Option<PathBuf>, is_root: bool) -> Self {
        Self {
            path,
            is_root,
            has_files: false,
            children: HashSet::new(),
            parent,
        }
    }
}

/// Because of rust ownership we don't want to use References or Mutexes.
/// Because of async operations ve cant use recursion.
/// We use paths identifier to handle the tree construction.
/// Rust sucks!!!
async fn build_directory_tree(root_path: &Path) -> HashMap<PathBuf, DirNode> {
    let mut nodes: HashMap<PathBuf, DirNode> = HashMap::new();
    nodes.insert(PathBuf::from(root_path), DirNode::new_root(root_path.to_path_buf()));
    let mut stack = vec![root_path.to_path_buf()];
    while let Some(current_path) = stack.pop() {
        if let Ok(mut dir_read) = tokio::fs::read_dir(&current_path).await {
            while let Ok(Some(entry)) = dir_read.next_entry().await {
                let entry_path = entry.path();
                if entry_path.is_dir() {
                    if !nodes.contains_key(&entry_path) {
                        let new_node = DirNode::new(entry_path.clone(), Some(current_path.clone()));
                        nodes.insert(entry_path.clone(), new_node);
                    }
                    if let Some(current_node) = nodes.get_mut(&current_path) {
                        current_node.children.insert(entry_path.clone());
                    }
                    stack.push(entry_path);
                } else if let Some(data) = nodes.get_mut(&current_path) {
                    data.has_files = true;
                    let mut parent_path_opt = data.parent.clone();

                    while let Some(parent_path) = parent_path_opt {
                        parent_path_opt = {
                            if let Some(parent) = nodes.get_mut(&parent_path) {
                                parent.has_files = true;
                                parent.parent.clone()
                            } else {
                                None
                            }
                        };
                    }
                }
            }
        }
    }
    nodes
}

// We have build the directory tree,
// now we need to build an ordered flat list,
// We walk from top to bottom.
// (PS: you can only delete in reverse order, because delete first children, then the parents)
fn flatten_tree(
    root_path: &Path,
    mut tree_nodes: HashMap<PathBuf, DirNode>,
) -> Vec<DirNode> {
    let mut paths_to_process = Vec::new(); // List of paths to process

    {
        let mut queue: VecDeque<PathBuf> = VecDeque::new(); // processing queue
        queue.push_back(PathBuf::from(root_path));

        while let Some(current_path) = queue.pop_front() {
            if let Some(current) = tree_nodes.get(&current_path) {
                current.children.iter().for_each(|child_path| {
                    if let Some(node) = tree_nodes.get(child_path) {
                        queue.push_back(node.path.clone());
                    }
                });
                paths_to_process.push(current.path.clone());
            }
        }
    }

    paths_to_process
        .iter()
        .filter_map(|path| tree_nodes.remove(path))
        .collect()
}

async fn delete_empty_dirs_from_tree(root_path: &Path, tree_nodes: HashMap<PathBuf, DirNode>) {
    let tree_stack = flatten_tree(root_path, tree_nodes);
    // reverse order  to delete from leaf to root
    for node in tree_stack.into_iter().rev() {
        if !node.has_files && !node.is_root {
            if let Err(err) = remove_dir(&node.path).await {
                trace!("Could not delete empty dir: {}, {err}", &node.path.display());
            }
        }
    }
}
async fn remove_empty_dirs(root_path: PathBuf) {
    let tree_nodes = build_directory_tree(&root_path).await;
    delete_empty_dirs_from_tree(&root_path, tree_nodes).await;
}


// #[cfg(test)]
// mod tests {
//     use crate::repository::kodi_repository::remove_empty_dirs;
//     use std::path::PathBuf;
//
//     #[tokio::test]
//     async fn test_empty_dirs() {
//         remove_empty_dirs(PathBuf::from("/tmp/hello")).await;
//     }
// }