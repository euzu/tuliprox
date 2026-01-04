use std::borrow::Cow;
use crate::model::ConfigInput;
use crate::model::{XtreamCategory};
use crate::utils::request::DynReader;
use crate::utils::xtream::get_xtream_stream_url_base;
use shared::error::{create_tuliprox_error_result, TuliproxError, TuliproxErrorKind};
use shared::model::{EpisodeStreamProperties, LiveStreamProperties, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType, SeriesStreamDetailEpisodeProperties, SeriesStreamProperties, StreamProperties, UUIDType, VideoStreamProperties, XtreamCluster};
use shared::utils::{generate_playlist_uuid, trim_last_slash, StringInterner};
use indexmap::IndexMap;
use tokio::task::spawn_blocking;

async fn map_to_xtream_category(categories: DynReader) -> Result<Vec<XtreamCategory>, TuliproxError> {
    spawn_blocking(move || {
        let reader = tokio_util::io::SyncIoBridge::new(categories);
        match serde_json::from_reader::<_, Vec<XtreamCategory>>(reader) {
            Ok(xtream_categories) => Ok(xtream_categories),
            Err(err) => {
                create_tuliprox_error_result!(TuliproxErrorKind::Notify, "Failed to process categories {}", &err)
            }
        }
    }).await.map_err(|e| TuliproxError::new(TuliproxErrorKind::Notify, format!("Mapping xtream categories failed: {e}")))?
}

async fn map_to_xtream_streams(xtream_cluster: XtreamCluster, streams: DynReader) -> Result<Vec<StreamProperties>, TuliproxError> {
    spawn_blocking(move || {
        let reader = tokio_util::io::SyncIoBridge::new(streams);

        let parsed: Result<Vec<StreamProperties>, serde_json::Error> = match xtream_cluster {
            XtreamCluster::Live => serde_json::from_reader::<_, Vec<LiveStreamProperties>>(reader).map(|list| list.into_iter().map(StreamProperties::Live).collect()),
            XtreamCluster::Video => serde_json::from_reader::<_, Vec<VideoStreamProperties>>(reader).map(|list| list.into_iter().map(Box::new).map(StreamProperties::Video).collect()),
            XtreamCluster::Series => serde_json::from_reader::<_, Vec<SeriesStreamProperties>>(reader).map(|list| list.into_iter().map(Box::new).map(StreamProperties::Series).collect()),
        };

        match parsed {
            Ok(mut stream_list) => {
                for stream in &mut stream_list {
                    stream.prepare();
                }
                Ok(stream_list)
            }
            Err(err) => {
                create_tuliprox_error_result!(TuliproxErrorKind::Notify, "Failed to map to xtream streams {xtream_cluster}: {err}", )
            }
        }
    }).await.map_err(|e| TuliproxError::new(TuliproxErrorKind::Notify, format!("Mapping xtream streams failed: {e}")))?
}

fn create_xtream_series_episode_url<'a>(url: &'a str, username: &'a str, password: &'a str, episode: &'a SeriesStreamDetailEpisodeProperties) -> Cow<'a, str> {
    if episode.direct_source.is_empty() {
        let ext = episode.container_extension.clone();
        let stream_base_url = format!("{url}/series/{username}/{password}/{}.{ext}", episode.id);
        Cow::Owned(stream_base_url)
    } else {
        Cow::Borrowed(&episode.direct_source)
    }
}

pub fn parse_xtream_series_info(parent_uuid: &UUIDType, series_info: &SeriesStreamProperties, group_title: &str, series_name: &str, input: &ConfigInput) -> Option<Vec<PlaylistItem>> {
    let url = input.url.as_str();
    let username = input.username.as_ref().map_or("", |v| v);
    let password = input.password.as_ref().map_or("", |v| v);

    if let Some(episodes) = series_info.details.as_ref().and_then(|d| d.episodes.as_ref()) {
        let result: Vec<PlaylistItem> = episodes.iter().map(|episode| {
            let episode_url = create_xtream_series_episode_url(url, username, password, episode);
            let episode_info = EpisodeStreamProperties::from_series(series_info, episode);
             PlaylistItem {
                 header: PlaylistItemHeader {
                     id: episode.id.to_string(),
                     uuid: generate_playlist_uuid(&input.name, &episode.id.to_string(), PlaylistItemType::Series, &episode_url),
                     // we use parent_code to track the parent series
                     parent_code: parent_uuid.to_string(),
                     name: series_name.to_string(),
                     logo: episode.movie_image.clone(),
                     group: shared::utils::intern(group_title),
                     title: episode.title.clone(),
                     url: episode_url.to_string(),
                     item_type: PlaylistItemType::Series,
                     xtream_cluster: XtreamCluster::Series,
                     additional_properties: Some(StreamProperties::Episode(episode_info)),
                     category_id: 0,
                     input_name: shared::utils::intern(&input.name),
                     ..Default::default()
                 }
             }
        }).collect();
        return if result.is_empty() { None } else { Some(result) };
    }
    None
}

