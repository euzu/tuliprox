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
//!
//! One [`InstallGuard`] (a mutex in [`PluginSystem`]) must be held across each
//! of these and across a settings save; the methods take it by reference so a
//! caller cannot forget.

use crate::{
    manifest::{package::check_package, Capability, Contribution, Manifest, RpcExport, RpcKind, Slot},
    registry::{
        capability_names, check_hooks, export_names, host_version_problem, sha256_hex, ApprovalPin, PluginState,
        MANIFEST_FILE, WASM_FILE,
    },
    runtime::PluginLimits,
    system::PluginSystem,
};
use http::StatusCode;
use serde::Deserialize;
use shared::model::{
    is_valid_plugin_id, PluginApproveDto, PluginCapabilityReviewDto, PluginContributionDto, PluginRpcExportDto,
    PluginRpcKindDto, PluginSlotDto, PluginSummaryDto, PluginUploadErrorDto, PluginUploadReviewDto,
};
use std::{
    fs,
    io::{self, Cursor, Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use tuliprox_core::model::PluginsConfig;
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
const RESERVED_IDS: [&str; 2] = [STAGING_DIR, "metrics"];
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
        let replaces = self
            .registry()
            .snapshot()
            .get(&id)
            .and_then(|record| record.manifest.as_ref().map(|manifest| manifest.version.to_string()));
        let answer = review(&validated, replaces);
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
        tokio::task::spawn_blocking(move || {
            system.approve_blocking(&id, &body, &config)?;
            system
                .api_summary(&id, Some(&config))
                .ok_or_else(|| InstallRejection::new(StatusCode::NOT_FOUND, "unknown plugin"))
        })
        .await
        .map_err(|err| InstallRejection::internal("the approve task failed", &err))?
    }

    fn approve_blocking(
        &self,
        id: &str,
        body: &PluginApproveDto,
        config: &PluginsConfig,
    ) -> Result<(), InstallRejection> {
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
        Ok(())
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
        for dir in [plugins_dir.join(id), self.staging_root().join(id)] {
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

#[cfg(test)]
mod tests;
