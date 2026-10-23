use super::{
    cluster_name, collect_page, drain_records, field_value, parse_cluster, project, read_stored,
    sanitized_additional_properties, skipped_records_note, visible_in_target_listing, EpgLookup, EpgStatus, ItemField,
    ItemFilter, ItemRow, ItemSearchField, ItemsQuery, ItemsResponse, SourceName, StoredPlaylistRecord,
    ValidatedItemsQuery, DEFAULT_ITEM_FIELDS, DEFAULT_SEARCH_FIELDS, NO_STORED_PLAYLIST_NOTE,
};
use crate::{
    api::endpoints::v1_api_playlist::inspect_common::{CaseInsensitiveNeedle, PageWindow},
    repository::LockedReceiverStream,
    utils::{canonicalize_untrusted_epg_id, EpgIdOutputCase},
};
use shared::{
    error::TuliproxError,
    model::{
        CatchupAttribute, CatchupProperties, EpisodeStreamProperties, LiveStreamProperties, M3uPlaylistItem,
        PlaylistItemType, SeriesStreamDetailEpisodeProperties, SeriesStreamDetailProperties, SeriesStreamProperties,
        StreamProperties, VideoStreamDetailProperties, VideoStreamProperties, VirtualId, XtreamCluster,
        XtreamPlaylistItem,
    },
    utils::Internable,
};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

/// A provider URL as stored on records: the credentials sit in the path.
pub(in crate::api::endpoints::v1_api_playlist) const SECRET_URL: &str =
    "http://provider.example:8080/live/secret-user/secret-pass/42.ts";

pub(in crate::api::endpoints::v1_api_playlist) fn m3u_item(
    virtual_id: u32,
    provider_id: &str,
    name: &str,
    group: &str,
    epg: Option<&str>,
) -> M3uPlaylistItem {
    M3uPlaylistItem {
        virtual_id: VirtualId::new(virtual_id),
        provider_id: provider_id.intern(),
        name: name.intern(),
        chno: 3,
        logo: "http://logo.example/a.png".intern(),
        logo_small: "".intern(),
        group: group.intern(),
        title: name.intern(),
        parent_code: "".intern(),
        audio_track: "".intern(),
        time_shift: "".intern(),
        rec: "".intern(),
        url: SECRET_URL.intern(),
        epg_channel_id: epg.map(Internable::intern),
        input_name: "input-a".intern(),
        item_type: PlaylistItemType::Live,
        t_stream_url: SECRET_URL.intern(),
        t_resource_url: None,
        t_catchup_source: None,
        t_catchup_mode: None,
        source_ordinal: 7,
        additional_properties: None,
        input_stream_id: provider_id.intern(),
        upstream_user_agent: None,
    }
}

pub(in crate::api::endpoints::v1_api_playlist) fn xtream_item(
    virtual_id: u32,
    provider_id: u32,
    name: &str,
    group: &str,
    epg: Option<&str>,
) -> XtreamPlaylistItem {
    XtreamPlaylistItem {
        virtual_id: VirtualId::new(virtual_id),
        provider_id,
        name: name.intern(),
        logo: "http://logo.example/a.png".intern(),
        logo_small: "".intern(),
        group: group.intern(),
        title: name.intern(),
        parent_code: "".intern(),
        rec: "".intern(),
        url: SECRET_URL.intern(),
        epg_channel_id: epg.map(Internable::intern),
        xtream_cluster: XtreamCluster::Live,
        additional_properties: None,
        item_type: PlaylistItemType::Live,
        category_id: 9,
        input_name: "input-a".intern(),
        channel_no: 3,
        source_ordinal: 7,
        input_stream_id: provider_id.to_string().intern(),
        upstream_user_agent: None,
    }
}

fn no_filter<'a>() -> ItemFilter<'a> {
    ItemFilter { needle: None, search_fields: DEFAULT_SEARCH_FIELDS, group: None, epg: None, epg_status: &[] }
}

fn lookup(keys: &[&str], case: EpgIdOutputCase) -> EpgLookup {
    EpgLookup::new(Arc::new(keys.iter().map(|key| Arc::<str>::from(*key)).collect()), case)
}

/// A finished record stream: the channel is pre-filled and its sender dropped, so the stream ends after `entries`.
/// `skipped` presets the skip counter the real reader increments for unreadable records.
pub(in crate::api::endpoints::v1_api_playlist) fn stream_with_skips<R>(
    entries: Vec<Result<R, TuliproxError>>,
    skipped: usize,
) -> LockedReceiverStream<Result<R, TuliproxError>> {
    let (tx, rx) = tokio::sync::mpsc::channel(entries.len().max(1));
    for entry in entries {
        assert!(tx.try_send(entry).is_ok(), "the channel holds every entry");
    }
    drop(tx);
    LockedReceiverStream::new_empty(rx).with_skip_counter(Arc::new(AtomicUsize::new(skipped)))
}

fn stream_of<R>(records: Vec<R>) -> LockedReceiverStream<Result<R, TuliproxError>> {
    stream_with_skips(records.into_iter().map(Ok).collect(), 0)
}

/// The row as the JSON object a client receives.
fn row_json(row: &ItemRow) -> serde_json::Value { serde_json::to_value(row).expect("a row serializes") }

