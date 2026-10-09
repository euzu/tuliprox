use super::xtream_get_file_path_for_name;
use crate::{
    playlist_backend::{ensure_storage_path, PlaylistBackend, Xtream},
    storage_const,
};
use shared::{
    error::{string_to_io_error, TuliproxError},
    model::XtreamCluster,
};
use std::{
    ffi::OsString,
    io::Error,
    path::{Path, PathBuf},
};
use tuliprox_core::model::Config;
use uuid::Uuid;

#[inline]
pub fn get_collection_path(path: &Path, collection: &str) -> PathBuf { path.join(format!("{collection}.json")) }

/// Returns the category-collection base name for an [`XtreamCluster`].
///
/// Centralizes the per-cluster `cat_live` / `cat_vod` / `cat_series` mapping so the
/// path-deriving call sites read a single property instead of re-matching the cluster.
#[inline]
pub const fn xtream_cluster_category_collection(cluster: XtreamCluster) -> &'static str {
    match cluster {
        XtreamCluster::Live => storage_const::COL_CAT_LIVE,
        XtreamCluster::Video => storage_const::COL_CAT_VOD,
        XtreamCluster::Series => storage_const::COL_CAT_SERIES,
    }
}

#[inline]
pub fn get_live_cat_collection_path(path: &Path) -> PathBuf { get_collection_path(path, storage_const::COL_CAT_LIVE) }

#[inline]
pub fn get_vod_cat_collection_path(path: &Path) -> PathBuf { get_collection_path(path, storage_const::COL_CAT_VOD) }

#[inline]
pub fn get_series_cat_collection_path(path: &Path) -> PathBuf {
    get_collection_path(path, storage_const::COL_CAT_SERIES)
}

pub(super) fn target_category_lock_path(category_path: &Path) -> PathBuf {
    category_path.with_extension("json.target-category.lock")
}

#[inline]
pub async fn ensure_xtream_storage_path(cfg: &Config, target_name: &str) -> Result<PathBuf, TuliproxError> {
    ensure_storage_path::<Xtream>(cfg, target_name).await
}

#[inline]
pub fn xtream_get_storage_path(cfg: &Config, target_name: &str) -> Option<PathBuf> {
    Xtream::storage_path(cfg, target_name)
}

pub fn xtream_get_file_path(storage_path: &Path, cluster: XtreamCluster) -> PathBuf {
    xtream_get_file_path_for_name(storage_path, &cluster.as_str().to_lowercase())
}

pub fn xtream_get_collection_path(cfg: &Config, target_name: &str, collection_name: &str) -> Result<PathBuf, Error> {
    if let Some(path) = xtream_get_storage_path(cfg, target_name) {
        let col_path = get_collection_path(&path, collection_name);
        if col_path.exists() {
            return Ok(col_path);
        }
    }
    Err(string_to_io_error(format!("Can't find collection: {target_name}/{collection_name}")))
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) struct XtreamRefreshPaths {
    pub(super) generation: Uuid,
    pub(super) published_database: PathBuf,
    pub(super) staging_database: PathBuf,
    pub(super) published_categories: PathBuf,
    pub(super) staging_categories: PathBuf,
}

impl XtreamRefreshPaths {
    pub(super) fn new(storage_path: &Path, cluster: XtreamCluster) -> Result<Self, TuliproxError> {
        Self::for_generation(storage_path, cluster, Uuid::new_v4())
    }

    pub(super) fn for_generation(
        storage_path: &Path,
        cluster: XtreamCluster,
        generation: Uuid,
    ) -> Result<Self, TuliproxError> {
        let published_database = xtream_get_file_path(storage_path, cluster);
        let published_categories = get_collection_path(storage_path, xtream_cluster_category_collection(cluster));
        let staging_database = refresh_staging_path(&published_database, generation)?;
        // The lock-domain check is repeated by `XtreamRefreshLease::new` with a stricter
        // aliasing scan; doing it here too would canonicalize the same paths twice.
        Ok(Self {
            generation,
            staging_database,
            staging_categories: refresh_staging_path(&published_categories, generation)?,
            published_database,
            published_categories,
        })
    }
}

pub(super) fn refresh_staging_path(path: &Path, generation: Uuid) -> Result<PathBuf, TuliproxError> {
    let stem = path
        .file_stem()
        .ok_or_else(|| TuliproxError::RepositoryXtream(format!("Refresh path has no file stem: {}", path.display())))?;
    let extension = path
        .extension()
        .ok_or_else(|| TuliproxError::RepositoryXtream(format!("Refresh path has no extension: {}", path.display())))?;
    let mut filename = OsString::from(stem);
    filename.push(".refresh-");
    filename.push(generation.simple().to_string());
    filename.push(".");
    filename.push(extension);
    Ok(path.with_file_name(filename))
}

#[cfg(windows)]
pub(super) fn encode_windows_path(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    let mut encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if encoded.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Windows path contains an embedded NUL: {}", path.display()),
        ));
    }
    encoded.push(0);
    Ok(encoded)
}
