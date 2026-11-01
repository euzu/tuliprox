//! `.tlpx` upload, staging, the capability review, approve, enable/disable and
//! uninstall (plan §10 install and approval flow). W6b lane M.
//!
//! The flow, with every filesystem step on a blocking thread:
//!
//! 1. **Upload** unpacks the package into `<data_dir>/staging/.upload-<n>`
//!    under hard limits (entry count, inflated bytes counted while copying,
//!    path and entry-type rules), reads the plugin id out of the manifest with a
//!    minimal struct and checks it against the id grammar **before** it names a
//!    path, renames the directory to `<data_dir>/staging/<id>`, then runs the
//!    full plan §4 validation on it (including compiling the wasm). Any failure
//!    removes the staging directory.
//! 2. **Approve** re-validates and re-hashes whatever it is about to pin, the
//!    staged package or the installed `pending_approval` one, compares hash and
//!    capability names with what the admin was shown, writes the pin, and moves a
//!    staged package over `<plugins dir>/<id>`.
//! 3. **Enable / disable** are config changes the `app` handler persists; this
//!    module only says whether enabling is allowed.
//! 4. **Uninstall** removes the pin and the directories, reloads, and optionally
//!    deletes the KV store once the registry has dropped its handle.
//! 5. **Update and rollback** (plan §18 *W8 seam freeze*, *Safe replacement*):
//!    approving a staged package over an installed one keeps the installed one as
//!    the single prior version under `<data_dir>/plugins-prev/<id>/`, and
//!    `rollback` swaps the two. Both run the activation check on the result and
//!    undo themselves when it fails. See [`FsSwap`] for the crash analysis.
//!
//! One [`InstallGuard`] (a mutex in [`PluginSystem`]) must be held across each
//! of these and across a settings save; the methods take it by reference so a
//! caller cannot forget.

use crate::{
    manifest::{package::check_package, Capability, Contribution, Manifest, RpcExport, RpcKind, Slot},
    registry::{
        approvals_path, capability_names, check_hooks, export_names, host_version_problem, read_approvals, sha256_hex,
        ApprovalPin, PluginState, MANIFEST_FILE, WASM_FILE,
    },
    runtime::PluginLimits,
    system::PluginSystem,
};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use shared::model::{
    is_valid_plugin_id, PluginApproveDto, PluginCapabilityDiffDto, PluginCapabilityReviewDto, PluginContributionDto,
    PluginRollbackDto, PluginRpcExportDto, PluginRpcKindDto, PluginSlotDto, PluginSummaryDto, PluginUpdateDto,
    PluginUploadErrorDto, PluginUploadReviewDto,
};
use std::{
    fs,
    io::{self, Cursor, Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use tuliprox_core::model::{PluginEntry, PluginsConfig};
use tuliprox_plugin_api::ABI_VERSION;
use zip::ZipArchive;

/// Entries a package may hold.
const MAX_ENTRIES: usize = 512;
/// Inflated bytes a package may hold in total.
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// Inflated bytes one entry may hold.
const MAX_ENTRY_BYTES: u64 = 32 * 1024 * 1024;
/// Uploaded packages wait here, inside the data dir, until approved.
const STAGING_DIR: &str = "staging";
/// Ids that would collide with a directory the host keeps in the data dir.
/// `metrics` would make `DELETE /api/v1/plugins/metrics` hit the static
/// `GET /api/v1/plugins/metrics` route (W6a) and never reach the uninstall.
const RESERVED_IDS: [&str; 3] = [STAGING_DIR, PRIOR_DIR, "metrics"];
/// The one retained prior version of each plugin lives in `<data_dir>/plugins-prev/<id>/`:
/// the package in [`PRIOR_PACKAGE`] and its [`PriorMeta`] in [`PRIOR_META`].
const PRIOR_DIR: &str = "plugins-prev";
const PRIOR_PACKAGE: &str = "package";
const PRIOR_META: &str = "prev.json";
/// The `unix_mode` file-type bits of a zip entry.
const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;

/// What `network` allows. The review must say it this way (plan §9).
const NETWORK_SENTENCE: &str = "Unrestricted network access: any public, private or loopback HTTP(S) destination.";

/// Proof that the install mutex is held. Obtained from
/// [`PluginSystem::install_lock`]; dropping it releases the mutex.
pub struct InstallGuard<'a> {
    _guard: tokio::sync::MutexGuard<'a, ()>,
}

/// A refused install step: the HTTP status and the body to answer with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRejection {
    pub status: StatusCode,
    pub body: PluginUploadErrorDto,
}

impl InstallRejection {
    fn new(status: StatusCode, error: impl Into<String>) -> Self {
        Self { status, body: PluginUploadErrorDto { error: error.into(), problems: Vec::new() } }
    }

    fn problems(status: StatusCode, error: impl Into<String>, problems: Vec<String>) -> Self {
        Self { status, body: PluginUploadErrorDto { error: error.into(), problems } }
    }

    fn internal(context: &str, err: &impl std::fmt::Display) -> Self {
        log::error!("plugin install: {context}: {err}");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{context}: {err}"))
    }

    fn invalid(problems: Vec<String>) -> Self {
        Self::problems(StatusCode::UNPROCESSABLE_ENTITY, "the package is not a valid plugin", problems)
    }
}

/// `true` for an id that may name a directory: the id grammar
/// (`^[a-z][a-z0-9-]{2,40}$`) and not a reserved name.
pub(crate) fn is_installable_id(id: &str) -> bool { is_valid_plugin_id(id) && !RESERVED_IDS.contains(&id) }

/// Text from a manifest for display: whitespace controls become spaces, every
/// other control and bidirectional-override character is dropped, and at most
/// `max_chars` characters are kept.
pub(crate) fn sanitize_text(text: &str, max_chars: usize) -> String {
    let cleaned: String = text
        .chars()
        .filter_map(|c| match c {
            '\n' | '\r' | '\t' => Some(' '),
            c if c.is_control() => None,
            '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => None,
            c => Some(c),
        })
        .collect();
    cleaned.trim().chars().take(max_chars).collect()
}

// ---------------------------------------------------------------------------
// Extraction
// ---------------------------------------------------------------------------

/// A directory removed on drop unless [`StagingDir::keep`] was called, so every
/// early return cleans up.
struct StagingDir {
    path: PathBuf,
    keep: bool,
}

impl StagingDir {
    fn new(path: PathBuf) -> Self { Self { path, keep: false } }

    fn keep(mut self) { self.keep = true; }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        if !self.keep {
            if let Err(err) = remove_path(&self.path) {
                log::warn!("cannot remove the staging directory {}: {err}", self.path.display());
            }
        }
    }
}

/// Remove a file, a symlink (never what it points at) or a directory tree. A
/// path that is not there is fine.
fn remove_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Whether `path` is a real directory (not a symlink to one).
fn is_real_dir(path: &Path) -> bool { fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()) }

