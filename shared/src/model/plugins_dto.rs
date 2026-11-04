//! DTOs of the plugin management API (`/api/v1/plugins…`), shared with the
//! web UI: the list, stats, settings, tasks and logs of a plugin, and the
//! settings update.

use crate::model::PluginHookStats;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where a plugin is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginStateDto {
    Discovered,
    /// The manifest or the wasm failed validation; see `failure`.
    Invalid,
    /// Not approved yet, or the wasm hash or capabilities changed since.
    PendingApproval,
    Approved,
    /// Compiled and linked, not enabled.
    Loaded,
    Enabled,
    Disabled,
    /// The circuit breaker tripped; skipped until re-enabled or reloaded.
    Failed,
}

/// One row of `GET /api/v1/plugins`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSummaryDto {
    pub id: String,
    /// The manifest name; the id when the manifest did not parse.
    pub name: String,
    /// `None` when the manifest did not parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The manifest `category`, when it declares one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub state: PluginStateDto,
    /// Why the plugin is `invalid`, `pending_approval` or `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// Whether `plugins.plugins[]` has an entry for it with `enabled: true`.
    pub enabled: bool,
    /// Lower runs first.
    pub priority: i32,
    /// The manifest `hooks:` ids.
    pub hooks: Vec<String>,
    /// The capabilities the manifest asks for (`network`, `storage`, ...).
    pub capabilities: Vec<String>,
    /// The host ABI the plugin was built against.
    pub abi: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wasm_sha256: Option<String>,
    /// Why the stored settings need a look: the
    /// settings-schema `version` moved past the stored one and no migration
    /// ran or it failed. Saving the settings clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings_review: Option<String>,
    /// An uploaded update of this installed plugin waits in staging for review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<PluginUpdateDto>,
    /// The one retained prior package `POST …/rollback` restores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback: Option<PluginRollbackDto>,
    /// The marketplace source and release this plugin was installed from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<crate::model::PluginOriginDto>,
    /// The entry's update policy; `auto` without an entry.
    #[serde(default)]
    pub update_policy: crate::model::PluginUpdatePolicy,
}

/// A staged update of an installed plugin, before approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginUpdateDto {
    pub version: String,
    /// Capabilities the update asks for that the installed approval lacks.
    pub added_capabilities: Vec<String>,
    /// Approved capabilities the update no longer asks for.
    pub removed_capabilities: Vec<String>,
}

/// The retained prior package of a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginRollbackDto {
    pub version: String,
}

/// What an update changes against the installed approval. Every capability in
/// the review still has to be echoed by the approve body; this only marks the
/// new ones, and `mcp` is never carried over (the approve body decides it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginCapabilityDiffDto {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub unchanged: Vec<String>,
    /// The installed approval had MCP exposure approved.
    pub mcp_was_approved: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginBreakerStateDto {
    Closed,
    Open,
    HalfOpen,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginBreakerDto {
    pub state: PluginBreakerStateDto,
    pub consecutive_failures: u32,
    pub failure_threshold: u32,
    pub forced_open: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

/// One plugin's instances.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginInstancesDto {
    pub live: u32,
    pub max: u16,
    pub instantiations: u64,
    /// Instantiations that queued behind the plugin's instance cap.
    pub waits: u64,
}

/// The global memory budget all plugin instances share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginBudgetDto {
    pub total_mib: u32,
    pub in_use_mib: u32,
    pub waits: u64,
    pub wait_ms_total: u64,
    pub wait_ms_max: u64,
    pub timeouts: u64,
}

/// Bytes used against a quota.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginUsageDto {
    pub used_bytes: u64,
    pub quota_bytes: u64,
}

/// What a plugin contributed to a run. The settings
/// hash is left out: it is derived from the resolved secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginFingerprintDto {
    pub version: String,
    pub wasm_sha256: String,
    pub abi: String,
}