fn names(page: &super::ItemPage) -> Vec<String> {
    page.items.iter().map(|item| row_json(item)["name"].as_str().unwrap_or_default().to_string()).collect()
}

fn playlist() -> Vec<M3uPlaylistItem> {
    vec![
        m3u_item(1, "p1", "BBC One", "UK", Some("bbc.one")),
        m3u_item(2, "p2", "BBC Two", "UK", Some("bbc.two")),
        m3u_item(3, "p3", "CNN News", "US", None),
        m3u_item(4, "p4", "Sky Sports", "UK", Some("sky.sports")),
        m3u_item(5, "p5", "Arte", "DE", Some("arte.de")),
    ]
}

#[test]
fn filter_search_is_case_insensitive_over_the_default_fields() {
    let needle = CaseInsensitiveNeedle::new(Some("bBc"));
    let filter = ItemFilter { needle: needle.as_ref(), ..no_filter() };
    let matched: Vec<_> = playlist().into_iter().filter(|item| filter.matches(item)).map(|i| i.name).collect();
    assert_eq!(matched.len(), 2);

    let by_group = CaseInsensitiveNeedle::new(Some("us"));
    let filter = ItemFilter { needle: by_group.as_ref(), ..no_filter() };
    let matched: Vec<_> = playlist().into_iter().filter(|item| filter.matches(item)).map(|i| i.name).collect();
    assert_eq!(matched, vec!["CNN News".intern()], "group is a default search field");
}

#[test]
fn filter_search_field_restricts_the_search() {
    let needle = CaseInsensitiveNeedle::new(Some("sky"));
    let by_epg_id = [ItemSearchField::EpgChannelId];
    let filter = ItemFilter { needle: needle.as_ref(), search_fields: &by_epg_id, ..no_filter() };
    assert_eq!(playlist().iter().filter(|item| filter.matches(*item)).count(), 1);

    let by_name_only = [ItemSearchField::Name];
    let needle = CaseInsensitiveNeedle::new(Some("uk"));
    let filter = ItemFilter { needle: needle.as_ref(), search_fields: &by_name_only, ..no_filter() };
    assert_eq!(playlist().iter().filter(|item| filter.matches(*item)).count(), 0, "group text is not searched");
}

#[test]
fn filter_group_is_exact_and_combines_with_search_by_and() {
    let filter = ItemFilter { group: Some("UK"), ..no_filter() };
    assert_eq!(playlist().iter().filter(|item| filter.matches(*item)).count(), 3);
    let filter = ItemFilter { group: Some("uk"), ..no_filter() };
    assert_eq!(playlist().iter().filter(|item| filter.matches(*item)).count(), 0, "exact match is case-sensitive");

    let needle = CaseInsensitiveNeedle::new(Some("sports"));
    let filter = ItemFilter { group: Some("UK"), needle: needle.as_ref(), ..no_filter() };
    let matched: Vec<_> = playlist().into_iter().filter(|item| filter.matches(item)).map(|i| i.name).collect();
    assert_eq!(matched, vec!["Sky Sports".intern()]);
    let filter = ItemFilter { group: Some("US"), needle: needle.as_ref(), ..no_filter() };
    assert_eq!(playlist().iter().filter(|item| filter.matches(*item)).count(), 0);
}

#[test]
fn filter_without_search_text_matches_everything() {
    assert!(CaseInsensitiveNeedle::new(Some("")).is_none());
    let filter = no_filter();
    assert_eq!(playlist().iter().filter(|item| filter.matches(*item)).count(), 5);
}

#[test]
fn epg_status_classification_and_filter() {
    let lookup = lookup(&["bbc.one", "arte.de"], EpgIdOutputCase::Preserve);
    assert_eq!(lookup.classify(None), EpgStatus::MissingId);
    assert_eq!(lookup.classify(Some("  ")), EpgStatus::MissingId);
    assert_eq!(lookup.classify(Some("bbc.one")), EpgStatus::Matched);
    assert_eq!(lookup.classify(Some("bbc.two")), EpgStatus::Unmatched);
    assert_eq!(lookup.classify(Some("BBC.One")), EpgStatus::Unmatched, "case is preserved for this target");

    let wanted = [EpgStatus::Unmatched, EpgStatus::MissingId];
    let filter = ItemFilter { epg: Some(&lookup), epg_status: &wanted, ..no_filter() };
    let matched: Vec<_> = playlist().into_iter().filter(|item| filter.matches(item)).map(|i| i.name).collect();
    assert_eq!(matched, vec!["BBC Two".intern(), "CNN News".intern(), "Sky Sports".intern()]);
}

#[test]
fn epg_status_canonicalization_matches_the_xmltv_output_casing() {
    let keys = ["bbc.one", "sky.sports"];
    let folded = lookup(&keys, EpgIdOutputCase::LowercaseAscii);
    for id in ["BBC.One", "bbc.one", "Sky.SPORTS", "other"] {
        let canonical = canonicalize_untrusted_epg_id(id, EpgIdOutputCase::LowercaseAscii);
        let expected = if keys.contains(&canonical.as_ref()) { EpgStatus::Matched } else { EpgStatus::Unmatched };
        assert_eq!(folded.classify(Some(id)), expected, "{id}");
    }
    let preserved = lookup(&["BBC.One"], EpgIdOutputCase::Preserve);
    assert_eq!(preserved.classify(Some("BBC.One")), EpgStatus::Matched);
    assert_eq!(preserved.classify(Some("bbc.one")), EpgStatus::Unmatched);
}

