//! The `marketplace.json` catalog (`format_version: 1`). The catalog is untrusted input: it is parsed leniently (unknown
//! fields ignored), every text is sanitized and cut, and an invalid plugin or
//! release is skipped with a warning instead of failing the whole catalog.
//! There is no hash, checksum or signature field and none is read: the host
//! pins its own hash when the admin approves the downloaded package.

use crate::install::{is_installable_id, sanitize_text};
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use shared::model::{MarketplaceInstalledDto, MarketplacePluginDto, MarketplaceReleaseDto};
use std::collections::HashSet;

/// The only catalog format this host reads.
pub(crate) const FORMAT_VERSION: u32 = 1;
const MAX_WARNINGS: usize = 50;
const MAX_CAPABILITIES: usize = 16;

/// A parsed catalog as it is cached: only valid plugins and releases remain.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Catalog {
    pub(crate) name: Option<String>,
    pub(crate) plugins: Vec<Plugin>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Plugin {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    pub(crate) author: Option<String>,
    pub(crate) license: Option<String>,
    pub(crate) category: Option<String>,
    pub(crate) homepage: Option<String>,
    pub(crate) releases: Vec<Release>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Release {
    pub(crate) version: String,
    pub(crate) package_url: String,
    pub(crate) min_tuliprox_version: Option<String>,
    pub(crate) plugin_api: u32,
    pub(crate) capabilities: Vec<String>,
    pub(crate) changelog: Option<String>,
    pub(crate) published: Option<String>,
}

/// Leniently read plugin: every field optional so a missing required field is
/// a per-entry warning, and unknown fields fall away.
#[derive(Deserialize)]
struct RawPlugin {
    id: Option<String>,
    name: Option<String>,
    description: Option<String>,
    author: Option<String>,
    license: Option<String>,
    category: Option<String>,
    homepage: Option<String>,
    releases: Option<Vec<Value>>,
}

#[derive(Deserialize)]
struct RawRelease {
    version: Option<String>,
    package_url: Option<String>,
    min_tuliprox_version: Option<String>,
    plugin_api: Option<u32>,
    capabilities: Option<Vec<String>>,
    changelog: Option<String>,
    published: Option<String>,
}

struct Warnings {
    list: Vec<String>,
    dropped: usize,
}

impl Warnings {
    fn push(&mut self, message: String) {
        if self.list.len() < MAX_WARNINGS {
            self.list.push(message);
        } else {
            self.dropped += 1;
        }
    }

    fn finish(mut self) -> Vec<String> {
        if self.dropped > 0 {
            self.list.push(format!("{} more warnings omitted", self.dropped));
        }
        self.list
    }
}

fn text(value: Option<String>, max_chars: usize) -> Option<String> {
    value.map(|value| sanitize_text(&value, max_chars)).filter(|value| !value.is_empty())
}

/// Parse a catalog. `allow_http` is the developer knob that lets `http://`
/// package URLs through.
///
/// # Errors
/// A catalog-level problem: not JSON, `format_version` missing, 0 or newer than
/// this host reads, or a duplicate plugin id. The previous cache is kept then.
pub(crate) fn parse(bytes: &[u8], allow_http: bool) -> Result<(Catalog, Vec<String>), String> {
    let root: Value = serde_json::from_slice(bytes).map_err(|_| "the catalog is not valid JSON".to_string())?;
    let Some(object) = root.as_object() else {
        return Err("the catalog is not a JSON object".to_string());
    };
    let version = object.get("format_version").and_then(Value::as_u64).unwrap_or(0);
    if version == 0 {
        return Err("format_version is missing or invalid; it must be 1".to_string());
    }
    if version > u64::from(FORMAT_VERSION) {
        return Err(format!("format_version {version} is newer than this Tuliprox understands ({FORMAT_VERSION})"));
    }
    let name = text(object.get("name").and_then(Value::as_str).map(str::to_string), 80);
    let entries = match object.get("plugins") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(entries)) => entries.clone(),
        Some(_) => return Err("`plugins` is not a list".to_string()),
    };

    let mut warnings = Warnings { list: Vec::new(), dropped: 0 };
    let mut seen: HashSet<String> = HashSet::new();
    let mut plugins = Vec::new();
    for (index, entry) in entries.into_iter().enumerate() {
        let Ok(raw) = serde_json::from_value::<RawPlugin>(entry) else {
            warnings.push(format!("plugin #{} skipped: not a valid plugin entry", index + 1));
            continue;
        };
        let id = raw.id.clone().unwrap_or_default();
        if !is_installable_id(&id) {
            warnings.push(format!("plugin #{} skipped: invalid id `{}`", index + 1, sanitize_text(&id, 48)));
            continue;
        }
        if !seen.insert(id.clone()) {
            return Err(format!("duplicate plugin id `{id}`"));
        }
        let Some(plugin_name) = text(raw.name.clone(), 80) else {
            warnings.push(format!("plugin `{id}` skipped: `name` is missing"));
            continue;
        };
        let releases = read_releases(&id, raw.releases.clone().unwrap_or_default(), allow_http, &mut warnings);
        if releases.is_empty() {
            warnings.push(format!("plugin `{id}` skipped: no valid release"));
            continue;
        }
        plugins.push(Plugin {
            id,
            name: plugin_name,
            description: text(raw.description, 512),
            author: text(raw.author, 80),
            license: text(raw.license, 64),
            category: text(raw.category, 40),
            homepage: text(raw.homepage, 512).filter(|url| url.starts_with("https://") || url.starts_with("http://")),
            releases,
        });
    }
    Ok((Catalog { name, plugins }, warnings.finish()))
}

