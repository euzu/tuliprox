//! `manifest.yml`: the typed plugin manifest and its validation.
//!
//! [`Manifest::parse`] reads the YAML, then checks every constraint of the
//! install-time validation that needs no other file, and reports **all**
//! problems in one `TuliproxError::Plugin`. Checks that need the package
//! (`plugin.wasm` exports, `config-schema.json`, locale files) belong to the
//! loader; [`Manifest::parse`] never reads files (`package` does, for the package-level
//! validation, at upload and at load). Unknown keys are ignored, so a manifest
//! with keys this host does not know still loads.
//!
//! The model lands ahead of the loader that consumes every field, hence the
//! module-wide `dead_code` allowance.
#![allow(dead_code)]

pub(crate) mod hook;
pub(crate) mod package;
pub(crate) mod secrets;

mod raw;
mod validate;

use hook::HookId;
use semver::Version;
use serde::Deserialize;
use shared::{
    error::TuliproxError,
    model::{EventKindMask, PlaylistItemType, PluginHook},
};

/// The host ABI major a manifest's `api_version` must equal.
pub(crate) const HOST_API_VERSION: u32 = tuliprox_plugin_api::ABI_MAJOR;

/// An explicit grant a plugin asks for (`capabilities:`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Capability {
    Network,
    Storage,
    EpgWrite,
    ImageCache,
    Notifications,
    StreamHeaders,
    Process,
    HttpRoutes,
}

impl Capability {
    /// Every capability, in manifest order.
    pub(crate) const ALL: [Self; 8] = [
        Self::Network,
        Self::Storage,
        Self::EpgWrite,
        Self::ImageCache,
        Self::Notifications,
        Self::StreamHeaders,
        Self::Process,
        Self::HttpRoutes,
    ];

    /// The manifest key.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::Storage => "storage",
            Self::EpgWrite => "epg_write",
            Self::ImageCache => "image_cache",
            Self::Notifications => "notifications",
            Self::StreamHeaders => "stream_headers",
            Self::Process => "process",
            Self::HttpRoutes => "http_routes",
        }
    }
}

/// `capabilities:`; everything off unless asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Capabilities {
    pub network: bool,
    pub storage: bool,
    pub epg_write: bool,
    pub image_cache: bool,
    pub notifications: bool,
    pub stream_headers: bool,
    pub process: bool,
    pub http_routes: bool,
}

impl Capabilities {
    pub(crate) const fn has(self, capability: Capability) -> bool {
        match capability {
            Capability::Network => self.network,
            Capability::Storage => self.storage,
            Capability::EpgWrite => self.epg_write,
            Capability::ImageCache => self.image_cache,
            Capability::Notifications => self.notifications,
            Capability::StreamHeaders => self.stream_headers,
            Capability::Process => self.process,
            Capability::HttpRoutes => self.http_routes,
        }
    }

    /// The capabilities the plugin asks for, for the admin's review.
    pub(crate) fn granted(self) -> Vec<Capability> {
        Capability::ALL.into_iter().filter(|capability| self.has(*capability)).collect()
    }
}

/// What a task does when its previous run is still going.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Concurrency {
    /// Drop the new fire.
    #[default]
    Skip,
    /// Keep one pending fire, never more.
    Queue,
}

/// One entry of `tasks:`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Task {
    pub id: String,
    pub title: String,
    /// A suggestion that prefills the schedule UI; the operator's config decides.
    pub default_schedule: Option<String>,
    /// The task's own deadline, clamped by `plugins.max_task_exec`.
    pub max_exec_ms: Option<u64>,
    pub concurrency: Concurrency,
    pub respect_maintenance: bool,
}

/// `provides:`; optional registrations.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Provides {
    /// Registers as a `type: plugin` input.
    pub input_type: bool,
    /// Registers an output format with this id.
    pub output_format: Option<String>,
}

/// `scope:`; the host-side pre-filter. Empty means all items.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Scope {
    /// Regexes on the group, as written (already checked to compile).
    pub groups: Vec<String>,
    pub item_types: Vec<PlaylistItemType>,
}

/// A place in the web UI a plugin can contribute a frame to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Slot {
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