#[tokio::test]
async fn collect_page_counts_all_matches_and_keeps_only_the_window() {
    let window = PageWindow::resolve(Some(1), Some(2), 50, 500);
    let page = collect_page(stream_of(playlist()), |_| true, &no_filter(), &[ItemField::Name], window).await.unwrap();
    assert_eq!(page.total_matched, 5);
    assert_eq!(names(&page), vec!["BBC Two", "CNN News"]);
    assert!(window.has_more(page.total_matched, page.items.len()));

    let last = PageWindow::resolve(Some(3), Some(10), 50, 500);
    let page = collect_page(stream_of(playlist()), |_| true, &no_filter(), &[ItemField::Name], last).await.unwrap();
    assert_eq!(names(&page), vec!["Sky Sports", "Arte"]);
    assert!(!last.has_more(page.total_matched, page.items.len()));

    let beyond = PageWindow::resolve(Some(50), Some(10), 50, 500);
    let page = collect_page(stream_of(playlist()), |_| true, &no_filter(), &[ItemField::Name], beyond).await.unwrap();
    assert_eq!((page.total_matched, page.items.len()), (5, 0));
    assert!(!beyond.has_more(page.total_matched, page.items.len()));
}

#[tokio::test]
async fn collect_page_total_counts_only_filtered_records() {
    let needle = CaseInsensitiveNeedle::new(Some("bbc"));
    let filter = ItemFilter { needle: needle.as_ref(), ..no_filter() };
    let window = PageWindow::resolve(None, Some(1), 50, 500);
    let page = collect_page(stream_of(playlist()), |_| true, &filter, &[ItemField::Name], window).await.unwrap();
    assert_eq!(page.total_matched, 2);
    assert_eq!(names(&page), vec!["BBC One"]);
    assert!(window.has_more(page.total_matched, page.items.len()));
}

