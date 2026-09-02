//! Format-neutral atomic write helper with an injection seam for tests.
//!
//! Production callers use [`write_file_atomic`], [`write_text_file_atomic`], or
//! [`write_json_atomic`], which delegate to [`RealAtomicWriteOps`]. Tests can inject a custom
//! [`AtomicWriteOps`] to exercise failures at each stage (temp-write, file sync, rename, parent dir sync)
//! without modifying the calling code.

use std::path::{Path, PathBuf};
use tokio::fs;

/// Discrete stages that can be exercised or simulated by tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomicWriteStage {
    WriteTemp,
    SyncFile,
    Rename,
    SyncParent,
}

/// Errors that wrap a caller-supplied [`std::io::Error`] with stage context.
#[derive(Debug, thiserror::Error)]
#[error("atomic write failed at {stage:?}: {source}")]
pub struct AtomicWriteError {
    pub stage: AtomicWriteStage,
    #[source]
    pub source: std::io::Error,
}

impl AtomicWriteError {
    pub fn new(stage: AtomicWriteStage, source: std::io::Error) -> Self { Self { stage, source } }
}

impl From<AtomicWriteError> for std::io::Error {
    fn from(err: AtomicWriteError) -> Self { std::io::Error::new(err.source.kind(), err) }
}

/// Filesystem operations needed for an atomic write. Default impls of the
/// sync stages are no-ops so the production code path matches the standard
/// `fs::write` + `fs::rename` behavior.
pub trait AtomicWriteOps: Send + Sync {
    fn write_temp(
        &self,
        tmp: &Path,
        content: &[u8],
    ) -> impl std::future::Future<Output = Result<(), AtomicWriteError>> + Send;
    fn sync_file(&self, _tmp: &Path) -> impl std::future::Future<Output = Result<(), AtomicWriteError>> + Send {
        async { Ok(()) }
    }
    fn rename(
        &self,
        tmp: &Path,
        final_path: &Path,
    ) -> impl std::future::Future<Output = Result<(), AtomicWriteError>> + Send;
    fn sync_parent(&self, _parent: &Path) -> impl std::future::Future<Output = Result<(), AtomicWriteError>> + Send {
        async { Ok(()) }
    }
}

/// Production implementation that delegates to `tokio::fs`.
pub struct RealAtomicWriteOps;