impl Slot {
    pub(crate) const ALL: [Self; 9] = [
        Self::Menu,
        Self::ConfigSection,
        Self::ConfigTab,
        Self::SourceEditorTile,
        Self::DashboardCard,
        Self::PlaylistExplorerAction,
        Self::ChannelDetailPanel,
        Self::EpgOverlay,
        Self::SetupWizardStep,
    ];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Menu => "menu",
            Self::ConfigSection => "config-section",
            Self::ConfigTab => "config-tab",
            Self::SourceEditorTile => "source-editor-tile",
            Self::DashboardCard => "dashboard-card",
            Self::PlaylistExplorerAction => "playlist-explorer-action",
            Self::ChannelDetailPanel => "channel-detail-panel",
            Self::EpgOverlay => "epg-overlay",
            Self::SetupWizardStep => "setup-wizard-step",
        }
    }

    pub(crate) fn parse(name: &str) -> Option<Self> { Self::ALL.into_iter().find(|slot| slot.as_str() == name) }
}

/// `ui.config_section`: a schema-driven settings form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigSection {
    /// Package-relative path of the JSON Schema.
    pub schema: String,
}

/// An entry of `ui.config_extensions`: extra fields in an existing config section.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ConfigExtension {
    /// The built-in config section id.
    pub section: String,
    /// Inline JSON Schema fragment.
    pub schema: serde_json::Value,
}

/// What a config tab renders: exactly one of a schema or a sandboxed frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigTabSource {
    /// Package-relative JSON Schema path.
    Schema(String),
    /// Package-relative frame document.
    Element(String),
}

/// `ui.config_tab`: a whole new tab on the Config page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigTab {
    pub title: Option<String>,
    pub title_key: Option<String>,
    pub icon: Option<String>,
    pub source: ConfigTabSource,
}

/// An entry of `ui.contributions`: a sandboxed frame in a UI slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Contribution {
    pub slot: Slot,
    /// Literal fallback title.
    pub title: String,
    pub title_key: Option<String>,
    pub icon: Option<String>,
    /// Package-relative frame document.
    pub element: String,
}

/// Whether an exported function changes anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RpcKind {
    Read,
    /// Needs `SystemWrite`.
    Write,
}

/// An entry of `ui.rpc_exports`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RpcExport {
    pub name: String,
    pub kind: RpcKind,
    /// Offered as an MCP tool (still off until the admin enables it).
    pub mcp: bool,
    pub description: Option<String>,
    /// Package-relative JSON Schema of the payload; present whenever `mcp` is.
    pub input_schema: Option<String>,
}

/// One `ui.input_form` / `ui.output_form` section: fields a plugin adds to a
/// `type: plugin` input or output, in the panel `id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FormSection {
    /// The panel id: `main` and `options` extend the host's panels, any other
    /// id adds a panel after them.
    pub id: String,
    pub title: Option<String>,
    pub title_key: Option<String>,
    /// Name of an icon of the web UI's icon set, for the panel's sidebar entry.
    pub icon: Option<String>,
    /// Shown at the top of the section.
    pub description: Option<String>,
    pub description_key: Option<String>,
    /// Package-relative path of the section's JSON Schema.
    pub schema: String,
    /// Buttons under the section's fields; each calls one of `ui.rpc_exports`.
    pub actions: Vec<FormAction>,
}

/// A button of a form section. Clicking it calls the rpc export `rpc` with the
/// form's current, unsaved `plugin_config` and shows what comes back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FormAction {
    /// Unique in its section.
    pub id: String,
    pub title: String,
    pub title_key: Option<String>,
    /// The name of an entry of `ui.rpc_exports`.
    pub rpc: String,
}

/// `ui.input_tile` / `ui.output_tile`: the provider's tile in the source
/// editor sidebar. Every field is optional; the host falls back to the
/// plugin's `name` and `description`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SourceTile {
    pub title: Option<String>,
    pub title_key: Option<String>,
    /// Name of an icon of the web UI's icon set.
    pub icon: Option<String>,
    pub description: Option<String>,
    pub description_key: Option<String>,
}

