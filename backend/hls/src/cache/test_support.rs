use super::HlsSegmentCache;
use crate::sync_ext::MutexExt;
use std::{
    io,
    path::{Path, PathBuf},
};
use tokio::sync::oneshot;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum StartupWritePhase {
    AfterInventorySnapshot,
    BeforeNormalCommit,
    AfterNormalCommit,
    BeforeRevisionLink,
    BeforeCacheRename,
    AfterPublication,
}

pub(super) struct StartupWriteCheckpoint {
    phase: StartupWritePhase,
    entered: oneshot::Sender<()>,
    resume: oneshot::Receiver<()>,
}

impl StartupWriteCheckpoint {
    pub(super) async fn wait(self) -> io::Result<()> {
        let _ = self.entered.send(());
        self.resume.await.map_err(|_| io::Error::from_raw_os_error(28))
    }
}

impl HlsSegmentCache {
    pub(crate) fn pause_startup_write(&self, phase: StartupWritePhase) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, reached) = oneshot::channel();
        let (resume, released) = oneshot::channel();
        *self.startup_test_checkpoint.lock_unpoisoned() =
            Some(StartupWriteCheckpoint { phase, entered, resume: released });
        (reached, resume)
    }

    pub(super) fn take_startup_write_checkpoint(&self, phase: StartupWritePhase) -> Option<StartupWriteCheckpoint> {
        let mut hook = self.startup_test_checkpoint.lock_unpoisoned();
        if hook.as_ref().is_some_and(|hook| hook.phase == phase) {
            hook.take()
        } else {
            None
        }
    }

    pub(crate) async fn startup_write_checkpoint(&self, phase: StartupWritePhase) -> io::Result<()> {
        if let Some(hook) = self.take_startup_write_checkpoint(phase) {
            hook.wait().await?;
        }
        Ok(())
    }

    pub(crate) fn capacity_scan_count(&self) -> u64 {
        self.capacity_scan_count.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn fail_next_cache_deletes(&self, path: PathBuf, attempts: usize) {
        self.delete_test_failures.lock_unpoisoned().insert(path, attempts);
    }

    pub(super) fn check_cache_delete_test_failure(&self, path: &Path) -> io::Result<()> {
        let mut failures = self.delete_test_failures.lock_unpoisoned();
        if let Some(remaining) = failures.get_mut(path).filter(|remaining| **remaining > 0) {
            *remaining -= 1;
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "injected cache deletion failure"));
        }
        Ok(())
    }
}