/// `GET /api/v1/plugins/{id}/stats`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginStatsDto {
    /// Hook invocations that ran to an outcome.
    pub calls: u64,
    /// Of those, the ones that trapped, timed out or returned `err`.
    pub failures: u64,
    /// Invocations the host could not start for lack of memory or an instance slot.
    pub budget_skips: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub breaker: PluginBreakerDto,
    /// `None` while the plugin is not loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instances: Option<PluginInstancesDto>,
    pub budget: PluginBudgetDto,
    /// `None` without the `storage` capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kv: Option<PluginUsageDto>,
    /// `None` without the `image_cache` capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<PluginUsageDto>,
    /// Log lines the rate limiter dropped.
    pub log_dropped: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<PluginFingerprintDto>,
    /// Per-hook totals since the plugin was loaded, summed over every target.
    #[serde(default)]
    pub hooks: Vec<PluginHookStats>,
}

/// One secret property of the settings schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSecretDto {
    /// `token`, `credentials.password`, `servers[].key`.
    pub path: String,
    /// Configured and not empty.
    pub set: bool,
}

/// `GET /api/v1/plugins/{id}/settings`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSettingsDto {
    /// The configured settings. Literal secrets are masked with
    /// [`REDACTED_SECRET`](crate::model::REDACTED_SECRET); `${env:VAR}`
    /// references are shown as the reference, never as the resolved value.
    pub settings: Value,
    /// The settings JSON Schema the package ships, if any.
    #[serde(default)]
    pub schema: Option<Value>,
    pub secrets: Vec<PluginSecretDto>,
    /// Non-fatal notes from a save: a secret stored as a literal rather than a
    /// `${env:VAR}` reference, `init` not run because the plugin is not loaded.
    /// Always empty from `GET`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Body of `PUT /api/v1/plugins/{id}/settings`.
///
/// `settings` replaces the stored settings. A secret whose value is exactly
/// [`REDACTED_SECRET`](crate::model::REDACTED_SECRET) keeps the stored value,
/// so the UI round-trips the masked `GET` without ever holding the secret. A
/// `${env:VAR}` reference is stored as the reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSettingsSaveDto {
    pub settings: Value,
}

/// One schema violation of a settings save.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginFieldErrorDto {
    /// The offending property, in the secret-path grammar (`token`,
    /// `credentials.password`, `servers[].key`); `(root)` for the whole object.
    pub path: String,
    pub message: String,
}

/// The body of a rejected settings save (`422`): schema violations in
/// `fields`, or `init` refusing the settings in `error` with `fields` empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSettingsErrorDto {
    pub error: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<PluginFieldErrorDto>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginTaskConcurrencyDto {
    /// Drop the new fire while the previous run is going.
    Skip,
    /// Keep one pending fire, never more.
    Queue,
}

/// One declared task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginTaskDto {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_schedule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_exec_ms: Option<u64>,
    pub concurrency: PluginTaskConcurrencyDto,
    pub respect_maintenance: bool,
    /// Always `null`: tasks do not run yet.
    #[serde(default)]
    pub last_run: Option<Value>,
}

/// `metrics-hooks.severity`: the accent of a metric card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginMetricSeverityDto {
    Info,
    Warn,
    Error,
}

/// One `collect-metrics` value, in the status
/// payload and `GET /api/v1/plugins/metrics`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginMetricDto {
    /// Namespaced by the host: `<plugin-id>.<key>`.
    pub key: String,
    pub plugin: String,
    /// Display label, or a key of the plugin's `ui/locales`.
    pub label: String,
    /// Preformatted by the plugin.
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<PluginMetricSeverityDto>,
}

/// The outcome of one task run: the answer
/// of `POST /api/v1/plugins/{id}/tasks/{task}/run`, and a task's `last_run`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginTaskRunDto {
    pub plugin: String,
    pub task: String,
    /// Unix seconds.
    pub started_at: i64,
    pub duration_ms: u64,
    /// `true` for "run now", `false` for a cron fire.
    pub manual: bool,
    /// `ok(summary)` of the guest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Why the run failed (guest `err`, trap, deadline), or why it was skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginLogLevelDto {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

/// One buffered log line; the message is already redacted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginLogEntryDto {
    pub ts_ms: u64,
    pub level: PluginLogLevelDto,
    pub message: String,
}

