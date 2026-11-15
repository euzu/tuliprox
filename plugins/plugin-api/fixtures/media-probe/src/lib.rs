wit_bindgen::generate!({
    path: ["../../wit", "wit"],
    world: "tuliprox:media-probe/media-probe",
    generate_all,
});

use exports::tuliprox::plugin::{
    lifecycle,
    media_provider_v1_1::{Guest, ImageRequest, MediaCatalogPage, MediaRequest, StreamRequest, StreamTarget},
};
use serde_json::Value;
use std::sync::Mutex;
use tuliprox::plugin::media_types_v1_1::{
    DescriptiveFacts, LibraryKind, MediaCatalog, MediaEpisode, MediaLibrary, MediaMovie, MediaSeason,
    MediaSeries, ProviderId, TechnicalFacts, VideoTechnicalFacts,
};

static MODE: Mutex<Option<String>> = Mutex::new(None);

struct MediaProbe;

impl lifecycle::Guest for MediaProbe {
    fn init(config: String) -> Result<(), String> {
        let parsed: Value = serde_json::from_str(&config).unwrap_or(Value::Null);
        if let Some(mode) = parsed.get("mode").and_then(Value::as_str) {
            if let Ok(mut guard) = MODE.lock() {
                *guard = Some(mode.to_string());
            }
        }
        Ok(())
    }
}

fn get_mode(req: &MediaRequest) -> String {
    for (key, val) in &req.options {
        if key == "mode" {
            return val.clone();
        }
    }
    MODE.lock().ok().and_then(|g| g.clone()).unwrap_or_else(|| "normal".to_string())
}

fn normal_libraries() -> Vec<MediaLibrary> {
    vec![
        MediaLibrary {
            id: "lib-movies".into(),
            name: "Movies".into(),
            kind: LibraryKind::Movies,
        },
        MediaLibrary {
            id: "lib-tv".into(),
            name: "TV Shows".into(),
            kind: LibraryKind::TvShows,
        },
    ]
}

fn normal_movie() -> MediaMovie {
    MediaMovie {
        library_id: "lib-movies".into(),
        item_id: "movie-1".into(),
        title: "The Big Movie".into(),
        year: Some(2023),
        release_date: Some("2023-05-15".into()),
        source_version_hint: None,
        provider_ids: vec![ProviderId {
            namespace: "tmdb".into(),
            value: "1001".into(),
        }],
        descriptive: Some(DescriptiveFacts {
            original_title: Some("The Big Movie".into()),
            sort_title: Some("Big Movie, The".into()),
            summary: Some("An epic movie description".into()),
            tagline: Some("The biggest of all".into()),
            studio: Some("Studio A".into()),
            network: None,
            content_rating: Some("PG-13".into()),
            parental_age: Some(13),
            audience_rating: Some("8.5".into()),
            genres: vec!["Action".into(), "Sci-Fi".into()],
            countries: vec!["US".into()],
            directors: vec!["Director One".into()],
            writers: vec!["Writer One".into()],
            cast: vec!["Actor One".into(), "Actor Two".into()],
            crew: vec!["Producer One".into()],
        }),
        technical: Some(TechnicalFacts {
            container: Some("mp4".into()),
            duration_secs: Some(7200),
            bitrate: Some(5_000_000),
            video: Some(VideoTechnicalFacts {
                codec: Some("h264".into()),
                width: Some(1920),
                height: Some(1080),
            }),
            audio: None,
        }),
        stream_key: Some("stream-movie-1".into()),
        image_key: Some("image-movie-1".into()),
    }
}

