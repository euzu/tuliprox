//! Cache, sync and browse tests; servers are loopback and the policy explicit.

use super::*;
use shared::model::PluginOriginDto;
use std::sync::{
    atomic::{AtomicUsize, Ordering as AtomicOrdering},
    Arc,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const OPEN: FetchPolicy = FetchPolicy { allow_http: true };

fn client() -> Client { Client::builder().redirect(reqwest::redirect::Policy::none()).no_proxy().build().unwrap() }

fn body(plugins: &str) -> String { format!(r#"{{"format_version":1,"name":"T","plugins":[{plugins}]}}"#) }

fn entry(id: &str, category: &str, version: &str, min: &str, api: u32) -> String {
    format!(
        r#"{{"id":"{id}","name":"Plugin {id}","author":"Ann","category":"{category}","releases":[{{"version":"{version}","package_url":"http://127.0.0.1/p.tlpx","min_tuliprox_version":"{min}","plugin_api":{api}}}]}}"#
    )
}

/// A server answering `catalog` (200 with an `ETag`, or 304 on a matching
/// `If-None-Match`), counting requests; `fail` makes it answer 500.
async fn server(catalog: String, fail: Arc<AtomicUsize>) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/marketplace.json", listener.local_addr().unwrap());
    let counter = Arc::clone(&hits);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { return };
            let mut buffer = vec![0_u8; 4096];
            let read = stream.read(&mut buffer).await.unwrap_or(0);
            let head = String::from_utf8_lossy(&buffer[..read]).to_lowercase();
            counter.fetch_add(1, AtomicOrdering::SeqCst);
            let reply = if fail.load(AtomicOrdering::SeqCst) > 0 {
                b"HTTP/1.1 500 Oops\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
            } else if head.contains("if-none-match: \"e1\"") {
                b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n".to_vec()
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\nETag: \"e1\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{catalog}",
                    catalog.len()
                )
                .into_bytes()
            };
            let _ = stream.write_all(&reply).await;
            let _ = stream.shutdown().await;
        }
    });
    (url, hits)
}

fn source(id: &str, url: &str) -> PluginRepository {
    PluginRepository { id: id.to_string(), name: format!("Source {id}"), url: url.to_string(), enabled: true }
}

#[tokio::test]
async fn a_sync_stores_the_catalog_and_a_304_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let (url, hits) = server(body(&entry("demo", "epg", "1.0.0", "3.0.0", 1)), Arc::new(AtomicUsize::new(0))).await;
    let src = source("main", &url);
    assert!(!has_catalog(dir.path(), &src).await);

    let row = sync_source(dir.path(), &client(), OPEN, &src).await.unwrap();
    assert_eq!((row.plugin_count, row.last_error.is_none(), row.last_sync.is_some()), (1, true, true));
    assert!(has_catalog(dir.path(), &src).await);
    assert!(dir.path().join("marketplace/main.json").is_file());

    let again = sync_source(dir.path(), &client(), OPEN, &src).await.unwrap();
    assert_eq!(again.plugin_count, 1, "the 304 kept the cache");
    assert_eq!(hits.load(AtomicOrdering::SeqCst), 2);
}

#[tokio::test]
async fn a_failed_sync_keeps_the_old_catalog_and_records_the_error() {
    let dir = tempfile::tempdir().unwrap();
    let fail = Arc::new(AtomicUsize::new(0));
    let (url, _) = server(body(&entry("demo", "epg", "1.0.0", "3.0.0", 1)), Arc::clone(&fail)).await;
    let src = source("main", &url);
    sync_source(dir.path(), &client(), OPEN, &src).await.unwrap();

    fail.store(1, AtomicOrdering::SeqCst);
    let err = sync_source(dir.path(), &client(), OPEN, &src).await.unwrap_err();
    assert_eq!(err.status(), http::StatusCode::BAD_GATEWAY);
    let row = source_info(dir.path(), &src).await;
    assert_eq!(row.plugin_count, 1);
    assert!(row.last_error.as_deref().is_some_and(|error| error.contains("500")));
    assert!(row.last_sync.is_some(), "the earlier success time stays");
}

#[tokio::test]
async fn a_catalog_that_does_not_parse_fails_the_sync_and_keeps_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let dup = body(&format!("{},{}", entry("demo", "a", "1.0.0", "3.0.0", 1), entry("demo", "a", "1.1.0", "3.0.0", 1)));
    let (url, _) = server(dup, Arc::new(AtomicUsize::new(0))).await;
    let src = source("main", &url);
    let err = sync_source(dir.path(), &client(), OPEN, &src).await.unwrap_err();
    assert!(matches!(err, MarketplaceError::Catalog(_)));
    assert!(source_info(dir.path(), &src).await.last_error.is_some());
}

