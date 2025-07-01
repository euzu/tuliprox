use shared::error::{create_tuliprox_error_result, TuliproxError, TuliproxErrorKind};
use crate::model::ConfigInput;
use shared::model::{PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType, XtreamCluster};
use crate::model::{XtreamCategory, XtreamSeriesInfo, XtreamSeriesInfoEpisode, XtreamStream};
use shared::utils::generate_playlist_uuid;
use crate::utils::xtream::{get_xtream_stream_url_base};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

fn map_to_xtream_category(categories: &Value) -> Result<Vec<XtreamCategory>, TuliproxError> {
    match serde_json::from_value::<Vec<XtreamCategory>>(categories.to_owned()) {
        Ok(xtream_categories) => Ok(xtream_categories),
        Err(err) => {
            create_tuliprox_error_result!(TuliproxErrorKind::Notify, "Failed to process categories {}", &err)
        }
    }
}

fn map_to_xtream_streams(xtream_cluster: XtreamCluster, streams: &Value) -> Result<Vec<XtreamStream>, TuliproxError> {
    match serde_json::from_value::<Vec<XtreamStream>>(streams.to_owned()) {
        Ok(stream_list) => Ok(stream_list),
        Err(err) => {
            create_tuliprox_error_result!(TuliproxErrorKind::Notify, "Failed to map to xtream streams {:?}: {}", xtream_cluster, &err)
        }
    }
}

fn create_xtream_series_episode_url(url: &str, username: &str, password: &str, episode: &XtreamSeriesInfoEpisode) -> Arc<String> {
    if episode.direct_source.is_empty() {
        let ext = episode.container_extension.clone();
        let stream_base_url = format!("{url}/series/{username}/{password}/{}.{ext}", episode.id);
        Arc::new(stream_base_url)
    } else {
        Arc::new(episode.direct_source.clone())
    }
}