/// Query of `GET /api/v1/plugins/{id}/logs`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginLogQueryDto {
    /// Least severe level returned: `info` returns info, warn and error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<PluginLogLevelDto>,
    /// Only entries at or after this Unix time in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
    /// The newest `limit` entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

// ---------------------------------------------------------------------------
// Install and approval, UI contributions, the frame bridge and rpc.
// ---------------------------------------------------------------------------

/// One capability in the install review, with the sentence the admin approves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginCapabilityReviewDto {
    /// `network`, `storage`, ... (the approval pin's names).
    pub name: String,
    /// What granting it allows; `network` says "unrestricted network access".
    pub description: String,
}

/// `ui.rpc_exports[].kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginRpcKindDto {
    Read,
    /// Needs `SystemWrite`; never an MCP tool while MCP is read-only.
    Write,
}

/// An `rpc_exports` entry, as the review and the web UI see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginRpcExportDto {
    pub name: String,
    pub kind: PluginRpcKindDto,
    /// The manifest asks to offer it as an MCP tool.
    pub mcp: bool,
    /// Sanitized: control characters stripped, at most 512 chars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The extension slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PluginSlotDto {
    Menu,
    ConfigSection,
    ConfigTab,
    SourceEditorTile,
    DashboardCard,
    PlaylistExplorerAction,
    ChannelDetailPanel,
    EpgOverlay,
    SetupWizardStep,
}

/// An entry of `ui.contributions`: a sandboxed frame in a slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginContributionDto {
    pub slot: PluginSlotDto,
    /// Literal fallback title.
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Package-relative frame document, e.g. `ui/sports-guide.html`.
    pub element: String,
}

/// `POST /api/v1/plugins`: the capability review of an uploaded `.tlpx`. The
/// package waits in staging; nothing is installed before `PUT …/approve`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginUploadReviewDto {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The host ABI the plugin was built against.
    pub abi: String,
    /// `None` for a UI-only package without `plugin.wasm`. The approve body
    /// must echo it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wasm_sha256: Option<String>,
    /// What the plugin asks for. The approve body must echo the names.
    pub capabilities: Vec<PluginCapabilityReviewDto>,
    pub hooks: Vec<String>,
    pub events: Vec<String>,
    /// Declared task ids.
    pub tasks: Vec<String>,
    /// Settings-schema paths that hold secrets.
    pub secrets: Vec<String>,
    /// Any `ui.contributions`, or a frame-backed `ui.config_tab`: plugin
    /// JavaScript will run in sandboxed frames.
    pub custom_ui: bool,
    pub contributions: Vec<PluginContributionDto>,
    pub rpc_exports: Vec<PluginRpcExportDto>,
    /// Some export has `mcp: true`: the admin decides MCP exposure separately.
    pub mcp_requested: bool,
    /// The version of an installed plugin with this id that approval replaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<String>,
    /// Set with `replaces`: the capability change against the installed approval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_diff: Option<PluginCapabilityDiffDto>,
    /// Non-fatal findings (unknown hook exports ignored, ...).
    pub warnings: Vec<String>,
}

/// `422` of `POST /api/v1/plugins`: every problem the validation found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginUploadErrorDto {
    pub error: String,
    pub problems: Vec<String>,
}

/// Body of `PUT /api/v1/plugins/{id}/approve`. Hash and capabilities must
/// equal the reviewed ones (`409` otherwise), so an approval never covers a
/// package the admin did not see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginApproveDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wasm_sha256: Option<String>,
    pub capabilities: Vec<String>,
    /// Admin approval for MCP exposure: offer the `mcp: true` exports as tools.
    #[serde(default)]
    pub mcp: bool,
}

/// Query of `DELETE /api/v1/plugins/{id}`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginUninstallQueryDto {
    /// Also delete the plugin's KV store.
    #[serde(default)]
    pub purge_kv: bool,
}

/// `ui.config_extensions[]`: fields shown inside a built-in config section,
/// stored in the plugin's settings under `ext.<section>.<field>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginConfigExtensionDto {
    pub section: String,
    /// Inline JSON Schema fragment (`SchemaForm` subset).
    pub schema: Value,
}

/// `ui.config_tab`: exactly one of `schema` / `element` is set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginConfigTabDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// The tab's schema, read from the package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
    /// Package-relative frame document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element: Option<String>,
}

