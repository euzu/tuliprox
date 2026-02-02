use crate::model::MediaQuality;
use crate::model::{ApiProxyServerInfo, AppConfig, ProxyUserCredentials};
use crate::model::{ConfigTarget, StrmTargetOutput};
use crate::repository::storage::ensure_target_storage_path;
use crate::repository::storage_const;
use crate::utils::{async_file_reader, async_file_writer, normalize_string_path, truncate_filename,
                   IO_BUFFER_SIZE};
use chrono::Datelike;
use filetime::{set_file_times, FileTime};
use log::{error, trace};
// Unused import 'regex::Regex' removed
use serde::Serialize;
use shared::error::{info_err_res, TuliproxError};
use shared::model::{ClusterFlags, PlaylistGroup, PlaylistItem, PlaylistItemType, StreamProperties, StrmExportStyle};
use shared::utils::{arc_str_option_serde, arc_str_serde, clean_playlist_title, extract_extension_from_url, hash_bytes,
                    hash_string_as_hex, is_blank_optional_arc_str, truncate_string, ExportStyleConfig, CONSTANTS};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::{create_dir_all, remove_dir, remove_file, File};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use shared::model::UUIDType;
use crate::api::model::{ActiveProviderManager};
use std::borrow::Cow;

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
            } else if "+=,._-@#()[]".contains(c) { // <-- Allow list of safe punctuation, added [ and ] for quality tags.
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

    // 4. Remove empty parentheses
    sanitized = CONSTANTS.export_style_config.paaren.replace_all(sanitized.as_str(), "").trim().to_string();

    // 5. Final check: If sanitization resulted in an empty string, return a default.
    if sanitized.is_empty() {
        EMPTY_FILENAME_REPLACEMENT.to_string()
    } else {
        sanitized
    }
}

/// Extracts and formats year information from media titles.
/// Prioritizes metadata `release_date`. If present, it cleans the year from the title to avoid duplication.
/// If absent, it attempts to parse the year from the title.
fn style_rename_year<'a>(
    name: &'a str,
    style: &ExportStyleConfig,
    release_date: Option<&Arc<str>>,
) -> (Cow<'a, str>, Option<u32>) {
    // 1. Try to get year from metadata first (most reliable)
    let meta_year = release_date.and_then(|rd| {
        // Expected format YYYY-MM-DD or just YYYY
        rd.split('-').next().and_then(|y| y.parse::<u32>().ok())
    });

    let cur_year = u32::try_from(chrono::Utc::now().year()).unwrap_or(0);
    
    // Check if we need to clean the title (remove year if present) or extract year from title
    // We iterate matches to either find the year (if meta_year is None) or remove it (if meta_year is Some)
    let mut new_name = String::with_capacity(name.len());
    let mut last_index = 0;
    let mut extracted_year = None;

    for caps in style.year.captures_iter(name) {
        if let Some(year_match) = caps.get(1) {
            if let Ok(year) = year_match.as_str().parse::<u32>() {
                if (1900..=cur_year + 5).contains(&year) { // Allow slightly future years
                    // Found a valid year in title
                    if extracted_year.is_none() {
                        extracted_year = Some(year);
                    }
                    
                    // We remove the year from the title in two cases:
                    // A) We have a metadata year (clean up title to avoid "Movie (2000) (2000)")
                    // B) We don't have metadata year (we extract it and remove it from title to re-append consistently later)
                    if let Some(matched) = caps.get(0) {
                        let match_start = matched.start();
                        let match_end = matched.end();
                        new_name.push_str(&name[last_index..match_start]);
                        last_index = match_end;
                    }
                }
            }
        }
    }
    
    new_name.push_str(&name[last_index..]);

    // Use metadata year if available, otherwise the one extracted from title
    let final_year = meta_year.or(extracted_year);
    
    // If we modified the string, trim it and return Owned
    if last_index > 0 {
        // Clean up potential double spaces or trailing punctuation left by removal
        // Remove trailing " -", ".", or "_" which might have been separators before the year
        let cleaned = new_name.trim().trim_end_matches(|c| " -_.".contains(c)).trim().to_string();
        
        // Ensure we didn't make the name empty
        if cleaned.is_empty() {
             return (Cow::Borrowed(name), final_year);
        }
        (Cow::Owned(cleaned), final_year)
    } else {
        (Cow::Borrowed(name), final_year)
    }
}

