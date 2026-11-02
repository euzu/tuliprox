//! Update discovery and eligibility for installed marketplace plugins.
//!
//! A candidate is a newer, compatible release of an installed plugin in the
//! source it was installed from (`ApprovalPin.origin`), read from the cached
//! catalog only: nothing here fetches a catalog. Eligibility is first judged
//! from the catalog entry (`verified: false`) and again, after the package is
//! staged through the marketplace upload path, from the real package (`verified: true`).
//! Nothing here installs, approves or enables anything.

use super::{
    catalog::{cache_path, read_cache},
    fetch::{fetch_package, FetchPolicy, MarketplaceError, PACKAGE_MAX_BYTES},
    format::{compatibility, Release as CatalogRelease},
};
use crate::{
    install::{InstallGuard, InstallRejection, MarketplaceExpect},
    manifest::Manifest,
    migration::SettingsDryRun,
    registry::{approvals_path, capability_names, read_approvals, ApprovalPin, MANIFEST_FILE, WASM_FILE},
    system::PluginSystem,
};
use http::StatusCode;
use semver::Version;
use shared::model::{
    PluginCapabilityDiffDto, PluginOriginDto, PluginUpdateBlockerDto, PluginUpdateCandidateDto, PluginUpdatePolicy,
    PluginUpdateSourceErrorDto, PluginUpdatesDto, PluginUploadErrorDto, PluginUploadReviewDto,
};
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use tuliprox_core::model::{PluginRepository, PluginsConfig};
use tuliprox_plugin_api::ABI_MAJOR;

/// Where `install.rs` stages packages, and the hidden directory in it that
/// holds the pending origin of a marketplace package (`<id>.json`). Kept in
/// step with `install.rs` (`STAGING_DIR`, `ORIGIN_DIR`).
const STAGING_DIR: &str = "staging";
const ORIGIN_DIR: &str = ".origin";

/// A newer release of an installed marketplace plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCandidate {
    pub id: String,
    /// The installed manifest's name.
    pub name: String,
    pub installed_version: String,
    /// The newest compatible release that is semver-newer than the installed one.
    pub version: String,
    /// The recorded source (`ApprovalPin.origin.source`).
    pub source: String,
    pub policy: PluginUpdatePolicy,
    pub enabled: bool,
    pub package_url: String,
    pub changelog: Option<String>,
    /// The capability names the catalog lists for the release, sorted. Only
    /// informational: the staged package decides.
    pub catalog_capabilities: Vec<String>,
}

/// Why an update cannot be applied without an admin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    CapabilitiesAdded(Vec<String>),
    McpExposureWidened(Vec<String>),
    SettingsMigrationMissing { from: u32, to: u32 },
    SettingsMigrationFailed { from: u32, to: u32, reason: String },
    SettingsVersionDowngrade { from: u32, to: u32 },
}

impl From<&Blocker> for PluginUpdateBlockerDto {
    fn from(blocker: &Blocker) -> Self {
        match blocker {
            Blocker::CapabilitiesAdded(capabilities) => Self::CapabilitiesAdded { capabilities: capabilities.clone() },
            Blocker::McpExposureWidened(exports) => Self::McpExposureWidened { exports: exports.clone() },
            Blocker::SettingsMigrationMissing { from, to } => Self::SettingsMigrationMissing { from: *from, to: *to },
            Blocker::SettingsMigrationFailed { from, to, reason } => {
                Self::SettingsMigrationFailed { from: *from, to: *to, reason: reason.clone() }
            }
            Blocker::SettingsVersionDowngrade { from, to } => Self::SettingsVersionDowngrade { from: *from, to: *to },
        }
    }
}