fn read_releases(id: &str, values: Vec<Value>, allow_http: bool, warnings: &mut Warnings) -> Vec<Release> {
    let mut seen: HashSet<Version> = HashSet::new();
    let mut releases = Vec::new();
    for value in values {
        let Ok(raw) = serde_json::from_value::<RawRelease>(value) else {
            warnings.push(format!("plugin `{id}`: a release was skipped, it is not a valid entry"));
            continue;
        };
        let version_text = raw.version.clone().unwrap_or_default();
        let Ok(version) = Version::parse(&version_text) else {
            warnings.push(format!(
                "plugin `{id}`: release `{}` skipped, not a semver version",
                sanitize_text(&version_text, 48)
            ));
            continue;
        };
        let at = |problem: &str| format!("plugin `{id}` {version}: release skipped, {problem}");
        if !seen.insert(version.clone()) {
            warnings.push(at("duplicate version"));
            continue;
        }
        let Some(plugin_api) = raw.plugin_api else {
            warnings.push(at("`plugin_api` is missing"));
            continue;
        };
        if let Err(problem) = check_package_url(raw.package_url.as_deref().unwrap_or_default(), allow_http) {
            warnings.push(at(problem));
            continue;
        }
        let min = match raw.min_tuliprox_version.as_deref().map(str::trim).filter(|min| !min.is_empty()) {
            None => None,
            Some(min) => {
                let Ok(min) = Version::parse(min) else {
                    warnings.push(at("`min_tuliprox_version` is not semver"));
                    continue;
                };
                Some(min.to_string())
            }
        };
        let mut capabilities: Vec<String> = raw
            .capabilities
            .unwrap_or_default()
            .iter()
            .map(|name| sanitize_text(name, 32))
            .filter(|name| !name.is_empty())
            .collect();
        capabilities.truncate(MAX_CAPABILITIES);
        releases.push(Release {
            version: version.to_string(),
            package_url: raw.package_url.unwrap_or_default().trim().to_string(),
            min_tuliprox_version: min,
            plugin_api,
            capabilities,
            changelog: text(raw.changelog, 2000),
            published: text(raw.published, 32),
        });
    }
    releases
}

/// `https` (or `http` under the developer knob), a host, no credentials.
fn check_package_url(url: &str, allow_http: bool) -> Result<(), &'static str> {
    let Ok(parsed) = reqwest::Url::parse(url.trim()) else { return Err("`package_url` is not a valid URL") };
    match parsed.scheme() {
        "https" => {}
        "http" if allow_http => {}
        _ => return Err("`package_url` must be an https URL"),
    }
    if parsed.host_str().is_none() {
        return Err("`package_url` has no host");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("`package_url` must not carry credentials");
    }
    Ok(())
}

/// Whether `release` installs on this host: `min_tuliprox_version` (absent =
/// any) at most the running version and `plugin_api` equal to the host's ABI
/// major.
///
/// # Errors
/// The reason the release cannot be installed.
pub(crate) fn compatibility(release: &Release, host_version: &Version, abi_major: u32) -> Result<(), String> {
    if release.plugin_api != abi_major {
        return Err(format!(
            "built for plugin API {}, this Tuliprox supports plugin API {abi_major}",
            release.plugin_api
        ));
    }
    if let Some(min) = release.min_tuliprox_version.as_deref().and_then(|min| Version::parse(min).ok()) {
        if min > *host_version {
            return Err(format!("requires Tuliprox {min} or newer (running {host_version})"));
        }
    }
    Ok(())
}

/// Releases as DTOs, newest first with compatible ones before incompatible.
pub(crate) fn release_dtos(releases: &[Release], host_version: &Version, abi_major: u32) -> Vec<MarketplaceReleaseDto> {
    let mut rows: Vec<(Version, MarketplaceReleaseDto)> = releases
        .iter()
        .filter_map(|release| {
            let version = Version::parse(&release.version).ok()?;
            let verdict = compatibility(release, host_version, abi_major);
            Some((
                version,
                MarketplaceReleaseDto {
                    version: release.version.clone(),
                    min_tuliprox_version: release.min_tuliprox_version.clone(),
                    plugin_api: release.plugin_api,
                    capabilities: release.capabilities.clone(),
                    changelog: release.changelog.clone(),
                    published: release.published.clone(),
                    compatible: verdict.is_ok(),
                    incompatible_reason: verdict.err(),
                },
            ))
        })
        .collect();
    rows.sort_by(|(left_version, left), (right_version, right)| {
        right.compatible.cmp(&left.compatible).then_with(|| right_version.cmp(left_version))
    });
    rows.into_iter().map(|(_, dto)| dto).collect()
}

/// One plugin of one source as the API shows it; `source` is `(id, name)`.
pub(crate) fn plugin_dto(
    plugin: &Plugin,
    source: (&str, &str),
    host_version: &Version,
    abi_major: u32,
    installed: Option<&MarketplaceInstalledDto>,
    also_in: Vec<String>,
) -> MarketplacePluginDto {
    MarketplacePluginDto {
        source: source.0.to_string(),
        source_name: source.1.to_string(),
        id: plugin.id.clone(),
        name: plugin.name.clone(),
        description: plugin.description.clone(),
        author: plugin.author.clone(),
        license: plugin.license.clone(),
        category: plugin.category.clone(),
        homepage: plugin.homepage.clone(),
        releases: release_dtos(&plugin.releases, host_version, abi_major),
        installed: installed.cloned(),
        also_in,
    }
}

#[cfg(test)]
mod tests;