/// One `ui.input_form` / `ui.output_form` section: the panel id and the schema
/// of the fields it adds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginFormSectionDto {
    /// The panel id: `main` and `options` extend the host's panels, any other
    /// id adds a panel after them.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_key: Option<String>,
    /// The section's schema, read from the package. Its properties are top-level
    /// keys of the input's / output's `plugin_config`.
    pub schema: Value,
}

/// The source editor tile of an input or output provider: `ui.input_tile` /
/// `ui.output_tile`, with the host's defaults filled in. It is sent exactly for
/// a plugin that declares the matching hook (`input.fetch` / `output.render`),
/// whether or not the manifest has the `ui` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSourceTileDto {
    /// Literal title; the manifest's `title`, else the plugin's `name`.
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_key: Option<String>,
    /// Name of an icon of the web UI's icon set. Never a URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Tooltip text; the manifest's tile `description`, else the plugin's
    /// `description`. Sanitized: control characters stripped, at most 512 chars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_key: Option<String>,
}

/// One plugin in `GET /api/v1/plugins/ui`: everything the web UI renders for
/// it. Listed for every plugin with a `ui:` part, and for every input or output
/// provider, whose state is `enabled`, `disabled` or `failed`, so disabled and
/// failed ones render greyed out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginUiDto {
    pub id: String,
    pub name: String,
    pub version: String,
    pub state: PluginStateDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// `ui.locales`; `en` is the fallback. Files: `ui/locales/<lang>.json`.
    pub locales: Vec<String>,
    /// Whether `ui.config_section` is set (rendered from `…/settings`).
    pub config_section: bool,
    pub config_extensions: Vec<PluginConfigExtensionDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_tab: Option<PluginConfigTabDto>,
    /// `ui.input_form`: the fields this plugin adds to a `type: plugin` input.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_form: Vec<PluginFormSectionDto>,
    /// `ui.output_form`: the fields this plugin adds to a `type: plugin` output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_form: Vec<PluginFormSectionDto>,
    /// Present iff the plugin provides inputs (`input.fetch`): its source
    /// editor tile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tile: Option<PluginSourceTileDto>,
    /// Present iff the plugin renders outputs (`output.render`): its source
    /// editor tile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tile: Option<PluginSourceTileDto>,
    pub contributions: Vec<PluginContributionDto>,
    pub rpc_exports: Vec<PluginRpcExportDto>,
}

/// Error body of `POST /api/v1/plugins/{id}/rpc/{method}` (any non-2xx).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginRpcErrorDto {
    pub error: String,
    /// Schema violations of the payload (`422`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<PluginFieldErrorDto>,
}

/// The `context` of the channel-carrying slots (explorer action, channel
/// detail, EPG overlay). **No URLs**: stream and catch-up URLs embed provider
/// credentials, and a frame can leak whatever it holds by navigating itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginChannelContextDto {
    pub virtual_id: u32,
    pub name: String,
    pub title: String,
    pub group: String,
    /// `live`, `movie`, `series`, ...
    pub item_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epg_channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_name: Option<String>,
}

