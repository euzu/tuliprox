//! Atomic JSON write helper with a narrowly scoped injection seam.
//!
//! Production callers use [`write_json_atomic`] which delegates to
//! [`RealAtomicWriteOps`]. Tests inject a custom [`AtomicWriteOps`] to exercise
//! failures at each stage (temp-write, file sync, rename, parent dir sync)
//! without modifying the calling code. Callers can opt into the file/parent
//! sync stages by overriding [`AtomicWriteOps::sync_file`] and
//! [`AtomicWriteOps::sync_parent`] on the provided implementation.
//!
//! Production behavior is unchanged when [`write_json_atomic`] is used: the
//! default implementations of `sync_file` and `sync_parent` are no-ops.

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
#[derive(Debug)]
pub struct AtomicWriteError {
    pub stage: AtomicWriteStage,
    pub source: std::io::Error,
}

impl AtomicWriteError {
    pub fn new(stage: AtomicWriteStage, source: std::io::Error) -> Self {
        Self { stage, source }
    }
}

impl std::fmt::Display for AtomicWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "atomic write failed at {:?}: {}", self.stage, self.source)
    }
}

impl std::error::Error for AtomicWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl From<AtomicWriteError> for std::io::Error {
    fn from(err: AtomicWriteError) -> Self {
        std::io::Error::new(err.source.kind(), err)
    }
}

/// Filesystem operations needed for an atomic write. Default impls of the
/// sync stages are no-ops so the production code path matches the previous
/// `fs::write` + `fs::rename` behavior exactly.
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

impl AtomicWriteOps for RealAtomicWriteOps {
    async fn write_temp(&self, tmp: &Path, content: &[u8]) -> Result<(), AtomicWriteError> {
        fs::write(tmp, content).await.map_err(|e| AtomicWriteError::new(AtomicWriteStage::WriteTemp, e))
    }

    async fn rename(&self, tmp: &Path, final_path: &Path) -> Result<(), AtomicWriteError> {
        fs::rename(tmp, final_path).await.map_err(|e| AtomicWriteError::new(AtomicWriteStage::Rename, e))
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

/// Atomic write with caller-supplied filesystem ops. Generic over the ops
/// type so the call site remains static dispatch.
pub async fn write_json_atomic_with_ops<O: AtomicWriteOps>(
    final_path: &Path,
    content: &[u8],
    ops: &O,
) -> Result<(), AtomicWriteError> {
    let tmp = tmp_path_for(final_path);
    ops.write_temp(&tmp, content).await?;
    ops.sync_file(&tmp).await?;
    ops.rename(&tmp, final_path).await?;
    if let Some(parent) = final_path.parent() {
        ops.sync_parent(parent).await?;
    }
    Ok(())
}

/// Atomic write using the production filesystem ops. Production callers should
/// use this entry point.
pub async fn write_json_atomic(final_path: &Path, content: &[u8]) -> std::io::Result<()> {
    write_json_atomic_with_ops(final_path, content, &RealAtomicWriteOps).await.map_err(std::io::Error::from)
}

/// Atomic write with a caller-supplied temp file path. Use this when
/// concurrent writers need disjoint temp names — the caller is
/// responsible for producing a unique path per call.
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
