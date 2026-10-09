use super::*;

#[test]
fn epg_priority_merge_backfills_duplicate_programme_metadata_without_overwriting() {
    let merged = merge_epg_channels_by_priority(vec![
        (
            0,
            vec![epg_channel(
                "demo.channel",
                Some("High"),
                None,
                vec![epg_programme("demo.channel", 10, 20, Some("High Title"), None)],
            )],
        ),
        (
            10,
            vec![epg_channel(
                "demo.channel",
                None,
                Some("http://fallback/icon.png"),
                vec![epg_programme("demo.channel", 10, 20, Some("Low Title"), Some("Recovered desc"))],
            )],
        ),
    ]);

    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].title.as_deref(), Some("High"));
    assert_eq!(merged[0].icon.as_deref(), Some("http://fallback/icon.png"));
    assert_eq!(merged[0].programmes.len(), 1);
    assert_eq!(merged[0].programmes[0].title.as_deref(), Some("High Title"));
    assert_eq!(merged[0].programmes[0].desc.as_deref(), Some("Recovered desc"));
}

#[test]
fn epg_priority_merge_normalizes_duplicates_within_single_source() {
    let merged = merge_epg_channels_by_priority(vec![(
        0,
        vec![epg_channel(
            "demo.channel",
            Some("Demo"),
            None,
            vec![
                epg_programme("demo.channel", 20, 30, Some("Later"), None),
                epg_programme("demo.channel", 10, 20, Some("First"), None),
                epg_programme("demo.channel", 10, 20, None, Some("Recovered desc")),
            ],
        )],
    )]);

    assert_eq!(merged.len(), 1);
    assert_eq!(
        merged[0].programmes.iter().map(|programme| (programme.start, programme.stop)).collect::<Vec<_>>(),
        vec![(10, 20), (20, 30)],
    );
    assert_eq!(merged[0].programmes[0].title.as_deref(), Some("First"));
    assert_eq!(merged[0].programmes[0].desc.as_deref(), Some("Recovered desc"));
}

#[test]
fn persisted_epg_source_read_uses_shared_file_lock() {
    run_async_test(async move {
        let dir = tempdir().expect("temp dir");
        let epg_path = dir.path().join("locked.xml");
        fs::write(
            &epg_path,
            r#"<tv>
  <channel id="demo.channel"><display-name>Demo</display-name></channel>
  <programme start="20260425000000 +0000" stop="20260425010000 +0000" channel="demo.channel">
    <title>Locked read</title>
  </programme>
</tv>"#,
        )
        .expect("write XMLTV fixture");

        let file_locks = Arc::new(FileLockManager::new());
        let guide =
            TVGuide::new(vec![xmltv_source(epg_path.clone(), 0, false)]).with_file_locks(Arc::clone(&file_locks));
        let mut id_cache = EpgIdCache::new(None);
        id_cache.insert_channel_epg_id("demo.channel");
        let write_guard = file_locks.write_lock(&epg_path).await;
        let filter = guide.filter_merged(&mut id_cache);
        tokio::pin!(filter);

        assert!(tokio::time::timeout(std::time::Duration::from_millis(25), filter.as_mut()).await.is_err());
        drop(write_guard);

        let merged = filter.await.expect("EPG parse after write lock release");
        assert_eq!(merged.children[0].programmes[0].title.as_deref(), Some("Locked read"));
    });
}