/// The focused programme of the EPG overlay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginProgrammeContextDto {
    pub title: String,
    /// Unix seconds.
    pub start: i64,
    pub stop: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summary_round_trips_in_camel_case() {
        let summary = PluginSummaryDto {
            id: "demo".to_string(),
            name: "Demo".to_string(),
            version: Some("1.2.3".to_string()),
            description: None,
            category: Some("notifier".to_string()),
            state: PluginStateDto::PendingApproval,
            failure: Some("not approved yet".to_string()),
            enabled: false,
            priority: 100,
            hooks: vec!["playlist.after-filter".to_string()],
            capabilities: vec!["network".to_string()],
            abi: "1.0.0".to_string(),
            wasm_sha256: Some("ab".repeat(32)),
            settings_review: None,
            update: None,
            rollback: None,
            origin: None,
            update_policy: crate::model::PluginUpdatePolicy::Pinned,
        };
        let value = serde_json::to_value(&summary).unwrap();
        assert_eq!(value["updatePolicy"], "pinned");
        assert_eq!(value["state"], "pending_approval");
        assert_eq!(value["category"], "notifier");
        assert_eq!(value["wasmSha256"], "ab".repeat(32));
        assert!(value.get("description").is_none());
        assert_eq!(serde_json::from_value::<PluginSummaryDto>(value).unwrap(), summary);
    }

    #[test]
    fn stats_round_trip() {
        let stats = PluginStatsDto {
            calls: 7,
            failures: 1,
            budget_skips: 2,
            last_error: Some("trap".to_string()),
            breaker: PluginBreakerDto {
                state: PluginBreakerStateDto::HalfOpen,
                consecutive_failures: 3,
                failure_threshold: 5,
                forced_open: false,
                retry_after_ms: Some(250),
            },
            instances: Some(PluginInstancesDto { live: 1, max: 4, instantiations: 9, waits: 0 }),
            budget: PluginBudgetDto {
                total_mib: 1024,
                in_use_mib: 64,
                waits: 0,
                wait_ms_total: 0,
                wait_ms_max: 0,
                timeouts: 0,
            },
            kv: Some(PluginUsageDto { used_bytes: 10, quota_bytes: 100 }),
            images: None,
            log_dropped: 4,
            fingerprint: Some(PluginFingerprintDto {
                version: "1.2.3".to_string(),
                wasm_sha256: "cd".repeat(32),
                abi: "1.0.0".to_string(),
            }),
            hooks: vec![PluginHookStats {
                kept: 3,
                dropped: 1,
                ..PluginHookStats::new("demo", crate::model::PluginHook::AfterFilter)
            }],
        };
        let value = serde_json::to_value(&stats).unwrap();
        assert_eq!(value["breaker"]["state"], "half_open");
        assert_eq!(value["budgetSkips"], 2);
        assert_eq!(value["logDropped"], 4);
        assert!(value.get("images").is_none());
        assert_eq!(serde_json::from_value::<PluginStatsDto>(value).unwrap(), stats);
    }

    #[test]
    fn settings_tasks_and_logs_round_trip() {
        let settings = PluginSettingsDto {
            settings: json!({"token": "********"}),
            schema: None,
            secrets: vec![PluginSecretDto { path: "token".to_string(), set: true }],
            warnings: Vec::new(),
        };
        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(value["schema"], Value::Null);
        assert!(value.get("warnings").is_none());
        assert_eq!(serde_json::from_value::<PluginSettingsDto>(value).unwrap(), settings);

        let error = PluginSettingsErrorDto {
            error: "settings do not match the schema".to_string(),
            fields: vec![PluginFieldErrorDto { path: "port".to_string(), message: "must be an integer".to_string() }],
        };
        let value = serde_json::to_value(&error).unwrap();
        assert_eq!(value["fields"][0]["path"], "port");
        assert_eq!(serde_json::from_value::<PluginSettingsErrorDto>(value).unwrap(), error);

        let task = PluginTaskDto {
            id: "refresh".to_string(),
            title: "Refresh".to_string(),
            default_schedule: Some("0 0 * * * *".to_string()),
            max_exec_ms: None,
            concurrency: PluginTaskConcurrencyDto::Queue,
            respect_maintenance: true,
            last_run: None,
        };
        let value = serde_json::to_value(&task).unwrap();
        assert_eq!(value["defaultSchedule"], "0 0 * * * *");
        assert_eq!(value["lastRun"], Value::Null);
        assert_eq!(serde_json::from_value::<PluginTaskDto>(value).unwrap(), task);

        let entry = PluginLogEntryDto { ts_ms: 5, level: PluginLogLevelDto::Warn, message: "x".to_string() };
        let value = serde_json::to_value(&entry).unwrap();
        assert_eq!(value, json!({"tsMs": 5, "level": "warn", "message": "x"}));
        assert_eq!(serde_json::from_value::<PluginLogEntryDto>(value).unwrap(), entry);

        let query: PluginLogQueryDto = serde_json::from_value(json!({"level": "info", "limit": 10})).unwrap();
        assert_eq!(query, PluginLogQueryDto { level: Some(PluginLogLevelDto::Info), since: None, limit: Some(10) });
    }
}