#[tokio::test]
async fn collect_page_aborts_on_a_stream_error_instead_of_returning_a_partial_page() {
    let records: Vec<Result<M3uPlaylistItem, TuliproxError>> = vec![
        Ok(m3u_item(1, "p1", "A", "g", None)),
        Err(TuliproxError::RepositoryM3u("boom".to_string())),
        Ok(m3u_item(2, "p2", "B", "g", None)),
    ];
    let window = PageWindow::resolve(None, None, 50, 500);
    let result = collect_page(stream_with_skips(records, 0), |_| true, &no_filter(), &[ItemField::Name], window).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn collect_page_reports_the_records_the_reader_skipped() {
    let window = PageWindow::resolve(None, Some(2), 50, 500);
    let page = collect_page(
        stream_with_skips::<M3uPlaylistItem>(vec![], 0),
        |_| true,
        &no_filter(),
        &[ItemField::Name],
        window,
    )
    .await
    .unwrap();
    assert_eq!(page.skipped_records, 0, "a healthy stream skips nothing");

    let entries = playlist().into_iter().map(Ok).collect();
    let page =
        collect_page(stream_with_skips(entries, 2), |_| true, &no_filter(), &[ItemField::Name], window).await.unwrap();
    assert_eq!((page.total_matched, page.skipped_records), (5, 2), "skipped records are not part of the total");

    let page = collect_page(stream_with_skips::<M3uPlaylistItem>(vec![], 3), |_| true, &no_filter(), &[], window)
        .await
        .unwrap();
    assert_eq!((page.total_matched, page.items.len(), page.skipped_records), (0, 0, 3));
}

#[tokio::test]
async fn collect_page_reads_the_skip_count_after_the_stream_is_drained() {
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let counter = Arc::new(AtomicUsize::new(0));
    let stream = LockedReceiverStream::new_empty(rx).with_skip_counter(Arc::clone(&counter));
    let producer = tokio::spawn(async move {
        assert!(tx.send(Ok(m3u_item(1, "p1", "A", "g", None))).await.is_ok());
        counter.fetch_add(1, Ordering::Release);
        assert!(tx.send(Ok(m3u_item(2, "p2", "B", "g", None))).await.is_ok());
        counter.fetch_add(1, Ordering::Release);
    });
    let window = PageWindow::resolve(None, None, 50, 500);
    let page = collect_page(stream, |_| true, &no_filter(), &[ItemField::Name], window).await.unwrap();
    producer.await.unwrap();
    assert_eq!((page.total_matched, page.skipped_records), (2, 2));
}

fn response_json(page: Option<super::ItemPage>) -> serde_json::Value {
    let query = ValidatedItemsQuery::parse(ItemsQuery {
        target: Some("t".to_string()),
        cluster: Some("live".to_string()),
        ..ItemsQuery::default()
    })
    .expect("a target alone is a valid query");
    serde_json::to_value(ItemsResponse::new("target", "t", &query, page)).expect("the response serializes")
}

#[test]
fn items_response_always_carries_skipped_records_and_notes_only_incomplete_listings() {
    let page = |skipped_records| super::ItemPage { total_matched: 1, skipped_records, items: Vec::new() };

    let healthy = response_json(Some(page(0)));
    assert_eq!(healthy["skipped_records"], 0);
    assert!(healthy.get("note").is_none(), "a healthy page has no note");

    let damaged = response_json(Some(page(4)));
    assert_eq!(damaged["skipped_records"], 4);
    assert_eq!(damaged["total_matched"], 1);
    assert_eq!(damaged["note"], skipped_records_note(4).as_str());
    assert_eq!(
        damaged["note"],
        "4 stored record(s) could not be read and are missing from this listing; total_matched does not include \
         them (see the server log)."
    );

    let no_database = response_json(None);
    assert_eq!(no_database["skipped_records"], 0);
    assert_eq!(no_database["total_matched"], 0);
    assert_eq!(no_database["note"], NO_STORED_PLAYLIST_NOTE);
}

#[test]
fn series_cluster_of_a_target_hides_episode_rows_like_the_web_ui_listing() {
    let mut episode = xtream_item(1, 1, "S01E01", "Shows", None);
    episode.item_type = PlaylistItemType::Series;
    let mut info = xtream_item(2, 2, "Show", "Shows", None);
    info.item_type = PlaylistItemType::SeriesInfo;
    let mut local_episode = m3u_item(3, "3", "Local", "Shows", None);
    local_episode.item_type = PlaylistItemType::LocalSeries;

    for cluster in [XtreamCluster::Live, XtreamCluster::Video] {
        assert!(visible_in_target_listing(cluster, &episode), "only the series cluster hides rows");
    }
    // Same predicate as `get_playlist_for_target`: `!item_type.is_series()` for the series cluster.
    for record_type in [
        PlaylistItemType::Series,
        PlaylistItemType::SeriesInfo,
        PlaylistItemType::LocalSeries,
        PlaylistItemType::LocalSeriesInfo,
        PlaylistItemType::Live,
        PlaylistItemType::Video,
    ] {
        let mut item = xtream_item(9, 9, "x", "g", None);
        item.item_type = record_type;
        assert_eq!(visible_in_target_listing(XtreamCluster::Series, &item), !record_type.is_series());
    }
    assert!(!visible_in_target_listing(XtreamCluster::Series, &episode));
    assert!(visible_in_target_listing(XtreamCluster::Series, &info));
    assert!(!visible_in_target_listing(XtreamCluster::Series, &local_episode));
}

#[test]
fn default_projection_is_compact_and_ordered() {
    let item = xtream_item(11, 22, "Name", "Group", Some("epg.id"));
    let projected = row_json(&project(&item, DEFAULT_ITEM_FIELDS, None));
    let keys: Vec<&str> = projected.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "virtual_id",
            "provider_id",
            "name",
            "title",
            "group",
            "item_type",
            "cluster",
            "epg_channel_id",
            "category_id",
            "channel_no"
        ]
    );
    assert_eq!(projected["virtual_id"], 11);
    assert_eq!(projected["provider_id"], "22");
    assert_eq!(projected["item_type"], "live");
    assert_eq!(projected["cluster"], "live");
    assert_eq!(projected["category_id"], 9);
    assert_eq!(projected["epg_channel_id"], "epg.id");
}

#[test]
fn projection_honors_requested_fields_and_order() {
    let item = xtream_item(11, 22, "Name", "Group", None);
    let projected = row_json(&project(&item, &[ItemField::SourceOrdinal, ItemField::InputName, ItemField::Logo], None));
    let keys: Vec<&str> = projected.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["source_ordinal", "input_name", "logo"]);
    assert_eq!(projected["source_ordinal"], 7);
    assert_eq!(projected["input_name"], "input-a");
    assert_eq!(projected["logo"], "http://logo.example/a.png");
    assert!(projected.get("epg_channel_id").is_none(), "unrequested field is absent");
}

#[test]
fn opt_in_fields_are_not_in_the_default_set() {
    for opt_in in [
        ItemField::Logo,
        ItemField::LogoSmall,
        ItemField::ParentCode,
        ItemField::Rec,
        ItemField::InputName,
        ItemField::InputStreamId,
        ItemField::SourceOrdinal,
        ItemField::AdditionalProperties,
        ItemField::EpgStatus,
    ] {
        assert!(!DEFAULT_ITEM_FIELDS.contains(&opt_in), "{}", opt_in.name());
    }
}

#[test]
fn epg_status_field_is_null_without_a_lookup_and_classified_with_one() {
    let item = xtream_item(1, 1, "n", "g", Some("known"));
    assert_eq!(field_value(&item, ItemField::EpgStatus, None), serde_json::Value::Null);
    let lookup = lookup(&["known"], EpgIdOutputCase::Preserve);
    assert_eq!(field_value(&item, ItemField::EpgStatus, Some(&lookup)), "matched");
}