#[tokio::test]
async fn a_cache_for_another_url_is_discarded() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _) = server(body(&entry("demo", "epg", "1.0.0", "3.0.0", 1)), Arc::new(AtomicUsize::new(0))).await;
    sync_source(dir.path(), &client(), OPEN, &source("main", &url)).await.unwrap();
    let moved = source("main", "http://127.0.0.1:9/other.json");
    assert!(!has_catalog(dir.path(), &moved).await);
    assert_eq!(source_info(dir.path(), &moved).await.plugin_count, 0);
}

#[tokio::test]
async fn removing_a_source_deletes_only_its_cache() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _) = server(body(&entry("demo", "epg", "1.0.0", "3.0.0", 1)), Arc::new(AtomicUsize::new(0))).await;
    sync_source(dir.path(), &client(), OPEN, &source("main", &url)).await.unwrap();
    remove_cache(dir.path(), "main").await;
    remove_cache(dir.path(), "main").await;
    remove_cache(dir.path(), "../evil").await;
    assert!(!dir.path().join("marketplace/main.json").exists());
}

fn category(id: &str, count: usize) -> MarketplaceCategoryDto { MarketplaceCategoryDto { id: id.to_string(), count } }

fn ids(view: &MarketplaceDto) -> Vec<&str> { view.plugins.iter().map(|plugin| plugin.id.as_str()).collect() }

fn seeded(dir: &Path, id: &str, url: &str, plugins: &str) {
    let (catalog, _) = parse(body(plugins).as_bytes(), true).unwrap();
    let cache = CacheFile { url: url.to_string(), catalog: Some(catalog), ..CacheFile::default() };
    write_cache(&cache_path(dir, id).unwrap(), &cache).unwrap();
}

#[tokio::test]
async fn browse_filters_marks_installed_and_names_other_sources() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (source("alpha", "http://a.invalid/c.json"), source("beta", "http://b.invalid/c.json"));
    seeded(
        dir.path(),
        "alpha",
        &a.url,
        &format!("{},{}", entry("sports", "epg", "1.0.0", "3.0.0", 1), entry("future", "misc", "2.0.0", "9.0.0", 1)),
    );
    seeded(dir.path(), "beta", &b.url, &entry("sports", "epg", "1.5.0", "3.0.0", 1));
    let installed = HashMap::from([(
        "sports".to_string(),
        MarketplaceInstalledDto {
            version: Some("1.0.0".into()),
            origin: Some(PluginOriginDto { source: "alpha".into(), version: "1.0.0".into() }),
        },
    )]);
    let all = browse(dir.path(), &[a.clone(), b.clone()], &installed, &MarketplaceQueryDto::default(), "3.3.175").await;
    assert_eq!(all.plugins.len(), 3);
    assert_eq!(all.categories, [category("epg", 2), category("misc", 1)]);
    assert_eq!(all.total, 3);
    assert_eq!(all.sources.len(), 2);
    let sports_a = all.plugins.iter().find(|plugin| plugin.id == "sports" && plugin.source == "alpha").unwrap();
    assert_eq!(sports_a.also_in, ["beta"]);
    assert_eq!(sports_a.installed.as_ref().and_then(|i| i.version.as_deref()), Some("1.0.0"));
    let future = all.plugins.iter().find(|plugin| plugin.id == "future").unwrap();
    assert!(!future.releases[0].compatible);

    let by_source = MarketplaceQueryDto { source: Some("beta".into()), ..MarketplaceQueryDto::default() };
    assert_eq!(browse(dir.path(), &[a.clone(), b.clone()], &installed, &by_source, "3.3.175").await.plugins.len(), 1);
    let by_text = MarketplaceQueryDto { q: Some("ANN".into()), category: Some("MISC".into()), ..Default::default() };
    let filtered = browse(dir.path(), &[a.clone(), b.clone()], &installed, &by_text, "3.3.175").await;
    assert_eq!(filtered.plugins.iter().map(|plugin| plugin.id.as_str()).collect::<Vec<_>>(), ["future"]);

    let disabled = PluginRepository { enabled: false, ..b };
    let enabled_only = browse(dir.path(), &[a, disabled], &installed, &MarketplaceQueryDto::default(), "3.3.175").await;
    assert_eq!((enabled_only.plugins.len(), enabled_only.sources.len()), (2, 1));
    assert!(enabled_only.plugins.iter().all(|plugin| plugin.also_in.is_empty()));
}