/// `ui:`.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Ui {
    pub config_section: Option<ConfigSection>,
    pub config_extensions: Vec<ConfigExtension>,
    pub config_tab: Option<ConfigTab>,
    pub locales: Vec<String>,
    pub contributions: Vec<Contribution>,
    pub rpc_exports: Vec<RpcExport>,
    /// Fields a `type: plugin` input shows, from this plugin.
    pub input_form: Vec<FormSection>,
    /// Fields a `type: plugin` output shows, from this plugin.
    pub output_form: Vec<FormSection>,
    /// The look of the input tile; needs `input.fetch`.
    pub input_tile: Option<SourceTile>,
    /// The look of the output tile; needs `output.render`.
    pub output_tile: Option<SourceTile>,
}

/// What happens to the target when a hook call fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum FailMode {
    /// Log, count, and carry on with the items untouched.
    #[default]
    Continue,
    /// Do not write the target.
    AbortTarget,
}

/// `resources:`; the plugin's requests. The host clamps them to `plugins.*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Resources {
    pub max_memory_mb: Option<u32>,
    pub max_exec_ms: Option<u64>,
    pub max_instances: Option<u16>,
    pub fail_mode: FailMode,
}

/// A validated `manifest.yml`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Manifest {
    pub id: String,
    pub name: String,
    pub version: Version,
    pub api_version: u32,
    pub author: Option<String>,
    pub homepage: Option<String>,
    pub description: Option<String>,
    /// What kind of plugin this is, for the marketplace and plugin lists to filter by.
    pub category: Option<String>,
    pub min_tuliprox_version: Option<Version>,
    pub capabilities: Capabilities,
    /// Declared hook ids, in manifest order, without duplicates.
    pub hooks: Vec<HookId>,
    /// `events:` as the host's subscription mask.
    pub events: EventKindMask,
    /// `events:` as written.
    pub event_names: Vec<String>,
    pub tasks: Vec<Task>,
    pub routes: Vec<HttpRoute>,
    pub provides: Provides,
    pub scope: Scope,
    pub ui: Ui,
    pub resources: Resources,
}

impl Manifest {
    /// Parse and validate `text`; `dir_name` is the folder the manifest sits
    /// in, which the `id` must equal so a plugin has exactly one identity on disk.
    ///
    /// # Errors
    /// `TuliproxError::Plugin` for malformed YAML, or listing **every**
    /// violated constraint, one per line.
    pub(crate) fn parse(text: &str, dir_name: &str) -> Result<Self, TuliproxError> {
        let raw: raw::RawManifest = serde_saphyr::from_str(text)
            .map_err(|err| TuliproxError::Plugin(format!("invalid manifest.yml in `{dir_name}`: {err}")))?;
        validate::build(raw, dir_name).map_err(|errors| {
            let lines = errors.iter().fold(String::new(), |mut lines, error| {
                lines.push_str("\n  - ");
                lines.push_str(error);
                lines
            });
            TuliproxError::Plugin(format!("invalid manifest.yml in `{dir_name}`:{lines}"))
        })
    }

    /// The hooks the playlist pipeline can run, as the shared host interface
    /// names them. Ids without a pipeline stage are validated but not listed.
    pub(crate) fn pipeline_hooks(&self) -> Vec<PluginHook> {
        let mut hooks = Vec::new();
        for hook in self.hooks.iter().filter_map(|hook| hook.pipeline_hook()) {
            if !hooks.contains(&hook) {
                hooks.push(hook);
            }
        }
        hooks
    }

    /// Whether the manifest lists `hook`.
    pub(crate) fn has_hook(&self, hook: HookId) -> bool { self.hooks.contains(&hook) }

    /// The `ui.config_extensions` sections that are not in `known`, the host's
    /// list of built-in config section ids. The manifest alone cannot say which
    /// sections exist, so the loader passes them in.
    pub(crate) fn unknown_config_sections<'a>(&'a self, known: &[&str]) -> Vec<&'a str> {
        self.ui
            .config_extensions
            .iter()
            .map(|extension| extension.section.as_str())
            .filter(|section| !known.contains(section))
            .collect()
    }

    /// Whether this host is older than the manifest's `min_tuliprox_version`.
    pub(crate) fn requires_newer_host(&self, host: &Version) -> bool {
        self.min_tuliprox_version.as_ref().is_some_and(|minimum| minimum > host)
    }
}

#[cfg(test)]
mod tests;

/// Authentication is host-managed unless explicitly delegated to the plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RouteAuth {
    #[default]
    Host,
    Plugin,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HttpRoute {
    pub id: String,
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub auth: RouteAuth,
}