/// Builds an accumulator with N channels and drains it to disk. Asserts:
/// (a) the temp file exists while the guard is alive, (b) it has
/// non-trivial size, (c) the file is removed after Drop. Together these
/// prove the writer runs end-to-end without panicking.
#[test]
fn finish_into_disk_writes_a_real_temp_tree() {
    use super::super::EpgMergeAccumulator;

    let mut acc = EpgMergeAccumulator::new();
    for i in 0..250u32 {
        let id: Arc<str> = format!("channel-{i:04}").into();
        let ch = shared::model::EpgChannel {
            id: Arc::clone(&id),
            title: Some(format!("title {i}").into()),
            icon: None,
            programmes: vec![shared::model::EpgProgramme::new(i64::from(i), i64::from(i + 1), id)],
        };
        acc.upsert_channel(i16::try_from(i).unwrap_or(0), 0, false, ch);
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("epg-source.db");
    let src = acc.finish_into_disk(path.clone(), 5, 0).unwrap();
    assert!(path.exists(), "temp tree file must exist while guard is alive");
    let size = std::fs::metadata(&path).unwrap().len();
    assert!(size > 1024, "expected non-trivial size, got {size}");
    drop(src);
    assert!(!path.exists(), "Drop must remove the temp file");
}

/// Contract test: two temp trees written from two accumulators must
/// merge into the same `Epg` as the in-memory `finish_epg_with_icon_overrides`
/// would produce from the same inputs. Without this, every other
/// optimisation is built on sand.
#[test]
fn disk_path_matches_in_memory_finish() {
    use super::super::{merge_epg_trees, EpgMergeAccumulator};

    // Build one source by appending one programme per channel through the
    // accumulator's primary entry point. `add_channel_with_programmes` is
    // the same call the in-memory `merge_epg_channels_by_priority` uses,
    // so the in-memory reference and the disk path go through identical
    // APIs. The two sources use disjoint programme intervals
    // (source 0: 0..1, source 1: 100..101) so the merged channel should
    // retain both programmes end-to-end.
    fn build_acc(source: usize, channels: std::ops::Range<u32>) -> EpgMergeAccumulator {
        let mut acc = EpgMergeAccumulator::new();
        for i in channels {
            let id: Arc<str> = format!("ch-{i:04}").into();
            let priority = if source == 0 { 5 } else { 3 }; // source 1 wins
            let (start, stop) =
                if source == 0 { (i64::from(i), i64::from(i + 1)) } else { (100 + i64::from(i), 101 + i64::from(i)) };
            acc.add_channel_with_programmes(
                i16::try_from(priority).unwrap_or(0),
                source,
                false,
                shared::model::EpgChannel {
                    id: Arc::clone(&id),
                    title: Some(format!("title-{source}-{i}").into()),
                    icon: None,
                    programmes: vec![shared::model::EpgProgramme::new(start, stop, id)],
                },
            );
        }
        acc
    }

    // Reference: in-memory merge of the same two sources.
    let ref_acc_a = build_acc(0, 0..50);
    let ref_acc_b = build_acc(1, 0..50);
    // `EpgMergeAccumulator` is single-shot, so the reference merge has to
    // rebuild a fresh accumulator. The two halves contribute disjoint
    // programme intervals, so a single accumulator sees both.
    let mut ref_acc = EpgMergeAccumulator::new();
    for i in 0..50 {
        let id: Arc<str> = format!("ch-{i:04}").into();
        ref_acc.add_channel_with_programmes(
            5,
            0,
            false,
            shared::model::EpgChannel {
                id: Arc::clone(&id),
                title: Some(format!("title-0-{i}").into()),
                icon: None,
                programmes: vec![shared::model::EpgProgramme::new(i64::from(i), i64::from(i + 1), id)],
            },
        );
    }
    for i in 0..50 {
        let id: Arc<str> = format!("ch-{i:04}").into();
        ref_acc.add_channel_with_programmes(
            3,
            1,
            false,
            shared::model::EpgChannel {
                id: Arc::clone(&id),
                title: Some(format!("title-1-{i}").into()),
                icon: None,
                programmes: vec![shared::model::EpgProgramme::new(100 + i64::from(i), 101 + i64::from(i), id)],
            },
        );
    }
    let reference = ref_acc.finish_epg_with_icon_overrides().unwrap().0;
    let _ = (ref_acc_a, ref_acc_b); // keep the per-source builder API in the test

    // Disk path: two temp trees, then merge.
    let dir = tempfile::tempdir().unwrap();
    let src_a = build_acc(0, 0..50).finish_into_disk(dir.path().join("a.db"), 5, 0).unwrap();
    let src_b = build_acc(1, 0..50).finish_into_disk(dir.path().join("b.db"), 3, 1).unwrap();
    let merged = merge_epg_trees(vec![src_a, src_b]).unwrap().unwrap().0;

    assert_eq!(reference.children.len(), merged.children.len(), "channel counts must match");
    for (left, right) in reference.children.iter().zip(merged.children.iter()) {
        assert_eq!(left.id, right.id, "channel order must match");
        // Source 1 had priority 3 (lower = wins) so its title should win.
        assert_eq!(left.title, right.title, "priority winner's title must propagate");
        // Both sources contribute distinct, non-overlapping programmes.
        // The merged channel must keep both — the disk path uses
        // `add_channel_with_programmes` so nothing is dropped on the way in.
        assert_eq!(
            left.programmes.len(),
            2,
            "channel {} should retain both source programmes, got {}",
            left.id,
            left.programmes.len()
        );
        assert_eq!(right.programmes.len(), 2, "disk-merged channel {} lost programmes", right.id);
        for (lp, rp) in left.programmes.iter().zip(right.programmes.iter()) {
            assert_eq!(lp.start, rp.start, "programme start must match for channel {}", left.id);
            assert_eq!(lp.stop, rp.stop, "programme stop must match for channel {}", left.id);
        }
    }
}
