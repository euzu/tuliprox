use crate::model::ConfigInput;
use crate::model::XtreamCategory;
use crate::utils::request::DynReader;
use crate::utils::xtream::get_xtream_stream_url_base;
use indexmap::IndexMap;
use serde::Deserializer;
use shared::error::{notify_err, notify_err_res, TuliproxError};
use shared::model::UUIDType;
use shared::model::{EpisodeStreamProperties, LiveStreamProperties, PlaylistGroup, PlaylistItem,
                    PlaylistItemHeader, PlaylistItemType, SeriesStreamDetailEpisodeProperties,
                    SeriesStreamProperties, StreamProperties, VideoStreamProperties,
                    XtreamCluster, XtreamPlaylistItem};
use shared::utils::{generate_playlist_uuid, trim_last_slash, Internable};
use std::sync::Arc;
use tokio::task::spawn_blocking;

async fn map_to_xtream_category(categories: DynReader) -> Result<Vec<XtreamCategory>, TuliproxError> {
    spawn_blocking(move || {
        let reader = tokio_util::io::SyncIoBridge::new(categories);
        match serde_json::from_reader::<_, Vec<XtreamCategory>>(reader) {
            Ok(xtream_categories) => Ok(xtream_categories),
            Err(err) => {
                notify_err_res!("Failed to process categories {}", &err)
            }
        }
    }).await.map_err(|e| notify_err!("Mapping xtream categories failed: {e}"))?
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
                notify_err_res!("Failed to map to xtream streams {xtream_cluster}: {err}")
            }
        }
    }).await.map_err(|e| notify_err!("Mapping xtream streams failed: {e}"))?
}

fn create_xtream_series_episode_url(url: &str, username: &str, password: &str, episode: &SeriesStreamDetailEpisodeProperties) -> Arc<str> {
    if episode.direct_source.is_empty() {
        let ext = episode.container_extension.clone();
        let stream_base_url = format!("{url}/series/{username}/{password}/{}.{ext}", episode.id);
        stream_base_url.into()
    } else {
        Arc::clone(&episode.direct_source)
    }
}