#[test]
fn url_is_never_returned_for_any_field_or_item_type() {
    let mut with_props = xtream_item(1, 2, "n", "g", Some("e"));
    with_props.additional_properties = Some(StreamProperties::Live(Box::new(LiveStreamProperties {
        direct_source: SECRET_URL.intern(),
        stream_icon: "http://img.example/i.png".intern(),
        ..LiveStreamProperties::default()
    })));
    let mut m3u = m3u_item(1, "p", "n", "g", Some("e"));
    m3u.additional_properties = with_props.additional_properties.clone();

    assert!(!ItemField::NAMES.contains(&"url"));
    for projected in [project(&with_props, ItemField::ALL, None), project(&m3u, ItemField::ALL, None)] {
        assert!(row_json(&projected).get("url").is_none());
        let json = serde_json::to_string(&projected).unwrap();
        assert!(!json.contains("secret-user"), "{json}");
        assert!(!json.contains("secret-pass"), "{json}");
        assert!(!json.contains("provider.example"), "{json}");
    }
}

#[test]
fn logo_with_credentials_is_not_returned() {
    let mut item = m3u_item(1, "p", "n", "g", None);
    item.logo = "http://user:pw@cdn.example/logo.png".intern();
    item.logo_small = "http://cdn.example/l.png?username=a&password=b".intern();
    assert_eq!(field_value(&item, ItemField::Logo, None), serde_json::Value::Null);
    assert_eq!(field_value(&item, ItemField::LogoSmall, None), serde_json::Value::Null);
}

#[test]
fn sanitizer_drops_url_and_credential_members_of_live_properties() {
    let properties = StreamProperties::Live(Box::new(LiveStreamProperties {
        name: "Chan".intern(),
        category_id: 5,
        stream_id: 77,
        stream_icon: "http://img.example/i.png".intern(),
        direct_source: SECRET_URL.intern(),
        video: Some("probe-video".intern()),
        audio: Some("probe-audio".intern()),
        bitrate: 1234,
        catchup: Some(CatchupProperties {
            mode: Some("append".intern()),
            days: Some("3".intern()),
            source: Some("http://provider.example/catchup/{utc}?token=catchup-token".intern()),
            extra_attributes: vec![CatchupAttribute { name: "k".intern(), value: "extra-secret".intern() }],
            ..CatchupProperties::default()
        }),
        ..LiveStreamProperties::default()
    }));
    let value = sanitized_additional_properties(&properties);
    let live = &value["Live"];
    assert_eq!(live["name"], "Chan");
    assert_eq!(live["category_id"], 5);
    assert_eq!(live["stream_id"], 77);
    assert_eq!(live["bitrate"], 1234);
    assert_eq!(live["stream_icon"], "http://img.example/i.png");
    assert_eq!(live["catchup"]["mode"], "append");
    assert_eq!(live["catchup"]["days"], "3");
    for dropped in ["direct_source", "video", "audio"] {
        assert!(live.get(dropped).is_none(), "{dropped}");
    }
    for dropped in ["source", "extra_attributes"] {
        assert!(live["catchup"].get(dropped).is_none(), "catchup.{dropped}");
    }
    let json = value.to_string();
    for secret in ["secret-user", "secret-pass", "catchup-token", "extra-secret", "probe-video"] {
        assert!(!json.contains(secret), "{secret} leaked in {json}");
    }
}

#[test]
fn sanitizer_drops_url_members_of_video_series_and_episode_properties() {
    let video = StreamProperties::Video(Box::new(VideoStreamProperties {
        name: "Movie".intern(),
        direct_source: SECRET_URL.intern(),
        container_extension: "mkv".intern(),
        trailer: Some("yt-id".intern()),
        details: Some(VideoStreamDetailProperties {
            kinopoisk_url: Some("https://kinopoisk.example/film/1".intern()),
            genre: Some("Drama".intern()),
            video: Some("probe".intern()),
            ..VideoStreamDetailProperties::default()
        }),
        ..VideoStreamProperties::default()
    }));
    let value = sanitized_additional_properties(&video);
    assert_eq!(value["Video"]["name"], "Movie");
    assert_eq!(value["Video"]["container_extension"], "mkv");
    assert_eq!(value["Video"]["trailer"], "yt-id");
    assert_eq!(value["Video"]["details"]["genre"], "Drama");
    assert!(value["Video"].get("direct_source").is_none());
    assert!(value["Video"]["details"].get("kinopoisk_url").is_none());
    assert!(value["Video"]["details"].get("video").is_none());

    let series = StreamProperties::Series(Box::new(SeriesStreamProperties {
        name: "Show".intern(),
        details: Some(SeriesStreamDetailProperties::new(
            None,
            Vec::new(),
            Some(vec![SeriesStreamDetailEpisodeProperties {
                title: "Pilot".intern(),
                direct_source: SECRET_URL.intern(),
                video: Some("probe".intern()),
                ..SeriesStreamDetailEpisodeProperties::default()
            }]),
        )),
        ..SeriesStreamProperties::default()
    }));
    let value = sanitized_additional_properties(&series);
    let episode = &value["Series"]["details"]["episodes"][0];
    assert_eq!(episode["title"], "Pilot");
    assert!(episode.get("direct_source").is_none());
    assert!(episode.get("video").is_none());
    assert!(!value.to_string().contains("secret-pass"));

    let episode_props = StreamProperties::Episode(Box::new(EpisodeStreamProperties {
        episode_id: 4,
        episode: 2,
        season: 1,
        added: None,
        release_date: None,
        series_release_date: None,
        plot: None,
        tmdb: None,
        movie_image: "http://img.example/e.png".intern(),
        container_extension: "mp4".intern(),
        video: Some("probe".intern()),
        audio: None,
    }));
    let value = sanitized_additional_properties(&episode_props);
    assert_eq!(value["Episode"]["episode_id"], 4);
    assert!(value["Episode"].get("video").is_none());
}