#[cfg(windows)]
fn replace_file_windows(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::{io, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};

    let mut source_wide: Vec<u16> = source.as_os_str().encode_wide().collect();
    source_wide.push(0);
    let mut target_wide: Vec<u16> = target.as_os_str().encode_wide().collect();
    target_wide.push(0);

    // SAFETY: both paths are NUL-terminated UTF-16 buffers that remain alive for
    // the call. `MOVEFILE_REPLACE_EXISTING` leaves the target in place on error.
    let ok = unsafe {
        MoveFileExW(source_wide.as_ptr(), target_wide.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl AtomicWriteOps for RealAtomicWriteOps {
    async fn write_temp(&self, tmp: &Path, content: &[u8]) -> Result<(), AtomicWriteError> {
        fs::write(tmp, content).await.map_err(|e| AtomicWriteError::new(AtomicWriteStage::WriteTemp, e))
    }

    async fn rename(&self, tmp: &Path, final_path: &Path) -> Result<(), AtomicWriteError> {
        match fs::rename(tmp, final_path).await {
            Ok(()) => Ok(()),
            Err(err) => {
                #[cfg(windows)]
                {
                    if replace_file_windows(tmp, final_path).is_ok() {
                        return Ok(());
                    }
                }
                Err(AtomicWriteError::new(AtomicWriteStage::Rename, err))
            }
        }
    }
}

/// Generates a unique temporary file path in the same parent directory as `final_path`.
pub fn create_unique_temp_path(final_path: &Path) -> PathBuf {
    let file_name = final_path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let temp_name = format!(".{file_name}.tmp-{}-{}", std::process::id(), fastrand::u64(..));
    match final_path.parent() {
        Some(parent) => parent.join(temp_name),
        None => PathBuf::from(temp_name),
    }
}

/// Build the temp file path used during the atomic write. Matches the
/// existing convention: `final.with_extension("json.tmp")` for a file ending
/// in `.json`, otherwise `final.with_extension("tmp")`.
pub fn tmp_path_for(final_path: &Path) -> PathBuf {
    match final_path.extension() {
        Some(ext) => {
            let mut s = ext.to_os_string();
            s.push(".tmp");
            final_path.with_extension(s)
        }
        None => final_path.with_extension("tmp"),
    }
}

/// Atomic write with caller-supplied filesystem ops and unique temp path.
pub async fn write_file_atomic_with_ops<O: AtomicWriteOps>(
    final_path: &Path,
    content: &[u8],
    ops: &O,
) -> Result<(), AtomicWriteError> {
    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent).await.map_err(|e| AtomicWriteError::new(AtomicWriteStage::WriteTemp, e))?;
    }
    let tmp = create_unique_temp_path(final_path);
    let result = async {
        ops.write_temp(&tmp, content).await?;
        ops.sync_file(&tmp).await?;
        ops.rename(&tmp, final_path).await?;
        if let Some(parent) = final_path.parent() {
            ops.sync_parent(parent).await?;
        }
        Ok(())
    }
    .await;

    if result.is_err() {
        let _ = fs::remove_file(&tmp).await;
    }
    result
}

/// Atomic write for bytes using production filesystem ops.
pub async fn write_file_atomic(final_path: &Path, content: &[u8]) -> std::io::Result<()> {
    write_file_atomic_with_ops(final_path, content, &RealAtomicWriteOps).await.map_err(std::io::Error::from)
}

/// Atomic write for UTF-8 text using production filesystem ops.
pub async fn write_text_file_atomic(final_path: &Path, content: &str) -> std::io::Result<()> {
    write_file_atomic(final_path, content.as_bytes()).await
}

/// Atomic write with caller-supplied filesystem ops. Generic over the ops
/// type so the call site remains static dispatch.
pub async fn write_json_atomic_with_ops<O: AtomicWriteOps>(
    final_path: &Path,
    content: &[u8],
    ops: &O,
) -> Result<(), AtomicWriteError> {
    let tmp = create_unique_temp_path(final_path);
    let result = async {
        ops.write_temp(&tmp, content).await?;
        ops.sync_file(&tmp).await?;
        ops.rename(&tmp, final_path).await?;
        if let Some(parent) = final_path.parent() {
            ops.sync_parent(parent).await?;
        }
        Ok(())
    }
    .await;

    if result.is_err() {
        let _ = fs::remove_file(&tmp).await;
    }
    result
}

/// Atomic write using the production filesystem ops. Production callers should
/// use this entry point.
pub async fn write_json_atomic(final_path: &Path, content: &[u8]) -> std::io::Result<()> {
    write_json_atomic_with_ops(final_path, content, &RealAtomicWriteOps).await.map_err(std::io::Error::from)
}

/// Atomic write with a caller-supplied temp file path.
pub async fn write_json_atomic_to_tmp(final_path: &Path, tmp: &Path, content: &[u8]) -> Result<(), AtomicWriteError> {
    write_json_atomic_with_ops_to_tmp(final_path, tmp, content, &RealAtomicWriteOps).await
}

/// Generic variant of [`write_json_atomic_to_tmp`] with caller-supplied
/// filesystem ops.
pub async fn write_json_atomic_with_ops_to_tmp<O: AtomicWriteOps>(
    final_path: &Path,
    tmp: &Path,
    content: &[u8],
    ops: &O,
) -> Result<(), AtomicWriteError> {
    ops.write_temp(tmp, content).await?;
    ops.sync_file(tmp).await?;
    ops.rename(tmp, final_path).await?;
    if let Some(parent) = final_path.parent() {
        ops.sync_parent(parent).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tempfile::TempDir;

    #[derive(Default)]
    struct CountingOps {
        write_temp_calls: AtomicUsize,
        sync_file_calls: AtomicUsize,
        rename_calls: AtomicUsize,
        sync_parent_calls: AtomicUsize,
        fail_at: Option<AtomicWriteStage>,
    }

    impl AtomicWriteOps for CountingOps {
        async fn write_temp(&self, tmp: &Path, content: &[u8]) -> Result<(), AtomicWriteError> {
            self.write_temp_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_at == Some(AtomicWriteStage::WriteTemp) {
                return Err(AtomicWriteError::new(
                    AtomicWriteStage::WriteTemp,
                    std::io::Error::other("write_temp injection"),
                ));
            }
            tokio::fs::write(tmp, content).await.map_err(|e| AtomicWriteError::new(AtomicWriteStage::WriteTemp, e))
        }

        fn sync_file(&self, _tmp: &Path) -> impl std::future::Future<Output = Result<(), AtomicWriteError>> + Send {
            self.sync_file_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_at == Some(AtomicWriteStage::SyncFile) {
                return std::future::ready(Err(AtomicWriteError::new(
                    AtomicWriteStage::SyncFile,
                    std::io::Error::other("sync_file injection"),
                )));
            }
            std::future::ready(Ok(()))
        }

        async fn rename(&self, tmp: &Path, final_path: &Path) -> Result<(), AtomicWriteError> {
            self.rename_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_at == Some(AtomicWriteStage::Rename) {
                return Err(AtomicWriteError::new(AtomicWriteStage::Rename, std::io::Error::other("rename injection")));
            }
            tokio::fs::rename(tmp, final_path).await.map_err(|e| AtomicWriteError::new(AtomicWriteStage::Rename, e))
        }

        fn sync_parent(
            &self,
            _parent: &Path,
        ) -> impl std::future::Future<Output = Result<(), AtomicWriteError>> + Send {
            self.sync_parent_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_at == Some(AtomicWriteStage::SyncParent) {
                return std::future::ready(Err(AtomicWriteError::new(
                    AtomicWriteStage::SyncParent,
                    std::io::Error::other("sync_parent injection"),
                )));
            }
            std::future::ready(Ok(()))
        }
    }

    fn make_ops(stage: Option<AtomicWriteStage>) -> Arc<CountingOps> {
        let ops = CountingOps { fail_at: stage, ..Default::default() };
        Arc::new(ops)
    }

    #[tokio::test]
    async fn write_json_atomic_round_trip_writes_expected_content() {
        let dir = TempDir::new().expect("tempdir");
        let final_path = dir.path().join("downloads_state.json");
        let content = br#"{"hello":"world"}"#;
        write_json_atomic(&final_path, content).await.expect("write");
        let read = tokio::fs::read(&final_path).await.expect("read");
        assert_eq!(read, content);
    }

    #[tokio::test]
    async fn write_file_atomic_round_trip_and_unique_temp() {
        let dir = TempDir::new().expect("tempdir");
        let final_path = dir.path().join("nested").join("test_file.yml");
        let content = b"version: 1\ntarget: family\n";
        write_file_atomic(&final_path, content).await.expect("write");
        let read = tokio::fs::read(&final_path).await.expect("read");
        assert_eq!(read, content);
    }

    #[tokio::test]
    async fn write_text_file_atomic_round_trip() {
        let dir = TempDir::new().expect("tempdir");
        let final_path = dir.path().join("test_text.txt");
        let text = "hello world text";
        write_text_file_atomic(&final_path, text).await.expect("write");
        let read = tokio::fs::read_to_string(&final_path).await.expect("read");
        assert_eq!(read, text);
    }

    #[tokio::test]
    async fn tmp_path_for_appends_json_tmp_to_json_extension() {
        let p = Path::new("/tmp/downloads_state.json");
        assert_eq!(tmp_path_for(p), Path::new("/tmp/downloads_state.json.tmp"));
    }

    #[tokio::test]
    async fn tmp_path_for_appends_tmp_when_no_extension() {
        let p = Path::new("/tmp/state");
        assert_eq!(tmp_path_for(p), Path::new("/tmp/state.tmp"));
    }

    #[tokio::test]
    async fn write_temp_failure_propagates_and_skips_rename() {
        let dir = TempDir::new().expect("tempdir");
        let final_path = dir.path().join("downloads_state.json");
        let ops = make_ops(Some(AtomicWriteStage::WriteTemp));
        let result = write_json_atomic_with_ops(&final_path, b"x", ops.as_ref()).await;
        let err = result.expect_err("write_temp should fail");
        assert_eq!(err.stage, AtomicWriteStage::WriteTemp);
        assert_eq!(ops.write_temp_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ops.rename_calls.load(Ordering::SeqCst), 0);
        assert_eq!(ops.sync_file_calls.load(Ordering::SeqCst), 0);
        assert_eq!(ops.sync_parent_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rename_failure_propagates_and_skips_parent_sync() {
        let dir = TempDir::new().expect("tempdir");
        let final_path = dir.path().join("downloads_state.json");
        let ops = make_ops(Some(AtomicWriteStage::Rename));
        let result = write_json_atomic_with_ops(&final_path, b"x", ops.as_ref()).await;
        let err = result.expect_err("rename should fail");
        assert_eq!(err.stage, AtomicWriteStage::Rename);
        assert_eq!(ops.write_temp_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ops.sync_file_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ops.rename_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ops.sync_parent_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn sync_file_failure_propagates_and_skips_rename() {
        let dir = TempDir::new().expect("tempdir");
        let final_path = dir.path().join("downloads_state.json");
        let ops = make_ops(Some(AtomicWriteStage::SyncFile));
        let result = write_json_atomic_with_ops(&final_path, b"x", ops.as_ref()).await;
        let err = result.expect_err("sync_file should fail");
        assert_eq!(err.stage, AtomicWriteStage::SyncFile);
        assert_eq!(ops.write_temp_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ops.sync_file_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ops.rename_calls.load(Ordering::SeqCst), 0);
        assert_eq!(ops.sync_parent_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn sync_parent_failure_propagates_after_rename() {
        let dir = TempDir::new().expect("tempdir");
        let final_path = dir.path().join("downloads_state.json");
        let ops = make_ops(Some(AtomicWriteStage::SyncParent));
        let result = write_json_atomic_with_ops(&final_path, b"x", ops.as_ref()).await;
        let err = result.expect_err("sync_parent should fail");
        assert_eq!(err.stage, AtomicWriteStage::SyncParent);
        assert_eq!(ops.write_temp_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ops.sync_file_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ops.rename_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ops.sync_parent_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn all_four_stages_run_in_order_on_success() {
        let dir = TempDir::new().expect("tempdir");
        let final_path = dir.path().join("downloads_state.json");
        let ops = make_ops(None);
        write_json_atomic_with_ops(&final_path, b"x", ops.as_ref()).await.expect("write");
        assert_eq!(ops.write_temp_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ops.sync_file_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ops.rename_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ops.sync_parent_calls.load(Ordering::SeqCst), 1);
    }
}
