//! DTOs of the plugin update API (`/api/v1/plugins/updates…`,
//! `/api/v1/plugins/{id}/update…`), shared with the web UI.

use crate::model::{PluginCapabilityDiffDto, PluginUpdatePolicy, PluginUploadReviewDto};
use serde::{Deserialize, Serialize};

/// Why an update cannot be applied without an admin. A policy is not a
/// blocker: it decides *when* an eligible update moves (see
/// [`PluginUpdateCandidateDto::policy`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginUpdateBlockerDto {
    /// The release asks for capabilities the approval lacks.
    CapabilitiesAdded { capabilities: Vec<String> },
    /// MCP is approved for the plugin and the release exposes rpc exports as
    /// MCP tools that the installed version did not.
    McpExposureWidened { exports: Vec<String> },
    /// The settings-schema version moves up and the release cannot migrate the
    /// stored settings (no `settings.migrate` export).
    SettingsMigrationMissing { from: u32, to: u32 },
    /// The release's `settings.migrate` failed, or its output does not
    /// validate against the new schema.
    SettingsMigrationFailed { from: u32, to: u32, reason: String },
    /// The settings-schema version moves down.
    SettingsVersionDowngrade { from: u32, to: u32 },
}

/// A newer release of an installed, marketplace-installed plugin, from the
/// source it was installed from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginUpdateCandidateDto {
    pub id: String,
    pub name: String,
    pub installed_version: String,
    /// The newest compatible release newer than the installed one.
    pub version: String,
    /// The recorded source (`ApprovalPin.origin.source`).
    pub source: String,
    pub policy: PluginUpdatePolicy,
    /// Whether the plugin is enabled. Boot updates only touch enabled plugins.
    pub enabled: bool,
    /// No blocker: the update can be applied without a review. Before the
    /// package is downloaded this is judged from the catalog only, see
    /// `verified`.
    pub eligible: bool,
    /// `true` when the release's package is staged and was checked (the
    /// capabilities and the settings migration of the real package); `false`
    /// when only the catalog entry was compared (its capability list is
    /// informational, so a staged check can still block).
    pub verified: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<PluginUpdateBlockerDto>,
    /// Against the installed approval.
    pub capability_diff: PluginCapabilityDiffDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changelog: Option<String>,
}

/// A source that could not be checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginUpdateSourceErrorDto {
    pub source: String,
    pub error: String,
}

/// `GET /api/v1/plugins/updates` and the answer of `POST …/updates/check`.
/// Computed from the cached catalogs; only the check fetches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginUpdatesDto {
    pub candidates: Vec<PluginUpdateCandidateDto>,
    /// Sources of installed plugins whose last sync failed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<PluginUpdateSourceErrorDto>,
}

/// `POST /api/v1/plugins/{id}/update` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginUpdateRequestDto {
    pub version: String,
    /// Activate at once when the staged package is eligible ("Update all
    /// eligible", or "Update" on an eligible badge). With `false`, or when it
    /// is not eligible, the package stays staged for the existing review.
    #[serde(default)]
    pub apply: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginUpdateResultKindDto {
    /// Active; the pin carries the new hash.
    Applied,
    /// Staged; approve it through `PUT …/approve` after the review.
    Staged,
    /// Rejected or rolled back; the installed package is unchanged.
    Failed,
}

/// The answer of `POST /api/v1/plugins/{id}/update`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginUpdateResultDto {
    pub result: PluginUpdateResultKindDto,
    /// The review of the staged package, for `staged`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<PluginUploadReviewDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<PluginUpdateBlockerDto>,
    /// Why it failed, for `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `PUT /api/v1/plugins/{id}/update-policy` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginUpdatePolicyDto {
    pub policy: PluginUpdatePolicy,
}

/// `GET` / `PUT /api/v1/plugins/updates/settings`: the global update fields of
/// the `plugins:` block, as written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginUpdateSettingsDto {
    pub auto_update_on_boot: bool,
    /// A duration with a unit (`30s`).
    pub boot_update_budget: String,
    /// Configured raw plugin directory as written in `config.yml` (empty for default).
    #[serde(default)]
    pub directory: String,
    /// Effective resolved install directory.
    #[serde(default, alias = "resolvedDirectory")]
    pub effective_directory: String,
    /// Whether `plugins.directories` or `TULIPROX_PLUGINS_DIRS` overrides `directory`.
    #[serde(default, alias = "overridden")]
    pub directory_overridden: bool,
    /// Effective search directories in scan order (bundled first, custom last).
    #[serde(default, alias = "searchDirectories")]
    pub search_directories: Vec<String>,
}

/// What the boot update run did with one plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginBootUpdateOutcomeDto {
    UpToDate,
    Applied,
    /// Staged for review; the current version keeps running.
    AwaitingApproval,
    /// Rejected, rolled back, unreachable, or the budget ran out; retried on
    /// the next boot.
    Failed,
    /// Not tried: policy `manual`/`pinned`, disabled, no recorded source, or
    /// in-flight work did not drain in time.
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginBootUpdateEntryDto {
    pub id: String,
    pub from: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub outcome: PluginBootUpdateOutcomeDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Unix seconds.
    pub attempted_at: i64,
}

/// `GET /api/v1/plugins/updates/boot-report`: the last boot update run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginBootReportDto {
    /// Unix seconds; absent when no boot ran an update check yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    pub budget_ms: u64,
    /// The run stopped at the budget; unfinished plugins are `failed`.
    pub budget_exceeded: bool,
    /// Plugins whose activation journal was rolled back at startup.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recovered: Vec<String>,
    pub entries: Vec<PluginBootUpdateEntryDto>,
}