#[test]
fn sanitizer_drops_credential_bearing_string_values_under_unknown_keys() {
    let mut value = serde_json::json!({
        "ok": "plain text",
        "link": "http://user:pw@host/x",
        "query": "http://host/x?password=1",
        "extra_url": "http://host/ok",
        "list": ["fine", "https://a:b@host/"],
        "nested": { "direct_source": "x", "keep": 1 },
    });
    super::sanitize_json(&mut value);
    assert_eq!(value, serde_json::json!({ "ok": "plain text", "list": ["fine"], "nested": { "keep": 1 } }));
}

#[test]
fn xtream_and_m3u_records_normalize_to_the_same_projection() {
    let xtream = xtream_item(5, 42, "Arte", "DE", Some("arte.de"));
    let mut m3u = m3u_item(5, "42", "Arte", "DE", Some("arte.de"));
    m3u.input_stream_id = "42".intern();
    let shared_fields = [
        ItemField::VirtualId,
        ItemField::ProviderId,
        ItemField::Name,
        ItemField::Title,
        ItemField::Group,
        ItemField::ItemType,
        ItemField::Cluster,
        ItemField::EpgChannelId,
        ItemField::ChannelNo,
        ItemField::Logo,
        ItemField::LogoSmall,
        ItemField::ParentCode,
        ItemField::Rec,
        ItemField::InputName,
        ItemField::InputStreamId,
        ItemField::SourceOrdinal,
        ItemField::AdditionalProperties,
    ];
    assert_eq!(project(&xtream, &shared_fields, None), project(&m3u, &shared_fields, None));
    // M3U records have no category id.
    assert_eq!(field_value(&xtream, ItemField::CategoryId, None), 9);
    assert_eq!(field_value(&m3u, ItemField::CategoryId, None), serde_json::Value::Null);
}

#[test]
fn m3u_cluster_is_derived_from_the_item_type() {
    let mut item = m3u_item(1, "1", "n", "g", None);
    for (item_type, cluster) in [
        (PlaylistItemType::LiveHls, "live"),
        (PlaylistItemType::Video, "video"),
        (PlaylistItemType::Catchup, "video"),
        (PlaylistItemType::SeriesInfo, "series"),
    ] {
        item.item_type = item_type;
        assert_eq!(field_value(&item, ItemField::Cluster, None), cluster);
        assert_eq!(cluster_name(item.cluster()), cluster);
    }
}

#[test]
fn field_and_search_field_lists_are_consistent() {
    assert_eq!(ItemField::ALL.len(), ItemField::NAMES.len());
    assert_eq!(ItemSearchField::ALL.len(), ItemSearchField::NAMES.len());
    let unique: HashSet<_> = ItemField::NAMES.iter().collect();
    assert_eq!(unique.len(), ItemField::NAMES.len());
    for field in DEFAULT_ITEM_FIELDS {
        assert!(ItemField::ALL.contains(field));
    }
    for field in DEFAULT_SEARCH_FIELDS {
        assert!(ItemSearchField::ALL.contains(field));
    }
    assert_eq!(EpgStatus::NAMES, &["missing_id", "unmatched", "matched"]);
}

fn target_query() -> ItemsQuery {
    ItemsQuery { target: Some("t".to_string()), cluster: Some("live".to_string()), ..ItemsQuery::default() }
}

#[test]
fn query_requires_exactly_one_of_target_and_input() {
    let neither = ValidatedItemsQuery::parse(ItemsQuery { cluster: Some("live".into()), ..ItemsQuery::default() });
    assert!(neither.unwrap_err().message().contains("Exactly one"));
    let both = ValidatedItemsQuery::parse(ItemsQuery { input: Some("i".to_string()), ..target_query() });
    assert!(both.unwrap_err().message().contains("not both"));
    let blank = ValidatedItemsQuery::parse(ItemsQuery { target: Some("  ".to_string()), ..target_query() });
    assert!(blank.is_err(), "a blank name counts as absent");
    assert!(matches!(ValidatedItemsQuery::parse(target_query()).unwrap().source, SourceName::Target(_)));
}

#[test]
fn query_rejects_unknown_enum_values_and_lists_the_allowed_ones() {
    let err =
        ValidatedItemsQuery::parse(ItemsQuery { cluster: Some("vod".to_string()), ..target_query() }).unwrap_err();
    assert!(err.message().contains("live, video, series"), "{}", err.message());
    let err = ValidatedItemsQuery::parse(ItemsQuery { cluster: None, ..target_query() }).unwrap_err();
    assert!(err.message().contains("Missing cluster"), "{}", err.message());

    let err =
        ValidatedItemsQuery::parse(ItemsQuery { fields: Some(vec!["url".to_string()]), ..target_query() }).unwrap_err();
    assert!(err.message().contains("Unknown fields 'url'"), "{}", err.message());
    assert!(err.message().contains(&ItemField::NAMES.join(", ")), "{}", err.message());

    let err =
        ValidatedItemsQuery::parse(ItemsQuery { search_field: Some(vec!["bogus".to_string()]), ..target_query() })
            .unwrap_err();
    assert!(err.message().contains(&ItemSearchField::NAMES.join(", ")), "{}", err.message());

    let err = ValidatedItemsQuery::parse(ItemsQuery { epg_status: Some(vec!["x".to_string()]), ..target_query() })
        .unwrap_err();
    assert!(err.message().contains("missing_id, unmatched, matched"), "{}", err.message());
}