fn normal_series() -> MediaSeries {
    MediaSeries {
        library_id: "lib-tv".into(),
        item_id: "series-1".into(),
        title: "Epic Series".into(),
        year: Some(2022),
        release_date: Some("2022-01-10".into()),
        source_version_hint: None,
        provider_ids: vec![ProviderId {
            namespace: "tmdb".into(),
            value: "2001".into(),
        }],
        descriptive: Some(DescriptiveFacts {
            original_title: Some("Epic Series".into()),
            sort_title: Some("Epic Series".into()),
            summary: Some("A gripping series drama".into()),
            tagline: Some("Winter is arriving".into()),
            studio: Some("Network TV".into()),
            network: Some("Network TV".into()),
            content_rating: Some("TV-MA".into()),
            parental_age: Some(17),
            audience_rating: Some("9.0".into()),
            genres: vec!["Drama".into()],
            countries: vec!["US".into()],
            directors: vec!["Showrunner One".into()],
            writers: vec!["Writer Two".into()],
            cast: vec!["Star Actor".into()],
            crew: vec![],
        }),
        child_count: Some(2),
        episode_count: Some(3),
        image_key: Some("image-series-1".into()),
    }
}

fn normal_seasons() -> Vec<MediaSeason> {
    vec![
        MediaSeason {
            library_id: "lib-tv".into(),
            item_id: "season-1".into(),
            series_id: Some("series-1".into()),
            series_title: Some("Epic Series".into()),
            title: "Season 1".into(),
            season: Some(1),
            year: Some(2022),
            release_date: Some("2022-01-10".into()),
            source_version_hint: None,
            provider_ids: vec![],
            descriptive: None,
            episode_count: Some(2),
            image_key: Some("image-season-1".into()),
        },
        MediaSeason {
            library_id: "lib-tv".into(),
            item_id: "season-2".into(),
            series_id: Some("series-1".into()),
            series_title: Some("Epic Series".into()),
            title: "Season 2".into(),
            season: Some(2),
            year: Some(2023),
            release_date: Some("2023-01-15".into()),
            source_version_hint: None,
            provider_ids: vec![],
            descriptive: None,
            episode_count: Some(1),
            image_key: Some("image-season-2".into()),
        },
    ]
}

fn normal_episodes() -> Vec<MediaEpisode> {
    vec![
        MediaEpisode {
            library_id: "lib-tv".into(),
            item_id: "ep-1".into(),
            season_id: Some("season-1".into()),
            series_id: Some("series-1".into()),
            series_title: Some("Epic Series".into()),
            title: "Pilot".into(),
            season: Some(1),
            episode: Some(1),
            release_date: Some("2022-01-10".into()),
            source_version_hint: None,
            provider_ids: vec![],
            descriptive: None,
            technical: None,
            stream_key: Some("stream-ep-1".into()),
            image_key: Some("image-ep-1".into()),
        },
        MediaEpisode {
            library_id: "lib-tv".into(),
            item_id: "ep-2".into(),
            season_id: Some("season-1".into()),
            series_id: Some("series-1".into()),
            series_title: Some("Epic Series".into()),
            title: "Second Chapter".into(),
            season: Some(1),
            episode: Some(2),
            release_date: Some("2022-01-17".into()),
            source_version_hint: None,
            provider_ids: vec![],
            descriptive: None,
            technical: None,
            stream_key: Some("stream-ep-2".into()),
            image_key: Some("image-ep-2".into()),
        },
        MediaEpisode {
            library_id: "lib-tv".into(),
            item_id: "ep-3".into(),
            season_id: Some("season-2".into()),
            series_id: Some("series-1".into()),
            series_title: Some("Epic Series".into()),
            title: "New Beginning".into(),
            season: Some(2),
            episode: Some(1),
            release_date: Some("2023-01-15".into()),
            source_version_hint: None,
            provider_ids: vec![],
            descriptive: None,
            technical: None,
            stream_key: Some("stream-ep-3".into()),
            image_key: Some("image-ep-3".into()),
        },
    ]
}