pub fn parse_xtream_series_info(info: &Value, group_title: &str, series_name: &str, input: &ConfigInput) -> Result<Option<Vec<(XtreamSeriesInfoEpisode, PlaylistItem)>>, TuliproxError> {
    let url = input.url.as_str();
    let username = input.username.as_ref().map_or("", |v| v);
    let password = input.password.as_ref().map_or("", |v| v);

    match serde_json::from_value::<XtreamSeriesInfo>(info.to_owned()) {
        Ok(series_info) => {
            if let Some(episodes) = &series_info.episodes {
                let result: Vec<(XtreamSeriesInfoEpisode, PlaylistItem)> = episodes.values().flatten().map(|episode| {
                    let episode_url = create_xtream_series_episode_url(url, username, password, episode);
                    (episode.clone(),
                     PlaylistItem {
                         header: PlaylistItemHeader {
                             id: episode.id.to_string(),
                             uuid: generate_playlist_uuid(&input.name, &episode.id, PlaylistItemType::Series, &episode_url),
                             name: series_name.to_string(),
                             logo: episode.info.as_ref().map_or_else(String::new, |info| info.movie_image.to_string()),
                             group: group_title.to_string(),
                             title: episode.title.clone(),
                             url: episode_url.to_string(),
                             item_type: PlaylistItemType::Series,
                             xtream_cluster: XtreamCluster::Series,
                             additional_properties: episode.get_additional_properties(&series_info),
                             category_id: 0,
                             input_name: input.name.to_string(),
                             ..Default::default()
                         }
                     })
                }).collect();
                return if result.is_empty() { Ok(None) } else { Ok(Some(result)) };
            }
            Ok(None)
        }
        Err(err) => {
            create_tuliprox_error_result!(TuliproxErrorKind::Notify, "Failed to process series info for {series_name} {err}")
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn get_xtream_url(xtream_cluster: XtreamCluster, url: &str,
                      username: &str, password: &str,
                      stream_id: u32, container_extension: Option<&String>,
                      live_stream_use_prefix: bool, live_stream_without_extension: bool) -> String {
    let stream_base_url = match xtream_cluster {
        XtreamCluster::Live => {
            let ctx_path = if live_stream_use_prefix { "live/" } else { "" };
            let suffix = if live_stream_without_extension { "" } else { ".ts" };
            format!("{url}/{ctx_path}{username}/{password}/{stream_id}{suffix}")
        }
        XtreamCluster::Video => {
            let ext = container_extension.as_ref().map_or("mp4", |e| e.as_str());
            format!("{url}/movie/{username}/{password}/{stream_id}.{ext}")
        }
        XtreamCluster::Series =>
            format!("{}&action={}&series_id={stream_id}", get_xtream_stream_url_base(url, username, password), crate::model::XC_ACTION_GET_SERIES_INFO)
    };
    stream_base_url
}

pub fn create_xtream_url(xtream_cluster: XtreamCluster, url: &str, username: &str, password: &str,
                         stream: &XtreamStream, live_stream_use_prefix: bool, live_stream_without_extension: bool) -> String {
    if stream.direct_source.is_empty() {
        get_xtream_url(xtream_cluster, url, username, password, stream.get_stream_id(),
                       stream.container_extension.as_ref().map(std::string::ToString::to_string).as_ref(),
                       live_stream_use_prefix, live_stream_without_extension)
    } else {
        stream.direct_source.to_string()
    }
}

pub fn parse_xtream(input: &ConfigInput,
                    xtream_cluster: XtreamCluster,
                    categories: &Value,
                    streams: &Value) -> Result<Option<Vec<PlaylistGroup>>, TuliproxError> {
    match map_to_xtream_category(categories) {
        Ok(xtream_categories) => {
            let input_name = Arc::new(input.name.to_string());
            let url = input.url.as_str();
            let username = input.username.as_ref().map_or("", |v| v);
            let password = input.password.as_ref().map_or("", |v| v);

            match map_to_xtream_streams(xtream_cluster, streams) {
                Ok(mut xtream_streams) => {
                    let mut group_map: HashMap<String, XtreamCategory> =
                        xtream_categories.into_iter().map(|category|
                            (category.category_id.to_string(), category)
                        ).collect();
                    let mut unknown_grp = XtreamCategory {
                        category_id: "0".to_string(),
                        category_name: "Unknown".to_string(),
                        channels: vec![],
                    };

                    let (live_stream_use_prefix, live_stream_without_extension) = input.options.as_ref()
                        .map_or((true, false), |o| (o.xtream_live_stream_use_prefix, o.xtream_live_stream_without_extension));

                    for stream in &mut xtream_streams {
                        let group = group_map.get_mut(&stream.category_id).unwrap_or(&mut unknown_grp);
                        let category_name = &group.category_name;
                        let stream_url = create_xtream_url(xtream_cluster, url, username, password, stream, live_stream_use_prefix, live_stream_without_extension);
                        let item_type = PlaylistItemType::from(xtream_cluster);
                        // EPG Channel id fix, remove empty
                        stream.epg_channel_id = if let XtreamCluster::Live = xtream_cluster {
                            stream.epg_channel_id.as_ref()
                                .filter(|epg_id| !epg_id.trim().is_empty())
                                .map(|epg_id| epg_id.to_lowercase())
                                .or(None)
                        } else {
                            None
                        };
                        let item = PlaylistItem {
                            header: PlaylistItemHeader {
                                id: stream.get_stream_id().to_string(),
                                uuid: generate_playlist_uuid(&input_name, &stream.get_stream_id().to_string(), item_type, &stream_url),
                                name: stream.name.to_string(),
                                logo: stream.stream_icon.to_string(),
                                group: category_name.to_string(),
                                title: stream.name.to_string(),
                                url: stream_url.to_string(),
                                epg_channel_id: stream.epg_channel_id.clone(),
                                item_type,
                                xtream_cluster,
                                additional_properties: stream.get_additional_properties(),
                                category_id: 0,
                                input_name: input_name.to_string(),
                                ..Default::default()
                            },
                        };
                        group.add(item);
                    }
                    let has_channels = !unknown_grp.channels.is_empty();
                    if has_channels {
                        group_map.insert("0".to_string(), unknown_grp);
                    }

                    Ok(Some(group_map.values().filter(|category| !category.channels.is_empty())
                        .map(|category| {
                            PlaylistGroup {
                                id: category.category_id.parse::<u32>().unwrap_or(0),
                                xtream_cluster,
                                title: category.category_name.to_string(),
                                channels: category.channels.clone(),
                            }
                        }).collect()))
                }
                Err(err) => Err(err)
            }
        }
        Err(err) => Err(err)
    }
}