/// The path inside the package an entry names, or why it is refused.
fn entry_path(raw: &str, enclosed: Option<PathBuf>) -> Result<PathBuf, String> {
    if raw.contains(['\\', '\0']) {
        return Err(format!("entry `{}` has a backslash or NUL in its name", sanitize_text(raw, 120)));
    }
    if raw.starts_with('/') || raw.split('/').any(|segment| segment == "..") {
        return Err(format!("entry `{}` is an absolute or `..` path", sanitize_text(raw, 120)));
    }
    let Some(enclosed) = enclosed else {
        return Err(format!("entry `{}` has an unsafe path", sanitize_text(raw, 120)));
    };
    let mut relative = PathBuf::new();
    for component in enclosed.components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            _ => return Err(format!("entry `{}` has an unsafe path", sanitize_text(raw, 120))),
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(format!("entry `{}` names nothing", sanitize_text(raw, 120)));
    }
    Ok(relative)
}

/// Unpack `.tlpx` bytes into the existing, empty directory `target`.
///
/// Refuses more than [`MAX_ENTRIES`] entries, an inflated total over
/// [`MAX_TOTAL_BYTES`] or an entry over [`MAX_ENTRY_BYTES`] (counted while
/// copying: the header sizes are only a hint a hostile archive can lie about),
/// unsafe names, symlinks and anything that is not a regular file or directory.
/// Files are created with `create_new`, so a name used twice fails.
fn extract(bytes: &[u8], target: &Path) -> Result<(), String> {
    let mut archive =
        ZipArchive::new(Cursor::new(bytes)).map_err(|err| format!("not a valid .tlpx (zip) package: {err}"))?;
    if archive.len() > MAX_ENTRIES {
        return Err(format!("the package has {} entries; at most {MAX_ENTRIES} are allowed", archive.len()));
    }
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|err| format!("cannot read entry {index}: {err}"))?;
        let raw = entry.name().to_string();
        let relative = entry_path(&raw, entry.enclosed_name())?;
        let kind = entry.unix_mode().map_or(0, |mode| mode & S_IFMT);
        if kind == S_IFLNK {
            return Err(format!("entry `{}` is a symlink", sanitize_text(&raw, 120)));
        }
        if kind != 0 && kind != S_IFREG && kind != S_IFDIR {
            return Err(format!("entry `{}` is not a regular file or directory", sanitize_text(&raw, 120)));
        }
        let destination = target.join(&relative);
        if entry.is_dir() {
            fs::create_dir_all(&destination).map_err(|err| format!("cannot create `{}`: {err}", relative.display()))?;
            continue;
        }
        if kind == S_IFDIR {
            return Err(format!("entry `{}` is a directory without a trailing slash", sanitize_text(&raw, 120)));
        }
        if entry.size() > MAX_ENTRY_BYTES {
            return Err(format!("entry `{}` is larger than {} MiB", sanitize_text(&raw, 120), MAX_ENTRY_BYTES >> 20));
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|err| format!("cannot create `{}`: {err}", relative.display()))?;
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
            .map_err(|err| format!("cannot create `{}`: {err}", relative.display()))?;
        let mut written = 0_u64;
        loop {
            let read =
                entry.read(&mut buffer).map_err(|err| format!("cannot unpack `{}`: {err}", relative.display()))?;
            if read == 0 {
                break;
            }
            written += read as u64;
            total += read as u64;
            if written > MAX_ENTRY_BYTES {
                return Err(format!(
                    "entry `{}` inflates past {} MiB",
                    sanitize_text(&raw, 120),
                    MAX_ENTRY_BYTES >> 20
                ));
            }
            if total > MAX_TOTAL_BYTES {
                return Err(format!("the package inflates past {} MiB", MAX_TOTAL_BYTES >> 20));
            }
            file.write_all(&buffer[..read]).map_err(|err| format!("cannot write `{}`: {err}", relative.display()))?;
        }
    }
    Ok(())
}

/// Just the id of a `manifest.yml`, so it can be checked before the rest of the
/// manifest or anything else is trusted.
#[derive(Deserialize)]
struct ManifestId {
    id: Option<String>,
}

/// The plugin id of `manifest_text`, if it is a usable one.
fn manifest_id(manifest_text: &str) -> Result<String, String> {
    let parsed: ManifestId = serde_saphyr::from_str(manifest_text)
        .map_err(|err| format!("{MANIFEST_FILE} is not valid YAML: {}", sanitize_text(&err.to_string(), 200)))?;
    let id = parsed.id.ok_or_else(|| format!("{MANIFEST_FILE} has no `id`"))?;
    if !is_valid_plugin_id(&id) {
        return Err(format!("id `{}` is invalid; it must match ^[a-z][a-z0-9-]{{2,40}}$", sanitize_text(&id, 64)));
    }
    if !is_installable_id(&id) {
        return Err(format!("id `{id}` is reserved"));
    }
    Ok(id)
}

/// A new, empty `.upload-<n>` directory inside `root`.
fn create_upload_dir(root: &Path) -> io::Result<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    fs::create_dir_all(root)?;
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos());
    let name = format!(".upload-{:x}-{nanos:x}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));
    let path = root.join(name);
    fs::create_dir(&path)?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// Moving packages
// ---------------------------------------------------------------------------

/// Copy a directory tree of regular files and directories.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let destination = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &destination)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &destination)?;
        } else {
            return Err(io::Error::other(format!("{} is not a regular file", entry.path().display())));
        }
    }
    Ok(())
}

/// Rename `from` to `to`; across filesystems (`EXDEV`) copy, then remove.
fn move_dir(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {
            if let Err(copy_err) = copy_tree(from, to) {
                let _ = fs::remove_dir_all(to);
                return Err(copy_err);
            }
            fs::remove_dir_all(from)
        }
        Err(err) => Err(err),
    }
}

