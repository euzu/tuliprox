use crate::{storage::ensure_target_storage_path, storage_const};
use chrono::Datelike;
use filetime::{set_file_times, FileTime};
use log::{error, trace};
use serde::Serialize;
use shared::{
    error::TuliproxError,
    model::{MediaQuality, PlaylistGroup, PlaylistItem, PlaylistItemType, StreamProperties, StrmExportStyle, UUIDType},
    utils::{
        arc_str_option_serde, arc_str_serde, clean_playlist_title, hash_bytes, hash_string_as_hex,
        is_blank_optional_arc_str, sanitize_sensitive_info, truncate_string, ExportStyleConfig, CONSTANTS,
        PROVIDER_SCHEME_PREFIX,
    },
};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    fs::{create_dir_all, remove_dir, remove_file, File},
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt},
};
use tuliprox_core::{
    model::{
        ApiProxyServerInfo, AppConfig, ConfigInput, ConfigTarget, ProxyUserCredentials, StrmTargetFlags,
        StrmTargetOutput,
    },
    utils::{
        async_file_reader, async_file_writer, encode_provider_resolve_playlist_item_token, normalize_string_path,
        truncate_filename, ProviderResolvePlaylistItemToken, IO_BUFFER_SIZE, PROVIDER_RESOLVE_ROUTE_PREFIX,
    },
};

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
            } else if "+=,._-@#()[]".contains(c) {
                // <-- Allow list of safe punctuation, added [ and ] for quality tags.
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
                if (1900..=cur_year + 5).contains(&year) {
                    // Allow slightly future years
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
    target_path.join(PathBuf::from(format!(
        "{file_prefix}_{}.{}",
        storage_const::FILE_STRM,
        storage_const::FILE_SUFFIX_DB
    )))
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
    pub fn get_file_ts(&self) -> Option<u64> {
        self.added
    }
}

fn extract_item_info(pli: &mut PlaylistItem, use_metadata: bool) -> StrmItemInfo {
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
    let (title, series_name, release_date, series_release_date, added, tmdb_id, season, episode) = match header
        .item_type
    {
        PlaylistItemType::Series | PlaylistItemType::LocalSeries => {
            let (prop_name, release_date, series_release_date, added, tmdb_id, season, episode) =
                match header.additional_properties.as_ref() {
                    None => (None, None, None, None, None, None, None),
                    Some(props) => (
                        if use_metadata {
                            if let StreamProperties::Series(series) = props {
                                (!series.name.is_empty()).then(|| series.name.clone())
                            } else {
                                None
                            }
                        } else {
                            None
                        },
                        props.get_release_date(),
                        // Extract series-level release date from Episode properties
                        if let StreamProperties::Episode(ep) = props { ep.series_release_date.clone() } else { None },
                        props.get_added(),
                        props.get_tmdb_id().filter(|&id| id != 0),
                        props.get_season(),
                        props.get_episode(),
                    ),
                };

            // For series title, we prefer the one from metadata (prop_name), then header.name, then header.title
            let final_series_name = prop_name.unwrap_or_else(|| {
                if header.name.is_empty() {
                    header.title.clone()
                } else {
                    header.name.clone()
                }
            });

            // Episode title relies on header.title unless we want to look deeper into props
            let ep_title = header.title.clone();

            (ep_title, Some(final_series_name), release_date, series_release_date, added, tmdb_id, season, episode)
        }
        PlaylistItemType::Video | PlaylistItemType::LocalVideo => {
            let (metadata_title, release_date, added, tmdb_id) = match header.additional_properties.as_ref() {
                None => (None, None, None, None),
                Some(props) => (
                    if use_metadata {
                        if let StreamProperties::Video(video) = props {
                            (!video.name.is_empty()).then(|| video.name.clone())
                        } else {
                            None
                        }
                    } else {
                        None
                    },
                    props.get_release_date(),
                    props.get_added(),
                    props.get_tmdb_id().filter(|&id| id != 0),
                ),
            };

            let final_title = metadata_title.unwrap_or_else(|| header.title.clone());

            (final_title, None, release_date, None, added, tmdb_id, None, None)
        }
        _ => (header.title.clone(), None, None, None, None, None, None, None),
    };

    StrmItemInfo {
        group,
        title,
        item_type,
        provider_id,
        virtual_id: virtual_id.get(),
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
        return Err(TuliproxError::Config(format!("Error creating STRM directory: {e}")));
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
        return Err(format!("Error: STRM directory does not exist: {}", root_path.display()));
    }

    let to_remove: HashSet<String> = if cleanup {
        // Remove al files which are not in `processed`
        let mut found_files = HashSet::new();
        let files = read_files_non_recursive(root_path).await.map_err(|err| err.to_string())?;
        for file_path in files {
            if let Some(file_name) = file_path.strip_prefix(root_path).ok().and_then(|p| p.to_str()) {
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
    matches!(
        item_type,
        PlaylistItemType::Live
            | PlaylistItemType::Video
            | PlaylistItemType::LocalVideo
            | PlaylistItemType::Series
            | PlaylistItemType::LocalSeries
    )
}

fn get_relative_path_str(full_path: &Path, root_path: &Path) -> String {
    full_path
        .strip_prefix(root_path)
        .map_or_else(|_| full_path.to_string_lossy(), |relative| relative.to_string_lossy())
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
    let id_string = if tmdb_id > 0 { id_format.replace("{}", &tmdb_id.to_string()) } else { String::new() };

    // Determine source name and date based on type
    let (raw_name, raw_date) = match strm_item_info.item_type {
        PlaylistItemType::Series | PlaylistItemType::LocalSeries => (
            strm_item_info.series_name.as_ref().unwrap_or(&strm_item_info.title),
            strm_item_info.series_release_date.as_ref(),
        ),
        _ => (&strm_item_info.title, strm_item_info.release_date.as_ref()),
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

/// Longest file stem the writer keeps; `.strm` is appended on top, staying inside the 255-byte
/// name limit common to ext4/APFS/NTFS.
const MAX_STRM_FILE_STEM_LEN: usize = 250;

/// Movie folders already claimed in `flat` mode, keyed by `TMDb` id. Holds the folder itself and
/// the file name the first item put in it.
type FlatDedupPaths = HashMap<u32, (PathBuf, String)>;

/// Points `dir_path`/`final_filename` at the flat folder for `tmdb_id`, creating it on first use.
///
/// Providers routinely list the same movie more than once with differently written titles. In
/// `flat` mode those listings are deduplicated onto one folder by `TMDb` id, so every listing after
/// the first lands in a folder that was named after *another* listing's title. Naming such a file
/// after its own title breaks the media servers' alternate-version detection, which requires the
/// file name to start with the name of the folder it sits in. So the first item to claim a folder
/// also fixes the file name for everything that follows it into that folder; the differing
/// quality suffix (or, failing that, the `[Version id#N]` collision suffix) is what keeps the
/// versions apart.
fn claim_flat_movie_folder(
    flat_dedup_paths: &mut FlatDedupPaths,
    tmdb_id: u32,
    folder_name: &str,
    dir_path: &mut PathBuf,
    final_filename: &mut String,
) {
    if let Some((claimed_path, claimed_filename)) = flat_dedup_paths.get(&tmdb_id) {
        dir_path.clone_from(claimed_path);
        final_filename.clone_from(claimed_filename);
    } else {
        dir_path.push(folder_name);
        flat_dedup_paths.insert(tmdb_id, (dir_path.clone(), final_filename.clone()));
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
    flat_dedup_paths: &mut FlatDedupPaths,
) -> (PathBuf, String) {
    // Kodi ID format: {tmdb=12345}
    let parts = prepare_filename_parts(strm_item_info, tmdb_id, separator, &format!("{separator}{{tmdb={{}}}}"));
    let mut dir_path = PathBuf::new();

    match strm_item_info.item_type {
        PlaylistItemType::Video | PlaylistItemType::LocalVideo => {
            let folder_name = format!("{}{}", parts.base_name, parts.id_string);
            let mut final_filename = parts.base_name;

            if flat {
                if tmdb_id > 0 {
                    claim_flat_movie_folder(
                        flat_dedup_paths,
                        tmdb_id,
                        &folder_name,
                        &mut dir_path,
                        &mut final_filename,
                    );
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

/// Formats names according to the official Emby documentation.
/// Movie: /Movie Name (Year)/Movie Name (Year) [tmdbid=XXXXX].strm
/// Series: /Show Name (Year) [tmdbid=XXXXX]/Season 01/Show Name - S01E01.strm
fn format_for_emby(
    strm_item_info: &StrmItemInfo,
    tmdb_id: u32,
    separator: &str,
    flat: bool,
    flat_dedup_paths: &mut FlatDedupPaths,
) -> (PathBuf, String) {
    // Emby ID format: [tmdbid=12345]
    let parts = prepare_filename_parts(strm_item_info, tmdb_id, separator, &format!("{separator}[tmdbid={{}}]"));
    let mut dir_path = PathBuf::new();

    match strm_item_info.item_type {
        PlaylistItemType::Video | PlaylistItemType::LocalVideo => {
            // Emby prefers the ID in the filename for movies, folder optional
            let folder_name = parts.base_name.clone(); // Folder name does not contain the ID usually, but can
            let mut final_filename = format!("{}{}", parts.base_name, parts.id_string);

            if flat {
                if tmdb_id > 0 {
                    claim_flat_movie_folder(
                        flat_dedup_paths,
                        tmdb_id,
                        &folder_name,
                        &mut dir_path,
                        &mut final_filename,
                    );
                } else {
                    // See format_for_jellyfin: without a TMDB id the category keeps the folder
                    // unique, so the file name must carry it as well.
                    let folder_with_category = format!("{folder_name}{separator}[{}]", parts.category);
                    final_filename = format!("{folder_with_category}{}", parts.id_string);
                    dir_path.push(folder_with_category);
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
    flat_dedup_paths: &mut FlatDedupPaths,
) -> (PathBuf, String) {
    // Jellyfin ID format: [tmdbid-12345]
    let parts = prepare_filename_parts(strm_item_info, tmdb_id, separator, &format!("{separator}[tmdbid-{{}}]"));
    let mut dir_path = PathBuf::new();

    match strm_item_info.item_type {
        PlaylistItemType::Video | PlaylistItemType::LocalVideo => {
            // Jellyfin requirement: file name MUST start with parent folder name to detect versions
            let folder_name = format!("{}{}", parts.base_name, parts.id_string);
            let mut final_filename = folder_name.clone();

            if flat {
                if tmdb_id > 0 {
                    claim_flat_movie_folder(
                        flat_dedup_paths,
                        tmdb_id,
                        &folder_name,
                        &mut dir_path,
                        &mut final_filename,
                    );
                } else {
                    // No TMDB id to deduplicate on, so the category is what keeps this folder
                    // unique. The file name has to carry it too, or it no longer starts with the
                    // name of the folder it sits in.
                    let folder_with_category = format!("{folder_name}{separator}[{}]", parts.category);
                    dir_path.push(&folder_with_category);
                    final_filename = folder_with_category;
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
    flat_dedup_paths: &mut FlatDedupPaths,
) -> (PathBuf, String) {
    let separator = if underscore_whitespace { "_" } else { " " };

    let tmdb_id = tmdb.or(strm_item_info.tmdb_id).unwrap_or(0);

    // Dispatch the call to the responsible function based on the style.
    match style {
        StrmExportStyle::Kodi => format_for_kodi(strm_item_info, tmdb_id, separator, flat, flat_dedup_paths),
        StrmExportStyle::Emby => format_for_emby(strm_item_info, tmdb_id, separator, flat, flat_dedup_paths),
        StrmExportStyle::Jellyfin => format_for_jellyfin(strm_item_info, tmdb_id, separator, flat, flat_dedup_paths),
    }
}

fn prepare_strm_files(new_playlist: &mut [PlaylistGroup], strm_target_output: &StrmTargetOutput) -> Vec<StrmFile> {
    let channel_count = new_playlist.iter().map(|g| g.filter_count(filter_strm_item)).sum();
    // contains all paths (dir + filename) to detect collisions
    let mut all_filenames: HashSet<PathBuf> = HashSet::with_capacity(channel_count);
    // contains only collision filenames (PathBuf)
    let mut collisions: HashSet<PathBuf> = HashSet::new();
    let mut result = Vec::with_capacity(channel_count);

    let mut flat_dedup_paths: FlatDedupPaths = HashMap::new();

    let underscore_whitespace = strm_target_output.flags.contains(StrmTargetFlags::UnderscoreWhitespace);
    let separator = if underscore_whitespace { "_" } else { " " };
    let flat = strm_target_output.flags.contains(StrmTargetFlags::Flat);

    // first we create the names to identify name collisions
    for pg in new_playlist.iter_mut() {
        for pli in pg.channels.iter_mut().filter(|c| filter_strm_item(c)) {
            let strm_item_info =
                extract_item_info(pli, strm_target_output.flags.contains(StrmTargetFlags::UseMetadata));

            let (dir_path, strm_file_name) = style_based_rename(
                &strm_item_info,
                pli.get_tmdb_id(),
                strm_target_output.style,
                underscore_whitespace,
                flat,
                &mut flat_dedup_paths,
            );

            // Conditionally generate the quality string based on the new config flag
            let quality_string = get_quality(strm_target_output, pli, separator);

            // No category suffix: in `flat` mode the category used to be appended to guard against
            // collisions, but the only files that can now collide are versions of the same movie
            // (same TMDB folder, same quality string), and the collision pass below separates those
            // with `[Version id#N]`. Keeping the category here would only pollute the name that
            // Jellyfin/Emby show as the *version label*, which should read as the quality alone.
            let filename = Arc::new(format!("{strm_file_name}{quality_string}"));

            // Construct the full relative path for collision checking
            let full_relative_path = dir_path.join(filename.as_str());

            if !all_filenames.insert(full_relative_path.clone()) {
                collisions.insert(full_relative_path);
            }
            result.push(StrmFile { file_name: filename, dir_path, strm_info: strm_item_info });
        }
    }

    if !collisions.is_empty() {
        // This separator is specifically for the multi-version naming convention.
        let version_separator = " ";

        for s in &mut result {
            let full_relative_path = s.dir_path.join(s.file_name.as_str());
            if collisions.contains(&full_relative_path) {
                // Create a descriptive and unique identifier for this version.
                let version_label = format!("Version{}id#{}", separator, s.strm_info.virtual_id);
                let suffix = format!("{version_separator}[{version_label}]");

                // The version label is the ONLY thing telling these files apart, so it has to
                // survive the truncation the writer applies. Trim the shared base instead --
                // truncating the tail here would collapse both versions onto one name, and the
                // second would silently overwrite the first.
                let budget = MAX_STRM_FILE_STEM_LEN.saturating_sub(suffix.chars().count());
                let base_filename = truncate_string(&s.file_name, budget);

                s.file_name = Arc::new(format!("{base_filename}{suffix}"));
            }
        }
    }
    result
}

fn get_quality(strm_target_output: &StrmTargetOutput, pli: &PlaylistItem, separator: &str) -> String {
    if strm_target_output.flags.contains(StrmTargetFlags::AddQualityToFilename) {
        // Use `additional_properties` which are populated by metadata_update_manager/probe
        let (audio, video) = match pli.header.additional_properties.as_ref() {
            None => (None, None),
            Some(props) => match props {
                StreamProperties::Live(_) | StreamProperties::Series(_) => (None, None),
                StreamProperties::Video(video) => {
                    video.details.as_ref().map_or_else(|| (None, None), |d| (d.audio.as_deref(), d.video.as_deref()))
                }
                StreamProperties::Episode(episode) => (episode.audio.as_deref(), episode.video.as_deref()),
            },
        };
        if let Some(media_quality) = MediaQuality::from_ffprobe_info(audio, video) {
            let formatted = media_quality.format_for_filename(separator);
            if !formatted.is_empty() {
                // Hard-coded separator for filename clarity.
                return format!(" - [{formatted}]");
            }
        }
    }
    String::new()
}

/// Returns `true` if `s` contains a known TMDB path marker.
fn strm_contains_tmdb_marker(s: &str) -> bool {
    s.contains(" {tmdb=")
        || s.contains(" {tmdb-")
        || s.contains(" [tmdbid=")
        || s.contains(" [tmdbid-")
        || s.contains("_{tmdb=")
        || s.contains("_{tmdb-")
        || s.contains("_[tmdbid=")
        || s.contains("_[tmdbid-")
}

/// Strips all TMDB markers (e.g. ` {tmdb=123}`, ` [tmdbid-456]`) from a path string,
/// returning the equivalent no-tmdb path used for identity matching.
fn strip_tmdb_markers(s: &str) -> String {
    let mut result = s.to_string();
    for marker_prefix in
        &[" {tmdb=", " {tmdb-", " [tmdbid=", " [tmdbid-", "_{tmdb=", "_{tmdb-", "_[tmdbid=", "_[tmdbid-"]
    {
        while let Some(start) = result.find(marker_prefix) {
            let close_char = if marker_prefix.contains('{') { '}' } else { ']' };
            let search_from = start + marker_prefix.len();
            if let Some(rel_close) = result[search_from..].find(close_char) {
                let end = search_from + rel_close + 1;
                result.replace_range(start..end, "");
            } else {
                break;
            }
        }
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
    let Some(root_path) = tuliprox_core::utils::get_file_path(
        &config.storage_dir,
        Some(std::path::PathBuf::from(&target_output.directory)),
    ) else {
        return Err(TuliproxError::Config(format!("Failed to get file path for {}", target_output.directory)));
    };

    let user_and_server_info = get_credentials_and_server_info(app_config, target_output.username.as_deref())
        .map_err(TuliproxError::Config)?;
    let normalized_dir = normalize_string_path(&target_output.directory);
    let strm_file_prefix = hash_string_as_hex(&normalized_dir);
    let strm_index_path =
        strm_get_file_paths(&strm_file_prefix, &ensure_target_storage_path(&config, target.name.as_str()).await?);
    let existing_strm = {
        let _file_lock = app_config.file_locks.read_lock(&strm_index_path).await;
        read_strm_file_index(&strm_index_path).await.unwrap_or_else(|_| HashSet::with_capacity(4096))
    };
    let mut processed_strm: HashSet<String> = HashSet::with_capacity(existing_strm.len());

    // Build a lookup map: stripped_path -> enriched_path for all existing STRM paths
    // that already contain a TMDB marker. Used to preserve enriched filenames when the
    // current playlist item has no tmdb_id yet (avoids rename back to plain name).
    let enriched_strm: std::collections::HashMap<String, String> = existing_strm
        .iter()
        .filter(|p| strm_contains_tmdb_marker(p))
        .map(|p| (strip_tmdb_markers(p), p.clone()))
        .collect();

    let mut failed = vec![];

    prepare_strm_output_directory(&root_path).await?;

    let mut input_by_name: HashMap<Arc<str>, Option<Arc<ConfigInput>>> = HashMap::new();

    let strm_files = prepare_strm_files(new_playlist, target_output);

    for strm_file in strm_files {
        // file paths
        let output_path = truncate_filename(&root_path.join(&strm_file.dir_path), 255);
        let file_path =
            output_path.join(format!("{}.strm", truncate_string(&strm_file.file_name, MAX_STRM_FILE_STEM_LEN)));

        let relative_file_path = get_relative_path_str(&file_path, &root_path);

        let (target_relative_file_path, target_file_path) = get_target_strm_file_path(
            &root_path,
            &enriched_strm,
            &relative_file_path,
            file_path,
            strm_file.strm_info.tmdb_id,
        );
        let file_exists = target_file_path.exists();

        // create content
        let url = match resolve_strm_file_url(
            app_config,
            &mut input_by_name,
            target,
            user_and_server_info.as_ref(),
            &strm_file.strm_info,
        ) {
            Ok(url) => url,
            Err(err) => {
                failed.push(err);
                continue;
            }
        };
        let (content_as_bytes, content_hash) = build_strm_content(target_output, &url);

        // check if file exists and has same hash
        if file_exists && has_strm_file_same_hash(&target_file_path, content_hash).await {
            processed_strm.insert(target_relative_file_path);
            continue; // skip creation
        }

        if !write_strm_output_file(
            &mut failed,
            &target_file_path,
            &output_path,
            &content_as_bytes,
            strm_file.strm_info.get_file_ts(),
        )
        .await
        {
            continue;
        }
        processed_strm.insert(target_relative_file_path);
    }

    if let Err(err) = write_strm_index_file(app_config, &processed_strm, &strm_index_path).await {
        failed.push(err);
    }

    if let Err(err) = cleanup_strm_output_directory(
        target_output.flags.contains(StrmTargetFlags::Cleanup),
        &root_path,
        &existing_strm,
        &processed_strm,
    )
    .await
    {
        failed.push(err);
    }

    if failed.is_empty() {
        Ok(())
    } else {
        Err(TuliproxError::Config(failed.join(", ")))
    }
}
async fn write_strm_index_file(
    cfg: &AppConfig,
    entries: &HashSet<String>,
    index_file_path: &PathBuf,
) -> Result<(), String> {
    let _file_lock = cfg.file_locks.write_lock(index_file_path).await;
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
        writer.write_all(bytes).await.map_err(|err| format!("Failed to write strm index entry: {err}"))?;
        writer.write_all(new_line).await.map_err(|err| format!("Failed to write strm index entry: {err}"))?;
        if write_counter >= IO_BUFFER_SIZE {
            write_counter = 0;
            writer.flush().await.map_err(|err| format!("Failed to flush: {err}"))?;
        }
    }
    writer.flush().await.map_err(|err| format!("failed to write strm index entry: {err}"))?;
    writer.shutdown().await.map_err(|err| format!("failed to write strm index entry: {err}"))?;
    Ok(())
}

async fn ensure_strm_file_directory(failed: &mut Vec<String>, output_path: &Path) -> bool {
    if !output_path.exists() {
        if let Err(e) = create_dir_all(output_path).await {
            let err_msg = format!("Failed to create directory for strm playlist: {} {e}", output_path.display());
            error!("{err_msg}");
            failed.push(err_msg);
            return false; // skip creation, could not create directory
        };
    }
    true
}

async fn write_strm_output_file(
    failed: &mut Vec<String>,
    target_file_path: &Path,
    output_path: &Path,
    content_as_bytes: &[u8],
    timestamp: Option<u64>,
) -> bool {
    let target_output_path =
        target_file_path.parent().map_or_else(|| output_path.to_path_buf(), std::path::Path::to_path_buf);
    if !ensure_strm_file_directory(failed, &target_output_path).await {
        return false;
    }

    match write_strm_file(target_file_path, content_as_bytes, timestamp).await {
        Ok(()) => true,
        Err(err) => {
            failed.push(err);
            false
        }
    }
}

async fn write_strm_file(file_path: &Path, content_as_bytes: &[u8], timestamp: Option<u64>) -> Result<(), String> {
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
) -> Result<Option<(Arc<ProxyUserCredentials>, ApiProxyServerInfo)>, String> {
    let Some(username) = username else {
        return Ok(None);
    };
    let credentials = cfg.get_user_credentials(username).ok_or_else(|| {
        format!(
            "STRM output references user '{}' but no matching API proxy user exists",
            sanitize_sensitive_info(username)
        )
    })?;
    let server_info = cfg.get_user_server_info(credentials.as_ref()).ok_or_else(|| {
        format!(
            "STRM output references user '{}' but no API proxy server info is configured",
            sanitize_sensitive_info(username)
        )
    })?;
    Ok(Some((credentials, server_info)))
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

fn resolve_strm_file_url(
    app_config: &AppConfig,
    input_by_name: &mut HashMap<Arc<str>, Option<Arc<ConfigInput>>>,
    target: &ConfigTarget,
    user_and_server_info: Option<&(Arc<ProxyUserCredentials>, ApiProxyServerInfo)>,
    str_item_info: &StrmItemInfo,
) -> Result<Arc<str>, String> {
    let input = if user_and_server_info.is_none() && str_item_info.url.starts_with(PROVIDER_SCHEME_PREFIX) {
        input_by_name
            .entry(Arc::clone(&str_item_info.input_name))
            .or_insert_with(|| app_config.get_input_by_name(&str_item_info.input_name))
            .clone()
    } else {
        None
    };

    let resolve_secret = app_config.get_reverse_proxy_rewrite_secret().unwrap_or(app_config.encrypt_secret);
    get_strm_url(&resolve_secret, target.id, user_and_server_info, input.as_deref(), str_item_info)
}

fn get_target_strm_file_path(
    root_path: &Path,
    enriched_strm: &HashMap<String, String>,
    relative_file_path: &str,
    file_path: PathBuf,
    tmdb_id: Option<u32>,
) -> (String, PathBuf) {
    if tmdb_id.is_none() {
        if let Some(enriched_path) = enriched_strm.get(relative_file_path) {
            return (enriched_path.clone(), root_path.join(enriched_path));
        }
    }
    (relative_file_path.to_string(), file_path)
}

fn build_strm_content(target_output: &StrmTargetOutput, url: &str) -> (Vec<u8>, UUIDType) {
    let mut content = target_output.strm_props.as_ref().map_or_else(Vec::new, std::clone::Clone::clone);
    content.push(url.to_string());
    let content_text = content.join("\r\n");
    let content_as_bytes = content_text.into_bytes();
    let content_hash = hash_bytes(&content_as_bytes);
    (content_as_bytes, content_hash)
}

fn resolve_strm_source_url(input: Option<&ConfigInput>, str_item_info: &StrmItemInfo) -> Result<Arc<str>, String> {
    if !str_item_info.url.starts_with(PROVIDER_SCHEME_PREFIX) {
        return Ok(Arc::clone(&str_item_info.url));
    }

    let input = input.ok_or_else(|| {
        format!(
            "Failed to resolve STRM provider URL for input '{}' because the source input is missing: {}",
            sanitize_sensitive_info(&str_item_info.input_name),
            sanitize_sensitive_info(&str_item_info.url)
        )
    })?;

    input.resolve_url(&str_item_info.url).map(|resolved| Arc::<str>::from(resolved.into_owned())).map_err(|err| {
        format!(
            "Failed to resolve STRM provider URL for input '{}': {} ({err})",
            sanitize_sensitive_info(&str_item_info.input_name),
            sanitize_sensitive_info(&str_item_info.url)
        )
    })
}

fn build_provider_resolve_url(
    secret: &[u8; 16],
    server_info: &ApiProxyServerInfo,
    user: &ProxyUserCredentials,
    target_id: u16,
    str_item_info: &StrmItemInfo,
) -> Result<String, String> {
    let cluster = str_item_info.item_type.cluster();
    let token = encode_provider_resolve_playlist_item_token(
        secret,
        &ProviderResolvePlaylistItemToken {
            username: user.username.clone(),
            target_id,
            virtual_id: str_item_info.virtual_id,
            cluster,
        },
    )
    .map_err(|err| format!("Failed to create STRM provider resolve token: {err}"))?;

    Ok(format!("{}{PROVIDER_RESOLVE_ROUTE_PREFIX}/{token}", server_info.get_base_url()))
}

fn get_strm_url(
    resolve_secret: &[u8; 16],
    target_id: u16,
    user_and_server_info: Option<&(Arc<ProxyUserCredentials>, ApiProxyServerInfo)>,
    input: Option<&ConfigInput>,
    str_item_info: &StrmItemInfo,
) -> Result<Arc<str>, String> {
    let Some((user, server_info)) = user_and_server_info else {
        return resolve_strm_source_url(input, str_item_info);
    };

    build_provider_resolve_url(resolve_secret, server_info, user.as_ref(), target_id, str_item_info).map(Arc::from)
}

// /////////////////////////////////////////////
// - Cleanup -
// We first build a Directory Tree to
//  identify the deletable files and directories
// /////////////////////////////////////////////
#[derive(Debug, Clone)]
struct DirNode {
    path: PathBuf,
    is_root: bool,   // is root -> do not delete!
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
        Self { path, is_root, has_files: false, children: HashSet::new(), parent }
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
fn flatten_tree(root_path: &Path, mut tree_nodes: HashMap<PathBuf, DirNode>) -> Vec<DirNode> {
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

    paths_to_process.iter().filter_map(|path| tree_nodes.remove(path)).collect()
}

async fn delete_empty_dirs_from_tree(root_path: &Path, tree_nodes: HashMap<PathBuf, DirNode>) {
    let tree_stack = flatten_tree(root_path, tree_nodes);
    // reverse order  to delete from leaf to root
    for node in tree_stack.into_iter().rev() {
        if !node.has_files && !node.is_root {
            if let Err(err) = remove_dir(&node.path).await {
                trace!("Could not delete empty dir: {}, {err}", node.path.display());
            }
        }
    }
}
async fn remove_empty_dirs(root_path: PathBuf) {
    let tree_nodes = build_directory_tree(&root_path).await;
    delete_empty_dirs_from_tree(&root_path, tree_nodes).await;
}

#[cfg(test)]
mod tests {
    use super::{
        build_provider_resolve_url, get_credentials_and_server_info, prepare_strm_files, resolve_strm_source_url,
        strip_tmdb_markers, strm_contains_tmdb_marker, StrmFile, StrmItemInfo,
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::model::{
        ConfigPaths, ConfigProviderDto, InputType, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType,
        ProviderUrlSelectionPolicy, ProxyType, SeriesStreamProperties, StreamProperties, StrmExportStyle,
        VideoStreamDetailProperties, VideoStreamProperties, VirtualId, XtreamCluster,
    };
    use std::{collections::HashMap, sync::Arc};
    use tuliprox_core::{
        model::{
            ApiProxyConfig, ApiProxyServerInfo, AppConfig, Config, ConfigInput, ConfigProvider, MediaToolCapabilities,
            ProxyUserCredentials, SourcesConfig, StrmTargetFlags, StrmTargetFlagsSet, StrmTargetOutput, TargetUser,
        },
        utils::{decode_provider_resolve_token, FileLockManager, ProviderResolveToken, PROVIDER_RESOLVE_ROUTE_PREFIX},
    };

    fn make_strm_item(url: &str, input_name: &str) -> StrmItemInfo {
        StrmItemInfo {
            group: Arc::from("group"),
            title: Arc::from("title"),
            item_type: PlaylistItemType::Live,
            provider_id: None,
            virtual_id: 1,
            input_name: Arc::from(input_name),
            url: Arc::from(url),
            series_name: None,
            release_date: None,
            series_release_date: None,
            season: None,
            episode: None,
            added: None,
            tmdb_id: None,
        }
    }

    fn make_input_with_provider() -> ConfigInput {
        let provider = ConfigProvider::from(&ConfigProviderDto {
            name: "myprovider".into(),
            urls: vec!["http://provider.example.com".into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
            dns: None,
        });

        ConfigInput {
            name: Arc::from("input-a"),
            input_type: InputType::M3u,
            headers: HashMap::new(),
            url: "http://input.example.com".to_string(),
            provider_configs: Some(vec![Arc::new(provider)]),
            ..Default::default()
        }
    }

    fn make_server_info() -> ApiProxyServerInfo {
        ApiProxyServerInfo {
            name: "default".to_string(),
            protocol: "http".to_string(),
            host: "proxy.example.com".to_string(),
            port: None,
            timezone: "UTC".to_string(),
            message: String::new(),
            path: None,
        }
    }

    fn make_user(proxy: ProxyType) -> ProxyUserCredentials {
        let mut user = ProxyUserCredentials::default();
        user.username = "alice".to_string();
        user.password = "secret".to_string();
        user.proxy = proxy;
        user
    }

    fn empty_paths() -> ConfigPaths {
        ConfigPaths {
            home_path: String::new(),
            config_path: String::new(),
            storage_path: String::new(),
            config_file_path: String::new(),
            sources_file_path: String::new(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        }
    }

    fn test_app_config(api_proxy: Option<ApiProxyConfig>) -> AppConfig {
        AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::new(api_proxy.map(Arc::new))),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(empty_paths())),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
        }
    }

    #[test]
    fn tmdb_marker_helpers_cover_supported_marker_variants() {
        let cases = [
            (
                "Movies/Movie Name (2020) {tmdb-12345}/Movie Name (2020).strm",
                "Movies/Movie Name (2020)/Movie Name (2020).strm",
            ),
            (
                "Movies/Movie Name (2020) {tmdb=12345}/Movie Name (2020).strm",
                "Movies/Movie Name (2020)/Movie Name (2020).strm",
            ),
            (
                "Movies/Movie Name (2020) [tmdbid-12345]/Movie Name (2020).strm",
                "Movies/Movie Name (2020)/Movie Name (2020).strm",
            ),
            (
                "Movies/Movie Name (2020) [tmdbid=12345]/Movie Name (2020).strm",
                "Movies/Movie Name (2020)/Movie Name (2020).strm",
            ),
            (
                "Movies/Movie_Name_(2020)_{tmdb-12345}/Movie_Name_(2020).strm",
                "Movies/Movie_Name_(2020)/Movie_Name_(2020).strm",
            ),
            (
                "Movies/Movie_Name_(2020)_{tmdb=12345}/Movie_Name_(2020).strm",
                "Movies/Movie_Name_(2020)/Movie_Name_(2020).strm",
            ),
            (
                "Movies/Movie_Name_(2020)_[tmdbid-12345]/Movie_Name_(2020).strm",
                "Movies/Movie_Name_(2020)/Movie_Name_(2020).strm",
            ),
            (
                "Movies/Movie_Name_(2020)_[tmdbid=12345]/Movie_Name_(2020).strm",
                "Movies/Movie_Name_(2020)/Movie_Name_(2020).strm",
            ),
        ];

        for (path, expected) in cases {
            assert!(strm_contains_tmdb_marker(path));
            assert_eq!(strip_tmdb_markers(path), expected);
        }
    }

    #[test]
    fn resolve_strm_source_url_resolves_provider_scheme_without_user_context() {
        let input = make_input_with_provider();
        let strm_item = make_strm_item("provider://myprovider/live/1.ts", "input-a");

        let resolved = resolve_strm_source_url(Some(&input), &strm_item);

        assert_eq!(resolved.as_deref(), Ok("http://provider.example.com/live/1.ts"));
    }

    #[test]
    fn resolve_strm_source_url_fails_when_provider_input_is_missing() {
        let strm_item = make_strm_item("provider://myprovider/live/1.ts", "missing-input");

        let err = resolve_strm_source_url(None, &strm_item).err();

        assert!(err.is_some_and(|message| message.contains("source input is missing")));
    }

    #[test]
    fn provider_resolve_url_contains_compact_playlist_item_token() {
        let secret = [5u8; 16];
        let strm_item = make_strm_item("provider://myprovider/live/1.ts", "input-a");
        let user = make_user(ProxyType::Reverse(None));
        let server_info = make_server_info();

        let url = build_provider_resolve_url(&secret, &server_info, &user, 17, &strm_item).unwrap();

        let prefix = format!("http://proxy.example.com{PROVIDER_RESOLVE_ROUTE_PREFIX}/");
        assert!(url.starts_with(&prefix));
        let token = &url[prefix.len()..];
        assert!(token.len() < 100, "token too long: {}", token.len());
        let decoded = decode_provider_resolve_token(&secret, token).unwrap();
        let ProviderResolveToken::PlaylistItem(decoded) = decoded;
        assert_eq!(decoded.username, "alice");
        assert_eq!(decoded.target_id, 17);
        assert_eq!(decoded.virtual_id, 1);
    }

    #[test]
    fn explicit_strm_username_fails_when_user_is_missing() {
        let app_config = test_app_config(None);

        let result = get_credentials_and_server_info(&app_config, Some("missing"));

        assert!(result.is_err());
    }

    #[test]
    fn explicit_strm_username_fails_when_server_is_missing() {
        let user = Arc::new(make_user(ProxyType::Reverse(None)));
        let app_config = test_app_config(Some(ApiProxyConfig {
            user: vec![TargetUser { target: "target".to_string(), credentials: vec![user] }],
            server: Vec::new(),
            ..Default::default()
        }));

        let result = get_credentials_and_server_info(&app_config, Some("alice"));

        assert!(result.is_err());
    }

    // ---- flat/tmdb multi-version naming -------------------------------------------------

    const VIDEO_1080P: &str = r#"{"codec_name":"h264","width":1920,"height":1080}"#;
    const VIDEO_720P: &str = r#"{"codec_name":"h264","width":1280,"height":720}"#;
    const AUDIO_EAC3_51: &str = r#"{"codec_name":"eac3","channels":6}"#;

    fn make_video_pli(
        title: &str,
        group: &str,
        tmdb: u32,
        virtual_id: u32,
        video: Option<&str>,
        audio: Option<&str>,
    ) -> PlaylistItem {
        let details = if video.is_some() || audio.is_some() {
            Some(VideoStreamDetailProperties {
                video: video.map(Arc::from),
                audio: audio.map(Arc::from),
                ..Default::default()
            })
        } else {
            None
        };
        let props = VideoStreamProperties { name: Arc::from(title), tmdb: Some(tmdb), details, ..Default::default() };

        PlaylistItem {
            header: PlaylistItemHeader {
                title: Arc::from(title),
                name: Arc::from(title),
                group: Arc::from(group),
                virtual_id: VirtualId::new(virtual_id),
                item_type: PlaylistItemType::Video,
                additional_properties: Some(StreamProperties::Video(Box::new(props))),
                ..Default::default()
            },
        }
    }

    fn make_series_pli(series_name: &str, group: &str) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                title: Arc::from("Episode Title"),
                name: Arc::from(series_name),
                group: Arc::from(group),
                item_type: PlaylistItemType::Series,
                additional_properties: Some(StreamProperties::Series(Box::new(SeriesStreamProperties {
                    name: Arc::from("Metadata Series"),
                    ..Default::default()
                }))),
                ..Default::default()
            },
        }
    }

    fn strm_output(style: StrmExportStyle, flags: &[StrmTargetFlags]) -> StrmTargetOutput {
        let mut flag_set = StrmTargetFlagsSet::new();
        for flag in flags {
            flag_set.set(*flag);
        }
        StrmTargetOutput {
            directory: String::new(),
            username: None,
            style,
            flags: flag_set,
            strm_props: None,
            filter: None,
            probe_probe_size_bytes: None,
            probe_analyze_duration: None,
        }
    }

    #[test]
    fn strm_filename_uses_mapped_video_title_over_metadata_title() {
        let mut item = make_video_pli("Metadata Movie", "Movies", 1, 1, None, None);
        item.header.title = Arc::from("Mapped Movie");
        let mut playlist = vec![PlaylistGroup {
            id: 1,
            title: Arc::from("Movies"),
            channels: vec![item],
            xtream_cluster: XtreamCluster::Video,
        }];

        let files = prepare_strm_files(&mut playlist, &strm_output(StrmExportStyle::Jellyfin, &[]));

        assert!(files[0].file_name.contains("Mapped Movie"));
        assert!(!files[0].file_name.contains("Metadata Movie"));
    }

    #[test]
    fn strm_filename_uses_metadata_title_when_configured() {
        let mut item = make_video_pli("Metadata Movie", "Movies", 1, 1, None, None);
        item.header.title = Arc::from("Mapped Movie");
        let mut playlist = vec![PlaylistGroup {
            id: 1,
            title: Arc::from("Movies"),
            channels: vec![item],
            xtream_cluster: XtreamCluster::Video,
        }];

        let files =
            prepare_strm_files(&mut playlist, &strm_output(StrmExportStyle::Jellyfin, &[StrmTargetFlags::UseMetadata]));

        assert!(files[0].file_name.contains("Metadata Movie"));
        assert!(!files[0].file_name.contains("Mapped Movie"));
    }

    #[test]
    fn strm_series_filename_uses_processed_name_without_metadata_flag() {
        let mut playlist = vec![PlaylistGroup {
            id: 1,
            title: Arc::from("Series"),
            channels: vec![make_series_pli("Mapped Series", "Series")],
            xtream_cluster: XtreamCluster::Series,
        }];

        let files = prepare_strm_files(&mut playlist, &strm_output(StrmExportStyle::Jellyfin, &[]));

        assert!(files[0].file_name.contains("Mapped Series"));
        assert!(!files[0].file_name.contains("Metadata Series"));
    }

    /// The two ways one provider spells the same film in the same category: same tmdb id,
    /// different titles. Under `flat` they are deduped into a single folder.
    fn duplicate_listings_of_one_movie() -> Vec<PlaylistGroup> {
        vec![PlaylistGroup {
            id: 1,
            title: Arc::from("EN MOVIES"),
            channels: vec![
                make_video_pli(
                    "400 Bullets [MULTI-SUB] - 2021",
                    "EN MOVIES",
                    788_672,
                    11,
                    Some(VIDEO_1080P),
                    Some(AUDIO_EAC3_51),
                ),
                make_video_pli(
                    "400 Bullets -  [Multi Sub] (2021)",
                    "EN MOVIES",
                    788_672,
                    12,
                    Some(VIDEO_720P),
                    Some(AUDIO_EAC3_51),
                ),
            ],
            xtream_cluster: XtreamCluster::Video,
        }]
    }

    /// Port of Jellyfin's `Emby.Naming.Video.VideoListResolver.IsEligibleForMultiVersion`:
    /// the file name must start with the folder name, and the remainder must be empty or
    /// start with `-`, `_`, `.`, or a `[bracketed]` token. Any file in the folder failing
    /// this makes Jellyfin abandon version grouping for the *whole* folder.
    fn jellyfin_multi_version_eligible(folder_name: &str, file_stem: &str) -> bool {
        if !file_stem.to_lowercase().starts_with(&folder_name.to_lowercase()) {
            return false;
        }
        let rest = file_stem[folder_name.len()..].trim();
        rest.is_empty() || rest.starts_with(['-', '_', '.']) || (rest.starts_with('[') && rest[1..].contains(']'))
    }

    fn assert_single_shared_folder(files: &[StrmFile]) -> String {
        assert_eq!(files.len(), 2, "both provider listings must be exported");
        assert_eq!(files[0].dir_path, files[1].dir_path, "same tmdb id must dedup into one folder");
        files[0].dir_path.file_name().unwrap().to_string_lossy().to_string()
    }

    #[test]
    fn jellyfin_flat_names_every_version_after_the_folder_it_lands_in() {
        let mut playlist = duplicate_listings_of_one_movie();
        let output =
            strm_output(StrmExportStyle::Jellyfin, &[StrmTargetFlags::Flat, StrmTargetFlags::AddQualityToFilename]);

        let files = prepare_strm_files(&mut playlist, &output);
        let folder_name = assert_single_shared_folder(&files);

        for file in &files {
            assert!(
                jellyfin_multi_version_eligible(&folder_name, &file.file_name),
                "Jellyfin will show this as a separate movie instead of an alternate version:\n  \
                 folder: {folder_name}\n  file:   {}",
                file.file_name
            );
        }
        assert_ne!(files[0].file_name, files[1].file_name, "versions must not overwrite each other");
    }

    #[test]
    fn emby_flat_names_every_version_after_the_folder_it_lands_in() {
        let mut playlist = duplicate_listings_of_one_movie();
        let output =
            strm_output(StrmExportStyle::Emby, &[StrmTargetFlags::Flat, StrmTargetFlags::AddQualityToFilename]);

        let files = prepare_strm_files(&mut playlist, &output);
        let folder_name = assert_single_shared_folder(&files);

        for file in &files {
            assert!(
                jellyfin_multi_version_eligible(&folder_name, &file.file_name),
                "folder: {folder_name}\n  file:   {}",
                file.file_name
            );
        }
        assert_ne!(files[0].file_name, files[1].file_name);
    }

    #[test]
    fn kodi_flat_names_every_version_after_the_folder_it_lands_in() {
        let mut playlist = duplicate_listings_of_one_movie();
        let output =
            strm_output(StrmExportStyle::Kodi, &[StrmTargetFlags::Flat, StrmTargetFlags::AddQualityToFilename]);

        let files = prepare_strm_files(&mut playlist, &output);
        assert_single_shared_folder(&files);

        // Kodi keeps the tmdb marker out of the file name, so compare against the shared base:
        // both versions must be named after the same movie, not after their own provider title.
        let base = strip_tmdb_markers(&files[0].dir_path.file_name().unwrap().to_string_lossy()).trim().to_string();
        for file in &files {
            assert!(file.file_name.starts_with(&base), "base: {base}\n  file: {}", file.file_name);
        }
        assert_ne!(files[0].file_name, files[1].file_name);
    }

    /// When both copies carry the same quality the names collide; the existing collision
    /// handler must disambiguate them with `[Version id#N]` while keeping the shared base.
    #[test]
    fn jellyfin_flat_versions_with_identical_quality_get_a_version_label() {
        let mut playlist = duplicate_listings_of_one_movie();
        // Make the second listing report exactly the same streams as the first.
        playlist[0].channels[1] = make_video_pli(
            "400 Bullets -  [Multi Sub] (2021)",
            "EN MOVIES",
            788_672,
            12,
            Some(VIDEO_1080P),
            Some(AUDIO_EAC3_51),
        );
        let output =
            strm_output(StrmExportStyle::Jellyfin, &[StrmTargetFlags::Flat, StrmTargetFlags::AddQualityToFilename]);

        let files = prepare_strm_files(&mut playlist, &output);
        let folder_name = assert_single_shared_folder(&files);

        for file in &files {
            assert!(
                jellyfin_multi_version_eligible(&folder_name, &file.file_name),
                "folder: {folder_name}\n  file:   {}",
                file.file_name
            );
            assert!(file.file_name.contains("[Version id#"), "expected a version label in {}", file.file_name);
        }
        assert_ne!(files[0].file_name, files[1].file_name, "versions must not overwrite each other");
    }

    /// Jellyfin/Emby show whatever follows the folder name as the *version label*, so in `flat`
    /// mode it must be the quality string alone — not the provider's category.
    #[test]
    fn flat_version_label_is_the_quality_alone() {
        let mut playlist = duplicate_listings_of_one_movie();
        let output =
            strm_output(StrmExportStyle::Jellyfin, &[StrmTargetFlags::Flat, StrmTargetFlags::AddQualityToFilename]);

        let files = prepare_strm_files(&mut playlist, &output);
        let folder_name = assert_single_shared_folder(&files);

        for file in &files {
            let label = file.file_name[folder_name.len()..].trim();
            assert!(!label.contains("EN MOVIES"), "category leaked into the version label: {label}");
        }
        assert!(files.iter().any(|f| f.file_name.ends_with("- [1080p FHD H.264 EAC3 5.1]")));
        assert!(files.iter().any(|f| f.file_name.ends_with("- [720p HD H.264 EAC3 5.1]")));
    }

    /// The writer truncates the file stem, so for two versions that differ ONLY by their version
    /// label the label has to survive: otherwise both truncate to the same name and one silently
    /// overwrites the other.
    #[test]
    fn colliding_versions_stay_distinct_after_the_writer_truncates_the_name() {
        let long_title = format!("{} (2021)", "A".repeat(300));
        let mut playlist = vec![PlaylistGroup {
            id: 1,
            title: Arc::from("EN MOVIES"),
            channels: vec![
                make_video_pli(&long_title, "EN MOVIES", 555, 31, Some(VIDEO_1080P), Some(AUDIO_EAC3_51)),
                make_video_pli(&long_title, "EN MOVIES", 555, 32, Some(VIDEO_1080P), Some(AUDIO_EAC3_51)),
            ],
            xtream_cluster: XtreamCluster::Video,
        }];
        let output =
            strm_output(StrmExportStyle::Jellyfin, &[StrmTargetFlags::Flat, StrmTargetFlags::AddQualityToFilename]);

        let files = prepare_strm_files(&mut playlist, &output);

        // Names as the writer will actually store them.
        let stored: Vec<String> =
            files.iter().map(|f| shared::utils::truncate_string(&f.file_name, super::MAX_STRM_FILE_STEM_LEN)).collect();

        assert!(stored.iter().all(|n| n.chars().count() <= super::MAX_STRM_FILE_STEM_LEN));
        assert!(stored[0].ends_with("[Version id#31]"), "version label was truncated away: {}", stored[0]);
        assert!(stored[1].ends_with("[Version id#32]"), "version label was truncated away: {}", stored[1]);
        assert_ne!(stored[0], stored[1], "one version would silently overwrite the other on disk");
    }

    /// Items without a TMDB id cannot share a deduplicated folder, so they keep the category in the
    /// *directory* name to stay unique.
    #[test]
    fn flat_without_tmdb_keeps_the_category_in_the_directory() {
        let mut playlist = vec![PlaylistGroup {
            id: 1,
            title: Arc::from("EN MOVIES"),
            channels: vec![make_video_pli("Some Obscure Film (1999)", "EN MOVIES", 0, 21, Some(VIDEO_720P), None)],
            xtream_cluster: XtreamCluster::Video,
        }];
        let output = strm_output(StrmExportStyle::Jellyfin, &[StrmTargetFlags::Flat]);

        let files = prepare_strm_files(&mut playlist, &output);

        let dir = files[0].dir_path.to_string_lossy();
        assert!(dir.contains("[EN MOVIES]"), "expected the category in the directory name, got {dir}");
        assert!(files[0].file_name.starts_with(dir.as_ref()), "file must still start with its folder name");
    }

    /// Without `flat` there is no folder reuse: each listing keeps its own folder and name.
    #[test]
    fn non_flat_keeps_each_listing_in_its_own_folder() {
        let mut playlist = duplicate_listings_of_one_movie();
        let output = strm_output(StrmExportStyle::Jellyfin, &[StrmTargetFlags::AddQualityToFilename]);

        let files = prepare_strm_files(&mut playlist, &output);

        assert_eq!(files.len(), 2);
        assert_ne!(files[0].dir_path, files[1].dir_path, "non-flat must not merge folders");
        for file in &files {
            let folder_name = file.dir_path.file_name().unwrap().to_string_lossy().to_string();
            assert!(jellyfin_multi_version_eligible(&folder_name, &file.file_name));
        }
    }
}