impl Guest for MediaProbe {
    fn fetch_catalog(req: MediaRequest) -> Result<MediaCatalogPage, String> {
        let mode = get_mode(&req);

        match mode.as_str() {
            "echo_request" => {
                let mut movie = normal_movie();
                movie.title = format!(
                    "refresh:{} last_success_at:{}",
                    req.refresh,
                    req.last_success_at.map_or_else(|| "none".to_string(), |ts| ts.to_string())
                );
                Ok(MediaCatalogPage {
                    catalog: MediaCatalog {
                        libraries: normal_libraries(),
                        movies: vec![movie],
                        series: vec![],
                        seasons: vec![],
                        episodes: vec![],
                    },
                    next_cursor: None,
                })
            }
            "trap" => panic!("evil probe: trap requested"),
            "hang" => loop {},
            "loop_cursor" => {
                let catalog = MediaCatalog {
                    libraries: normal_libraries(),
                    movies: vec![normal_movie()],
                    series: vec![],
                    seasons: vec![],
                    episodes: vec![],
                };
                Ok(MediaCatalogPage {
                    catalog,
                    next_cursor: Some("looping-cursor".into()),
                })
            }
            "oversized" => {
                let oversized_movie = MediaMovie {
                    library_id: "lib-movies".into(),
                    item_id: "movie-oversized".into(),
                    title: "A".repeat(1000),
                    year: Some(2020),
                    release_date: None,
                    source_version_hint: None,
                    provider_ids: (0..100)
                        .map(|i| ProviderId {
                            namespace: "tmdb".into(),
                            value: format!("{i}"),
                        })
                        .collect(),
                    descriptive: Some(DescriptiveFacts {
                        original_title: None,
                        sort_title: None,
                        summary: Some("B".repeat(30_000)),
                        tagline: None,
                        studio: None,
                        network: None,
                        content_rating: None,
                        parental_age: None,
                        audience_rating: None,
                        genres: (0..100).map(|i| format!("genre-{i}")).collect(),
                        countries: (0..100).map(|i| format!("country-{i}")).collect(),
                        directors: (0..100).map(|i| format!("dir-{i}")).collect(),
                        writers: (0..100).map(|i| format!("writer-{i}")).collect(),
                        cast: (0..200).map(|i| format!("actor-{i}")).collect(),
                        crew: (0..200).map(|i| format!("crew-{i}")).collect(),
                    }),
                    technical: None,
                    stream_key: None,
                    image_key: None,
                };
                Ok(MediaCatalogPage {
                    catalog: MediaCatalog {
                        libraries: normal_libraries(),
                        movies: vec![oversized_movie],
                        series: vec![],
                        seasons: vec![],
                        episodes: vec![],
                    },
                    next_cursor: None,
                })
            }
            "duplicate_ids" => {
                let mut m1 = normal_movie();
                m1.item_id = "dup-movie".into();
                let mut m2 = normal_movie();
                m2.item_id = "dup-movie".into();
                m2.title = "Duplicate Movie Title".into();

                let mut s1 = normal_series();
                s1.item_id = "dup-series".into();
                let mut s2 = normal_series();
                s2.item_id = "dup-series".into();
                s2.title = "Duplicate Series Title".into();

                Ok(MediaCatalogPage {
                    catalog: MediaCatalog {
                        libraries: normal_libraries(),
                        movies: vec![m1, m2],
                        series: vec![s1, s2],
                        seasons: vec![],
                        episodes: vec![],
                    },
                    next_cursor: None,
                })
            }
            "dangling_ref" => {
                let mut movie = normal_movie();
                movie.library_id = "unknown-lib".into();

                let mut series = normal_series();
                series.library_id = "unknown-lib".into();

                let mut season = normal_seasons().remove(0);
                season.series_id = Some("unknown-series".into());

                let mut episode = normal_episodes().remove(0);
                episode.series_id = Some("unknown-series".into());

                Ok(MediaCatalogPage {
                    catalog: MediaCatalog {
                        libraries: normal_libraries(),
                        movies: vec![movie],
                        series: vec![series],
                        seasons: vec![season],
                        episodes: vec![episode],
                    },
                    next_cursor: None,
                })
            }
            "paged" => {
                match req.cursor.as_deref() {
                    None => Ok(MediaCatalogPage {
                        catalog: MediaCatalog {
                            libraries: normal_libraries(),
                            movies: vec![normal_movie()],
                            series: vec![],
                            seasons: vec![],
                            episodes: vec![],
                        },
                        next_cursor: Some("page-2".into()),
                    }),
                    Some("page-2") => Ok(MediaCatalogPage {
                        catalog: MediaCatalog {
                            libraries: vec![],
                            movies: vec![],
                            series: vec![normal_series()],
                            seasons: normal_seasons(),
                            episodes: vec![],
                        },
                        next_cursor: Some("page-3".into()),
                    }),
                    Some("page-3") => Ok(MediaCatalogPage {
                        catalog: MediaCatalog {
                            libraries: vec![],
                            movies: vec![],
                            series: vec![],
                            seasons: vec![],
                            episodes: normal_episodes(),
                        },
                        next_cursor: None,
                    }),
                    Some(other) => Err(format!("unknown cursor {other}")),
                }
            }
            _ => {
                // normal mode
                Ok(MediaCatalogPage {
                    catalog: MediaCatalog {
                        libraries: normal_libraries(),
                        movies: vec![normal_movie()],
                        series: vec![normal_series()],
                        seasons: normal_seasons(),
                        episodes: normal_episodes(),
                    },
                    next_cursor: None,
                })
            }
        }
    }

