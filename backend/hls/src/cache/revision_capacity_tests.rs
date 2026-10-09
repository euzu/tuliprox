use super::*;
use std::{io, path::Path, time::SystemTime};
use tokio::{io::AsyncWriteExt, time::Instant};

mod publication;
mod recovery;

#[tokio::test]
#[ignore = "subprocess checkpoint helper, invoked by the crash recovery test"]
async fn revision_crash_checkpoint() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::var("TULIPROX_REVISION_CRASH_ROOT")?;
    let phase = std::env::var("TULIPROX_REVISION_CRASH_PHASE")?;
    let cache = HlsSegmentCache::with_cache_path(Path::new(&root).join("cache"));
    let legacy = SegmentCacheKey::new(ProxySessionId("same-session".into()), 0, "ts");
    cache.write_bytes_and_commit(&legacy, b"valid-processed-media").await?;
    let revisions = crate::SegmentRevisionStore::default();
    let kind =
        if phase.starts_with("raw") { crate::SegmentRevisionKind::Raw } else { crate::SegmentRevisionKind::Processed };
    let revision = revisions.create(legacy.proxy_session_id().clone(), 0, kind)?;
    let mut _staged = None;
    let _pin = if phase.starts_with("raw") {
        let (mut spool, pin) = cache.create_revision_spool(&revision.revision().key).await?;
        spool.write_all(b"unbound-revision-checkpoint").await?;
        spool.flush().await?;
        if phase == "raw_eof" {
            revision.revision().complete(CachedSegmentMetadata {
                path: cache.object_path(&revision.revision().key),
                size: u64::try_from(b"unbound-revision-checkpoint".len())?,
            });
        }
        pin
    } else if phase == "processed_staged" {
        let pin = cache.pin_revision_file(&cache.object_path(&revision.revision().key))?;
        _staged = Some(
            cache
                .stage_temp_with_deadline(
                    &revision.revision().key,
                    b"repair-output".as_slice(),
                    Instant::now() + std::time::Duration::from_secs(5),
                )
                .await?,
        );
        pin
    } else {
        let pin = cache.pin_revision_file(&cache.object_path(&revision.revision().key))?;
        cache
            .publish_processed_revision(
                &revision.revision().key,
                &cache.object_path(&legacy),
                Instant::now() + std::time::Duration::from_secs(5),
            )
            .await?;
        pin
    };
    std::fs::write(Path::new(&root).join("ready"), b"ready")?;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}