pub fn parse_xtream_series_info(parent_uuid: &UUIDType, series_info: &SeriesStreamProperties,
                                group_title: &str, series_name: &Arc<str>, input: &ConfigInput) -> Option<Vec<PlaylistItem>> {
    let url = input.url.as_str();
    let username = input.username.as_ref().map_or("", |v| v);
    let password = input.password.as_ref().map_or("", |v| v);

    if let Some(episodes) = series_info.details.as_ref().and_then(|d| d.episodes.as_ref()) {
        let result: Vec<PlaylistItem> = episodes.iter().map(|episode| {
            let episode_id = episode.id.to_string();
            let episode_url = create_xtream_series_episode_url(url, username, password, episode);
            let episode_info = EpisodeStreamProperties::from_series(series_info, episode);
            PlaylistItem {
                header: PlaylistItemHeader {
                    uuid: generate_playlist_uuid(&input.name, &episode_id, PlaylistItemType::Series, &episode_url),
                    id: episode_id.into(),
                    // we use parent_code to track the parent series
                    parent_code: parent_uuid.intern(),
                    name: Arc::clone(series_name),
                    logo: Arc::clone(&episode.movie_image),
                    group: group_title.intern(),
                    title: Arc::clone(&episode.title),
                    url: episode_url,
                    item_type: PlaylistItemType::Series,
                    xtream_cluster: XtreamCluster::Series,
                    additional_properties: Some(StreamProperties::Episode(episode_info)),
                    category_id: 0,
                    input_name: input.name.intern(),
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

pub fn create_xtream_url(xtream_cluster: XtreamCluster, url: &str, username: &str, password: &str,
                         stream: &StreamProperties, live_stream_use_prefix: bool, live_stream_without_extension: bool) -> Arc<str> {
    stream.get_direct_source().unwrap_or_else(||
        get_xtream_url(xtream_cluster, url, username, password, stream.get_stream_id(),
                       stream.get_container_extension().as_deref(),
                       live_stream_use_prefix, live_stream_without_extension).into()
    )
}

pub async fn parse_xtream(input: &ConfigInput,
                          xtream_cluster: XtreamCluster,
                          categories: DynReader,
                          streams: DynReader) -> Result<Option<Vec<PlaylistGroup>>, TuliproxError> {
    match map_to_xtream_category(categories).await {
        Ok(xtream_categories) => {
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
                        category_name: "Unknown".intern(),
                        channels: vec![],
                    };

                    let (live_stream_use_prefix, live_stream_without_extension) = input.options.as_ref()
                        .map_or((true, false), |o| (o.xtream_live_stream_use_prefix, o.xtream_live_stream_without_extension));

                    // Re-implement the loop to add ordinal
                    let mut ord_counter: u32 = 1;
                    for stream in xtream_streams {
                        let group = group_map.get_mut(&stream.get_category_id()).unwrap_or(&mut unknown_grp);
                        let category_name = &group.category_name;
                        let stream_url = create_xtream_url(xtream_cluster, url, username, password, &stream, live_stream_use_prefix, live_stream_without_extension);
                        let item_type = PlaylistItemType::from(xtream_cluster);
                        let mut item = PlaylistItem {
                            header: PlaylistItemHeader {
                                id: stream.get_stream_id().intern(),
                                uuid: generate_playlist_uuid(&input_name, &stream.get_stream_id().to_string(), item_type, &stream_url),
                                name: Arc::clone(&stream.get_name()),
                                logo: Arc::clone(&stream.get_stream_icon()),
                                group: Arc::clone(category_name),
                                title: Arc::clone(&stream.get_name()),
                                url: stream_url.clone(),
                                epg_channel_id: stream.get_epg_channel_id().clone(),
                                item_type,
                                xtream_cluster,
                                category_id: stream.get_category_id(),
                                additional_properties: Some(stream),
                                input_name: input_name.clone(),
                                ..Default::default()
                            },
                        };
                        item.header.source_ordinal = ord_counter;
                        ord_counter += 1;
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
                                title: Arc::clone(&category.category_name),
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

pub async fn parse_xtream_streaming<F>(
    input: &ConfigInput,
    xtream_cluster: XtreamCluster,
    categories: DynReader,
    streams: DynReader,
    mut on_item: F,
) -> Result<Vec<XtreamCategory>, TuliproxError>
where
    F: FnMut(XtreamPlaylistItem) -> Result<(), TuliproxError> + Send + 'static,
{
    // 1. Parse Categories
    let xtream_categories = map_to_xtream_category(categories).await?;

    // 2. Prepare for Stream Parsing
    let input_name = input.name.clone();
    let url = input.url.as_str().to_string();
    let username = input.username.as_ref().map_or("", |v| v).to_string();
    let password = input.password.as_ref().map_or("", |v| v).to_string();
    let options = input.options.clone();

    // Map categories for lookup
    let group_map: IndexMap<u32, Arc<str>> = xtream_categories.iter().map(|c| (c.category_id, c.category_name.clone())).collect();
    let unknown_group_name = "Unknown".intern();

    spawn_blocking(move || {
        let reader = tokio_util::io::SyncIoBridge::new(streams);
        let mut deserializer = serde_json::Deserializer::from_reader(reader);

        let (live_stream_use_prefix, live_stream_without_extension) = options.as_ref()
            .map_or((true, false), |o| (o.xtream_live_stream_use_prefix, o.xtream_live_stream_without_extension));

        let mut source_ordinal = 0u32;

        match xtream_cluster {
            XtreamCluster::Live => {
                let mut on_stream = |stream: LiveStreamProperties| {
                    source_ordinal += 1;
                    let stream_prop = StreamProperties::Live(stream);
                    process_stream_item(&input_name, &url, &username, &password,
                                        xtream_cluster, &group_map, &unknown_group_name,
                                        stream_prop, &mut on_item, live_stream_use_prefix, live_stream_without_extension, source_ordinal)
                };
                let visitor = XtreamItemVisitor { on_item: &mut on_stream, _marker: std::marker::PhantomData };
                deserializer.deserialize_any(visitor).map_err(|e| notify_err!("JSON parse error: {e}"))?;
            }
            XtreamCluster::Video => {
                let mut on_stream = |stream: VideoStreamProperties| {
                    source_ordinal += 1;
                    let stream_prop = StreamProperties::Video(Box::new(stream));
                    process_stream_item(&input_name, &url, &username, &password,
                                        xtream_cluster, &group_map, &unknown_group_name,
                                        stream_prop, &mut on_item, live_stream_use_prefix, live_stream_without_extension, source_ordinal)
                };
                let visitor = XtreamItemVisitor { on_item: &mut on_stream, _marker: std::marker::PhantomData };
                deserializer.deserialize_any(visitor).map_err(|e| notify_err!("JSON parse error: {e}"))?;
            }
            XtreamCluster::Series => {
                let mut on_stream = |stream: SeriesStreamProperties| {
                    source_ordinal += 1;
                    let stream_prop = StreamProperties::Series(Box::new(stream));
                    process_stream_item(&input_name, &url, &username, &password,
                                        xtream_cluster, &group_map, &unknown_group_name,
                                        stream_prop, &mut on_item, live_stream_use_prefix, live_stream_without_extension, source_ordinal)
                };
                let visitor = XtreamItemVisitor { on_item: &mut on_stream, _marker: std::marker::PhantomData };
                deserializer.deserialize_any(visitor).map_err(|e| notify_err!("JSON parse error: {e}"))?;
            }
        }
        Ok(())
    }).await.map_err(|e| notify_err!("Streaming parse failed: {e}"))??;

    Ok(xtream_categories)
}

#[allow(clippy::too_many_arguments)]
fn process_stream_item<F>(
    input_name: &Arc<str>,
    url: &str, username: &str, password: &str,
    cluster: XtreamCluster,
    group_map: &IndexMap<u32, Arc<str>>,
    unknown_group_name: &Arc<str>,
    mut stream: StreamProperties,
    callback: &mut F,
    live_stream_use_prefix: bool,
    live_stream_without_extension: bool,
    source_ordinal: u32,
) -> Result<(), TuliproxError>
where
    F: FnMut(XtreamPlaylistItem) -> Result<(), TuliproxError>,
{
    stream.prepare();
    let category_id = stream.get_category_id();
    let category_name = group_map.get(&category_id).unwrap_or(unknown_group_name);
    let stream_url = create_xtream_url(cluster, url, username, password, &stream, live_stream_use_prefix, live_stream_without_extension);

    let item_type = PlaylistItemType::from(cluster);
    let item = PlaylistItem {
        header: PlaylistItemHeader {
            id: stream.get_stream_id().intern(),
            uuid: generate_playlist_uuid(input_name, &stream.get_stream_id().to_string(), item_type, &stream_url),
            name: stream.get_name(),
            logo: stream.get_stream_icon(),
            group: category_name.clone(),
            title: stream.get_name(),
            url: stream_url,
            epg_channel_id: stream.get_epg_channel_id(),
            item_type,
            xtream_cluster: cluster,
            additional_properties: Some(stream),
            category_id,
            source_ordinal,
            input_name: Arc::clone(input_name),
            ..Default::default()
        },
    };

    // if let Some(StreamProperties::Series(props)) = item.header.additional_properties.as_mut() {
    //      // We need to set category_id for Series properties just like parse_xtream might expect or use?
    //      // Actually parse_xtream doesn't modify internal category_ids, but mapping to XtreamCategory struct relies on it.
    //      // Here we are creating PlaylistItem.
    //      let _ = props;
    // }

    callback(XtreamPlaylistItem::from(&item))
}

struct XtreamItemVisitor<'a, T, F> {
    on_item: &'a mut F,
    _marker: std::marker::PhantomData<T>,
}

impl<'de, T, F> serde::de::Visitor<'de> for XtreamItemVisitor<'_, T, F>
where
    T: serde::Deserialize<'de>,
    F: FnMut(T) -> Result<(), TuliproxError>,
{
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a JSON array or an error object")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        while let Some(item) = seq.next_element::<T>()? {
            (self.on_item)(item).map_err(serde::de::Error::custom)?;
        }
        Ok(())
    }

    fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let val: serde_json::Value = serde::de::Deserialize::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
        if let Some(msg) = val.get("message").and_then(|m| m.as_str()) {
            return Err(serde::de::Error::custom(format!("Xtream API error: {msg}")));
        }
        Err(serde::de::Error::custom(format!("Expected array, got object: {val}")))
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

    #[test]
    fn test_xtream_item_visitor_array() {
        use serde_json::Deserializer;
        use shared::model::LiveStreamProperties;
        let data = r#"[{"name":"stream1", "stream_id": 1, "category_id": 1, "added": "0"}]"#;
        let mut deserializer = Deserializer::from_str(data);
        let mut count = 0;
        let mut on_item = |_: LiveStreamProperties| {
            count += 1;
            Ok(())
        };
        let visitor = super::XtreamItemVisitor { on_item: &mut on_item, _marker: std::marker::PhantomData };
        serde::Deserializer::deserialize_any(&mut deserializer, visitor).unwrap();
        assert_eq!(count, 1);
    }
}