#[tokio::test]
async fn an_install_resolves_the_exact_release_and_reports_incompatibility() {
    let dir = tempfile::tempdir().unwrap();
    let a = source("alpha", "http://a.invalid/c.json");
    seeded(
        dir.path(),
        "alpha",
        &a.url,
        &format!("{},{}", entry("sports", "epg", "1.0.0", "3.0.0", 1), entry("future", "misc", "2.0.0", "9.0.0", 1)),
    );
    let found = resolve_release(dir.path(), &a, "sports", "1.0.0", "3.3.175").await;
    assert_eq!(found, ReleaseLookup::Found(ResolvedRelease { package_url: "http://127.0.0.1/p.tlpx".into() }));
    assert_eq!(resolve_release(dir.path(), &a, "sports", "9.9.9", "3.3.175").await, ReleaseLookup::NotFound);
    assert_eq!(resolve_release(dir.path(), &a, "nope", "1.0.0", "3.3.175").await, ReleaseLookup::NotFound);
    assert!(matches!(
        resolve_release(dir.path(), &a, "future", "2.0.0", "3.3.175").await,
        ReleaseLookup::Incompatible(reason) if reason.contains("9.0.0")
    ));
}

/// A catalog of five plugins in three spellings of two categories, to search and filter.
fn categorised() -> (tempfile::TempDir, PluginRepository) {
    let dir = tempfile::tempdir().unwrap();
    let a = source("alpha", "http://a.invalid/c.json");
    seeded(
        dir.path(),
        "alpha",
        &a.url,
        &[
            entry("guide-one", "EPG", "1.0.0", "3.0.0", 1),
            entry("guide-two", "epg", "1.0.0", "3.0.0", 1),
            entry("channel-tv", "Live TV", "1.0.0", "3.0.0", 1),
            entry("channel-radio", "live_tv", "1.0.0", "3.0.0", 1),
            entry("nothing", "", "1.0.0", "3.0.0", 1),
        ]
        .join(","),
    );
    (dir, a)
}

async fn view(dir: &Path, a: &PluginRepository, query: MarketplaceQueryDto) -> MarketplaceDto {
    browse(dir, std::slice::from_ref(a), &HashMap::new(), &query, "3.3.175").await
}

#[tokio::test]
async fn categories_are_normalised_so_spellings_of_one_category_are_one_filter() {
    let (dir, a) = categorised();
    let all = view(dir.path(), &a, MarketplaceQueryDto::default()).await;
    assert_eq!(all.categories, [category("epg", 2), category("live-tv", 2)], "a plugin without one is in none");
    assert_eq!(all.total, 5);
    assert!(all.plugins.iter().all(|plugin| plugin.category.as_deref().is_none_or(|own| own == own.to_lowercase())));
    // The filter is normalised too: any spelling selects the category.
    for spelling in ["epg", "EPG", " Epg "] {
        let query = MarketplaceQueryDto { category: Some(spelling.into()), ..Default::default() };
        assert_eq!(ids(&view(dir.path(), &a, query).await), ["guide-one", "guide-two"], "{spelling:?}");
    }
    for spelling in ["live-tv", "Live TV", "live_tv"] {
        let query = MarketplaceQueryDto { category: Some(spelling.into()), ..Default::default() };
        assert_eq!(ids(&view(dir.path(), &a, query).await), ["channel-radio", "channel-tv"], "{spelling:?}");
    }
}

#[tokio::test]
async fn a_cache_written_before_categories_were_normalised_is_still_read_as_one_category() {
    let dir = tempfile::tempdir().unwrap();
    let a = source("alpha", "http://a.invalid/c.json");
    let (mut catalog, _) = parse(
        body(&[entry("one", "x", "1.0.0", "3.0.0", 1), entry("two", "x", "1.0.0", "3.0.0", 1)].join(",")).as_bytes(),
        true,
    )
    .unwrap();
    catalog.plugins[0].category = Some("EPG".into());
    catalog.plugins[1].category = Some("epg".into());
    write_cache(
        &cache_path(dir.path(), "alpha").unwrap(),
        &CacheFile { url: a.url.clone(), catalog: Some(catalog), ..CacheFile::default() },
    )
    .unwrap();
    let all = view(dir.path(), &a, MarketplaceQueryDto::default()).await;
    assert_eq!(all.categories, [category("epg", 2)]);
    assert!(all.plugins.iter().all(|plugin| plugin.category.as_deref() == Some("epg")));
}

