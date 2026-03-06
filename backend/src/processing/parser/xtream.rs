use crate::model::{ConfigInput, ConfigInputFlags};
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

async fn map_to_xtream_category(categories: DynReader, input_name: &Arc<str>) -> Result<Vec<XtreamCategory>, TuliproxError> {
    let input_name_clone = Arc::clone(input_name);
    spawn_blocking(move || {
        let reader = tokio_util::io::SyncIoBridge::new(categories);
        match serde_json::from_reader::<_, Vec<XtreamCategory>>(reader) {
            Ok(xtream_categories) => Ok(xtream_categories),
            Err(err) => {
                notify_err_res!("Failed to process categories input {input_name_clone}: {err}")
            }
        }
    }).await.map_err(|err| notify_err!("Mapping xtream categories failed for input {input_name}: {err}"))?
}

async fn map_to_xtream_streams(xtream_cluster: XtreamCluster, streams: DynReader, input_name: &Arc<str>) -> Result<Vec<StreamProperties>, TuliproxError> {
    let input_name_clone = Arc::clone(input_name);
    spawn_blocking(move || {
        let reader = tokio_util::io::SyncIoBridge::new(streams);

        let parsed: Result<Vec<StreamProperties>, serde_json::Error> = match xtream_cluster {
            XtreamCluster::Live => serde_json::from_reader::<_, Vec<LiveStreamProperties>>(reader).map(|list| list.into_iter().map(Box::new).map(StreamProperties::Live).collect()),
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
                notify_err_res!("Failed to map to xtream streams {xtream_cluster} for input {input_name_clone}: {err}")
            }
        }
    }).await.map_err(|e| notify_err!("Mapping xtream streams failed for input {input_name}: {e}"))?
}

pub fn create_xtream_series_episode_url(url: &str, username: &str, password: &str, episode: &SeriesStreamDetailEpisodeProperties) -> Arc<str> {
    if episode.direct_source.is_empty() {
        let ext = episode.container_extension.clone();
        let stream_base_url = format!("{url}/series/{username}/{password}/{}.{ext}", episode.id);
        stream_base_url.into()
    } else {
        Arc::clone(&episode.direct_source)
    }
}

