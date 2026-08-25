use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Debug, Clone)]
pub struct UpdateGuard {
    playlist: Arc<Semaphore>,
    library: Arc<Semaphore>,
}

impl Default for UpdateGuard {
    fn default() -> Self { Self { playlist: Arc::new(Semaphore::new(1)), library: Arc::new(Semaphore::new(1)) } }
}

impl UpdateGuard {
    pub fn new() -> Self { Self::default() }

    pub fn try_playlist(&self) -> Option<UpdateGuardPermit> {
        self.playlist.clone().try_acquire_owned().ok().map(|permit| UpdateGuardPermit { _permit: permit })
    }

    pub async fn acquire_playlist_lock(&self) -> Option<UpdateGuardPermit> {
        self.playlist.clone().acquire_owned().await.ok().map(|permit| UpdateGuardPermit { _permit: permit })
    }

    pub fn try_library(&self) -> Option<UpdateGuardPermit> {
        self.library.clone().try_acquire_owned().ok().map(|permit| UpdateGuardPermit { _permit: permit })
    }
}

pub struct UpdateGuardPermit {
    _permit: OwnedSemaphorePermit,
}