#[tokio::test]
async fn the_search_matches_every_word_against_id_name_author_and_category() {
    let (dir, a) = categorised();
    let search = |q: &str| MarketplaceQueryDto { q: Some(q.into()), ..Default::default() };
    // The category is searchable: `live` is in no id or name of the radio plugin, but it is in `live-tv`.
    assert_eq!(ids(&view(dir.path(), &a, search("live")).await), ["channel-radio", "channel-tv"]);
    // Words match one by one and in any order; one field need not hold them all.
    assert_eq!(ids(&view(dir.path(), &a, search("ann radio")).await), ["channel-radio"]);
    assert_eq!(ids(&view(dir.path(), &a, search("radio ann")).await), ["channel-radio"]);
    assert_eq!(ids(&view(dir.path(), &a, search("live tv radio")).await), ["channel-radio"]);
    assert_eq!(ids(&view(dir.path(), &a, search("guide epg")).await), ["guide-one", "guide-two"]);
    // Every word has to match somewhere.
    assert_eq!(ids(&view(dir.path(), &a, search("radio zebra")).await), Vec::<&str>::new());
    // Case, surrounding space and a blank search change nothing.
    assert_eq!(ids(&view(dir.path(), &a, search("  GUIDE  ")).await), ["guide-one", "guide-two"]);
    assert_eq!(view(dir.path(), &a, search("   ")).await.plugins.len(), 5);
}

#[tokio::test]
async fn the_category_counts_follow_the_search_and_ignore_the_selected_category() {
    let (dir, a) = categorised();
    // Searching narrows the counts...
    let guide = view(dir.path(), &a, MarketplaceQueryDto { q: Some("guide".into()), ..Default::default() }).await;
    assert_eq!(guide.categories, [category("epg", 2)]);
    assert_eq!(guide.total, 2);
    // ...but selecting a category does not: the other categories stay choosable and `total` stays the search's.
    let selected = MarketplaceQueryDto { category: Some("epg".into()), ..Default::default() };
    let epg = view(dir.path(), &a, selected).await;
    assert_eq!(ids(&epg), ["guide-one", "guide-two"]);
    assert_eq!(epg.categories, [category("epg", 2), category("live-tv", 2)]);
    assert_eq!(epg.total, 5);
    // A selected category the search leaves empty is kept, with a count of 0, so it can be unselected.
    let empty = MarketplaceQueryDto { q: Some("radio".into()), category: Some("epg".into()), ..Default::default() };
    let none = view(dir.path(), &a, empty).await;
    assert_eq!(ids(&none), Vec::<&str>::new());
    assert_eq!(none.categories, [category("epg", 0), category("live-tv", 1)]);
    // A category that is no valid id matches nothing, and is not offered.
    let invalid = MarketplaceQueryDto { category: Some("123".into()), ..Default::default() };
    let nothing = view(dir.path(), &a, invalid).await;
    assert_eq!(ids(&nothing), Vec::<&str>::new());
    assert_eq!(nothing.categories, [category("epg", 2), category("live-tv", 2)]);
}

#[tokio::test]
async fn a_search_word_matches_where_a_word_starts_not_in_the_middle_of_one() {
    let dir = tempfile::tempdir().unwrap();
    let a = source("alpha", "http://a.invalid/c.json");
    let courier = r#"{"id":"courier","name":"Courier","description":"Delivers things, even the unusual","releases":[{"version":"1.0.0","package_url":"http://127.0.0.1/p.tlpx","plugin_api":1}]}"#;
    seeded(
        dir.path(),
        "alpha",
        &a.url,
        &[courier.to_string(), entry("tv-guide", "Live TV", "1.0.0", "3.0.0", 1)].join(","),
    );
    let search = |q: &str| MarketplaceQueryDto { q: Some(q.into()), ..Default::default() };
    // `live` is inside `delivers`, but that does not start a word; the category `live-tv` does.
    assert_eq!(ids(&view(dir.path(), &a, search("live")).await), ["tv-guide"]);
    // A word still matches by its start: a prefix of a word, in any field.
    assert_eq!(ids(&view(dir.path(), &a, search("deliv")).await), ["courier"]);
    assert_eq!(ids(&view(dir.path(), &a, search("cour")).await), ["courier"]);
    // After a separator counts as a start, so ids and categories can be typed as they are written.
    assert_eq!(ids(&view(dir.path(), &a, search("guide")).await), ["tv-guide"]);
    assert_eq!(ids(&view(dir.path(), &a, search("tv-guide")).await), ["tv-guide"]);
    assert_eq!(ids(&view(dir.path(), &a, search("live-tv")).await), ["tv-guide"]);
    // The middle of a word does not.
    assert_eq!(ids(&view(dir.path(), &a, search("uide")).await), Vec::<&str>::new());
    assert_eq!(ids(&view(dir.path(), &a, search("ivers")).await), Vec::<&str>::new());
}
