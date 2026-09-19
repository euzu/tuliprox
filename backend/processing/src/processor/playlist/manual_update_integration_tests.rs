mod manual_update_integration {
    use super::*;
    use shared::model::{ConfigRenameDto, ItemField, LibraryConfigDto, LibraryContentType, LibraryScanResult};
    use tuliprox_core::model::{LibraryConfig, LibraryScanDirectory, M3uTargetOutput, MetadataUpdateConfig};

    async fn library_context(root: &Path, order: ProcessingOrder) -> PlaylistProcessingContext<CollectSink> {
        let ctx = processing_context_with_events(root, CollectSink::default());
        let media = root.join("media");
        tokio::fs::create_dir_all(&media).await.unwrap();
        tokio::fs::write(media.join("Library Movie (2025).mp4"), b"fixture").await.unwrap();
        let companion_path = root.join("companion.m3u");
        tokio::fs::write(&companion_path, "#EXTM3U\n#EXTINF:-1,Companion\nhttp://stream.example/companion\n")
            .await
            .unwrap();
        let mut config = (**ctx.config.config.load()).clone();
        let mut library = LibraryConfig::from(&LibraryConfigDto { enabled: true, ..LibraryConfigDto::default() });
        library.metadata.fallback_to_filename = true;
        library.thumbnails.enabled = false;
        library.scan_directories = vec![LibraryScanDirectory {
            enabled: true,
            path: media.to_string_lossy().into_owned(),
            content_type: LibraryContentType::Movie,
            recursive: true,
        }];
        config.library = Some(library);
        let mut metadata = MetadataUpdateConfig::default();
        metadata.ffprobe.enabled = false;
        metadata.tmdb.enabled = false;
        config.metadata_update = Some(metadata);
        ctx.config.config.store(Arc::new(config));
        let library_input = Arc::new(ConfigInput {
            id: 2,
            name: "local".intern(),
            input_type: InputType::Library,
            enabled: true,
            cache_duration_seconds: 3600,
            ..ConfigInput::default()
        });
        let companion = Arc::new(ConfigInput {
            id: 3,
            name: "companion".intern(),
            input_type: InputType::M3u,
            enabled: true,
            url: companion_path.to_string_lossy().into_owned(),
            ..ConfigInput::default()
        });
        let mut target = ConfigTarget::from(&ConfigTargetDto {
            id: 20,
            enabled: true,
            name: "selected".into(),
            processing_order: order,
            rename: Some(vec![ConfigRenameDto {
                field: ItemField::Name,
                pattern: "^MAPPED$".into(),
                new_name: "RENAMED".into(),
                t_pattern: None,
            }]),
            ..ConfigTargetDto::default()
        });
        target.output = vec![TargetOutput::M3u(M3uTargetOutput {
            filename: Some(root.join("selected.m3u").to_string_lossy().into_owned()),
            include_type_in_url: false,
            mask_redirect_url: false,
            filter: None,
        })];
        target.filter = get_filter(r#"Name ~ ".*""#, None).unwrap().into();
        let mapped = super::super::mapping_stage::build_target(
            vec![super::super::mapping_stage::build_mapping("manual", MappingStage::Processing, r#"@Name = "MAPPED""#)],
            false,
        );
        target.mapping = mapped.mapping;
        let mut unselected = target.clone();
        unselected.id = 21;
        unselected.name = "unselected".into();
        unselected.output = vec![TargetOutput::M3u(M3uTargetOutput {
            filename: Some(root.join("unselected.m3u").to_string_lossy().into_owned()),
            include_type_in_url: false,
            mask_redirect_url: false,
            filter: None,
        })];
        ctx.config.sources.store(Arc::new(SourcesConfig {
            inputs: vec![library_input.clone(), companion.clone()],
            sources: vec![ConfigSource {
                inputs: vec![library_input.name.clone(), companion.name.clone()],
                targets: vec![Arc::new(target), Arc::new(unselected)],
            }],
            ..SourcesConfig::default()
        }));
        ctx
    }

    fn rescan_run(ctx: &PlaylistProcessingContext<CollectSink>, guard: UpdateGuard) -> ProcessingRun<CollectSink> {
        ProcessingRun::for_run(
            "manual-library-run".into(),
            ctx.client.clone(),
            ctx.config.clone(),
            Arc::new(ProcessTargets {
                enabled: true,
                inputs: vec![2, 3],
                targets: vec![20],
                target_names: vec!["selected".into()],
            }),
            ctx.events.clone(),
        )
        .with_update_guard(guard)
        .with_manual_input_update(Some(InputUpdateRequest { input_id: 2, action: InputUpdateAction::Rescan }))
    }

    fn scan_results(ctx: &PlaylistProcessingContext<CollectSink>) -> Vec<LibraryScanResult> {
        ctx.events
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                EventMessage::PlaylistUpdateProgress(progress) if progress.input_id == Some(2) => {
                    if progress.library_scan_result.is_some() {
                        assert_eq!(
                            progress.run_id.as_ref().map(PlaylistUpdateRunId::as_ref),
                            Some("manual-library-run")
                        );
                        assert!(progress.execution_order.is_some());
                    }
                    progress.library_scan_result.clone()
                }
                _ => None,
            })
            .collect()
    }

    fn assert_run_state(ctx: &PlaylistProcessingContext<CollectSink>, state: PlaylistUpdateState) {
        let events = ctx.events.0.lock().unwrap();
        assert!(
            events.iter().any(|event| matches!(event, EventMessage::PlaylistUpdate(summary) if summary.state == state)),
            "{events:?}"
        );
        assert!(events.iter().any(|event| matches!(event, EventMessage::PlaylistUpdateProgress(progress) if progress.input_id == Some(2) && progress.state == Some(state))), "{events:?}");
    }

    #[tokio::test]
    async fn manual_update_library_empty_rescan_persists_zero_to_zero_and_nonzero_to_zero_with_companion() {
        for disk in [false, true] {
            for previous_population in [false, true] {
                for companion in [false, true] {
                    let temp = tempfile::tempdir().unwrap();
                    let ctx = library_context(temp.path(), ProcessingOrder::Frm).await;
                    let mut config = (**ctx.config.config.load()).clone();
                    config.disk_based_processing = disk;
                    ctx.config.config.store(Arc::new(config));
                    if !companion {
                        let mut sources = (**ctx.config.sources.load()).clone();
                        sources.sources[0].inputs.retain(|name| name.as_ref() == "local");
                        ctx.config.sources.store(Arc::new(sources));
                    }
                    if previous_population {
                        exec_processing(rescan_run(&ctx, UpdateGuard::new())).await;
                        assert_run_state(&ctx, PlaylistUpdateState::Success);
                        ctx.events.0.lock().unwrap().clear();
                    }
                    tokio::fs::remove_file(temp.path().join("media/Library Movie (2025).mp4")).await.unwrap();
                    exec_processing(rescan_run(&ctx, UpdateGuard::new())).await;
                    assert_run_state(&ctx, PlaylistUpdateState::Success);
                    for name in std::iter::once("local").chain(companion.then_some("companion")) {
                        let path = input_cache::resolve_input_storage_path(&temp.path().to_string_lossy(), name).await;
                        let status = input_cache::load_input_status(&path);
                        assert_eq!(status.last_input_update.unwrap().state, PlaylistUpdateState::Success, "{name}");
                    }
                    assert_eq!(
                        scan_results(&ctx),
                        vec![LibraryScanResult {
                            files_scanned: 0,
                            groups_scanned: 0,
                            files_added: 0,
                            files_updated: 0,
                            files_removed: usize::from(previous_population),
                            errors: 0,
                        }]
                    );
                    let output = tokio::fs::read_to_string(temp.path().join("selected.m3u")).await.unwrap();
                    assert_eq!(output.matches("#EXTINF").count(), usize::from(companion), "{output}");
                    assert_eq!(output.contains("http://stream.example/companion"), companion);
                    assert!(!output.contains("Library%20Movie") && !output.contains("file://"));
                    assert!(!temp.path().join("unselected.m3u").exists());
                    let input = ctx.config.sources.load().inputs[0].clone();
                    let mut persisted = load_input_playlist(&ctx.config, &input, None).await.unwrap();
                    assert!(persisted.is_empty(), "Empty Library must replace its old persisted input");
                    assert!(!ctx.events.0.lock().unwrap().iter().any(|event| matches!(event, EventMessage::PlaylistUpdateProgress(progress) if progress.message.contains("playlist is empty"))));
                    // Same canonical empty input remains readable on subsequent cache/reuse paths.
                    let first = download_input(&ctx, &input, false).await;
                    assert!(first.errors.is_empty() && first.storage_error.is_none());
                    let mut reuse = download_input(&ctx, &input, false).await;
                    assert_eq!(reuse.job_state(), InputJobState::Ready);
                    assert_eq!(reuse.input_telemetry, None, "In-run reuse must not emit another acquisition");
                    assert_eq!(scan_results(&ctx).len(), 1, "Reuse must not manufacture a second rescan");
                }
            }
        }
    }

    #[tokio::test]
    async fn coderabbit_library_unavailable_is_not_an_authoritative_empty_catalog() {
        for disabled in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let ctx = library_context(temp.path(), ProcessingOrder::Frm).await;
            exec_processing(rescan_run(&ctx, UpdateGuard::new())).await;
            assert_run_state(&ctx, PlaylistUpdateState::Success);
            let mut input = (*ctx.config.sources.load().inputs[0]).clone();
            input.cache_duration_seconds = 0;
            let input = Arc::new(input);
            let storage = input_storage_path(&ctx, &input).await;
            let file = tuliprox_repository::get_input_local_library_playlist_file_path(&storage, &input.name);
            let before = tokio::fs::read(&file).await.unwrap();
            let output = tokio::fs::read(temp.path().join("selected.m3u")).await.unwrap();
            let mut config = (**ctx.config.config.load()).clone();
            if disabled {
                config.library.as_mut().unwrap().enabled = false;
            } else {
                config.library = None;
            }
            ctx.config.config.store(Arc::new(config));

            let mut result = download_input(&ctx, &input, false).await;
            assert_eq!(result.job_state(), InputJobState::Failed);
            assert_eq!(result.update_state(), PlaylistUpdateState::Failure);
            assert!(!result.authoritative_library);
            assert!(!result.errors.is_empty());
            assert_eq!(tokio::fs::read(&file).await.unwrap(), before);
            assert_eq!(tokio::fs::read(temp.path().join("selected.m3u")).await.unwrap(), output);
        }
    }

    #[tokio::test]
    async fn coderabbit_library_missing_input_triggers_catalog_recovery_not_empty_success() {
        for disk in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let ctx = library_context(temp.path(), ProcessingOrder::Frm).await;
            let mut config = (**ctx.config.config.load()).clone();
            config.disk_based_processing = disk;
            ctx.config.config.store(Arc::new(config));
            exec_processing(rescan_run(&ctx, UpdateGuard::new())).await;
            assert_run_state(&ctx, PlaylistUpdateState::Success);
            let input = ctx.config.sources.load().inputs[0].clone();
            let storage = input_storage_path(&ctx, &input).await;
            let file = tuliprox_repository::get_input_local_library_playlist_file_path(&storage, &input.name);
            tokio::fs::remove_file(&file).await.unwrap();

            assert!(load_input_playlist(&ctx.config, &input, None).await.is_err());
            let mut recovered = download_input(&ctx, &input, false).await;
            assert_eq!(recovered.job_state(), InputJobState::Ready);
            assert!(recovered.errors.is_empty() && recovered.storage_error.is_none());
            assert!(!recovered.source.is_empty(), "missing cache must be repaired from the populated catalog");
            assert!(file.exists());
            let mut persisted = load_input_playlist(&ctx.config, &input, None).await.unwrap();
            assert!(!persisted.is_empty());

            let media_server = ConfigInput { input_type: InputType::Plex, ..(*input).clone() };
            assert!(
                load_input_playlist(&ctx.config, &media_server, None).await.is_err(),
                "the shared loader must not turn a missing Plex cache into a successful empty input"
            );
        }
    }

    #[tokio::test]
    async fn manual_update_library_scan_errors_keep_real_result_and_block_rebuild() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = library_context(temp.path(), ProcessingOrder::Frm).await;
        let mut config = (**ctx.config.config.load()).clone();
        config.library.as_mut().unwrap().metadata.fallback_to_filename = false;
        ctx.config.config.store(Arc::new(config));
        exec_processing(rescan_run(&ctx, UpdateGuard::new())).await;
        assert_run_state(&ctx, PlaylistUpdateState::Failure);
        let path = input_cache::resolve_input_storage_path(&temp.path().to_string_lossy(), "local").await;
        assert_eq!(
            input_cache::load_input_status(&path).last_input_update.unwrap().state,
            PlaylistUpdateState::Failure
        );
        assert_eq!(
            scan_results(&ctx),
            vec![LibraryScanResult {
                files_scanned: 1,
                groups_scanned: 1,
                files_added: 0,
                files_updated: 0,
                files_removed: 0,
                errors: 1,
            }]
        );
        assert!(!temp.path().join("selected.m3u").exists());
    }

    #[tokio::test]
    async fn manual_update_library_persisted_input_status_keeps_failed_companion_separate() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = library_context(temp.path(), ProcessingOrder::Frm).await;
        exec_processing(rescan_run(&ctx, UpdateGuard::new())).await;
        let path = input_cache::resolve_input_storage_path(&temp.path().to_string_lossy(), "companion").await;
        assert_eq!(
            input_cache::load_input_status(&path).last_input_update.unwrap().state,
            PlaylistUpdateState::Success
        );
        let before = std::fs::read(temp.path().join("selected.m3u")).unwrap();
        tokio::fs::write(temp.path().join("companion.m3u"), "#EXTM3U\n").await.unwrap();
        exec_processing(rescan_run(&ctx, UpdateGuard::new())).await;
        assert_eq!(
            input_cache::load_input_status(&path).last_input_update.unwrap().state,
            PlaylistUpdateState::Failure
        );
        let library_path = input_cache::resolve_input_storage_path(&temp.path().to_string_lossy(), "local").await;
        assert_eq!(
            input_cache::load_input_status(&library_path).last_input_update.unwrap().state,
            PlaylistUpdateState::Success
        );
        assert_eq!(std::fs::read(temp.path().join("selected.m3u")).unwrap(), before);
    }

    #[tokio::test]
    async fn manual_update_library_empty_xtream_and_strm_use_existing_publish_and_keep_companions() {
        use tuliprox_repository::{xtream_get_file_path, xtream_get_storage_path, BPlusTreeQuery};
        for companion in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let ctx = library_context(temp.path(), ProcessingOrder::Frm).await;
            tokio::fs::write(
                temp.path().join("companion.m3u"),
                "#EXTM3U\n#EXTINF:-1 type=\"vod\",Companion\nhttp://stream.example/companion.mp4\n",
            )
            .await
            .unwrap();
            let mut sources = (**ctx.config.sources.load()).clone();
            if !companion {
                sources.sources[0].inputs.retain(|name| name.as_ref() == "local");
            }
            let target = Arc::make_mut(&mut sources.sources[0].targets[0]);
            target.output.extend([
                TargetOutput::Xtream(XtreamTargetOutput {
                    flags: XtreamTargetFlagsSet::new(),
                    trakt: None,
                    filter: None,
                }),
                TargetOutput::Strm(tuliprox_core::model::StrmTargetOutput::from(&shared::model::StrmTargetOutputDto {
                    directory: temp.path().join("strm").to_string_lossy().into_owned(),
                    flat: true,
                    ..shared::model::StrmTargetOutputDto::default()
                })),
            ]);
            ctx.config.sources.store(Arc::new(sources));
            exec_processing(rescan_run(&ctx, UpdateGuard::new())).await;
            assert_run_state(&ctx, PlaylistUpdateState::Success);
            ctx.events.0.lock().unwrap().clear();
            tokio::fs::remove_file(temp.path().join("media/Library Movie (2025).mp4")).await.unwrap();
            exec_processing(rescan_run(&ctx, UpdateGuard::new())).await;
            assert_run_state(&ctx, PlaylistUpdateState::Success);
            let config = ctx.config.config.load();
            let xtream = xtream_get_storage_path(&config, "selected").unwrap();
            for (cluster, expected) in
                [(XtreamCluster::Live, 0), (XtreamCluster::Video, usize::from(companion)), (XtreamCluster::Series, 0)]
            {
                let path = xtream_get_file_path(&xtream, cluster);
                if !path.exists() {
                    assert_eq!(expected, 0);
                    continue;
                }
                let mut query = BPlusTreeQuery::<u32, shared::model::XtreamPlaylistItem>::try_new(&path).unwrap();
                assert_eq!(query.iter().count(), expected, "{cluster:?}");
            }
            let entries = strm_artifacts(&temp.path().join("strm")).await;
            assert_eq!(entries.len(), usize::from(companion), "{entries:?}");
            if companion {
                assert!(entries[0].1.contains("http://stream.example/companion"));
            }
        }
    }

    #[tokio::test]
    async fn manual_update_library_empty_permission_does_not_loosen_other_provider_inputs() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = library_context(temp.path(), ProcessingOrder::Frm).await;
        for input_type in [InputType::M3u, InputType::Xtream, InputType::Stalker, InputType::Plex] {
            let input =
                ConfigInput { input_type, name: format!("empty-{input_type:?}").intern(), ..ConfigInput::default() };
            let (_, error) = tuliprox_repository::persist_input_playlist(&ctx.config, &input, Vec::new()).await;
            assert!(error.is_some(), "{input_type:?} must retain its generic empty-input guard");
        }
        exec_processing(rescan_run(&ctx, UpdateGuard::new())).await;
        let before = tokio::fs::read(temp.path().join("selected.m3u")).await.unwrap();
        ctx.events.0.lock().unwrap().clear();
        tokio::fs::write(temp.path().join("companion.m3u"), "#EXTM3U\n").await.unwrap();
        exec_processing(rescan_run(&ctx, UpdateGuard::new())).await;
        {
            let events = ctx.events.0.lock().unwrap();
            assert!(events.iter().any(|event| matches!(event, EventMessage::PlaylistUpdate(summary) if summary.state == PlaylistUpdateState::Failure)));
            assert!(events.iter().any(|event| matches!(event, EventMessage::PlaylistUpdateProgress(progress) if progress.input_id == Some(3) && progress.state == Some(PlaylistUpdateState::Failure))));
        }
        assert_eq!(before, tokio::fs::read(temp.path().join("selected.m3u")).await.unwrap());
        // An authoritatively empty Library must not mask a failed companion either.
        ctx.events.0.lock().unwrap().clear();
        tokio::fs::remove_file(temp.path().join("media/Library Movie (2025).mp4")).await.unwrap();
        exec_processing(rescan_run(&ctx, UpdateGuard::new())).await;
        {
            let events = ctx.events.0.lock().unwrap();
            assert!(events.iter().any(|event| matches!(event, EventMessage::PlaylistUpdate(summary) if summary.state == PlaylistUpdateState::Failure)));
            assert!(events.iter().any(|event| matches!(event, EventMessage::PlaylistUpdateProgress(progress) if progress.input_id == Some(3) && progress.state == Some(PlaylistUpdateState::Failure))));
        }
        assert_eq!(before, tokio::fs::read(temp.path().join("selected.m3u")).await.unwrap());
    }

    #[derive(Clone, Copy, Debug)]
    enum LibraryFilterBoundary {
        Processing,
        Persist,
        M3u,
        Strm,
    }

    async fn strm_artifacts(root: &Path) -> Vec<(PathBuf, String)> {
        let mut directories = vec![root.to_path_buf()];
        let mut artifacts = Vec::new();
        while let Some(directory) = directories.pop() {
            let mut entries = tokio::fs::read_dir(directory).await.unwrap();
            while let Some(entry) = entries.next_entry().await.unwrap() {
                if entry.file_type().await.unwrap().is_dir() {
                    directories.push(entry.path());
                } else if entry.path().extension().is_some_and(|extension| extension == "strm") {
                    artifacts.push((entry.path(), tokio::fs::read_to_string(entry.path()).await.unwrap()));
                }
            }
        }
        artifacts.sort();
        artifacts
    }

    async fn assert_library_filtered_empty_publication(boundary: LibraryFilterBoundary) {
        for disk in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let ctx = library_context(temp.path(), ProcessingOrder::Frm).await;
            let mut config = (**ctx.config.config.load()).clone();
            config.disk_based_processing = disk;
            ctx.config.config.store(Arc::new(config));
            tokio::fs::write(
                temp.path().join("companion.m3u"),
                "#EXTM3U\n#EXTINF:-1 type=\"vod\",Companion\nhttp://stream.example/companion.mp4\n",
            )
            .await
            .unwrap();
            let mut sources = (**ctx.config.sources.load()).clone();
            let target = Arc::make_mut(&mut sources.sources[0].targets[0]);
            target.rename = None;
            target.mapping = Arc::default();
            let library_only = get_filter(r#"Name ~ "^Library Movie.*""#, None).unwrap();
            if matches!(boundary, LibraryFilterBoundary::Processing) {
                target.filter = library_only.clone().into();
            }
            if matches!(boundary, LibraryFilterBoundary::Persist) {
                target.filter.persist = Some(library_only.clone());
            }
            if let TargetOutput::M3u(output) = &mut target.output[0] {
                output.filter = matches!(boundary, LibraryFilterBoundary::M3u).then(|| library_only.clone());
            }
            let strm_root = temp.path().join("strm");
            let mut strm = tuliprox_core::model::StrmTargetOutput::from(&shared::model::StrmTargetOutputDto {
                directory: strm_root.to_string_lossy().into_owned(),
                flat: true,
                cleanup: true,
                ..shared::model::StrmTargetOutputDto::default()
            });
            strm.filter = matches!(boundary, LibraryFilterBoundary::Strm).then_some(library_only);
            target.output.push(TargetOutput::Strm(strm));
            ctx.config.sources.store(Arc::new(sources));

            exec_processing(rescan_run(&ctx, UpdateGuard::new())).await;
            assert_run_state(&ctx, PlaylistUpdateState::Success);
            let m3u_path = temp.path().join("selected.m3u");
            let before_m3u = tokio::fs::read_to_string(&m3u_path).await.unwrap();
            let before_strm = strm_artifacts(&strm_root).await;
            let m3u_keeps_companion = matches!(boundary, LibraryFilterBoundary::Strm);
            let strm_keeps_companion = matches!(boundary, LibraryFilterBoundary::M3u);
            assert_eq!(before_m3u.matches("#EXTINF").count(), 1 + usize::from(m3u_keeps_companion));
            assert_eq!(before_strm.len(), 1 + usize::from(strm_keeps_companion));
            assert_eq!(before_m3u.contains("http://stream.example/companion"), m3u_keeps_companion);
            assert_eq!(
                before_strm.iter().any(|(_, body)| body.contains("http://stream.example/companion")),
                strm_keeps_companion
            );

            ctx.events.0.lock().unwrap().clear();
            tokio::fs::remove_file(temp.path().join("media/Library Movie (2025).mp4")).await.unwrap();
            exec_processing(rescan_run(&ctx, UpdateGuard::new())).await;
            let after_m3u = tokio::fs::read_to_string(&m3u_path).await.unwrap();
            let after_strm = strm_artifacts(&strm_root).await;
            println!("Library filter {boundary:?}, disk={disk}: M3U before={before_m3u:?}, after={after_m3u:?}; STRM before={before_strm:?}, after={after_strm:?}");
            assert_run_state(&ctx, PlaylistUpdateState::Success);
            assert_eq!(
                scan_results(&ctx),
                vec![LibraryScanResult {
                    files_scanned: 0,
                    groups_scanned: 0,
                    files_added: 0,
                    files_updated: 0,
                    files_removed: 1,
                    errors: 0
                }]
            );
            assert_ne!(before_m3u, after_m3u, "old Library entries must really be replaced");
            assert_eq!(after_m3u.matches("#EXTINF").count(), usize::from(m3u_keeps_companion));
            assert_eq!(after_m3u.contains("http://stream.example/companion"), m3u_keeps_companion);
            assert_eq!(after_strm.len(), usize::from(strm_keeps_companion));
            for (path, contents) in &before_strm {
                if contents.contains("http://stream.example/companion") {
                    assert!(
                        after_strm.contains(&(path.clone(), contents.clone())),
                        "valid companion must be retained byte-for-byte"
                    );
                } else {
                    assert!(!path.exists(), "cleanup must remove old Library artifact {path:?}");
                }
            }
            // The existing M3U database is refreshed too, not only the client text file.
            let target_path =
                tuliprox_repository::get_target_storage_path(&ctx.config.config.load(), "selected").unwrap();
            let mut db = tuliprox_repository::BPlusTreeQuery::<u32, shared::model::M3uPlaylistItem>::try_new(
                &tuliprox_repository::m3u_get_file_path_for_db(&target_path),
            )
            .unwrap();
            assert_eq!(db.iter().count(), usize::from(m3u_keeps_companion));
            assert!(!temp.path().join("unselected.m3u").exists());
        }
    }

    #[tokio::test]
    async fn manual_update_library_processing_filter_replaces_old_output_after_last_movie_removed() {
        assert_library_filtered_empty_publication(LibraryFilterBoundary::Processing).await;
    }

    #[tokio::test]
    async fn manual_update_library_persist_filter_replaces_old_output_after_last_movie_removed() {
        assert_library_filtered_empty_publication(LibraryFilterBoundary::Persist).await;
    }

    #[tokio::test]
    async fn manual_update_library_m3u_output_filter_replaces_old_output_and_preserves_unfiltered_strm() {
        assert_library_filtered_empty_publication(LibraryFilterBoundary::M3u).await;
    }

    #[tokio::test]
    async fn manual_update_library_strm_output_filter_cleans_old_files_and_preserves_unfiltered_m3u() {
        assert_library_filtered_empty_publication(LibraryFilterBoundary::Strm).await;
    }

    #[tokio::test]
    async fn manual_update_library_empty_does_not_authorize_foreign_forced_empty_target() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = library_context(temp.path(), ProcessingOrder::Frm).await;
        let sources = ctx.config.sources.load_full();
        let mut playlists = sources
            .inputs
            .iter()
            .map(|input| FetchedPlaylist { input, source: MemoryPlaylistSource::default().into_source(), epg: None })
            .collect::<Vec<_>>();
        let prepared = prepare_playlist_for_target(
            &ctx,
            &mut playlists,
            &sources.sources[0].targets[0],
            &mut HashMap::new(),
            &mut Vec::new(),
            ClusterFlags::Vod,
            false,
        )
        .await
        .unwrap();
        assert_eq!(prepared.library_empty, tuliprox_repository::LibraryEmptyPublication::Contribution);
        let (result, _) = finalize_prepared_target(Arc::new(ctx), prepared).await;
        assert!(result.is_err(), "Library must not relax the existing non-Xtream force-empty output guard");
        assert!(!temp.path().join("selected.m3u").exists());
    }

    #[tokio::test]
    async fn manual_update_library_incomplete_discovery_has_no_invented_scan_result_or_rebuild() {
        for failure in ["missing", "not-directory", "partial", "no-directories", "corrupt-catalog"] {
            let temp = tempfile::tempdir().unwrap();
            let ctx = library_context(temp.path(), ProcessingOrder::Frm).await;
            exec_processing(rescan_run(&ctx, UpdateGuard::new())).await;
            let before = tokio::fs::read(temp.path().join("selected.m3u")).await.unwrap();
            let status_path = input_cache::resolve_input_storage_path(&temp.path().to_string_lossy(), "local").await;
            let cache_before = input_cache::load_input_status(&status_path).clusters;
            ctx.events.0.lock().unwrap().clear();
            let mut config = (**ctx.config.config.load()).clone();
            let library = config.library.as_mut().unwrap();
            match failure {
                "missing" => {
                    library.scan_directories[0].path = temp.path().join("missing").to_string_lossy().into_owned();
                }
                "not-directory" => {
                    library.scan_directories[0].path =
                        temp.path().join("media/Library Movie (2025).mp4").to_string_lossy().into_owned();
                }
                "partial" => {
                    let mut missing = library.scan_directories[0].clone();
                    missing.path = temp.path().join("missing").to_string_lossy().into_owned();
                    library.scan_directories.push(missing);
                }
                "no-directories" => library.scan_directories.clear(),
                "corrupt-catalog" => {
                    let path = tuliprox_library::library::resolve_metadata_storage_path(
                        config.metadata_update.as_ref(),
                        &config.storage_dir,
                    )
                    .join("library/broken.json");
                    tokio::fs::write(path, "invalid json").await.unwrap();
                }
                _ => unreachable!(),
            }
            ctx.config.config.store(Arc::new(config));
            exec_processing(rescan_run(&ctx, UpdateGuard::new())).await;
            assert_run_state(&ctx, PlaylistUpdateState::Failure);
            assert!(scan_results(&ctx).is_empty(), "{failure}");
            let status = input_cache::load_input_status(&status_path);
            assert_eq!(status.last_input_update.unwrap().state, PlaylistUpdateState::Failure, "{failure}");
            assert_eq!(status.clusters, cache_before, "failed rescan must not mutate cache validity");
            assert_eq!(before, tokio::fs::read(temp.path().join("selected.m3u")).await.unwrap());
        }
    }

    #[tokio::test]
    async fn manual_update_library_selected_target_keeps_companion_and_real_processing_order() {
        for (order, expected) in [(ProcessingOrder::Frm, "MAPPED"), (ProcessingOrder::Fmr, "RENAMED")] {
            let temp = tempfile::tempdir().unwrap();
            let ctx = library_context(temp.path(), order).await;
            exec_processing(rescan_run(&ctx, UpdateGuard::new())).await;
            let output = tokio::fs::read_to_string(temp.path().join("selected.m3u")).await.unwrap();
            assert_eq!(output.matches("#EXTINF").count(), 2, "{output}");
            assert!(output.contains("http://stream.example/companion"), "{output}");
            assert!(output.contains(expected), "{output}");
            assert!(!temp.path().join("unselected.m3u").exists());
            let events = ctx.events.0.lock().unwrap();
            let scan = events
                .iter()
                .position(|event| {
                    matches!(event, EventMessage::PlaylistUpdateProgress(progress)
                if progress.input_id == Some(2) && progress.message.starts_with("Library rescan completed"))
                })
                .unwrap();
            let acquired = events
                .iter()
                .position(|event| {
                    matches!(event, EventMessage::PlaylistUpdateProgress(progress)
                if progress.input_id == Some(2) && progress.state == Some(PlaylistUpdateState::Success))
                })
                .unwrap();
            let published = events
                .iter()
                .position(|event| {
                    matches!(event, EventMessage::PlaylistUpdate(summary)
                if summary.state == PlaylistUpdateState::Success)
                })
                .unwrap();
            assert!(scan < acquired && acquired < published);
            assert!(events.iter().all(|event| !matches!(event, EventMessage::PlaylistUpdateProgress(progress)
                if progress.input_id == Some(2) && progress.input_telemetry.as_ref().is_some_and(|telemetry|
                    !telemetry.clusters.is_empty() || telemetry.refresh_policy != InputRefreshPolicy::NORMAL))));
        }
    }

    #[tokio::test]
    async fn manual_update_library_unreadable_catalog_and_busy_scan_do_not_publish() {
        for busy in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let ctx = library_context(temp.path(), ProcessingOrder::Frm).await;
            let guard = UpdateGuard::new();
            let _busy = busy.then(|| guard.try_library().unwrap());
            if !busy {
                let mut config = (**ctx.config.config.load()).clone();
                config.library.as_mut().unwrap().scan_directories[0].path =
                    temp.path().join("missing").to_string_lossy().into_owned();
                ctx.config.config.store(Arc::new(config));
            }
            exec_processing(rescan_run(&ctx, guard.clone())).await;
            assert!(!temp.path().join("selected.m3u").exists());
            assert!(!temp.path().join("unselected.m3u").exists());
            assert!(ctx
                .events
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, EventMessage::PlaylistUpdate(summary)
                if summary.state == PlaylistUpdateState::Failure)));
            assert!(guard.try_playlist().is_some());
        }
    }

    #[tokio::test]
    async fn manual_update_library_input_success_remains_separate_from_target_publish_failure() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = library_context(temp.path(), ProcessingOrder::Frm).await;
        tokio::fs::create_dir(temp.path().join("selected.m3u")).await.unwrap();
        exec_processing(rescan_run(&ctx, UpdateGuard::new())).await;
        let events = ctx.events.0.lock().unwrap();
        assert!(events.iter().any(|event| matches!(event, EventMessage::PlaylistUpdateProgress(progress)
            if progress.input_id == Some(2) && progress.state == Some(PlaylistUpdateState::Success))));
        assert!(events.iter().any(|event| matches!(event, EventMessage::PlaylistUpdate(summary)
            if summary.state == PlaylistUpdateState::Failure)));
    }

    #[tokio::test]
    async fn manual_update_library_rescan_reloads_valid_cache_but_bulk_keeps_catalog() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = library_context(temp.path(), ProcessingOrder::Frm).await;
        exec_processing(rescan_run(&ctx, UpdateGuard::new())).await;
        tokio::fs::write(temp.path().join("media/Second Movie (2025).mp4"), b"fixture").await.unwrap();
        // Ordinary bulk does not discover the new file and does not acquire a library permit.
        let guard = UpdateGuard::new();
        let standalone_scan = guard.try_library().unwrap();
        exec_processing(
            ProcessingRun::new(ctx.client.clone(), ctx.config.clone(), ctx.user_targets.clone(), ctx.events.clone())
                .with_update_guard(guard.clone()),
        )
        .await;
        let bulk = tokio::fs::read_to_string(temp.path().join("selected.m3u")).await.unwrap();
        assert_eq!(bulk.matches("#EXTINF").count(), 2);
        drop(standalone_scan);
        exec_processing(rescan_run(&ctx, guard)).await;
        let rescanned = tokio::fs::read_to_string(temp.path().join("selected.m3u")).await.unwrap();
        assert_eq!(rescanned.matches("#EXTINF").count(), 3, "{rescanned}");
    }
}