pub fn strm_get_file_paths(file_prefix: &str, target_path: &Path) -> PathBuf {
    target_path.join(PathBuf::from(format!("{file_prefix}_{}.{}", storage_const::FILE_STRM, storage_const::FILE_SUFFIX_DB)))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StrmItemInfo {
    #[serde(with = "arc_str_serde")]
    group: Arc<str>,
    #[serde(with = "arc_str_serde")]
    title: Arc<str>,
    item_type: PlaylistItemType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_id: Option<u32>,
    virtual_id: u32,
    #[serde(with = "arc_str_serde")]
    input_name: Arc<str>,
    #[serde(with = "arc_str_serde")]
    url: Arc<str>,
    #[serde(with = "arc_str_option_serde", skip_serializing_if = "is_blank_optional_arc_str")]
    series_name: Option<Arc<str>>,
    #[serde(with = "arc_str_option_serde", skip_serializing_if = "is_blank_optional_arc_str")]
    release_date: Option<Arc<str>>,
    #[serde(with = "arc_str_option_serde", skip_serializing_if = "is_blank_optional_arc_str")]
    series_release_date: Option<Arc<str>>, // Global series release date
    #[serde(default, skip_serializing_if = "Option::is_none")]
    season: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    episode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    added: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tmdb_id: Option<u32>,
}

impl StrmItemInfo {
    pub(crate) fn get_file_ts(&self) -> Option<u64> {
        self.added
    }
}

fn extract_item_info(pli: &mut PlaylistItem) -> StrmItemInfo {
    let header = &mut pli.header;
    // Clone necessary fields cheaply (Arc)
    let group = header.group.clone();
    let item_type = header.item_type;
    let provider_id = header.get_provider_id();
    let virtual_id = header.virtual_id;
    let input_name = header.input_name.clone();
    let url = header.url.clone();
    
    // Extract properties based on type
    // We prioritize name/title from additional_properties if available (e.g. from TMDB)
    let (title, series_name, release_date, series_release_date, added, tmdb_id, season, episode) = match header.item_type {
        PlaylistItemType::Series
        | PlaylistItemType::LocalSeries => {
            let (prop_name, release_date, series_release_date, added, tmdb_id, season, episode) = match header.additional_properties.as_ref() {
                None => (None, None, None, None, None, None, None),
                Some(props) => (
                    // If series props are available, check if we have a valid name there
                    if let StreamProperties::Series(s) = props { 
                        if !s.name.is_empty() { Some(s.name.clone()) } else { None } 
                    } else { None },
                    
                    props.get_release_date(),
                    // Extract series-level release date from Episode properties
                    if let StreamProperties::Episode(ep) = props { ep.series_release_date.clone() } else { None },
                    props.get_added(),
                    props.get_tmdb_id().filter(|&id| id != 0),
                    props.get_season(),
                    props.get_episode(),
                )
            };
            
            // For series title, we prefer the one from metadata (prop_name), then header.name, then header.title
            let final_series_name = prop_name.unwrap_or_else(|| {
                if !header.name.is_empty() { header.name.clone() } else { header.title.clone() }
            });
            
            // Episode title relies on header.title unless we want to look deeper into props
            let ep_title = header.title.clone();
            
            (ep_title, Some(final_series_name), release_date, series_release_date, added, tmdb_id, season, episode)
        }
        PlaylistItemType::Video
        | PlaylistItemType::LocalVideo => {
            let (prop_name, release_date, added, tmdb_id) = match header.additional_properties.as_ref() {
                None => (None, None, None, None),
                Some(props) => (
                    if let StreamProperties::Video(v) = props {
                         if !v.name.is_empty() { Some(v.name.clone()) } else { None }
                    } else { None },
                    props.get_release_date(),
                    props.get_added(),
                    props.get_tmdb_id().filter(|&id| id != 0),
                )
            };
            
            let final_title = prop_name.unwrap_or_else(|| header.title.clone());
            
            (final_title, None, release_date, None, added, tmdb_id, None, None)
        }
        _ => (header.title.clone(), None, None, None, None, None, None, None),
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
        series_release_date,
        season,
        episode,
        added: added.as_ref().map_or_else(|| Some(0), |a| a.parse::<u64>().ok()),
        tmdb_id,
    }
}

async fn prepare_strm_output_directory(path: &Path) -> Result<(), TuliproxError> {
    // Ensure the directory exists
    if let Err(e) = tokio::fs::create_dir_all(path).await {
        error!("Failed to create directory {}: {e}", path.display());
        return info_err_res!("Error creating STRM directory: {e}");
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
    matches!(item_type, PlaylistItemType::Live | PlaylistItemType::Video | PlaylistItemType::LocalVideo | PlaylistItemType::Series | PlaylistItemType::LocalSeries)
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

// Helper struct to hold common filename parts to avoid repetition
struct FilenameParts {
    sanitized_name: String,
    // removed year_string from struct as requested
    id_string: String,
    category: String,
    base_name: String,
}

fn prepare_filename_parts(
    strm_item_info: &StrmItemInfo,
    tmdb_id: u32,
    separator: &str,
    id_format: &str, // e.g. "{tmdb={}}" or "[tmdbid={}]"
) -> FilenameParts {
    let id_string = if tmdb_id > 0 { 
        id_format.replace("{}", &tmdb_id.to_string()) 
    } else { 
        String::new() 
    };
    
    // Determine source name and date based on type
    let (raw_name, raw_date) = match strm_item_info.item_type {
        PlaylistItemType::Series | PlaylistItemType::LocalSeries => (
             strm_item_info.series_name.as_ref().unwrap_or(&strm_item_info.title),
             strm_item_info.series_release_date.as_ref()
        ),
        _ => (
            &strm_item_info.title,
            strm_item_info.release_date.as_ref()
        )
    };

    // Use clean_playlist_title to remove IPTV garbage BEFORE parsing years
    let cleaned_name = clean_playlist_title(raw_name);

    let (name_cow, year) = style_rename_year(&cleaned_name, &CONSTANTS.export_style_config, raw_date);
    let sanitized_name = sanitize_for_filename(name_cow.trim(), false);
    let year_string = year.map_or(String::new(), |y| format!("{separator}({y})"));
    let base_name = format!("{sanitized_name}{year_string}");
    let category = sanitize_for_filename(&strm_item_info.group, false);

    FilenameParts {
        sanitized_name,
        // year_string not needed in public struct
        id_string,
        category,
        base_name,
    }
}

/// Formats names according to the official Kodi documentation, with `TMDb` ID for better matching.
/// Movie: /Movie Name (Year) {tmdb=XXXXX}/Movie Name (Year).strm
/// Series: /Show Name (Year) {tmdb=XXXXX}/Season 01/Show Name S01E01.strm
fn format_for_kodi(
    strm_item_info: &StrmItemInfo,
    tmdb_id: u32,
    separator: &str,
    flat: bool,
    flat_dedup_paths: &mut HashMap<u32, PathBuf>,
) -> (PathBuf, String) {
    // Kodi ID format: {tmdb=12345}
    let parts = prepare_filename_parts(strm_item_info, tmdb_id, separator, &format!("{separator}{{tmdb={{}}}}"));
    let mut dir_path = PathBuf::new();

    match strm_item_info.item_type {
        PlaylistItemType::Video | PlaylistItemType::LocalVideo => {
            let folder_name = format!("{}{}", parts.base_name, parts.id_string);
            let final_filename = parts.base_name;

            if flat {
                if tmdb_id > 0 {
                     if let Some(path) = flat_dedup_paths.get(&tmdb_id) {
                        dir_path = path.clone();
                    } else {
                        dir_path.push(&folder_name);
                        flat_dedup_paths.insert(tmdb_id, dir_path.clone());
                    }
                } else {
                    dir_path.push(format!("{folder_name}{separator}[{}]", parts.category));
                }
            } else {
                dir_path.push(parts.category);
                dir_path.push(folder_name);
            }
            (dir_path, final_filename)
        }
        PlaylistItemType::Series | PlaylistItemType::LocalSeries => {
            let series_folder_name = format!("{}{}", parts.base_name, parts.id_string);
            let season_num = strm_item_info.season.unwrap_or(1);
            let episode_num = strm_item_info.episode.unwrap_or(1);

            let final_filename = format!("{}{separator}S{season_num:02}E{episode_num:02}", parts.sanitized_name);
            let season_folder = format!("Season{separator}{season_num:02}");

            if flat {
                dir_path.push(format!("{series_folder_name}{separator}[{}]", parts.category));
                dir_path.push(season_folder);
            } else {
                dir_path.push(parts.category);
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
    flat_dedup_paths: &mut HashMap<u32, PathBuf>,
) -> (PathBuf, String) {
     // Plex ID format: {tmdb-12345}
    let parts = prepare_filename_parts(strm_item_info, tmdb_id, separator, &format!("{separator}{{tmdb-{{}}}}"));
    let mut dir_path = PathBuf::new();

    match strm_item_info.item_type {
        PlaylistItemType::Video | PlaylistItemType::LocalVideo => {
            let folder_name = format!("{}{}", parts.base_name, parts.id_string);
            let final_filename = parts.base_name;

            if flat {
               if tmdb_id > 0 {
                    if let Some(path) = flat_dedup_paths.get(&tmdb_id) {
                        dir_path = path.clone();
                    } else {
                        dir_path.push(&folder_name);
                        flat_dedup_paths.insert(tmdb_id, dir_path.clone());
                    }
                } else {
                    dir_path.push(format!("{folder_name}{separator}[{}]", parts.category));
                }
            } else {
                dir_path.push(parts.category);
                dir_path.push(folder_name);
            }
            (dir_path, final_filename)
        }
        PlaylistItemType::Series | PlaylistItemType::LocalSeries => {
            let series_folder_name = format!("{}{}", parts.base_name, parts.id_string);
            let season_num = strm_item_info.season.unwrap_or(1);
            let episode_num = strm_item_info.episode.unwrap_or(1);

            // Plex standard: lowercase 's' and hyphens as separators.
            let final_filename = format!("{} - s{season_num:02}e{episode_num:02}", parts.sanitized_name);
            let season_folder = format!("Season{separator}{season_num:02}");

            if flat {
                dir_path.push(format!("{series_folder_name}{separator}[{}]", parts.category));
                dir_path.push(season_folder);
            } else {
                dir_path.push(parts.category);
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
    flat_dedup_paths: &mut HashMap<u32, PathBuf>,
) -> (PathBuf, String) {
    // Emby ID format: [tmdbid=12345]
    let parts = prepare_filename_parts(strm_item_info, tmdb_id, separator, &format!("{separator}[tmdbid={{}}]"));
    let mut dir_path = PathBuf::new();

    match strm_item_info.item_type {
        PlaylistItemType::Video | PlaylistItemType::LocalVideo => {
            // Emby prefers the ID in the filename for movies, folder optional
            let folder_name = parts.base_name.clone(); // Folder name does not contain the ID usually, but can
            let final_filename = format!("{}{}", parts.base_name, parts.id_string);

            if flat {
               if tmdb_id > 0 {
                    if let Some(path) = flat_dedup_paths.get(&tmdb_id) {
                        dir_path = path.clone();
                    } else {
                        dir_path.push(&folder_name);
                        flat_dedup_paths.insert(tmdb_id, dir_path.clone());
                    }
                } else {
                    dir_path.push(format!("{folder_name}{separator}[{}]", parts.category));
                }
            } else {
                dir_path.push(parts.category);
                dir_path.push(folder_name);
            }
            (dir_path, final_filename)
        }
        PlaylistItemType::Series | PlaylistItemType::LocalSeries => {
             // For series, the ID goes in the folder name.
            let series_folder_name = format!("{}{}", parts.base_name, parts.id_string);
            let season_num = strm_item_info.season.unwrap_or(1);
            let episode_num = strm_item_info.episode.unwrap_or(1);

            // Emby/Jellyfin standard: uppercase 'S' and hyphens.
            let final_filename = format!("{} - S{season_num:02}E{episode_num:02}", parts.sanitized_name);
            let season_folder = format!("Season{separator}{season_num:02}");

            if flat {
                dir_path.push(format!("{series_folder_name}{separator}[{}]", parts.category));
                dir_path.push(season_folder);
            } else {
                dir_path.push(parts.category);
                dir_path.push(series_folder_name);
                dir_path.push(season_folder);
            }
            (dir_path, final_filename)
        }
        _ => (PathBuf::new(), sanitize_for_filename(&strm_item_info.title, separator == "_")),
    }
}

/// Formats names according to the official Jellyfin documentation.
/// Movie: /Movie Name (Year) [tmdbid-XXXXX]/Movie Name (Year) [tmdbid-XXXXX].strm
/// Series: /Show Name (Year) [tmdbid-XXXXX]/Season 01/Show Name - S01E01.strm
fn format_for_jellyfin(
    strm_item_info: &StrmItemInfo,
    tmdb_id: u32,
    separator: &str,
    flat: bool,
    flat_dedup_paths: &mut HashMap<u32, PathBuf>,
) -> (PathBuf, String) {
     // Jellyfin ID format: [tmdbid-12345]
    let parts = prepare_filename_parts(strm_item_info, tmdb_id, separator, &format!("{separator}[tmdbid-{{}}]"));
    let mut dir_path = PathBuf::new();

    match strm_item_info.item_type {
        PlaylistItemType::Video | PlaylistItemType::LocalVideo => {
            // Jellyfin requirement: file name MUST start with parent folder name to detect versions
            let folder_name = format!("{}{}", parts.base_name, parts.id_string);
            let final_filename = folder_name.clone();

            if flat {
               if tmdb_id > 0 {
                    if let Some(path) = flat_dedup_paths.get(&tmdb_id) {
                        dir_path = path.clone();
                    } else {
                        dir_path.push(&folder_name);
                        flat_dedup_paths.insert(tmdb_id, dir_path.clone());
                    }
                } else {
                    dir_path.push(format!("{folder_name}{separator}[{}]", parts.category));
                }
            } else {
                dir_path.push(parts.category);
                dir_path.push(folder_name);
            }
            (dir_path, final_filename)
        }
        PlaylistItemType::Series | PlaylistItemType::LocalSeries => {
            let series_folder_name = format!("{}{}", parts.base_name, parts.id_string);
            let season_num = strm_item_info.season.unwrap_or(1);
            let episode_num = strm_item_info.episode.unwrap_or(1);

            let final_filename = format!("{} - S{season_num:02}E{episode_num:02}", parts.sanitized_name);
            let season_folder = format!("Season{separator}{season_num:02}");

            if flat {
                dir_path.push(format!("{series_folder_name}{separator}[{}]", parts.category));
                dir_path.push(season_folder);
            } else {
                dir_path.push(parts.category);
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
fn style_based_rename(
    strm_item_info: &StrmItemInfo,
    tmdb: Option<u32>,
    style: StrmExportStyle,
    underscore_whitespace: bool,
    flat: bool,
    flat_dedup_paths: &mut HashMap<u32, PathBuf>,
) -> (PathBuf, String) {
    let separator = if underscore_whitespace { "_" } else { " " };


    let tmdb_id = tmdb.or(strm_item_info.tmdb_id).unwrap_or(0);

    // Dispatch the call to the responsible function based on the style.
    match style {
        StrmExportStyle::Kodi => format_for_kodi(strm_item_info, tmdb_id, separator, flat, flat_dedup_paths),
        StrmExportStyle::Plex => format_for_plex(strm_item_info, tmdb_id, separator, flat, flat_dedup_paths),
        StrmExportStyle::Emby => format_for_emby(strm_item_info, tmdb_id, separator, flat, flat_dedup_paths),
        StrmExportStyle::Jellyfin => format_for_jellyfin(strm_item_info, tmdb_id, separator, flat, flat_dedup_paths),
    }
}

async fn prepare_strm_files(
    new_playlist: &mut [PlaylistGroup],
    strm_target_output: &StrmTargetOutput,
) -> Vec<StrmFile> {
    let channel_count = new_playlist
        .iter()
        .map(|g| g.filter_count(filter_strm_item))
        .sum();
    // contains all paths (dir + filename) to detect collisions
    let mut all_filenames: HashSet<PathBuf> = HashSet::with_capacity(channel_count);
    // contains only collision filenames (PathBuf)
    let mut collisions: HashSet<PathBuf> = HashSet::new();
    let mut result = Vec::with_capacity(channel_count);

    let mut flat_dedup_paths = HashMap::new();

    // first we create the names to identify name collisions
    for pg in new_playlist.iter_mut() {
        for pli in pg.channels.iter_mut().filter(|c| filter_strm_item(c)) {
            
            let strm_item_info = extract_item_info(pli);

            let (dir_path, strm_file_name) = style_based_rename(
                &strm_item_info,
                pli.get_tmdb_id(),
                strm_target_output.style,
                strm_target_output.underscore_whitespace,
                strm_target_output.flat,
                &mut flat_dedup_paths,
            );

            // Conditionally generate the quality string based on the new config flag
            let separator = if strm_target_output.underscore_whitespace { "_" } else { " " };
            let quality_string = get_quality(strm_target_output, pli, separator);
            
            // Add category suffix for flat movie structure to avoid collisions
            let category_suffix = if strm_target_output.flat && pli.get_tmdb_id().is_some() && pli.header.item_type == PlaylistItemType::Video {
                let cat = sanitize_for_filename(&strm_item_info.group, false);
                format!("{separator}[{cat}]")
            } else { 
                String::new() 
            };

            let final_filename = format!("{strm_file_name}{quality_string}{category_suffix}");
            let filename = Arc::new(final_filename);
            
            // Construct the full relative path for collision checking
            let full_relative_path = dir_path.join(filename.as_str());

            if all_filenames.contains(&full_relative_path) {
                collisions.insert(full_relative_path.clone());
            }
            all_filenames.insert(full_relative_path);
            result.push(StrmFile {
                file_name: filename,
                dir_path,
                strm_info: strm_item_info,
            });
        }
    }

    if !collisions.is_empty() {
        // This separator is specifically for the multi-version naming convention.
        let version_separator = " ";
        let separator = if strm_target_output.underscore_whitespace { "_" } else { " " };
        
        result
            .iter_mut()
            .for_each(|s| {
                let full_relative_path = s.dir_path.join(s.file_name.as_str());
                if collisions.contains(&full_relative_path) {
                    // Create a descriptive and unique identifier for this version.
                    let version_label = format!("Version{}id#{}", separator, s.strm_info.virtual_id);

                    // The base filename is the part that is identical for all versions.
                    let base_filename = &s.file_name;

                    // Apply the specific multi-version naming convention for the selected style.
                    let new_filename = format!("{base_filename}{version_separator}[{version_label}]");

                    s.file_name = Arc::new(new_filename);
                }
            });
    }
    result
}

fn get_quality(strm_target_output: &StrmTargetOutput, pli: &PlaylistItem, separator: &str) -> String {
    if strm_target_output.add_quality_to_filename {
        // Use `additional_properties` which are populated by metadata_update_manager/probe
        let (audio, video) = match pli.header.additional_properties.as_ref() {
            None => (None, None),
            Some(props) => {
                match props {
                    StreamProperties::Live(_)
                    | StreamProperties::Series(_) => (None, None),
                    StreamProperties::Video(video) =>
                        video.details.as_ref().map_or_else(|| (None, None), |d| (d.audio.as_deref(), d.video.as_deref())),
                    StreamProperties::Episode(episode) =>
                        (episode.audio.as_deref(), episode.video.as_deref())
                }
            }
        };
        if let Some(media_quality) = MediaQuality::from_ffprobe_info(audio, video) {
            let formatted = media_quality.format_for_filename(separator);
            if !formatted.is_empty() {
                // Hard-coded separator for filename clarity.
                return format!(" - [{formatted}]")
            }
        }
    }
    String::new()
}

pub async fn write_strm_playlist(
    app_config: &AppConfig,
    target: &ConfigTarget,
    target_output: &StrmTargetOutput,
    new_playlist: &mut [PlaylistGroup],
    // Inject provider manager for connection checking
    _provider_manager: Option<&Arc<ActiveProviderManager>>, 
) -> Result<(), TuliproxError> {
    if new_playlist.is_empty() {
        return Ok(());
    }

    let config = app_config.config.load();
    let Some(root_path) = crate::utils::get_file_path(
        &config.working_dir,
        Some(std::path::PathBuf::from(&target_output.directory)),
    ) else {
        return info_err_res!("Failed to get file path for {}",target_output.directory);
    };

    let user_and_server_info = get_credentials_and_server_info(app_config, target_output.username.as_deref());
    let normalized_dir = normalize_string_path(&target_output.directory);
    let strm_file_prefix = hash_string_as_hex(&normalized_dir);
    let strm_index_path =
        strm_get_file_paths(&strm_file_prefix, &ensure_target_storage_path(&config, target.name.as_str()).await?);
    let existing_strm = {
        let _file_lock = app_config
            .file_locks
            .read_lock(&strm_index_path).await;
        read_strm_file_index(&strm_index_path)
            .await
            .unwrap_or_else(|_| HashSet::with_capacity(4096))
    };
    let mut processed_strm: HashSet<String> = HashSet::with_capacity(existing_strm.len());

    let mut failed = vec![];

    prepare_strm_output_directory(&root_path).await?;

    let target_force_redirect = target.options.as_ref().and_then(|o| o.force_redirect.as_ref());

    let strm_files = prepare_strm_files(
        new_playlist,
        target_output,
    ).await;
    
    for strm_file in strm_files {
        // file paths
        let output_path = truncate_filename(&root_path.join(&strm_file.dir_path), 255);
        let file_path = output_path.join(format!("{}.strm", truncate_string(&strm_file.file_name, 250)));

        let file_exists = file_path.exists();
        let relative_file_path = get_relative_path_str(&file_path, &root_path);

        // create content
        let url = get_strm_url(target_force_redirect, user_and_server_info.as_ref(), &strm_file.strm_info);
        let mut content = target_output.strm_props.as_ref().map_or_else(Vec::new, std::clone::Clone::clone);
        content.push(url.to_string());
        let content_text = content.join("\r\n");
        let content_as_bytes = content_text.as_bytes();
        let content_hash = hash_bytes(content_as_bytes);

        // check if file exists and has same hash
        if file_exists && has_strm_file_same_hash(&file_path, content_hash).await {
            processed_strm.insert(relative_file_path);
            continue; // skip creation
        }

        // if we can't create the directory skip this entry
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
        info_err_res!("{}", failed.join(", "))
    }
}
async fn write_strm_index_file(
    cfg: &AppConfig,
    entries: &HashSet<String>,
    index_file_path: &PathBuf,
) -> Result<(), String> {
    let _file_lock = cfg
        .file_locks
        .write_lock(index_file_path).await;
    let file = File::create(index_file_path)
        .await
        .map_err(|err| format!("Failed to create strm index file: {} {err}", index_file_path.display()))?;
    // Use a larger buffered writer for sequential writes to reduce syscalls
    let mut writer = async_file_writer(file);
    let mut write_counter = 0usize;
    let new_line = "\n".as_bytes();
    for entry in entries {
        let bytes = entry.as_bytes();
        write_counter += bytes.len() + 1;
        writer
            .write_all(bytes)
            .await
            .map_err(|err| format!("Failed to write strm index entry: {err}"))?;
        writer
            .write_all(new_line)
            .await
            .map_err(|err| format!("Failed to write strm index entry: {err}"))?;
        if write_counter >= IO_BUFFER_SIZE {
            write_counter = 0;
            writer.flush().await.map_err(|err| format!("Failed to flush: {err}"))?;
        }
    }
    writer
        .flush()
        .await
        .map_err(|err| format!("failed to write strm index entry: {err}"))?;
    writer
        .shutdown()
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
        let mut reader = async_file_reader(file);
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
    username: Option<&str>,
) -> Option<(ProxyUserCredentials, ApiProxyServerInfo)> {
    let username = username?;
    let credentials = cfg.get_user_credentials(username)?;
    let server_info = cfg.get_user_server_info(&credentials);
    Some((credentials, server_info))
}

async fn read_strm_file_index(strm_file_index_path: &Path) -> std::io::Result<HashSet<String>> {
    let file = File::open(strm_file_index_path).await?;
    let reader = async_file_reader(file);
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
) -> Arc<str> {
    let Some((user, server_info)) = user_and_server_info else { return str_item_info.url.clone(); };

    let redirect = user.proxy.is_redirect(str_item_info.item_type) || target_force_redirect.is_some_and(|f| f.has_cluster(str_item_info.item_type));
    if redirect {
        return str_item_info.url.clone();
    }

    if let Some(stream_type) = match str_item_info.item_type {
        PlaylistItemType::Live => Some("live"),
        PlaylistItemType::Series
        | PlaylistItemType::SeriesInfo
        | PlaylistItemType::LocalSeries
        | PlaylistItemType::LocalSeriesInfo => Some("series"),
        PlaylistItemType::Video
        | PlaylistItemType::LocalVideo => Some("movie"),
        _ => None,
    } {
        let url = &str_item_info.url;
        let ext = extract_extension_from_url(url).unwrap_or_default();
        format!(
            "{}/{stream_type}/{}/{}/{}{ext}",
            server_info.get_base_url(),
            user.username,
            user.password,
            str_item_info.virtual_id
        ).into()
    } else {
        str_item_info.url.clone()
    }
}

// /////////////////////////////////////////////
// - Cleanup -
// We first build a Directory Tree to
//  identify the deletable files and directories
// /////////////////////////////////////////////
#[derive(Debug, Clone)]
struct DirNode {
    path: PathBuf,
    is_root: bool, // is root -> do not delete!
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
/// Because of async operations ve can't use recursion.
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