#[test]
fn query_applies_defaults_and_paging_limits() {
    let query = ValidatedItemsQuery::parse(target_query()).unwrap();
    assert_eq!(query.fields, DEFAULT_ITEM_FIELDS);
    assert_eq!(query.search_fields, DEFAULT_SEARCH_FIELDS);
    assert_eq!((query.window.offset, query.window.limit, query.window.limit_clamped), (0, 50, false));
    assert!(!query.wants_epg());

    let query = ValidatedItemsQuery::parse(ItemsQuery {
        limit: Some(100_000),
        offset: Some(10),
        fields: Some(vec!["name".to_string(), "epg_status".to_string()]),
        ..target_query()
    })
    .unwrap();
    assert_eq!((query.window.offset, query.window.limit, query.window.limit_clamped), (10, 500, true));
    assert!(query.wants_epg(), "epg_status field needs the EPG lookup");
}

#[test]
fn epg_status_on_an_input_is_rejected() {
    let input = |extra: ItemsQuery| ItemsQuery {
        target: None,
        input: Some("i".to_string()),
        cluster: Some("live".to_string()),
        ..extra
    };
    let by_filter = ValidatedItemsQuery::parse(input(ItemsQuery {
        epg_status: Some(vec!["matched".to_string()]),
        ..ItemsQuery::default()
    }));
    assert!(by_filter.unwrap_err().message().contains("only available for a target"));
    let by_field = ValidatedItemsQuery::parse(input(ItemsQuery {
        fields: Some(vec!["epg_status".to_string()]),
        ..ItemsQuery::default()
    }));
    assert!(by_field.is_err());
    assert!(ValidatedItemsQuery::parse(input(ItemsQuery::default())).is_ok());
}

#[test]
fn cluster_param_parsing() {
    assert_eq!(parse_cluster(Some("live")).unwrap(), XtreamCluster::Live);
    assert_eq!(parse_cluster(Some(" video ")).unwrap(), XtreamCluster::Video);
    assert_eq!(parse_cluster(Some("series")).unwrap(), XtreamCluster::Series);
    assert!(parse_cluster(Some("movie")).is_err());
    assert!(parse_cluster(Some("")).is_err());
}

#[tokio::test]
async fn drain_records_visits_every_record_and_returns_the_skip_count_after_the_drain() {
    let mut seen = Vec::new();
    let skipped = drain_records(stream_with_skips(playlist().into_iter().map(Ok).collect(), 3), |record| {
        seen.push(record.virtual_id());
    })
    .await
    .unwrap();
    assert_eq!((seen.len(), skipped), (5, 3));

    let skipped = drain_records(stream_with_skips::<M3uPlaylistItem>(vec![], 0), |_| unreachable!()).await.unwrap();
    assert_eq!(skipped, 0, "a healthy empty stream skips nothing");
}

#[tokio::test]
async fn drain_records_propagates_a_stream_error_and_stops_visiting() {
    let entries = vec![
        Ok(m3u_item(1, "p1", "A", "g", None)),
        Err(TuliproxError::RepositoryM3u("boom".to_string())),
        Ok(m3u_item(2, "p2", "B", "g", None)),
    ];
    let mut visited = 0_usize;
    let result = drain_records(stream_with_skips(entries, 1), |_| visited += 1).await;
    assert!(result.is_err());
    assert_eq!(visited, 1, "records behind the error are not visited");
}

#[tokio::test]
async fn read_stored_is_none_without_a_stream_and_runs_the_reader_otherwise() {
    let none = read_stored::<M3uPlaylistItem, usize, _>(None, |_| async { unreachable!() }).await.unwrap();
    assert_eq!(none, None);

    let counted = read_stored(Some(stream_with_skips(playlist().into_iter().map(Ok).collect(), 2)), |records| {
        drain_records(records, |_| {})
    })
    .await
    .unwrap();
    assert_eq!(counted, Some(2));
}

/// The allocation-free provider id search of the filter.
fn provider_id_matches(record: &impl StoredPlaylistRecord, needle: &CaseInsensitiveNeedle) -> bool {
    record.with_provider_id(|provider_id| needle.is_in(provider_id))
}

fn needle(text: &str) -> CaseInsensitiveNeedle { CaseInsensitiveNeedle::new(Some(text)).expect("a non-blank needle") }

