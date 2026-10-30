use super::DatabaseHeader;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VerifiedIdentity {
    pub(crate) database_id: [u8; 16],
    pub(crate) generation: u64,
    pub(crate) next_page_id: u64,
}

impl VerifiedIdentity {
    #[cfg(test)]
    pub(crate) const fn new(database_id: [u8; 16], generation: u64, next_page_id: u64) -> Self {
        Self { database_id, generation, next_page_id }
    }

    pub(crate) const fn from_header(header: &DatabaseHeader) -> Self {
        Self { database_id: header.database_id, generation: header.generation, next_page_id: header.next_page_id }
    }
}

static VERIFIED_IDENTITIES: LazyLock<Mutex<HashMap<PathBuf, VerifiedIdentity>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn canonical_path(path: &Path) -> PathBuf {
    match std::fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(_) => path.to_path_buf(),
    }
}

pub(crate) fn is_verified_with_cache(
    cache: &Mutex<HashMap<PathBuf, VerifiedIdentity>>,
    path: &Path,
    identity: VerifiedIdentity,
) -> bool {
    let canonical = canonical_path(path);
    let Ok(guard) = cache.lock() else {
        return false;
    };
    guard.get(&canonical).copied() == Some(identity)
}

pub(crate) fn mark_verified_with_cache(
    cache: &Mutex<HashMap<PathBuf, VerifiedIdentity>>,
    path: &Path,
    identity: VerifiedIdentity,
) {
    let canonical = canonical_path(path);
    let Ok(mut guard) = cache.lock() else {
        return;
    };
    let _ = guard.insert(canonical, identity);
}

pub(crate) fn is_verified(path: &Path, identity: VerifiedIdentity) -> bool {
    is_verified_with_cache(&VERIFIED_IDENTITIES, path, identity)
}

pub(crate) fn mark_verified(path: &Path, identity: VerifiedIdentity) {
    mark_verified_with_cache(&VERIFIED_IDENTITIES, path, identity);
}