impl Blocker {
    fn describe(&self) -> String {
        match self {
            Self::CapabilitiesAdded(names) => format!("asks for new capabilities: {}", names.join(", ")),
            Self::McpExposureWidened(names) => format!("exposes more MCP tools: {}", names.join(", ")),
            Self::SettingsMigrationMissing { from, to } => {
                format!("the settings schema moves from version {from} to {to} and the release cannot migrate them")
            }
            Self::SettingsMigrationFailed { from, to, reason } => {
                format!("migrating the settings from version {from} to {to} failed: {reason}")
            }
            Self::SettingsVersionDowngrade { from, to } => {
                format!("the settings schema would go down from version {from} to {to}")
            }
        }
    }
}

/// The verdict on one candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Eligibility {
    /// `true` when `staging/<id>` holds exactly this marketplace release and
    /// the verdict comes from the package; `false` when only the catalog entry
    /// was compared.
    pub verified: bool,
    pub blockers: Vec<Blocker>,
    pub capability_diff: PluginCapabilityDiffDto,
}

impl Eligibility {
    #[must_use]
    pub fn is_eligible(&self) -> bool { self.blockers.is_empty() }

    /// One line naming every blocker, for events and logs; `None` when eligible.
    #[must_use]
    pub fn reason(&self) -> Option<String> {
        (!self.blockers.is_empty()).then(|| self.blockers.iter().map(Blocker::describe).collect::<Vec<_>>().join("; "))
    }
}

/// An installed plugin that came from a marketplace source.
struct Installed {
    id: String,
    name: String,
    version: Version,
    origin: PluginOriginDto,
    /// The rpc exports the installed manifest offers as MCP tools.
    mcp_exports: BTreeSet<String>,
    /// The capabilities the pin approves.
    approved: Vec<String>,
}

/// A staged package read for the verified judgment.
struct Staged {
    dir: PathBuf,
    manifest: Arc<Manifest>,
    wasm: Option<Vec<u8>>,
}

/// Versions are equal by semver, or by text when one does not parse.
fn same_version(left: &str, right: &str) -> bool {
    match (Version::parse(left.trim()), Version::parse(right.trim())) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn host_version() -> Version { Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| Version::new(0, 0, 0)) }

fn origin_file(staging_root: &Path, id: &str) -> PathBuf { staging_root.join(ORIGIN_DIR).join(format!("{id}.json")) }

/// The pending origin recorded for the package staged as `id`, whatever its version.
fn pending_origin(staging_root: &Path, id: &str) -> Option<PluginOriginDto> {
    serde_json::from_str(&fs::read_to_string(origin_file(staging_root, id)).ok()?).ok()
}

/// The capability change of a package asking for `wanted` against the pin.
fn capability_diff(wanted: &[String], pin: Option<&ApprovalPin>) -> PluginCapabilityDiffDto {
    let approved: &[String] = pin.map_or(&[], |pin| pin.capabilities.as_slice());
    PluginCapabilityDiffDto {
        added: wanted.iter().filter(|name| !approved.contains(name)).cloned().collect(),
        removed: approved.iter().filter(|name| !wanted.contains(name)).cloned().collect(),
        unchanged: wanted.iter().filter(|name| approved.contains(name)).cloned().collect(),
        mcp_was_approved: pin.is_some_and(|pin| pin.mcp),
    }
}

fn is_real_dir(path: &Path) -> bool { fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()) }

/// The package staged for `candidate`, when `staging/<id>` holds exactly this
/// marketplace release: the pending origin names the candidate's source and
/// version, and the manifest's id and version agree. The package was validated
/// when it was staged and is validated again by approve; this only reads it.
fn read_staged(data_dir: &Path, candidate: &UpdateCandidate) -> Option<Staged> {
    let root = data_dir.join(STAGING_DIR);
    let dir = root.join(&candidate.id);
    if !is_real_dir(&dir) {
        return None;
    }
    let origin = pending_origin(&root, &candidate.id)?;
    if origin.source != candidate.source || !same_version(&origin.version, &candidate.version) {
        return None;
    }
    let text = fs::read_to_string(dir.join(MANIFEST_FILE)).ok()?;
    let manifest = Manifest::parse(&text, &candidate.id).ok()?;
    if manifest.id != candidate.id || !same_version(&manifest.version.to_string(), &candidate.version) {
        return None;
    }
    let wasm = fs::read(dir.join(WASM_FILE)).ok();
    Some(Staged { dir, manifest: Arc::new(manifest), wasm })
}

