//! Streams postprocessor output through admission before each filesystem write.
use super::{HlsPostProcessingDeadline, HlsSegmentCache, ProxySessionId, SegmentCacheKey, StagedCacheObject};
use std::{io, sync::Arc};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::timeout,
};

/// Peak ownership and still-live staging bytes for streamed repair output.
#[derive(Clone, Copy)]
pub(crate) struct RepairDiskBudget<'a> {
    pub(crate) cache: &'a HlsSegmentCache,
    pub(crate) reservation: &'a Arc<crate::HlsRevisionDiskReservation>,
    pub(crate) session: &'a ProxySessionId,
    pub(crate) retained_staging_bytes: u64,
}

async fn copy_reserved_output<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    file: &mut File,
    budget: RepairDiskBudget<'_>,
) -> io::Result<()> {
    let key = SegmentCacheKey::new(budget.session.clone(), 0, "ts");
    let mut size = 0u64;
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(u64::try_from(read).map_err(io::Error::other)?)
            .ok_or_else(|| io::Error::other("postprocessor output overflow"))?;
        budget
            .cache
            .ensure_write_budget_available(&key, budget.reservation, size.saturating_add(budget.retained_staging_bytes))
            .await?;
        file.write_all(&buffer[..read]).await?;
    }
    file.flush().await?;
    Ok(())
}

/// Runs an MPEG-TS writer with stdout backpressure so it cannot grow a disk
/// file past its peak reservation between two capacity checks.
pub(crate) async fn run_reserved_remux(
    args: &[&str],
    output: &StagedCacheObject,
    budget: RepairDiskBudget<'_>,
    deadline: &HlsPostProcessingDeadline,
) -> Result<(), String> {
    let remaining = deadline.remaining().ok_or_else(|| "timeout".to_string())?;
    timeout(remaining, async {
        let writer = output.track_writer(File::create(&output.path).await?);
        let mut writer = writer.lock().await;
        let file = writer.as_mut().ok_or_else(|| io::Error::other("postprocessor output already closed"))?;
        let mut command = Command::new("ffmpeg");
        let mut child = command
            .args(args)
            .arg("pipe:1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdout = child.stdout.take().ok_or_else(|| io::Error::other("postprocessor stdout unavailable"))?;
        copy_reserved_output(&mut stdout, file, budget).await?;
        writer.take();
        if !child.wait().await?.success() {
            return Err(io::Error::other("postprocessor failed"));
        }
        Ok::<(), io::Error>(())
    })
    .await
    .map_err(|_| "timeout".to_string())?
    .map_err(|error| format!("reserved_postprocess_error:{:?}", error.kind()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn postprocessor_output_extends_its_peak_before_writing_or_aborts_without_overshoot() -> io::Result<()> {
        for output_size in [100usize, 128] {
            let directory = tempfile::tempdir()?;
            let cache = HlsSegmentCache::with_cache_path(directory.path());
            cache.update_cache_limits(120, 120);
            let session = ProxySessionId("postprocess".into());
            let key = SegmentCacheKey::new(session.clone(), 0, "ts");
            let reservation = Arc::new(cache.reserve_revision_peak(&key, 24).await?);
            let raw = cache
                .stage_temp_with_budget(
                    &key,
                    &[0; 8][..],
                    tokio::time::Instant::now() + std::time::Duration::from_secs(2),
                    Some(reservation.clone()),
                )
                .await?;
            let path = raw.path.with_extension("ts.tmp.output");
            let mut output = cache.adopt_staged_file(path.clone(), 0)?.with_write_budget(Some(reservation.clone()))?;
            let mut file = File::create(&path).await?;
            let bytes = vec![7u8; output_size];
            let result = copy_reserved_output(
                &mut bytes.as_slice(),
                &mut file,
                RepairDiskBudget {
                    cache: &cache,
                    reservation: &reservation,
                    session: &session,
                    retained_staging_bytes: raw.size,
                },
            )
            .await;
            drop(file);
            output.size = tokio::fs::metadata(&path).await?.len();
            assert!(output.size + raw.size <= 120);
            if output_size == 100 {
                result?;
                assert_eq!(output.size, 100);
                assert!(cache.remaining_revision_bytes(&session) >= 108);
                cache.remove_staged(raw).await?;
                cache.commit_staged(&key, output).await?;
                assert_eq!(cache.capacity_usage(&session).await?.session_bytes, 100);
            } else {
                assert!(result.is_err());
                assert_eq!(output.size, 0);
                cache.remove_staged(raw).await?;
                cache.remove_staged(output).await?;
            }
            drop(reservation);
            assert_eq!(cache.remaining_revision_bytes(&session), 0);
        }
        Ok(())
    }
}
