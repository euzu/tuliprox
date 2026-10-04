use chrono::Utc;
use std::{
    io,
    path::{Path, PathBuf},
};

/// Retains the latest ten snapshots for each canonical source path.
pub const CONFIG_BACKUP_LIMIT: usize = 10;

/// Snapshot name prefix: file name plus a hash of the canonical path, so equal names in
/// different directories never share a retention bucket.
async fn backup_prefix(source: &Path) -> io::Result<String> {
    let canonical = tokio::fs::canonicalize(source).await?;
    let identity = blake3::hash(canonical.to_string_lossy().as_bytes());
    let filename = source.file_name().ok_or_else(|| io::Error::other("Backup source has no filename"))?;
    Ok(format!("{}-{}-", filename.to_string_lossy(), identity.to_hex()))
}

pub async fn backup_config_file(source: &Path, directory: &Path) -> io::Result<PathBuf> {
    let prefix = backup_prefix(source).await?;
    tokio::fs::create_dir_all(directory).await?;
    let snapshot =
        directory.join(format!("{prefix}{}-{:016x}", Utc::now().format("%Y%m%dT%H%M%S%9f"), fastrand::u64(..)));
    let mut input = tokio::fs::File::open(source).await?;
    let mut output = tokio::fs::OpenOptions::new().write(true).create_new(true).open(&snapshot).await?;
    let result = async {
        output.set_permissions(input.metadata().await?.permissions()).await?;
        tokio::io::copy(&mut input, &mut output).await?;
        output.sync_all().await?;
        Ok::<(), io::Error>(())
    }
    .await;
    if let Err(error) = result {
        drop(output);
        let _ = tokio::fs::remove_file(snapshot).await;
        return Err(error);
    }
    Ok(snapshot)
}

pub async fn prune_config_backups(source: &Path, directory: &Path) -> io::Result<()> {
    let prefix = backup_prefix(source).await?;
    let mut entries = tokio::fs::read_dir(directory).await?;
    let mut snapshots: Vec<PathBuf> = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_file() && entry.file_name().to_string_lossy().starts_with(&prefix) {
            snapshots.push(entry.path());
        }
    }
    snapshots.sort_unstable();
    let excess = snapshots.len().saturating_sub(CONFIG_BACKUP_LIMIT);
    for snapshot in snapshots.into_iter().take(excess) {
        remove_snapshot(&snapshot).await?;
    }
    Ok(())
}

/// Snapshots copy the source permissions; Windows refuses to delete read-only files.
async fn remove_snapshot(snapshot: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        let mut permissions = tokio::fs::metadata(snapshot).await?.permissions();
        if permissions.readonly() {
            permissions.set_readonly(false);
            tokio::fs::set_permissions(snapshot, permissions).await?;
        }
    }
    tokio::fs::remove_file(snapshot).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn retains_ten_per_source_and_preserves_unrelated_files() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source.yml");
        let other = temp.path().join("other.yml");
        let directory = temp.path().join("backups");
        tokio::fs::write(&source, "original").await?;
        tokio::fs::write(&other, "other").await?;
        backup_config_file(&other, &directory).await?;
        for index in 0..15 {
            tokio::fs::write(&source, index.to_string()).await?;
            backup_config_file(&source, &directory).await?;
            prune_config_backups(&source, &directory).await?;
        }
        tokio::fs::write(directory.join("unrelated"), "keep").await?;
        prune_config_backups(&source, &directory).await?;
        let count = std::fs::read_dir(&directory)?.count();
        assert_eq!(count, CONFIG_BACKUP_LIMIT + 2);
        Ok(())
    }
}
