//! DTOs of the repository marketplace API (`/api/v1/plugins/repositories…`,
//! `/api/v1/plugins/marketplace…`), shared with the web UI. The request body of a
//! repository create or update is the config entry itself,
//! [`crate::model::PluginRepositoryDto`].

use serde::{Deserialize, Serialize};

/// The request body of `POST /api/v1/plugins/repositories`. The id and the name
/// of a new source come from its catalog, so only the url is given. A new
/// source is enabled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRepositoryAddDto {
    pub url: String,
}

/// Where an installed plugin came from: the marketplace source id and the
/// release version installed from it. Absent for an upload or a directory drop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginOriginDto {
    pub source: String,
    pub version: String,
}

/// A configured marketplace source with its sync state: one row of
/// `GET /api/v1/plugins/repositories`, and the answer of a create, update or
/// sync. `url` is shown as written (`${env:VAR}` unresolved).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginRepositoryInfoDto {
    pub id: String,
    pub name: String,
    pub url: String,
    pub enabled: bool,
    /// Unix seconds of the last successful sync; absent before the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync: Option<i64>,
    /// The error of the last sync attempt, cleared by a successful one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Plugins in the cached catalog; 0 before the first sync.
    pub plugin_count: usize,
}

/// `GET /api/v1/plugins/marketplace` query. Every filter is optional and they
/// combine with AND.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketplaceQueryDto {
    /// Case-insensitive substring of id, name, description or author.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// A source id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// One release of a catalog plugin, with the host's compatibility verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceReleaseDto {
    pub version: String,
    /// The lowest Tuliprox version the release runs on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_tuliprox_version: Option<String>,
    /// The plugin API (ABI) major the release is built for.
    pub plugin_api: u32,
    /// Capability names the catalog says the package requests. Informational:
    /// the review of the downloaded package is what the admin approves.
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changelog: Option<String>,
    /// Release date as the catalog writes it (`YYYY-MM-DD`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    pub compatible: bool,
    /// Why an incompatible release cannot be installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incompatible_reason: Option<String>,
}

/// The installed copy of a catalog plugin, for the "installed" badges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceInstalledDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<PluginOriginDto>,
}

/// One plugin of one source. A plugin id offered by several sources appears
/// once per source; `also_in` names the other sources, and an install must
/// name the source it installs from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplacePluginDto {
    pub source: String,
    pub source_name: String,
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    /// Newest first, compatible releases before incompatible ones.
    pub releases: Vec<MarketplaceReleaseDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed: Option<MarketplaceInstalledDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_in: Vec<String>,
}

/// `GET /api/v1/plugins/marketplace`: the filtered plugins of every enabled
/// source, the source list with its sync state, and every category seen.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceDto {
    pub plugins: Vec<MarketplacePluginDto>,
    pub sources: Vec<PluginRepositoryInfoDto>,
    pub categories: Vec<String>,
}

/// Body of `POST /api/v1/plugins/marketplace/install`. Answered with the
/// existing upload review (`PluginUploadReviewDto`); approval and enabling
/// then use the existing routes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketplaceInstallDto {
    pub source: String,
    pub id: String,
    pub version: String,
}