impl PluginSystem {
    /// Every installed plugin whose pin records a marketplace origin.
    fn installed_marketplace(&self) -> Vec<Installed> {
        let pins = read_approvals(&approvals_path(&self.marketplace_data_dir()));
        let snapshot = self.registry().snapshot();
        snapshot
            .plugins
            .iter()
            .filter_map(|record| {
                let manifest = record.manifest.as_deref()?;
                let pin = pins.get(&*record.id)?;
                let origin = pin.origin.clone()?;
                Some(Installed {
                    id: record.id.to_string(),
                    name: manifest.name.clone(),
                    version: manifest.version.clone(),
                    origin,
                    mcp_exports: mcp_exports(manifest),
                    approved: pin.capabilities.clone(),
                })
            })
            .collect()
    }

    /// The enabled, configured sources of the installed marketplace plugins,
    /// sorted and without duplicates: what an update check has to sync.
    #[must_use]
    pub fn update_source_ids(&self, config: &PluginsConfig) -> Vec<String> {
        self.installed_marketplace()
            .into_iter()
            .filter_map(|installed| enabled_source(config, &installed.origin.source).map(|source| source.id.clone()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// The newer, compatible releases of the installed marketplace plugins,
    /// sorted by id and then newest first, from the cached catalogs (no
    /// network).
    ///
    /// Per plugin: the newest such release, and, when its catalog capabilities
    /// ask for more than the approval covers, also the newest release whose
    /// catalog capabilities the approval covers, so an update that needs no
    /// review stays reachable.
    ///
    /// Only the recorded source counts: a removed or disabled source gives no
    /// candidate, and the same id in another source is ignored. `config`
    /// supplies the policy and the enabled flag of each plugin.
    pub async fn update_candidates(&self, config: &PluginsConfig) -> Vec<UpdateCandidate> {
        let data_dir = self.marketplace_data_dir();
        let host = host_version();
        let mut caches = HashMap::new();
        let mut candidates = Vec::new();
        for installed in self.installed_marketplace() {
            let Some(source) = enabled_source(config, &installed.origin.source) else { continue };
            if !caches.contains_key(&source.id) {
                caches.insert(source.id.clone(), read_source_cache(&data_dir, source).await);
            }
            let Some(catalog) = caches.get(&source.id).and_then(Option::as_ref) else { continue };
            let Some(plugin) = catalog.plugins.iter().find(|plugin| plugin.id == installed.id) else { continue };
            let mut newer: Vec<_> = plugin
                .releases
                .iter()
                .filter_map(|release| Some((Version::parse(&release.version).ok()?, release)))
                .filter(|(version, release)| {
                    *version > installed.version && compatibility(release, &host, ABI_MAJOR).is_ok()
                })
                .collect();
            newer.sort_by(|(left, _), (right, _)| right.cmp(left));
            let covered = |release: &&CatalogRelease| {
                release.capabilities.iter().all(|capability| installed.approved.contains(capability))
            };
            let Some(&(ref version, newest)) = newer.first() else { continue };
            let mut picked = vec![(version.clone(), newest)];
            if !covered(&newest) {
                if let Some((version, release)) = newer.iter().find(|(_, release)| covered(release)) {
                    picked.push((version.clone(), *release));
                }
            }
            let entry = config.entry(&installed.id);
            for (version, release) in picked {
                let mut catalog_capabilities = release.capabilities.clone();
                catalog_capabilities.sort_unstable();
                catalog_capabilities.dedup();
                candidates.push(UpdateCandidate {
                    id: installed.id.clone(),
                    name: installed.name.clone(),
                    installed_version: installed.version.to_string(),
                    version: version.to_string(),
                    source: source.id.clone(),
                    policy: entry.map_or_else(PluginUpdatePolicy::default, |entry| entry.update_policy),
                    enabled: entry.is_some_and(|entry| entry.enabled),
                    package_url: release.package_url.clone(),
                    changelog: release.changelog.clone(),
                    catalog_capabilities,
                });
            }
        }
        // Stable: per id the newest stays first.
        candidates.sort_by(|left, right| left.id.cmp(&right.id));
        candidates
    }

    /// Judge `candidate`: from the staged package when `staging/<id>` holds
    /// exactly this marketplace release (`verified: true`), else from the
    /// catalog entry alone, where only a capability the pin lacks can block.
    ///
    /// `config` must hold the plugins' settings **as written** (the migration
    /// dry run feeds them to the release's `settings.migrate`); the policy does
    /// not matter here.
    pub async fn evaluate_update(&self, config: &PluginsConfig, candidate: &UpdateCandidate) -> Eligibility {
        let data_dir = self.marketplace_data_dir();
        let (read_dir, read_candidate) = (data_dir.clone(), candidate.clone());
        let (pin, staged) = tokio::task::spawn_blocking(move || {
            let pin = read_approvals(&approvals_path(&read_dir)).remove(&read_candidate.id);
            (pin, read_staged(&read_dir, &read_candidate))
        })
        .await
        .unwrap_or((None, None));

        let Some(staged) = staged else {
            let diff = capability_diff(&candidate.catalog_capabilities, pin.as_ref());
            let blockers =
                if diff.added.is_empty() { Vec::new() } else { vec![Blocker::CapabilitiesAdded(diff.added.clone())] };
            return Eligibility { verified: false, blockers, capability_diff: diff };
        };

        let wanted = capability_names(staged.manifest.capabilities);
        let diff = capability_diff(&wanted, pin.as_ref());
        let mut blockers = Vec::new();
        if !diff.added.is_empty() {
            blockers.push(Blocker::CapabilitiesAdded(diff.added.clone()));
        }
        if pin.as_ref().is_some_and(|pin| pin.mcp) {
            let installed = self
                .installed_marketplace()
                .into_iter()
                .find(|installed| installed.id == candidate.id)
                .map(|installed| installed.mcp_exports)
                .unwrap_or_default();
            let widened: Vec<String> = mcp_exports(&staged.manifest).difference(&installed).cloned().collect();
            if !widened.is_empty() {
                blockers.push(Blocker::McpExposureWidened(widened));
            }
        }
        let dry_run = self
            .dry_run_settings_migration(staged.dir, staged.manifest, staged.wasm, config.entry(&candidate.id))
            .await;
        match dry_run {
            SettingsDryRun::Unchanged | SettingsDryRun::Migrates { .. } => {}
            SettingsDryRun::Downgrade { from, to } => blockers.push(Blocker::SettingsVersionDowngrade { from, to }),
            SettingsDryRun::Missing { from, to } => blockers.push(Blocker::SettingsMigrationMissing { from, to }),
            SettingsDryRun::Failed { from, to, reason } => {
                blockers.push(Blocker::SettingsMigrationFailed { from, to, reason });
            }
        }
        Eligibility { verified: true, blockers, capability_diff: diff }
    }

    /// Download the package of `candidate` with the marketplace fetch policy
    /// (https only, size capped, destination checked).
    ///
    /// # Errors
    /// The [`MarketplaceError`] of the fetch.
    pub async fn download_update(
        &self,
        client: &reqwest::Client,
        policy: FetchPolicy,
        candidate: &UpdateCandidate,
    ) -> Result<Vec<u8>, MarketplaceError> {
        fetch_package(client, policy, &candidate.package_url, PACKAGE_MAX_BYTES).await
    }

    /// Stage the downloaded `package` of `candidate` through the marketplace upload path
    /// ([`PluginSystem::api_upload_from_marketplace`]): the manifest id and
    /// version must equal the candidate's, the origin is recorded as pending.
    /// The one staging slot of the id is never taken over: a package in it that
    /// is not a marketplace package from the candidate's source (a plain
    /// upload, or an install from another source) stays untouched.
    ///
    /// # Errors
    /// `409` when the slot holds such a package, `422` when the package is not
    /// the requested release or does not validate, `500` on a filesystem error.
    pub async fn stage_update(
        &self,
        guard: &InstallGuard<'_>,
        candidate: &UpdateCandidate,
        package: Vec<u8>,
    ) -> Result<PluginUploadReviewDto, InstallRejection> {
        let root = self.marketplace_data_dir().join(STAGING_DIR);
        let (slot_id, slot_source) = (candidate.id.clone(), candidate.source.clone());
        let foreign = tokio::task::spawn_blocking(move || {
            fs::symlink_metadata(root.join(&slot_id)).is_ok()
                && pending_origin(&root, &slot_id).is_none_or(|origin| origin.source != slot_source)
        })
        .await
        .map_err(|err| InstallRejection {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: PluginUploadErrorDto { error: format!("the staging check failed: {err}"), problems: Vec::new() },
        })?;
        if foreign {
            return Err(InstallRejection {
                status: StatusCode::CONFLICT,
                body: PluginUploadErrorDto {
                    error: format!(
                        "another package is already staged for `{}`; approve or discard it first",
                        candidate.id
                    ),
                    problems: Vec::new(),
                },
            });
        }
        let expect = MarketplaceExpect {
            id: candidate.id.clone(),
            version: candidate.version.clone(),
            origin: PluginOriginDto { source: candidate.source.clone(), version: candidate.version.clone() },
        };
        self.api_upload_from_marketplace(guard, package, expect).await
    }

    /// `GET /api/v1/plugins/updates`: every candidate with its verdict.
    /// `config` as for [`PluginSystem::evaluate_update`]; `source_errors` is
    /// passed through.
    pub async fn updates_dto(
        &self,
        config: &PluginsConfig,
        source_errors: Vec<PluginUpdateSourceErrorDto>,
    ) -> PluginUpdatesDto {
        let mut candidates = Vec::new();
        for candidate in self.update_candidates(config).await {
            let eligibility = self.evaluate_update(config, &candidate).await;
            candidates.push(PluginUpdateCandidateDto {
                eligible: eligibility.is_eligible(),
                verified: eligibility.verified,
                blockers: eligibility.blockers.iter().map(PluginUpdateBlockerDto::from).collect(),
                capability_diff: eligibility.capability_diff,
                id: candidate.id,
                name: candidate.name,
                installed_version: candidate.installed_version,
                version: candidate.version,
                source: candidate.source,
                policy: candidate.policy,
                enabled: candidate.enabled,
                changelog: candidate.changelog,
            });
        }
        PluginUpdatesDto { candidates, errors: source_errors }
    }
}

/// The configured source `id` when it is enabled.
fn enabled_source<'a>(config: &'a PluginsConfig, id: &str) -> Option<&'a PluginRepository> {
    config.repository(id).filter(|source| source.enabled)
}

/// The names of the rpc exports a manifest offers as MCP tools.
fn mcp_exports(manifest: &Manifest) -> BTreeSet<String> {
    manifest.ui.rpc_exports.iter().filter(|export| export.mcp).map(|export| export.name.clone()).collect()
}

/// The cached catalog of `source`, `None` when it was never synced.
async fn read_source_cache(data_dir: &Path, source: &PluginRepository) -> Option<super::format::Catalog> {
    let path = cache_path(data_dir, &source.id)?;
    let url = source.resolved_url();
    tokio::task::spawn_blocking(move || read_cache(&path, &url).catalog).await.ok().flatten()
}
