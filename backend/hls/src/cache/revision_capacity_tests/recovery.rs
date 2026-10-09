use super::*;

#[tokio::test]
async fn restart_discards_unbound_revisions_and_preserves_legacy_cache() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let legacy = SegmentCacheKey::new(ProxySessionId("same-session".into()), 0, "ts");
    let cache = HlsSegmentCache::with_cache_path(directory.path());
    cache.write_bytes_and_commit(&legacy, b"valid-processed-media").await?;
    let revisions = crate::SegmentRevisionStore::default();
    let revision = revisions.create(legacy.proxy_session_id().clone(), 0, crate::SegmentRevisionKind::Raw)?;
    let (mut spool, pin) = cache.create_revision_spool(&revision.revision().key).await?;
    spool.write_all(b"partial-origin-body").await?;
    spool.flush().await?;
    assert_eq!(cache.delete_orphan_revision_files(SystemTime::now()).await?, 0);
    drop(spool);
    drop(pin);
    drop(cache);
    let restarted = HlsSegmentCache::with_cache_path(directory.path());
    assert_eq!(restarted.delete_orphan_revision_files(SystemTime::now()).await?, 1);
    assert_eq!(tokio::fs::read(restarted.object_path(&legacy)).await?, b"valid-processed-media");
    assert_eq!(restarted.capacity_usage(legacy.proxy_session_id()).await?.global_bytes, 21);
    Ok(())
}

#[tokio::test]
async fn killed_process_revisions_are_reconciled_after_restart() -> Result<(), Box<dyn std::error::Error>> {
    for phase in ["raw", "raw_eof", "processed_staged", "processed"] {
        let directory = tempfile::tempdir()?;
        let mut child = std::process::Command::new(std::env::current_exe()?)
            .args(["--ignored", "--exact", "cache::revision_capacity_tests::revision_crash_checkpoint"])
            .env("TULIPROX_REVISION_CRASH_ROOT", directory.path())
            .env("TULIPROX_REVISION_CRASH_PHASE", phase)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let checkpoint = directory.path().join("ready");
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !checkpoint.exists() && Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let reached = checkpoint.exists();
        let killed = child.kill();
        let _ = child.wait();
        killed?;
        assert!(reached, "child reached the durable crash checkpoint");
        let restarted = HlsSegmentCache::with_cache_path(directory.path().join("cache"));
        assert_eq!(
            restarted.delete_orphan_revision_files(SystemTime::now()).await?,
            usize::from(phase != "processed_staged")
        );
        assert_eq!(
            restarted.delete_temp_files_older_than(SystemTime::now()).await?,
            usize::from(phase == "processed_staged")
        );
        assert_eq!(restarted.delete_orphan_revision_files(SystemTime::now()).await?, 0);
        let legacy = SegmentCacheKey::new(ProxySessionId("same-session".into()), 0, "ts");
        assert_eq!(tokio::fs::read(restarted.object_path(&legacy)).await?, b"valid-processed-media");
        assert_eq!(restarted.capacity_usage(legacy.proxy_session_id()).await?.global_bytes, 21);
    }
    Ok(())
}
