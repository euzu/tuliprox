use super::*;

fn changed_catalog() -> Vec<PlaylistGroup> {
    let mut playlist = catalog();
    for group in &mut playlist {
        for item in &mut group.channels {
            item.header.title = format!("Candidate {}", item.header.title).intern();
            item.header.url = format!("http://candidate.invalid/{}.mkv", item.header.id).intern();
        }
    }
    playlist
}

fn fail_late(server: &DiscoveryServer, failure: &str) {
    match failure {
        "status" => server.reply(MOVIES_2, 503, "sensitive-body-must-not-appear"),
        "wire" => {
            server.reply(MOVIES_2, 200, r#"{"page":2,"total_pages":2,"total_results":2,"results":[{"id":7},{"id":0}]}"#)
        }
        "no-progress" => {
            server.reply(MOVIES_2, 200, r#"{"page":2,"total_pages":2,"total_results":1,"results":[{"id":8}]}"#)
        }
        "empty" => server.reply(MOVIES_2, 200, r#"{"page":2,"total_pages":2,"total_results":0,"results":[]}"#),
        "body" => server.reply_raw(
            MOVIES_2,
            format!("HTTP/1.1 200 OK\r\nContent-Length: 10000\r\nConnection: close\r\n\r\n{MOVIE_PAGE_2}").into_bytes(),
        ),
        "gzip" | "gzip-transport" | "gzip-suffix" => {
            let mut compressed = tuliprox_core::utils::compression_utils::compress_string(MOVIE_PAGE_2).unwrap();
            if failure == "gzip" {
                compressed.truncate(compressed.len() - 8);
            } else if failure == "gzip-suffix" {
                let mut suffix = tuliprox_core::utils::compression_utils::compress_string(" ").unwrap();
                suffix.truncate(suffix.len() - 8);
                compressed.extend(suffix);
            }
            let advertised = compressed.len() + usize::from(failure == "gzip-transport");
            let mut response = format!("HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {advertised}\r\nConnection: close\r\n\r\n").into_bytes();
            response.extend(compressed);
            server.reply_raw(MOVIES_2, response);
        }
        "bytes" => {
            server.reply(MOVIES_2, 200, &format!("{MOVIE_PAGE_2}{}", " ".repeat(1024 * 1024 + 1 - MOVIE_PAGE_2.len())))
        }
        "requests" => {
            for p in 1..=33 {
                let id = if p == 1 {
                    8
                } else if p == 2 {
                    7
                } else {
                    1000 + p
                };
                server.reply(
                    &format!("/3/trending/movie/week?language=en-US&page={p}"),
                    200,
                    &json!({"page":p,"total_pages":33,"total_results":1,"results":[{"id":id}]}).to_string(),
                );
            }
        }
        _ => panic!("unknown fixture failure"),
    }
}

#[tokio::test]
async fn curation_late_failure_keeps_all_files_epg_cache_mapping_and_watches_in_both_policies() {
    for policy in ["full", "curated"] {
        for failure in [
            "status",
            "wire",
            "no-progress",
            "empty",
            "body",
            "gzip",
            "gzip-transport",
            "gzip-suffix",
            "bytes",
            "requests",
        ] {
            let server = DiscoveryServer::start().await;
            let mut dto = target(policy, true, true);
            // A selection-only selector is just as required as its successful projecting siblings.
            dto.curation.as_mut().unwrap().tmdb.as_mut().unwrap().trending[0].create_xtream_category = false;
            let run = Publication::new(&server, &dto);
            let successful_replies = server.replies.lock().unwrap().clone();
            let initial = file_snapshot(run.directory.path());
            fail_late(&server, failure);
            assert!(run.publish_catalog(changed_catalog()).await.is_err(), "{policy}/{failure}: first refresh");
            assert_eq!(file_snapshot(run.directory.path()), initial);
            assert!(run.context.playlist_state.as_ref().unwrap().data.read().await.is_empty());
            assert!(run.cache_signature().await.is_empty());
            let count = server.requests.lock().unwrap().len();
            assert_eq!(
                count,
                if failure == "requests" { 34 } else { 4 },
                "Trakt and TV are evaluated too, but not published"
            );
            *server.replies.lock().unwrap() = successful_replies;
            run.publish().await.unwrap();
            let files = file_snapshot(run.directory.path());
            assert!(
                files.keys().any(|path| path.to_string_lossy().contains("epg")),
                "EPG must be in the retention inventory: {:?}",
                files.keys().collect::<Vec<_>>()
            );
            let cache = run.cache_signature().await;
            assert!(cache.iter().any(|entry| entry.starts_with("mapping:")));
            let watch_path = run.directory.path().join("publication.groups.bin");
            let watch_before: std::collections::BTreeSet<Arc<str>> =
                tuliprox_core::utils::binary_deserialize(&std::fs::read(&watch_path).unwrap()).unwrap();
            assert!(!watch_before.is_empty());
            fail_late(&server, failure);
            assert!(
                run.publish_catalog(changed_catalog()).await.is_err(),
                "{policy}/{failure}: refresh with prior state"
            );
            assert_eq!(file_snapshot(run.directory.path()), files, "inventory and bytes, not just file sizes");
            assert_eq!(run.cache_signature().await, cache, "all cached rows and mapping records, including timestamps");
            let watch_after: std::collections::BTreeSet<Arc<str>> =
                tuliprox_core::utils::binary_deserialize(&std::fs::read(&watch_path).unwrap()).unwrap();
            assert_eq!(watch_before, watch_after);
        }
    }
}

#[tokio::test]
async fn curation_shared_batch_exhaustion_marks_pending_selectors_required_without_129th_get() {
    for policy in ["full", "curated"] {
        let server = DiscoveryServer::start().await;
        let dto = target(policy, false, true);
        let mut run = Publication::new(&server, &dto);
        let ordinary = run.target.clone();
        let source = run.target.curation.as_mut().unwrap().tmdb.as_mut().unwrap();
        let mut selector = source.trending[0].clone();
        selector.limit = 1;
        selector.create_xtream_category = false;
        source.trending = vec![selector; 129];
        let failing = run.target.clone();
        let initial = file_snapshot(run.directory.path());
        assert!(run.publish().await.is_err());
        assert_eq!(server.requests.lock().unwrap().len(), 128);
        assert_eq!(file_snapshot(run.directory.path()), initial);
        assert!(run.cache_signature().await.is_empty());
        run.target = ordinary;
        run.publish().await.unwrap();
        let before = file_snapshot(run.directory.path());
        let cache = run.cache_signature().await;
        let requests = server.requests.lock().unwrap().len();
        run.target = failing;
        assert!(run.publish_catalog(changed_catalog()).await.is_err());
        assert_eq!(server.requests.lock().unwrap().len() - requests, 128);
        assert_eq!(file_snapshot(run.directory.path()), before);
        assert_eq!(run.cache_signature().await, cache);
    }
}

#[tokio::test]
async fn curation_limit_changes_keep_surviving_ids_and_independent_daily_weekly_memberships() {
    const DAILY: &str = "/3/trending/movie/day?language=en-US&page=1";
    for include_base in [false, true] {
        let server = DiscoveryServer::start().await;
        let mut dto = target("curated", false, true);
        let curation = dto.curation.as_mut().unwrap();
        curation.include_xtream_base_categories = include_base;
        let source = curation.tmdb.as_mut().unwrap();
        let mut daily = source.trending[0].clone();
        daily.time_window = shared::model::TmdbTrendingTimeWindow::Day;
        daily.category_name = Some("TMDB Daily".into());
        daily.limit = 2;
        source.trending.push(daily);
        let mut run = Publication::new(&server, &dto);
        server.reply(DAILY, 200, r#"{"page":1,"total_pages":1,"total_results":2,"results":[{"id":8},{"id":7}]}"#);
        let mut prior = BTreeMap::new();
        for (limit, ids) in [(1, vec![7, 7, 8]), (2, vec![7, 7, 8]), (2, vec![8, 7]), (1, vec![8, 7])] {
            run.target.curation.as_mut().unwrap().tmdb.as_mut().unwrap().trending[0].limit = limit;
            server.reply(MOVIES, 200, &json!({"page":1,"total_pages":9,"total_results":ids.len(),"results":ids.iter().map(|id| json!({"id":id})).collect::<Vec<_>>()} ).to_string());
            run.publish().await.unwrap();
            let rows = run.xtream_rows().await;
            let alive: BTreeMap<_, _> =
                rows.iter().map(|row| ((row.group.to_string(), row.title.to_string()), row.virtual_id)).collect();
            for (subject, id) in &prior {
                if let Some(current) = alive.get(subject) {
                    assert_eq!(current, id, "surviving subject/alias {subject:?}");
                }
            }
            prior = alive;
            let identities = run.identity_signature().await;
            for title in ["Show", "Show S01E01"] {
                let uuid = hash_string(&format!("trakt-category:TMDB TV:{}", hash_string(title))).to_string();
                assert!(identities.contains_key(&uuid));
            }
            assert_eq!(rows.iter().any(|row| row.group.as_ref() == "Movies"), include_base);
            let mut weekly: Vec<_> = rows.iter().filter(|row| row.group.as_ref() == "TMDB Movies").collect();
            weekly.sort_by_key(|row| row.source_ordinal);
            assert_eq!(weekly[0].title.as_ref(), if ids[0] == 7 { "First" } else { "Second" });
            let mut daily: Vec<_> = rows.iter().filter(|row| row.group.as_ref() == "TMDB Daily").collect();
            daily.sort_by_key(|row| row.source_ordinal);
            assert_eq!(
                daily.iter().map(|row| row.title.as_ref()).collect::<Vec<_>>(),
                ["Second", "First", "Local alias"]
            );
            let m3u = load_m3u_target_storage(&run.context.config, &run.target).await.unwrap();
            for title in ["First", "Local alias", "Second", "Show S01E01"] {
                assert_eq!(m3u.iter().filter(|(_, item)| item.title.as_ref() == title).count(), 1);
            }
            assert!(m3u.iter().all(|(_, item)| !item.group.starts_with("TMDB")));
        }
        assert_eq!(server.requests.lock().unwrap().len(), 12, "no extra page after N despite total_pages=9");
    }
}

#[tokio::test]
async fn curation_full_empty_or_no_match_removes_projections_not_ordinary_catalog() {
    for body in [EMPTY, r#"{"page":1,"total_pages":99,"total_results":2,"results":[{"id":999},{"id":7}]}"#] {
        let server = DiscoveryServer::start().await;
        let mut run = Publication::new(&server, &target("full", false, true));
        run.publish().await.unwrap();
        for selector in &mut run.target.curation.as_mut().unwrap().tmdb.as_mut().unwrap().trending {
            selector.limit = 1;
        }
        server.reply(MOVIES, 200, body);
        server.reply(TV, 200, body);
        run.publish().await.unwrap();
        let rows = run.xtream_rows().await;
        assert!(rows.iter().all(|row| !row.group.starts_with("TMDB")));
        assert!(rows.iter().any(|row| row.title.as_ref() == "Unselected"));
        assert!(rows.iter().any(|row| row.title.as_ref() == "Live"));
        assert!(rows.iter().any(|row| row.title.as_ref() == "Other show"));
        assert_eq!(
            server.requests.lock().unwrap().len(),
            5,
            "do not seek N matches beyond the first unmatched reference"
        );
    }
}

#[tokio::test]
async fn curation_profile_requires_the_fixture_ca_and_has_no_unconfigured_fallback() {
    let server = DiscoveryServer::start().await;
    let mut run = Publication::new(&server, &target("full", false, true));
    run.tmdb_client = tuliprox_core::utils::network::request::create_tmdb_client(&run.context.config)
        .unwrap()
        .no_proxy()
        .resolve("api.themoviedb.org", server.address)
        .build()
        .unwrap();
    assert!(run.publish().await.is_err(), "untrusted CA fails rather than disabling TLS verification");
    assert!(server.requests.lock().unwrap().is_empty());
    assert!(file_snapshot(run.directory.path()).is_empty());
}

#[tokio::test]
async fn curation_production_wiring_does_not_fall_back_to_the_generic_client() {
    let server = DiscoveryServer::start().await;
    let run = Publication::new(&server, &target("full", false, true));
    let mut config = (*run.context.config.config.load_full()).clone();
    config.proxy =
        Some(tuliprox_core::model::ProxyConfig { url: "invalid proxy".into(), username: None, password: None });
    run.context.config.config.store(Arc::new(config));
    let prepared = PreparedTarget {
        target: run.target.clone(),
        playlist: catalog(),
        epg: Vec::new(),
        processing: PipelineStats::default(),
        accepted_empty_clusters: ClusterFlags::empty(),
        library_empty: tuliprox_repository::LibraryEmptyPublication::None,
    };
    let (result, _) = finalize_prepared_target(Arc::clone(&run.context), prepared).await;
    assert!(result.is_err());
    assert!(
        server.requests.lock().unwrap().is_empty(),
        "generic client has the fixture CA and could succeed, but is not used"
    );
    assert!(run.cache_signature().await.is_empty());
    assert!(file_snapshot(run.directory.path()).is_empty());
}

#[tokio::test]
async fn curation_does_not_authorize_empty_inputs_and_authorized_empty_still_waits_for_all_selectors() {
    for authorized in [false, true] {
        let server = DiscoveryServer::start().await;
        let run = Publication::new(&server, &target("curated", false, true));
        server.reply(MOVIES_2, 503, "unavailable");
        let prepared = PreparedTarget {
            target: run.target.clone(),
            playlist: Vec::new(),
            epg: Vec::new(),
            processing: PipelineStats::default(),
            accepted_empty_clusters: if authorized { ClusterFlags::Vod } else { ClusterFlags::empty() },
            library_empty: tuliprox_repository::LibraryEmptyPublication::None,
        };
        let (result, _) =
            target::finalize_prepared_target_with_tmdb(Arc::clone(&run.context), prepared, Some(&run.tmdb_client))
                .await;
        assert_eq!(result.is_err(), authorized);
        assert_eq!(server.requests.lock().unwrap().len(), if authorized { 3 } else { 0 });
        assert!(file_snapshot(run.directory.path()).is_empty());
        assert!(run.cache_signature().await.is_empty());
    }
}

#[tokio::test]
async fn curation_post_admission_writer_failure_remains_best_effort_not_a_rollback() {
    let server = DiscoveryServer::start().await;
    let run = Publication::new(&server, &target("curated", false, true));
    std::fs::create_dir(run.directory.path().join("published.m3u")).unwrap();
    assert!(run.publish().await.is_err());
    assert!(!run.xtream_rows().await.is_empty(), "other admitted output was still written");
    assert_eq!(server.requests.lock().unwrap().len(), 3);
    assert!(
        !run.directory.path().join("publication.groups.bin").exists(),
        "watches do not finalize a failed writer run"
    );
}