pub fn parse_xtream_series_info(parent_uuid: &UUIDType, series_info: &SeriesStreamProperties, group_title: &str, series_name: &Arc<str>, input: &ConfigInput,
                                // Add series_release_date parameter
                                series_release_date: Option<&Arc<str>>,
                                parent_source_ordinal: u32,
) -> Option<Vec<PlaylistItem>> {
    let url = input.url.as_str();
    let (username, password) = (
        input.username.as_deref().unwrap_or(""),
        input.password.as_deref().unwrap_or(""),
    );

    if let Some(episodes) = series_info.details.as_ref().and_then(|d| d.episodes.as_ref()) {
        let base_source_ordinal = if parent_source_ordinal == 0 { 1 } else { parent_source_ordinal };
        let result: Vec<PlaylistItem> = episodes.iter().enumerate().map(|(index, episode)| {
            let episode_id = episode.id.to_string();
            let episode_url = create_xtream_series_episode_url(url, username, password, episode);

            // Create properties and inject global release date if available
            let mut episode_info = EpisodeStreamProperties::from_series(series_info, episode);
            if let Some(date) = series_release_date {
                episode_info.series_release_date = Some(Arc::clone(date));
            }


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
                    additional_properties: Some(StreamProperties::Episode(Box::new(episode_info))),
                    category_id: 0,
                    input_name: input.name.intern(),
                    source_ordinal: base_source_ordinal.saturating_add(u32::try_from(index).unwrap_or(u32::MAX)),
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
    match map_to_xtream_category(categories, &input.name).await {
        Ok(xtream_categories) => {
            let input_name = input.name.clone();
            let url = input.url.as_str();
            let (username, password) = (
                input.username.as_deref().unwrap_or(""),
                input.password.as_deref().unwrap_or(""),
            );

            match map_to_xtream_streams(xtream_cluster, streams, &input.name).await {
                Ok(xtream_streams) => {
                    let mut group_map: IndexMap<u32, XtreamCategory> =
                        xtream_categories.into_iter().map(|category| (category.category_id, category)).collect();
                    let mut unknown_grp = XtreamCategory {
                        category_id: 0u32,
                        category_name: "Unknown".intern(),
                        channels: vec![],
                    };

                    let (live_stream_use_prefix, live_stream_without_extension) = (
                        input.has_flag(ConfigInputFlags::XtreamLiveStreamUsePrefix),
                        input.has_flag(ConfigInputFlags::XtreamLiveStreamWithoutExtension),
                    );

                    for (ord_counter, stream) in (1_u32..).zip(xtream_streams) {
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
    let xtream_categories = map_to_xtream_category(categories, &input.name).await?;

    // 2. Prepare for Stream Parsing
    let input_name = input.name.clone();
    let url = input.url.as_str().to_string();
    let (username, password) = (
        input.username.as_deref().unwrap_or("").to_string(),
        input.password.as_deref().unwrap_or("").to_string(),
    );
    let live_stream_use_prefix = input.has_flag(ConfigInputFlags::XtreamLiveStreamUsePrefix);
    let live_stream_without_extension = input.has_flag(ConfigInputFlags::XtreamLiveStreamWithoutExtension);

    // Map categories for lookup
    let group_map: IndexMap<u32, Arc<str>> = xtream_categories.iter().map(|c| (c.category_id, c.category_name.clone())).collect();
    let unknown_group_name = "Unknown".intern();

    spawn_blocking(move || {
        let reader = tokio_util::io::SyncIoBridge::new(streams);
        let mut deserializer = serde_json::Deserializer::from_reader(reader);

        let mut source_ordinal = 0u32;

        match xtream_cluster {
            XtreamCluster::Live => {
                let mut on_stream = |stream: LiveStreamProperties| {
                    source_ordinal += 1;
                    let stream_prop = StreamProperties::Live(Box::new(stream));
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
    use super::parse_xtream_series_info;
    use crate::processing::parser::xtream::map_to_xtream_streams;
    use crate::model::ConfigInput;
    use crate::utils::async_file_reader;
    use shared::model::{
        UUIDType,
        SeriesStreamDetailEpisodeProperties, SeriesStreamDetailProperties, SeriesStreamProperties,
        XtreamCluster, XtreamSeriesInfo,
    };
    use shared::utils::Internable;
    use std::fs;

    #[test]
    fn test_read_json_file_into_struct() {
        if fs::exists("/tmp/series-info.json").unwrap_or(false) {
            let file_content = fs::read_to_string("/tmp/series-info.json").expect("Unable to read file");
            match serde_json::from_str::<XtreamSeriesInfo>(&file_content) {
                Ok(series_info) => {
                    println!("{series_info:#?}");
                }
                Err(err) => {
                    panic!("Failed to parse json file: {err}");
                }
            }
        }
    }

    #[tokio::test]
    async fn test_read_json_stream_into_struct() -> std::io::Result<()> {
        if fs::exists("/tmp/vod_streams.json").unwrap_or(false) {
            let reader = Box::pin(async_file_reader(tokio::fs::File::open("/tmp/vod_streams.json").await?));
            match map_to_xtream_streams(XtreamCluster::Video, reader, &"test".intern()).await {
                Ok(streams) => {
                    println!("{:?}", streams.get(1));
                    println!("{:?}", streams.get(100));
                    println!("{:?}", streams.get(200));
                }
                Err(err) => {
                    panic!("Failed to parse json file: {err}");
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

    #[test]
    fn test_parse_xtream_series_info_sets_episode_source_ordinals_from_parent() {
        let input = ConfigInput {
            name: "input".intern(),
            url: "http://provider.example".to_string(),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            ..ConfigInput::default()
        };

        let episode_1: SeriesStreamDetailEpisodeProperties = serde_json::from_str(
            r#"{"id":101,"episode_num":1,"season":1,"title":"S01E01","container_extension":"mp4"}"#,
        )
        .unwrap();
        let episode_2: SeriesStreamDetailEpisodeProperties = serde_json::from_str(
            r#"{"id":102,"episode_num":2,"season":1,"title":"S01E02","container_extension":"mp4"}"#,
        )
        .unwrap();

        let series_props = SeriesStreamProperties {
            details: Some(SeriesStreamDetailProperties {
                year: None,
                seasons: None,
                episodes: Some(vec![episode_1, episode_2]),
            }),
            ..SeriesStreamProperties::default()
        };
        let parent_uuid = UUIDType::from_valid_uuid("parent_uuid");

        let episodes = parse_xtream_series_info(
            &parent_uuid,
            &series_props,
            "Series Group",
            &"Series Name".intern(),
            &input,
            None,
            42,
        )
        .expect("episodes should be parsed");

        assert_eq!(episodes[0].header.source_ordinal, 42);
        assert_eq!(episodes[1].header.source_ordinal, 43);
    }
}