    fn resolve_stream(req: StreamRequest) -> Result<StreamTarget, String> {
        let mode = find_option(&req.options, "stream_mode")
            .or_else(|| find_option(&req.options, "mode"))
            .or_else(|| req.stream_key.as_deref())
            .unwrap_or("normal");
        let target_url = find_option(&req.options, "target_url").unwrap_or(&req.url);
        resolve_target_from_mode(mode, target_url)
    }

    fn resolve_image(req: ImageRequest) -> Result<StreamTarget, String> {
        let mode = find_option(&req.options, "image_mode")
            .or_else(|| find_option(&req.options, "mode"))
            .unwrap_or(req.image_key.as_str());
        let target_url = find_option(&req.options, "image_target_url")
            .or_else(|| find_option(&req.options, "target_url"))
            .unwrap_or(&req.url);
        resolve_target_from_mode(mode, target_url)
    }
}

fn resolve_target_from_mode(mode: &str, default_url: &str) -> Result<StreamTarget, String> {
    match mode {
        "hang" | "hang past deadline" => loop {
            std::hint::spin_loop();
        },
        "trap" => panic!("simulated media-probe trap"),
        "non-http scheme" => Ok(StreamTarget {
            url: "ftp://example.invalid/media.mp4".into(),
            headers: vec![],
            ttl_secs: None,
        }),
        "private/disallowed host" => Ok(StreamTarget {
            url: "http://".into(),
            headers: vec![],
            ttl_secs: None,
        }),
        "oversized and denylisted headers" => Ok(StreamTarget {
            url: default_url.to_string(),
            headers: vec![
                ("Host".into(), "denied.invalid".into()),
                ("Connection".into(), "close".into()),
            ],
            ttl_secs: None,
        }),
        "oversized" => {
            let headers = (0..35).map(|i| (format!("X-Header-{i}"), "val".into())).collect();
            Ok(StreamTarget {
                url: default_url.to_string(),
                headers,
                ttl_secs: None,
            })
        }
        "ttl" => Ok(StreamTarget {
            url: default_url.to_string(),
            headers: vec![("X-Media-Probe-Auth".into(), "probe-secret-token".into())],
            ttl_secs: Some(30),
        }),
        _ => Ok(StreamTarget {
            url: default_url.to_string(),
            headers: vec![("X-Media-Probe-Auth".into(), "probe-secret-token".into())],
            ttl_secs: None,
        }),
    }
}

fn find_option<'a>(options: &'a [(String, String)], key: &str) -> Option<&'a str> {
    options.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

export!(MediaProbe);