/// A sibling of `<dir>/<id>` for the swap, hidden so a scan skips it.
fn swap_name(dir: &Path, role: &str, id: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    dir.join(format!(".{role}-{id}-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)))
}

/// Replace `<plugins_dir>/<id>` with the staged package: bring it next to the
/// target (a copy when staging is on another filesystem), rename the old
/// directory aside, rename the new one in, delete the old one. A failure puts
/// the old directory back.
fn install_staged(staged: &Path, plugins_dir: &Path, id: &str) -> io::Result<()> {
    fs::create_dir_all(plugins_dir)?;
    let target = plugins_dir.join(id);
    let incoming = swap_name(plugins_dir, "incoming", id);
    move_dir(staged, &incoming)?;
    let aside = if fs::symlink_metadata(&target).is_ok() {
        let aside = swap_name(plugins_dir, "old", id);
        if let Err(err) = fs::rename(&target, &aside) {
            let _ = fs::remove_dir_all(&incoming);
            return Err(err);
        }
        Some(aside)
    } else {
        None
    };
    if let Err(err) = fs::rename(&incoming, &target) {
        if let Some(aside) = &aside {
            let _ = fs::rename(aside, &target);
        }
        let _ = fs::remove_dir_all(&incoming);
        return Err(err);
    }
    if let Some(aside) = aside {
        if let Err(err) = remove_path(&aside) {
            log::warn!("cannot remove the replaced plugin directory {}: {err}", aside.display());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Replacing an installed package, keeping one prior version
// ---------------------------------------------------------------------------

/// The settings entry a retained package ran with, so a rollback can put the
/// settings that package understood back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PriorEntry {
    pub(crate) enabled: bool,
    pub(crate) priority: i32,
    pub(crate) settings: Map<String, Value>,
    /// The `settings_version` the settings were written for (W8 lane C).
    #[serde(default = "default_settings_version")]
    pub(crate) settings_version: u32,
}

const fn default_settings_version() -> u32 { shared::model::DEFAULT_PLUGIN_SETTINGS_VERSION }

impl From<&PluginEntry> for PriorEntry {
    fn from(entry: &PluginEntry) -> Self {
        Self {
            enabled: entry.enabled,
            priority: entry.priority,
            settings: entry.settings.clone(),
            settings_version: entry.settings_version,
        }
    }
}

/// `plugins-prev/<id>/prev.json`: what goes with the retained package.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PriorMeta {
    pub(crate) version: String,
    /// The approval the package ran under; `None` when it was never approved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pin: Option<ApprovalPin>,
    /// Its `plugins.plugins[]` entry; `None` when the operator had none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) entry: Option<PriorEntry>,
}

/// Write a file only its owner can read: the settings may hold secrets.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// A hidden sibling for a swap in `dir`. Hidden, so a scan skips it; the `~`
/// separators cannot occur in an id, so [`sweep_leftovers`] never matches
/// another plugin's names.
fn swap_path(dir: &Path, role: &str, id: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    dir.join(format!(".swap~{role}~{id}~{}~{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)))
}

/// Remove what a swap of `id` that was killed halfway left in `dirs`. Called
/// under the install mutex, so no swap of this plugin is in flight.
fn sweep_leftovers(dirs: &[&Path], id: &str) {
    let marker = format!("~{id}~");
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(".swap~") && name.contains(&marker) {
                if let Err(err) = remove_path(&entry.path()) {
                    log::warn!("cannot remove the swap leftover {}: {err}", entry.path().display());
                }
            }
        }
    }
}

/// Where the incoming package of an [`FsSwap`] comes from.
enum Incoming {
    /// An uploaded package in staging; moved in.
    Staged(PathBuf),
    /// The retained prior package; copied in, so it stays until the swap commits.
    Prior,
}

/// The filesystem half of replacing `<plugins dir>/<id>` while keeping the
/// package it replaces as the one prior version.
///
/// Order: [`FsSwap::prepare`] copies the current package into a hidden new
/// prior directory and renames it over `plugins-prev/<id>` (the old prior waits
/// in a hidden `bak` sibling); the caller swaps the pin; [`FsSwap::enter`]
/// renames the current directory aside and the incoming one in;
/// [`FsSwap::commit`] deletes the aside and the old prior, or [`FsSwap::undo`]
/// puts everything back. Nothing the old package or the old prior needs is
/// deleted before `commit`.
///
/// A kill is recoverable at every point (the hidden `.swap~…` leftovers of this
/// plugin are removed by the next swap): before `prepare` ends, the current
/// package and its pin are untouched and the prior is the old one or the new
/// copy (a copy of the current package, so a rollback to it is harmless); after
/// the pin and before `enter`, the pin is the new one and the directory the old
/// one, which reads as `pending_approval`, and approving the still staged
/// package, or a rollback (its prior is a copy of exactly that old package and
/// pin), fixes it; after `enter` the new package is live with its pin and the
/// prior is the old one, only `commit`'s cleanup is missing.
struct FsSwap {
    plugins_dir: PathBuf,
    id: String,
    target: PathBuf,
    prior: PathBuf,
    source: Incoming,
    incoming: Option<PathBuf>,
    aside: Option<PathBuf>,
    backup: Option<PathBuf>,
    retained: bool,
}

impl FsSwap {
    /// Retain the current package as the prior version (with `meta`) and, for a
    /// rollback, copy the prior package next to the target. The current package
    /// must exist.
    fn prepare(
        source: Incoming,
        plugins_dir: &Path,
        prior_root: &Path,
        id: &str,
        meta: &PriorMeta,
    ) -> io::Result<Self> {
        fs::create_dir_all(plugins_dir)?;
        fs::create_dir_all(prior_root)?;
        sweep_leftovers(&[plugins_dir, prior_root], id);
        let mut swap = Self {
            plugins_dir: plugins_dir.to_path_buf(),
            id: id.to_string(),
            target: plugins_dir.join(id),
            prior: prior_root.join(id),
            source,
            incoming: None,
            aside: None,
            backup: None,
            retained: false,
        };
        if let Err(err) = swap.retain(prior_root, meta) {
            swap.undo();
            return Err(err);
        }
        Ok(swap)
    }

    fn retain(&mut self, prior_root: &Path, meta: &PriorMeta) -> io::Result<()> {
        if !is_real_dir(&self.target) {
            return Err(io::Error::new(io::ErrorKind::NotFound, "the installed package is missing"));
        }
        if matches!(self.source, Incoming::Prior) {
            let incoming = swap_path(&self.plugins_dir, "in", &self.id);
            self.incoming = Some(incoming.clone());
            copy_tree(&self.prior.join(PRIOR_PACKAGE), &incoming)?;
        }
        let fresh = swap_path(prior_root, "new", &self.id);
        let built = (|| {
            fs::create_dir(&fresh)?;
            copy_tree(&self.target, &fresh.join(PRIOR_PACKAGE))?;
            let text = serde_json::to_vec_pretty(meta).map_err(io::Error::other)?;
            write_private(&fresh.join(PRIOR_META), &text)
        })();
        if let Err(err) = built {
            let _ = remove_path(&fresh);
            return Err(err);
        }
        if fs::symlink_metadata(&self.prior).is_ok() {
            let backup = swap_path(prior_root, "bak", &self.id);
            if let Err(err) = fs::rename(&self.prior, &backup) {
                let _ = remove_path(&fresh);
                return Err(err);
            }
            self.backup = Some(backup);
        }
        if let Err(err) = fs::rename(&fresh, &self.prior) {
            if let Some(backup) = self.backup.take() {
                let _ = fs::rename(backup, &self.prior);
            }
            let _ = remove_path(&fresh);
            return Err(err);
        }
        self.retained = true;
        Ok(())
    }

    /// Put the incoming package in place of the current one, which is kept
    /// aside. On an error the current package is back in place.
    fn enter(&mut self) -> io::Result<()> {
        let incoming = match (&self.source, &self.incoming) {
            (_, Some(incoming)) => incoming.clone(),
            (Incoming::Staged(staged), None) => {
                let incoming = swap_path(&self.plugins_dir, "in", &self.id);
                move_dir(staged, &incoming)?;
                self.incoming = Some(incoming.clone());
                incoming
            }
            (Incoming::Prior, None) => return Err(io::Error::other("the prior package was not prepared")),
        };
        let aside = swap_path(&self.plugins_dir, "old", &self.id);
        fs::rename(&self.target, &aside)?;
        if let Err(err) = fs::rename(&incoming, &self.target) {
            let _ = fs::rename(&aside, &self.target);
            return Err(err);
        }
        self.incoming = None;
        self.aside = Some(aside);
        Ok(())
    }

    /// The new package is active: delete what only an undo would have needed.
    fn commit(self) {
        for path in [self.aside, self.backup].into_iter().flatten() {
            if let Err(err) = remove_path(&path) {
                log::warn!("cannot remove {}: {err}", path.display());
            }
        }
    }

    /// Restore the directory and the prior exactly as they were. The incoming
    /// package, a failed update included, is discarded.
    fn undo(self) {
        if let Some(aside) = &self.aside {
            if let Err(err) = remove_path(&self.target).and_then(|()| fs::rename(aside, &self.target)) {
                log::error!("cannot restore the plugin directory {}: {err}", self.target.display());
            }
        }
        if let Some(incoming) = &self.incoming {
            let _ = remove_path(incoming);
        }
        if self.retained {
            let restored = remove_path(&self.prior).and_then(|()| match &self.backup {
                Some(backup) => fs::rename(backup, &self.prior),
                None => Ok(()),
            });
            if let Err(err) = restored {
                log::error!("cannot restore the retained package {}: {err}", self.prior.display());
            }
        }
    }
}

/// An update or rollback whose directory and pin are swapped but not yet
/// confirmed by the activation check.
struct Replacement {
    id: String,
    swap: FsSwap,
    previous_pin: Option<ApprovalPin>,
    /// The version that is restored if the activation check fails.
    kept_version: String,
    /// The version the swap activates.
    target_version: String,
    rollback: bool,
    /// Whether the new package has a wasm, so there is an `init` to check.
    has_wasm: bool,
}

/// The answer of a rollback: the version now live and the settings of the
/// restored package's entry, which the caller must persist (`config.yml`) and
/// reload with.
#[derive(Debug, Clone, PartialEq)]
pub struct RollbackOutcome {
    pub version: String,
    /// `None` when the operator has no entry or the prior had none.
    pub settings: Option<Map<String, Value>>,
    /// The `settings_version` of [`RollbackOutcome::settings`].
    pub settings_version: u32,
}

/// `config` with the settings (and their version) of `id`'s entry replaced;
/// unchanged when there is no entry.
fn with_settings(config: &PluginsConfig, id: &str, settings: &Map<String, Value>, version: u32) -> PluginsConfig {
    let mut next = config.clone();
    if let Some(entry) = next.plugins.iter_mut().find(|entry| entry.id == id) {
        entry.settings.clone_from(settings);
        entry.settings_version = version;
    }
    next
}

/// The capability change of a package asking for `wanted` (sorted names)
/// against the installed approval.
fn capability_diff(wanted: &[String], pin: Option<&ApprovalPin>) -> PluginCapabilityDiffDto {
    let approved: &[String] = pin.map_or(&[], |pin| pin.capabilities.as_slice());
    PluginCapabilityDiffDto {
        added: wanted.iter().filter(|name| !approved.contains(name)).cloned().collect(),
        removed: approved.iter().filter(|name| !wanted.contains(name)).cloned().collect(),
        unchanged: wanted.iter().filter(|name| approved.contains(name)).cloned().collect(),
        mcp_was_approved: pin.is_some_and(|pin| pin.mcp),
    }
}

// ---------------------------------------------------------------------------
// Validation and the review
// ---------------------------------------------------------------------------

/// What validating a package directory found.
struct Validated {
    manifest: Manifest,
    /// `None` for a UI-only package without `plugin.wasm`.
    wasm_sha256: Option<String>,
    secret_paths: Vec<String>,
    warnings: Vec<String>,
}

/// The lines of a manifest error (`invalid manifest.yml in `x`:\n  - a\n  - b`).
fn manifest_problems(message: &str) -> Vec<String> {
    let lines: Vec<String> = message
        .lines()
        .map(|line| line.trim().trim_start_matches("- ").trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        vec![message.to_string()]
    } else {
        lines
    }
}

impl PluginSystem {
    /// The full plan §4 validation of the package at `dir`, as plugin `id`:
    /// manifest, host version, schemas, locales and extension sections, and the
    /// wasm (compiled, linked, and its exports matched to the manifest hooks).
    /// CPU-bound: call it from a blocking thread. Reports every problem.
    fn validate_package(&self, dir: &Path, id: &str) -> Result<Validated, Vec<String>> {
        let text = fs::read_to_string(dir.join(MANIFEST_FILE))
            .map_err(|err| vec![format!("cannot read {MANIFEST_FILE}: {err}")])?;
        let manifest = Manifest::parse(&text, id).map_err(|err| manifest_problems(err.message()))?;
        if let Some(problem) = host_version_problem(&manifest) {
            return Err(vec![problem]);
        }
        let secret_paths = check_package(dir, &manifest)?;

        let mut warnings = Vec::new();
        let wasm = match fs::read(dir.join(WASM_FILE)) {
            Ok(wasm) => Some(wasm),
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => return Err(vec![format!("cannot read {WASM_FILE}: {err}")]),
        };
        let Some(wasm) = wasm else {
            if manifest.hooks.is_empty() {
                return Ok(Validated { manifest, wasm_sha256: None, secret_paths, warnings });
            }
            return Err(vec![format!("hooks are listed in {MANIFEST_FILE} but {WASM_FILE} is missing")]);
        };
        let wasm_sha256 = sha256_hex(&wasm);
        let runtime = self.runtime();
        let limits = PluginLimits::clamp(&manifest.resources, runtime.config());
        let (component, _prepared) =
            runtime.prepare_cached(&wasm_sha256, &wasm, limits).map_err(|err| vec![err.message().to_string()])?;
        let (exports, unknown) = export_names(runtime.engine(), &component);
        for name in unknown {
            warnings.push(format!("the component exports `{name}`, which this host does not know; it is ignored"));
        }
        check_hooks(&manifest.hooks, &exports).map_err(|problem| vec![problem])?;
        Ok(Validated { manifest, wasm_sha256: Some(wasm_sha256), secret_paths, warnings })
    }
}

const fn capability_description(capability: Capability) -> &'static str {
    match capability {
        Capability::Network => NETWORK_SENTENCE,
        Capability::Storage => {
            "Persistent storage: a private key-value store of up to 50 MiB, kept until the plugin is uninstalled with its data."
        }
        Capability::EpgWrite => "EPG write: may add or change programmes in the EPG.",
        Capability::ImageCache => {
            "Image cache: may fetch images through the host image cache and serve them from this server (needs network access)."
        }
        Capability::Notifications => "Notifications: may register as a notification provider and send messages through it.",
        Capability::StreamHeaders => {
            "Stream header injection: may add HTTP headers to upstream stream requests when a client session starts."
        }
    }
}

const fn slot_dto(slot: Slot) -> PluginSlotDto {
    match slot {
        Slot::Menu => PluginSlotDto::Menu,
        Slot::ConfigSection => PluginSlotDto::ConfigSection,
        Slot::ConfigTab => PluginSlotDto::ConfigTab,
        Slot::SourceEditorTile => PluginSlotDto::SourceEditorTile,
        Slot::DashboardCard => PluginSlotDto::DashboardCard,
        Slot::PlaylistExplorerAction => PluginSlotDto::PlaylistExplorerAction,
        Slot::ChannelDetailPanel => PluginSlotDto::ChannelDetailPanel,
        Slot::EpgOverlay => PluginSlotDto::EpgOverlay,
        Slot::SetupWizardStep => PluginSlotDto::SetupWizardStep,
    }
}

fn contribution_dto(contribution: &Contribution) -> PluginContributionDto {
    PluginContributionDto {
        slot: slot_dto(contribution.slot),
        title: sanitize_text(&contribution.title, 128),
        title_key: contribution.title_key.clone(),
        icon: contribution.icon.clone(),
        element: contribution.element.clone(),
    }
}

fn rpc_export_dto(export: &RpcExport) -> PluginRpcExportDto {
    PluginRpcExportDto {
        name: export.name.clone(),
        kind: match export.kind {
            RpcKind::Read => PluginRpcKindDto::Read,
            RpcKind::Write => PluginRpcKindDto::Write,
        },
        mcp: export.mcp,
        description: export.description.as_deref().map(|text| sanitize_text(text, 512)),
    }
}

/// The capability review (plan §4 step 7) of a validated package.
fn review(validated: &Validated, replaces: Option<String>) -> PluginUploadReviewDto {
    let manifest = &validated.manifest;
    let capabilities = manifest
        .capabilities
        .granted()
        .into_iter()
        .map(|capability| PluginCapabilityReviewDto {
            name: capability.as_str().to_string(),
            description: capability_description(capability).to_string(),
        })
        .collect();
    let mut warnings = validated.warnings.clone();
    if manifest.provides.input_type {
        warnings
            .push(format!("Registers an input type: sources can use `type: plugin` with `plugin: {}`.", manifest.id));
    }
    if let Some(format) = &manifest.provides.output_format {
        warnings.push(format!(
            "Registers the output format `{}`: targets can use `type: plugin` with `plugin: {}`.",
            sanitize_text(format, 64),
            manifest.id
        ));
    }
    if manifest.ui.contributions.iter().any(|contribution| contribution.slot == Slot::SetupWizardStep) {
        // Accepted (the slot list is frozen), but the setup wizard runs only before a
        // config exists, so no plugin is ever loaded while it shows.
        warnings.push(
            "Contributes a `setup-wizard-step`, which is never shown: the setup wizard runs before plugins load."
                .to_string(),
        );
    }
    let frame_tab = matches!(
        manifest.ui.config_tab.as_ref().map(|tab| &tab.source),
        Some(crate::manifest::ConfigTabSource::Element(_))
    );
    PluginUploadReviewDto {
        id: manifest.id.clone(),
        name: sanitize_text(&manifest.name, 128),
        version: manifest.version.to_string(),
        author: manifest.author.as_deref().map(|text| sanitize_text(text, 256)),
        homepage: manifest.homepage.as_deref().map(|text| sanitize_text(text, 512)),
        description: manifest.description.as_deref().map(|text| sanitize_text(text, 512)),
        abi: ABI_VERSION.to_string(),
        wasm_sha256: validated.wasm_sha256.clone(),
        capabilities,
        hooks: manifest.hooks.iter().map(|hook| hook.as_str().to_string()).collect(),
        events: manifest.event_names.clone(),
        tasks: manifest.tasks.iter().map(|task| task.id.clone()).collect(),
        secrets: validated.secret_paths.clone(),
        custom_ui: !manifest.ui.contributions.is_empty() || frame_tab,
        contributions: manifest.ui.contributions.iter().map(contribution_dto).collect(),
        rpc_exports: manifest.ui.rpc_exports.iter().map(rpc_export_dto).collect(),
        mcp_requested: manifest.ui.rpc_exports.iter().any(|export| export.mcp),
        replaces,
        capability_diff: None,
        warnings,
    }
}

// ---------------------------------------------------------------------------
// The management operations
// ---------------------------------------------------------------------------

impl PluginSystem {
    /// Take the install mutex. Hold the guard across one upload, approve,
    /// enable, disable, uninstall or settings save.
    pub async fn install_lock(&self) -> InstallGuard<'_> { InstallGuard { _guard: self.install_mutex().lock().await } }

    fn staging_root(&self) -> PathBuf { self.shared().data_dir.join(STAGING_DIR) }

    fn prior_root(&self) -> PathBuf { self.shared().data_dir.join(PRIOR_DIR) }

    fn prior_dir(&self, id: &str) -> PathBuf { self.prior_root().join(id) }

    /// `POST /api/v1/plugins`: unpack, validate and stage `package` (the bytes
    /// of a `.tlpx`) and return the capability review. Nothing is installed.
    ///
    /// # Errors
    /// `422` with every problem found; the staging directory is removed.
    /// `500` when the filesystem fails.
    pub async fn api_upload(
        &self,
        _guard: &InstallGuard<'_>,
        package: Vec<u8>,
    ) -> Result<PluginUploadReviewDto, InstallRejection> {
        let system = self.clone();
        tokio::task::spawn_blocking(move || system.upload_blocking(&package))
            .await
            .map_err(|err| InstallRejection::internal("the upload task failed", &err))?
    }

    /// The review of a validated package: `replaces` and the capability diff
    /// are set when `id` is installed.
    fn review_of(&self, validated: &Validated, id: &str) -> PluginUploadReviewDto {
        let replaces = self
            .registry()
            .snapshot()
            .get(id)
            .and_then(|record| record.manifest.as_ref().map(|manifest| manifest.version.to_string()));
        let mut answer = review(validated, replaces);
        if answer.replaces.is_some() {
            let pins = read_approvals(&approvals_path(&self.shared().data_dir));
            answer.capability_diff =
                Some(capability_diff(&capability_names(validated.manifest.capabilities), pins.get(id)));
        }
        answer
    }

    /// `GET /api/v1/plugins/{id}/staged`: the review of the package staged for
    /// `id`, validated again, so an update waiting in staging can be reviewed
    /// after the upload dialog is gone. Nothing changes.
    ///
    /// # Errors
    /// `404` when nothing is staged for `id`, `422` when it no longer validates.
    pub async fn api_staged_review(
        &self,
        _guard: &InstallGuard<'_>,
        id: &str,
    ) -> Result<PluginUploadReviewDto, InstallRejection> {
        let system = self.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let staged = system.staging_root().join(&id);
            if !is_installable_id(&id) || !is_real_dir(&staged) {
                return Err(InstallRejection::new(StatusCode::NOT_FOUND, "nothing is staged for this plugin"));
            }
            let validated = system.validate_package(&staged, &id).map_err(InstallRejection::invalid)?;
            Ok(system.review_of(&validated, &id))
        })
        .await
        .map_err(|err| InstallRejection::internal("the review task failed", &err))?
    }

    fn upload_blocking(&self, package: &[u8]) -> Result<PluginUploadReviewDto, InstallRejection> {
        let root = self.staging_root();
        let work = create_upload_dir(&root)
            .map_err(|err| InstallRejection::internal("cannot create the staging directory", &err))?;
        let mut staging = StagingDir::new(work);

        extract(package, &staging.path).map_err(|problem| InstallRejection::invalid(vec![problem]))?;

        let manifest_path = staging.path.join(MANIFEST_FILE);
        if !fs::symlink_metadata(&manifest_path).is_ok_and(|meta| meta.is_file()) {
            return Err(InstallRejection::invalid(vec![format!("{MANIFEST_FILE} is missing from the package root")]));
        }
        let text = fs::read_to_string(&manifest_path)
            .map_err(|err| InstallRejection::invalid(vec![format!("cannot read {MANIFEST_FILE}: {err}")]))?;
        let id = manifest_id(&text).map_err(|problem| InstallRejection::invalid(vec![problem]))?;

        // The id is a checked path segment from here on.
        let staged = root.join(&id);
        remove_path(&staged).map_err(|err| InstallRejection::internal("cannot replace the earlier upload", &err))?;
        fs::rename(&staging.path, &staged)
            .map_err(|err| InstallRejection::internal("cannot stage the package", &err))?;
        staging.path = staged;

        let validated = self.validate_package(&staging.path, &id).map_err(InstallRejection::invalid)?;
        let answer = self.review_of(&validated, &id);
        staging.keep();
        log::info!("plugin `{id}` {} staged for review", answer.version);
        Ok(answer)
    }

    /// `PUT /api/v1/plugins/{id}/approve`: pin what the admin reviewed and
    /// install it. The hash and the capability names must equal those of the
    /// staged package, or of the installed `pending_approval` one when nothing
    /// is staged. The files are re-validated and re-hashed here; the earlier
    /// review is never trusted. `config` is the plugins config the registry
    /// reloads with.
    ///
    /// # Errors
    /// `404` for an unknown id, `409` for a mismatch or a plugin that is not
    /// waiting for approval, `422` when the package no longer validates.
    pub async fn api_approve(
        &self,
        _guard: &InstallGuard<'_>,
        id: &str,
        body: &PluginApproveDto,
        config: &PluginsConfig,
    ) -> Result<PluginSummaryDto, InstallRejection> {
        let system = self.clone();
        let (id, body, config) = (id.to_string(), body.clone(), config.clone());
        // Must not be cancelled between the directory swap and its activation
        // check: the route runs this on a detached task that also holds `_guard`.
        async move {
            let blocking = system.clone();
            let (blocking_id, blocking_config) = (id.clone(), config.clone());
            let replacement =
                tokio::task::spawn_blocking(move || blocking.approve_blocking(&blocking_id, &body, &blocking_config))
                    .await
                    .map_err(|err| InstallRejection::internal("the approve task failed", &err))??;
            if let Some(replacement) = replacement {
                let check = system.check_replacement(&replacement, &config).await;
                system.finish_replacement(replacement, &config, check).await?;
            }
            system
                .api_summary(&id, Some(&config))
                .ok_or_else(|| InstallRejection::new(StatusCode::NOT_FOUND, "unknown plugin"))
        }
        .await
    }

    fn approve_blocking(
        &self,
        id: &str,
        body: &PluginApproveDto,
        config: &PluginsConfig,
    ) -> Result<Option<Replacement>, InstallRejection> {
        if !is_installable_id(id) {
            return Err(InstallRejection::new(StatusCode::NOT_FOUND, "unknown plugin"));
        }
        let registry = self.registry();
        let staged = self.staging_root().join(id);
        let is_staged = is_real_dir(&staged);
        let plugins_dir = registry.directory();
        let dir = if is_staged {
            staged.clone()
        } else {
            let snapshot = registry.snapshot();
            let Some(record) = snapshot.get(id) else {
                return Err(InstallRejection::new(StatusCode::NOT_FOUND, "unknown plugin"));
            };
            if record.state() != PluginState::PendingApproval {
                return Err(InstallRejection::new(
                    StatusCode::CONFLICT,
                    format!("plugin `{id}` is not waiting for approval ({})", record.state().as_str()),
                ));
            }
            record.dir.clone()
        };

        let validated = self.validate_package(&dir, id).map_err(InstallRejection::invalid)?;
        if body.wasm_sha256 != validated.wasm_sha256 {
            return Err(InstallRejection::new(
                StatusCode::CONFLICT,
                "the package differs from the one that was reviewed (wasm hash mismatch); review it again",
            ));
        }
        let wanted = capability_names(validated.manifest.capabilities);
        let mut approved = body.capabilities.clone();
        approved.sort_unstable();
        approved.dedup();
        if approved != wanted {
            return Err(InstallRejection::new(
                StatusCode::CONFLICT,
                "the capabilities differ from the ones that were reviewed; review the package again",
            ));
        }
        let pin = ApprovalPin {
            wasm_sha256: validated.wasm_sha256.clone().unwrap_or_default(),
            capabilities: wanted,
            mcp: body.mcp && validated.manifest.ui.rpc_exports.iter().any(|export| export.mcp),
        };

        if is_staged && is_real_dir(&plugins_dir.join(id)) {
            // An update: keep the installed package as the prior version.
            let target_version = validated.manifest.version.to_string();
            let replacement = self.begin_replacement(
                id,
                Incoming::Staged(staged),
                pin.clone(),
                (validated.wasm_sha256.is_some(), target_version, false),
                config,
            )?;
            log::info!(
                "plugin `{id}` updated to {} with capabilities: {}",
                replacement.target_version,
                pin.capabilities.join(", ")
            );
            self.reload(config);
            return Ok(Some(replacement));
        }

        // The pin goes first: the watcher can rescan as soon as the directory
        // moves, and it must find the pin there.
        let mut previous = None;
        registry
            .update_approvals(|approvals| previous = approvals.insert(id.to_string(), pin.clone()))
            .map_err(|err| InstallRejection::internal("cannot write the approvals", &err))?;
        if is_staged {
            if let Err(err) = install_staged(&staged, &plugins_dir, id) {
                let restored = registry.update_approvals(|approvals| match previous {
                    Some(old) => {
                        approvals.insert(id.to_string(), old);
                    }
                    None => {
                        approvals.remove(id);
                    }
                });
                if let Err(restore_err) = restored {
                    log::error!("cannot restore the approval of plugin `{id}`: {restore_err}");
                }
                return Err(InstallRejection::internal("cannot install the package", &err));
            }
        }
        log::info!("plugin `{id}` approved with capabilities: {}", pin.capabilities.join(", "));
        self.reload(config);
        Ok(None)
    }

    /// Whether plugin `id` is installed or staged.
    #[must_use]
    pub fn api_exists(&self, id: &str) -> bool {
        is_installable_id(id)
            && (self.registry().snapshot().get(id).is_some() || is_real_dir(&self.staging_root().join(id)))
    }

    /// `PUT …/enable` precondition. Enabling a plugin that is `pending_approval`
    /// or `invalid` is a `409`; a `failed` one may be enabled again (the reload
    /// that follows gives it a fresh breaker).
    ///
    /// # Errors
    /// `404` for an unknown id, `409` for a plugin that cannot run.
    pub fn api_enable_check(&self, _guard: &InstallGuard<'_>, id: &str) -> Result<(), InstallRejection> {
        let snapshot = self.registry().snapshot();
        let Some(record) = snapshot.get(id) else {
            return Err(InstallRejection::new(StatusCode::NOT_FOUND, "unknown plugin"));
        };
        match record.state() {
            PluginState::Loaded | PluginState::Enabled | PluginState::Disabled | PluginState::Failed => Ok(()),
            other => {
                let why =
                    record.failure().map(|reason| format!(": {}", sanitize_text(&reason, 300))).unwrap_or_default();
                Err(InstallRejection::new(
                    StatusCode::CONFLICT,
                    format!("plugin `{id}` cannot be enabled while it is {}{why}", other.as_str()),
                ))
            }
        }
    }

    /// `DELETE /api/v1/plugins/{id}`: remove the pin, the plugin directory and
    /// any staged copy, reload with `config` (which no longer lists the plugin),
    /// and with `purge_kv` delete the KV store once the registry has dropped its
    /// handle.
    ///
    /// # Errors
    /// `404` for an unknown id, `500` when the filesystem fails.
    pub async fn api_uninstall(
        &self,
        _guard: &InstallGuard<'_>,
        id: &str,
        purge_kv: bool,
        config: &PluginsConfig,
    ) -> Result<(), InstallRejection> {
        let system = self.clone();
        let (id, config) = (id.to_string(), config.clone());
        tokio::task::spawn_blocking(move || system.uninstall_blocking(&id, purge_kv, &config))
            .await
            .map_err(|err| InstallRejection::internal("the uninstall task failed", &err))?
    }

    fn uninstall_blocking(&self, id: &str, purge_kv: bool, config: &PluginsConfig) -> Result<(), InstallRejection> {
        if !self.api_exists(id) {
            return Err(InstallRejection::new(StatusCode::NOT_FOUND, "unknown plugin"));
        }
        let registry = self.registry();
        let plugins_dir = registry.directory();
        registry
            .update_approvals(|approvals| {
                approvals.remove(id);
            })
            .map_err(|err| InstallRejection::internal("cannot update the approvals", &err))?;
        for dir in [plugins_dir.join(id), self.staging_root().join(id), self.prior_dir(id)] {
            remove_path(&dir).map_err(|err| InstallRejection::internal("cannot remove the plugin", &err))?;
        }
        self.reload(config);
        if purge_kv {
            // The reload built the registry without this plugin, so its handle
            // is out of the snapshot; drop the shared one, then the files.
            self.shared().kv.forget(id);
            remove_path(&self.shared().data_dir.join(id))
                .map_err(|err| InstallRejection::internal("cannot delete the plugin data", &err))?;
        }
        log::info!("plugin `{id}` uninstalled{}", if purge_kv { " with its stored data" } else { "" });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Update and rollback
// ---------------------------------------------------------------------------

impl PluginSystem {
    /// Swap in `source` over the installed package of `id`, with `new_pin`, and
    /// keep the installed package, its pin and `current`'s entry of it as the
    /// prior version. Blocking. The caller reloads, runs the activation check
    /// and calls [`PluginSystem::finish_replacement`]. `facts` is
    /// `(has_wasm, target_version, is_rollback)`.
    fn begin_replacement(
        &self,
        id: &str,
        source: Incoming,
        new_pin: ApprovalPin,
        facts: (bool, String, bool),
        current: &PluginsConfig,
    ) -> Result<Replacement, InstallRejection> {
        let (has_wasm, target_version, rollback) = facts;
        let registry = self.registry();
        let plugins_dir = registry.directory();
        let kept_version = registry
            .snapshot()
            .get(id)
            .and_then(|record| record.manifest.as_ref().map(|manifest| manifest.version.to_string()))
            .unwrap_or_else(|| "unknown".to_string());
        let current_pin = read_approvals(&approvals_path(&self.shared().data_dir)).remove(id);
        let meta = PriorMeta {
            version: kept_version.clone(),
            pin: current_pin,
            entry: current.entry(id).map(PriorEntry::from),
        };
        let mut swap = FsSwap::prepare(source, &plugins_dir, &self.prior_root(), id, &meta)
            .map_err(|err| InstallRejection::internal("cannot keep the installed package", &err))?;
        let previous_pin = match registry.swap_pin(id, new_pin) {
            Ok(previous) => previous,
            Err(err) => {
                swap.undo();
                return Err(InstallRejection::internal("cannot write the approvals", &err));
            }
        };
        if let Err(err) = swap.enter() {
            if let Err(restore_err) = registry.restore_pin(id, previous_pin) {
                log::error!("cannot restore the approval of plugin `{id}`: {restore_err}");
            }
            swap.undo();
            return Err(InstallRejection::internal("cannot install the package", &err));
        }
        Ok(Replacement { id: id.to_string(), swap, previous_pin, kept_version, target_version, rollback, has_wasm })
    }

    /// The activation check of a swapped package. A package without a wasm has
    /// no component, so validating it was the check. A plugin that runs (an
    /// enabled entry) must pass [`PluginSystem::activation_check`]: it loads and
    /// `init` accepts its settings. One that does not run must still load, but
    /// its `init` is not checked: it may need settings the operator has not
    /// written yet, and it gets them checked when it is enabled.
    async fn check_replacement(&self, replacement: &Replacement, config: &PluginsConfig) -> Result<(), String> {
        if !replacement.has_wasm {
            return Ok(());
        }
        if config.entry(&replacement.id).is_some_and(|entry| entry.enabled) {
            return self.activation_check(&replacement.id, config).await;
        }
        let snapshot = self.registry().snapshot();
        match snapshot.get(&replacement.id) {
            Some(record) if record.loaded.is_some() => Ok(()),
            Some(record) => Err(format!(
                "plugin `{}` did not load ({}): {}",
                replacement.id,
                record.state().as_str(),
                record.failure().unwrap_or_default()
            )),
            None => Err(format!("plugin `{}` is not installed", replacement.id)),
        }
    }

    /// Settle a swapped package. `Ok` keeps it and deletes what the undo would
    /// have needed. `Err(reason)` restores the directory, the pin and the prior
    /// version, reloads with `undo_config`, discards the new package and
    /// answers `409`.
    async fn finish_replacement(
        &self,
        replacement: Replacement,
        undo_config: &PluginsConfig,
        check: Result<(), String>,
    ) -> Result<(), InstallRejection> {
        let Err(reason) = check else {
            return tokio::task::spawn_blocking(move || replacement.swap.commit())
                .await
                .map_err(|err| InstallRejection::internal("the commit task failed", &err));
        };
        let reason = sanitize_text(&reason, 300);
        let message = if replacement.rollback {
            format!(
                "rollback to {} failed and was undone, still on {}: {reason}",
                replacement.target_version, replacement.kept_version
            )
        } else {
            format!("rolled back to {}: {reason}", replacement.kept_version)
        };
        log::warn!("plugin `{}`: {message}", replacement.id);
        let (system, config) = (self.clone(), undo_config.clone());
        tokio::task::spawn_blocking(move || {
            replacement.swap.undo();
            if let Err(err) = system.registry().restore_pin(&replacement.id, replacement.previous_pin) {
                log::error!("cannot restore the approval of plugin `{}`: {err}", replacement.id);
            }
            system.reload(&config);
        })
        .await
        .map_err(|err| InstallRejection::internal("the undo task failed", &err))?;
        Err(InstallRejection::new(StatusCode::CONFLICT, message))
    }

    /// `POST /api/v1/plugins/{id}/rollback`: swap the installed package and the
    /// retained prior one, with their pins, then reload and run the activation
    /// check; a failed check swaps everything back and is a `409`. The prior
    /// entry's settings are put in the candidate config the check runs with;
    /// the caller persists [`RollbackOutcome::settings`] to `config.yml` and
    /// reloads once this returns.
    ///
    /// # Errors
    /// `404` when the plugin or its prior version does not exist, `409` for a
    /// prior that was never approved, changed on disk or fails activation,
    /// `422` when the prior no longer validates.
    pub async fn api_rollback(
        &self,
        _guard: &InstallGuard<'_>,
        id: &str,
        config: &PluginsConfig,
    ) -> Result<RollbackOutcome, InstallRejection> {
        let system = self.clone();
        let (id, config) = (id.to_string(), config.clone());
        // Not cancellable halfway, for the same reason as `api_approve`.
        async move {
            let blocking = system.clone();
            let (blocking_id, blocking_config) = (id.clone(), config.clone());
            let (replacement, candidate, outcome) =
                tokio::task::spawn_blocking(move || blocking.rollback_blocking(&blocking_id, &blocking_config))
                    .await
                    .map_err(|err| InstallRejection::internal("the rollback task failed", &err))??;
            let check = system.check_replacement(&replacement, &candidate).await;
            system.finish_replacement(replacement, &config, check).await?;
            log::info!("plugin `{id}` rolled back to {}", outcome.version);
            Ok(outcome)
        }
        .await
    }

    fn rollback_blocking(
        &self,
        id: &str,
        config: &PluginsConfig,
    ) -> Result<(Replacement, PluginsConfig, RollbackOutcome), InstallRejection> {
        let unknown = || InstallRejection::new(StatusCode::NOT_FOUND, "unknown plugin");
        if !is_installable_id(id) || !is_real_dir(&self.registry().directory().join(id)) {
            return Err(unknown());
        }
        let prior_dir = self.prior_dir(id);
        let meta = read_prior_meta(&prior_dir).ok_or_else(|| {
            InstallRejection::new(StatusCode::NOT_FOUND, "there is no earlier version to roll back to")
        })?;
        let package = prior_dir.join(PRIOR_PACKAGE);
        if !is_real_dir(&package) {
            return Err(InstallRejection::new(StatusCode::NOT_FOUND, "the earlier version is missing on disk"));
        }
        let Some(pin) = meta.pin.clone() else {
            return Err(InstallRejection::new(StatusCode::CONFLICT, "the earlier version was never approved"));
        };
        let validated = self.validate_package(&package, id).map_err(InstallRejection::invalid)?;
        if validated.wasm_sha256.clone().unwrap_or_default() != pin.wasm_sha256 {
            return Err(InstallRejection::new(
                StatusCode::CONFLICT,
                "the earlier version changed on disk since it was approved",
            ));
        }
        let (settings, settings_version) = match (&meta.entry, config.entry(id)) {
            (Some(entry), Some(_)) => (Some(entry.settings.clone()), entry.settings_version),
            _ => (None, shared::model::DEFAULT_PLUGIN_SETTINGS_VERSION),
        };
        let candidate = settings
            .as_ref()
            .map_or_else(|| config.clone(), |settings| with_settings(config, id, settings, settings_version));
        let replacement = self.begin_replacement(
            id,
            Incoming::Prior,
            pin,
            (validated.wasm_sha256.is_some(), meta.version.clone(), true),
            config,
        )?;
        self.reload(&candidate);
        Ok((replacement, candidate, RollbackOutcome { version: meta.version, settings, settings_version }))
    }

    /// The summary field `update`: a staged package of an installed plugin and
    /// what it changes in the approval. Reads only the staged manifest (no
    /// compile), so a list call stays cheap.
    pub(crate) fn update_summary(&self, id: &str) -> Option<PluginUpdateDto> {
        if !is_installable_id(id) || self.registry().snapshot().get(id).is_none() {
            return None;
        }
        let staged = self.staging_root().join(id);
        if !is_real_dir(&staged) {
            return None;
        }
        let text = fs::read_to_string(staged.join(MANIFEST_FILE)).ok()?;
        let manifest = Manifest::parse(&text, id).ok()?;
        let pins = read_approvals(&approvals_path(&self.shared().data_dir));
        let diff = capability_diff(&capability_names(manifest.capabilities), pins.get(id));
        Some(PluginUpdateDto {
            version: manifest.version.to_string(),
            added_capabilities: diff.added,
            removed_capabilities: diff.removed,
        })
    }

    /// The summary field `rollback`: the retained prior version of `id`.
    pub(crate) fn rollback_summary(&self, id: &str) -> Option<PluginRollbackDto> {
        if !is_installable_id(id) {
            return None;
        }
        read_prior_meta(&self.prior_dir(id)).map(|meta| PluginRollbackDto { version: sanitize_text(&meta.version, 64) })
    }
}

/// `prev.json` of a retained prior directory, if it is there and readable.
fn read_prior_meta(prior_dir: &Path) -> Option<PriorMeta> {
    let text = fs::read(prior_dir.join(PRIOR_META)).ok()?;
    serde_json::from_slice(&text).ok()
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod update_tests;