#[allow(clippy::too_many_arguments)]
pub fn get_xtream_url(xtream_cluster: XtreamCluster, url: &str,
                      username: &str, password: &str,
                      stream_id: u32, container_extension: Option<&str>,
                      live_stream_use_prefix: bool, live_stream_without_extension: bool) -> String {
    let url = trim_last_slash(url);
    let stream_base_url = match xtream_cluster {
        XtreamCluster::Live => {
            let ctx_path = if live_stream_use_prefix { "live/" } else { "" };
            let suffix = if live_stream_without_extension { "" } else { ".ts" };
            format!("{url}/{ctx_path}{username}/{password}/{stream_id}{suffix}")
        }
        XtreamCluster::Video => {
            if let Some(extension) = container_extension {
                format!("{url}/movie/{username}/{password}/{stream_id}.{extension}")
            } else {
                format!("{url}/movie/{username}/{password}/{stream_id}")
            }
        }
        XtreamCluster::Series =>
            format!("{}&action={}&series_id={stream_id}", get_xtream_stream_url_base(url.as_ref(), username, password), crate::model::XC_ACTION_GET_SERIES_INFO)
    };
    stream_base_url
}

pub fn create_xtream_url<'a>(xtream_cluster: XtreamCluster, url: &'a str, username: &'a str, password: &'a str,
                         stream: &'a StreamProperties, live_stream_use_prefix: bool, live_stream_without_extension: bool) -> Cow<'a, str> {
    stream.get_direct_source().unwrap_or_else(||
        Cow::Owned(get_xtream_url(xtream_cluster, url, username, password, stream.get_stream_id(),
                       stream.get_container_extension().as_deref(),
                       live_stream_use_prefix, live_stream_without_extension))
    )
}

pub async fn parse_xtream(input: &ConfigInput,
                          xtream_cluster: XtreamCluster,
                          categories: DynReader,
                          streams: DynReader) -> Result<Option<Vec<PlaylistGroup>>, TuliproxError> {
    match map_to_xtream_category(categories).await {
        Ok(xtream_categories) => {
            let mut interner = StringInterner::new();
            let input_name = input.name.clone();
            let url = input.url.as_str();
            let username = input.username.as_ref().map_or("", |v| v);
            let password = input.password.as_ref().map_or("", |v| v);

            match map_to_xtream_streams(xtream_cluster, streams).await {
                Ok(xtream_streams) => {
                    let mut group_map: IndexMap<u32, XtreamCategory> =
                        xtream_categories.into_iter().map(|category| (category.category_id, category)).collect();
                    let mut unknown_grp = XtreamCategory {
                        category_id: 0u32,
                        category_name: "Unknown".to_string(),
                        channels: vec![],
                    };

                    let (live_stream_use_prefix, live_stream_without_extension) = input.options.as_ref()
                        .map_or((true, false), |o| (o.xtream_live_stream_use_prefix, o.xtream_live_stream_without_extension));

                    for stream in xtream_streams {
                        let group = group_map.get_mut(&stream.get_category_id()).unwrap_or(&mut unknown_grp);
                        let category_name = &group.category_name;
                        let stream_url = create_xtream_url(xtream_cluster, url, username, password, &stream, live_stream_use_prefix, live_stream_without_extension);
                        let item_type = PlaylistItemType::from(xtream_cluster);
                        let item = PlaylistItem {
                            header: PlaylistItemHeader {
                                id: stream.get_stream_id().to_string(),
                                uuid: generate_playlist_uuid(&input_name, &stream.get_stream_id().to_string(), item_type, &stream_url),
                                name: stream.get_name().to_string(),
                                logo: stream.get_stream_icon().to_string(),
                                group: interner.intern(category_name),
                                title: stream.get_name().to_string(),
                                url: stream_url.to_string(),
                                epg_channel_id: stream.get_epg_channel_id(),
                                item_type,
                                xtream_cluster,
                                additional_properties: Some(stream),
                                category_id: 0,
                                input_name: interner.intern(&input_name),
                                ..Default::default()
                            },
                        };
                        group.add(item);
                    }


                    let has_channels = !unknown_grp.channels.is_empty();
                    if has_channels {
                        group_map.insert(0, unknown_grp);
                    }

                    Ok(Some(group_map.values().filter(|category| !category.channels.is_empty())
                        .map(|category| {
                            PlaylistGroup {
                                id: category.category_id,
                                xtream_cluster,
                                title: category.category_name.clone(),
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

#[cfg(test)]
mod tests {
    use crate::processing::parser::xtream::map_to_xtream_streams;
    use crate::utils::async_file_reader;
    use shared::model::{XtreamCluster, XtreamSeriesInfo};
    use std::fs;

    #[test]
    fn test_read_json_file_into_struct() {
        if fs::exists("/tmp/series-info.json").unwrap_or(false) {
            let file_content = fs::read_to_string("/tmp/series-info.json").expect("Unable to read file");
            match serde_json::from_str::<XtreamSeriesInfo>(&file_content) {
                Ok(series_info) => {
                    println!("{:#?}", series_info);
                    assert!(true);
                }
                Err(err) => {
                    assert!(false, "Failed to parse json file: {err}");
                }
            }
        }
    }

    #[tokio::test]
    async fn test_read_json_stream_into_struct() -> std::io::Result<()> {
        if fs::exists("/tmp/vod_streams.json").unwrap_or(false) {
            let reader = Box::pin(async_file_reader(tokio::fs::File::open("/tmp/vod_streams.json").await?));
            match map_to_xtream_streams(XtreamCluster::Video, reader).await {
                Ok(_streams) => {
                    println!("{:?}", _streams.get(1));
                    println!("{:?}", _streams.get(100));
                    println!("{:?}", _streams.get(200));
                    assert!(true);
                }
                Err(err) => {
                    assert!(false, "Failed to parse json file: {err}");
                }
            };
        }
        Ok(())
    }
}