#[test]
fn xtream_provider_id_matches_substrings_of_the_decimal_id() {
    let record = xtream_item(1, 1234, "n", "g", None);
    assert!(provider_id_matches(&record, &needle("23")));
    assert!(provider_id_matches(&record, &needle("1234")));
    assert!(provider_id_matches(&record, &needle(" 4 ")), "the needle is trimmed");
    assert!(!provider_id_matches(&record, &needle("13")), "digits must be adjacent");
    assert!(!provider_id_matches(&record, &needle("x")), "non-digits never match");
    assert!(!provider_id_matches(&record, &needle("12345")), "a needle longer than the id never matches");
    assert!(!provider_id_matches(&record, &needle("+1234")), "the id has no sign");

    assert!(provider_id_matches(&xtream_item(1, 0, "n", "g", None), &needle("0")), "id 0 has one digit");
    assert!(!provider_id_matches(&xtream_item(1, 0, "n", "g", None), &needle("00")));

    let max = xtream_item(1, u32::MAX, "n", "g", None);
    assert!(provider_id_matches(&max, &needle("4294967295")), "all ten digits");
    assert!(provider_id_matches(&max, &needle("967295")));
    assert!(!provider_id_matches(&max, &needle("42949672950")), "eleven digits cannot match");
}

#[test]
fn m3u_provider_id_matches_case_insensitively() {
    let record = m3u_item(1, "Ab-12", "n", "g", None);
    assert!(provider_id_matches(&record, &needle("b-1")));
    assert!(provider_id_matches(&record, &needle("AB")));
    assert!(!provider_id_matches(&record, &needle("ba")));
    assert!(!provider_id_matches(&record, &needle("ab-123")));
    assert!(provider_id_matches(&m3u_item(1, "Über", "n", "g", None), &needle("üBER")), "non-ASCII needle");
    assert!(!provider_id_matches(&m3u_item(1, "", "n", "g", None), &needle("a")), "an empty id matches nothing");
}

#[test]
fn provider_id_matches_agrees_with_the_materialized_provider_id() {
    let mut ids = vec![0, 1, 9, 10, 99, 100, 999_999_999, 1_000_000_000, 4_294_967_290, u32::MAX - 1, u32::MAX];
    let mut state = 0x9E37_79B9_u32;
    for _ in 0..200 {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        // Vary the digit count: shifting right yields ids of every length.
        ids.push(state >> (state % 32));
    }
    let needles = ["0", "1", "5", "23", "99", "00", "10", "4294967295", "967", "x", "42949672950"];
    for id in ids {
        let record = xtream_item(1, id, "n", "g", None);
        for text in needles {
            let needle = needle(text);
            assert_eq!(
                provider_id_matches(&record, &needle),
                needle.is_in(&record.provider_id()),
                "id {id}, needle {text}"
            );
            assert_eq!(provider_id_matches(&record, &needle), id.to_string().contains(text.trim()), "id {id}, {text}");
        }
    }
}

#[test]
fn filter_searches_the_provider_id_of_both_record_types() {
    let by_provider_id = [ItemSearchField::ProviderId];
    let needle = needle("42");
    let filter = ItemFilter { needle: Some(&needle), search_fields: &by_provider_id, ..no_filter() };
    assert!(filter.matches(&xtream_item(1, 1042, "n", "g", None)));
    assert!(!filter.matches(&xtream_item(1, 1043, "n", "g", None)));
    assert!(filter.matches(&m3u_item(1, "x42y", "n", "g", None)));
    assert!(!filter.matches(&m3u_item(1, "p4", "n", "g", None)));
}

#[test]
fn item_rows_serialize_as_objects_in_field_order() {
    let item = xtream_item(11, 22, "Name", "Group", Some("epg.id"));
    // Deliberately not alphabetical: the output follows the requested order.
    let fields = [ItemField::Name, ItemField::VirtualId, ItemField::Group, ItemField::ProviderId, ItemField::Cluster];
    let row = project(&item, &fields, None);
    assert_eq!(
        serde_json::to_string(&row).unwrap(),
        r#"{"name":"Name","virtual_id":11,"group":"Group","provider_id":"22","cluster":"live"}"#
    );
    assert_eq!(serde_json::to_string(&project(&item, &[], None)).unwrap(), "{}");
}

#[test]
fn item_rows_serialize_like_the_former_string_keyed_map() {
    let item = xtream_item(11, 22, "Name", "Group", Some("epg.id"));
    let former = |fields: &[ItemField]| -> serde_json::Map<String, serde_json::Value> {
        fields.iter().map(|field| (field.name().to_string(), field_value(&item, *field, None))).collect()
    };
    for fields in [DEFAULT_ITEM_FIELDS, ItemField::ALL, &[ItemField::SourceOrdinal, ItemField::Name]] {
        assert_eq!(
            serde_json::to_string(&project(&item, fields, None)).unwrap(),
            serde_json::to_string(&former(fields)).unwrap()
        );
    }
}

#[tokio::test]
async fn items_response_carries_the_rows_in_field_order() {
    let fields = [ItemField::Name, ItemField::ProviderId, ItemField::Group];
    let window = PageWindow::resolve(None, Some(2), 50, 500);
    let page = collect_page(stream_of(playlist()), |_| true, &no_filter(), &fields, window).await.unwrap();
    let query = ValidatedItemsQuery::parse(target_query()).unwrap();
    let json = serde_json::to_string(&ItemsResponse::new("target", "t", &query, Some(page))).unwrap();
    assert!(
        json.contains(
            r#""items":[{"name":"BBC One","provider_id":"p1","group":"UK"},{"name":"BBC Two","provider_id":"p2","group":"UK"}]"#
        ),
        "{json}"
    );
}
