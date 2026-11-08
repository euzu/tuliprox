//! The manifest as written: every field optional and every enum a string, so
//! validation can report all problems at once instead of stopping at the first
//! serde error. `validate` turns it into the typed model.

use super::Capabilities;
use serde::{Deserialize, Deserializer};

/// A key that is present but empty (`provides:` with every child commented
/// out) reads as null; treat it as the default instead of a type error.
fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct RawManifest {
    pub id: Option<String>,
    pub name: Option<String>,
    pub version: Option<String>,
    pub api_version: Option<u32>,
    pub author: Option<String>,
    pub homepage: Option<String>,
    pub description: Option<String>,
    pub category: Option<String>,
    pub min_tuliprox_version: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub capabilities: Capabilities,
    #[serde(default, deserialize_with = "null_default")]
    pub hooks: Vec<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub events: Vec<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub tasks: Vec<RawTask>,
    #[serde(default, deserialize_with = "null_default")]
    pub routes: Vec<super::HttpRoute>,
    #[serde(default, deserialize_with = "null_default")]
    pub provides: RawProvides,
    #[serde(default, deserialize_with = "null_default")]
    pub scope: RawScope,
    #[serde(default, deserialize_with = "null_default")]
    pub ui: RawUi,
    #[serde(default, deserialize_with = "null_default")]
    pub resources: RawResources,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct RawTask {
    pub id: Option<String>,
    pub title: Option<String>,
    pub default_schedule: Option<String>,
    pub max_exec_ms: Option<u64>,
    pub concurrency: Option<String>,
    pub respect_maintenance: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct RawProvides {
    #[serde(default, deserialize_with = "null_default")]
    pub input_type: bool,
    pub output_format: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct RawScope {
    #[serde(default, deserialize_with = "null_default")]
    pub groups: Vec<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub item_types: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct RawUi {
    pub config_section: Option<RawConfigSection>,
    #[serde(default, deserialize_with = "null_default")]
    pub config_extensions: Vec<RawConfigExtension>,
    pub config_tab: Option<RawConfigTab>,
    #[serde(default, deserialize_with = "null_default")]
    pub locales: Vec<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub contributions: Vec<RawContribution>,
    #[serde(default, deserialize_with = "null_default")]
    pub rpc_exports: Vec<RawRpcExport>,
    #[serde(default, deserialize_with = "null_default")]
    pub input_form: Vec<RawFormSection>,
    #[serde(default, deserialize_with = "null_default")]
    pub output_form: Vec<RawFormSection>,
    pub input_tile: Option<RawSourceTile>,
    pub output_tile: Option<RawSourceTile>,
}

/// `ui.input_tile` / `ui.output_tile`: how the provider's tile looks in the
/// source editor sidebar.
#[derive(Debug, Default, Deserialize)]
pub(super) struct RawSourceTile {
    pub title: Option<String>,
    pub title_key: Option<String>,
    pub icon: Option<String>,
    pub description: Option<String>,
    pub description_key: Option<String>,
}

/// One entry of `ui.input_form` / `ui.output_form`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct RawFormSection {
    pub id: Option<String>,
    pub title: Option<String>,
    pub title_key: Option<String>,
    pub icon: Option<String>,
    pub description: Option<String>,
    pub description_key: Option<String>,
    pub schema: Option<String>,
    #[serde(default)]
    pub actions: Vec<RawFormAction>,
}

/// One entry of a form section's `actions`: a button that calls an rpc export.
#[derive(Debug, Default, Deserialize)]
pub(super) struct RawFormAction {
    pub id: Option<String>,
    pub title: Option<String>,
    pub title_key: Option<String>,
    pub rpc: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct RawConfigSection {
    pub schema: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct RawConfigExtension {
    pub section: Option<String>,
    pub schema: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct RawConfigTab {
    pub title: Option<String>,
    pub title_key: Option<String>,
    pub icon: Option<String>,
    pub schema: Option<String>,
    pub element: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct RawContribution {
    pub slot: Option<String>,
    pub title: Option<String>,
    pub title_key: Option<String>,
    pub icon: Option<String>,
    pub element: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct RawRpcExport {
    pub name: Option<String>,
    pub kind: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub mcp: bool,
    pub description: Option<String>,
    pub input_schema: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct RawResources {
    pub max_memory_mb: Option<u32>,
    pub max_exec_ms: Option<u64>,
    pub max_instances: Option<u16>,
    pub fail_mode: Option<String>,
}
