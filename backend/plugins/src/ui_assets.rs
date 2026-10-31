//! Plugin UI: the `GET /api/v1/plugins/ui` contribution list and the
//! path-safe lookup of `/plugins/{id}/ui/{path…}` files (plan §9 *UI trust
//! boundary*, §11, §18 *W6b seam freeze* "Frame serving"). W6b lane R.
//!
//! Frame files are served without auth, so everything here fails closed: a
//! path, plugin or file that is not clearly safe is `None`, which the route
//! answers with a plain `404`. Resolution is pure filesystem work over the
//! plugin directory; the route in `app` opens and streams the file.

use crate::{
    manifest::{ConfigTabSource, Manifest, RpcKind, Slot},
    registry::{PluginRecord, PluginState},
    system::PluginSystem,
};
use http::{header, HeaderName, HeaderValue};
use serde_json::Value;
use shared::model::{
    PluginConfigExtensionDto, PluginConfigTabDto, PluginContributionDto, PluginRpcExportDto, PluginRpcKindDto,
    PluginSlotDto, PluginStateDto, PluginUiDto,
};
use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::UNIX_EPOCH,
};

/// The CSP of every frame document and asset. It is a header and not a `<meta>`
/// tag so that it also covers a document opened as a top-level page: the
/// `sandbox` directive gives that page an opaque origin, which keeps plugin
/// script away from the admin token in `LocalStorage` (plan §9).
pub(crate) const FRAME_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
                                    img-src 'self' data:; connect-src 'none'; frame-ancestors 'self'; \
                                    sandbox allow-scripts";

/// The largest file the frame route serves.
pub(crate) const MAX_ASSET_BYTES: u64 = 16 * 1024 * 1024;

/// The largest package JSON file (an rpc input schema, a config tab schema) the
/// host reads.
const MAX_PACKAGE_JSON_BYTES: u64 = 256 * 1024;

/// Longest `rpc_exports[].description` handed to the web UI, in chars.
const MAX_DESCRIPTION_CHARS: usize = 512;

/// File extensions the frame route serves, with their `Content-Type`.
const MIME_TYPES: [(&str, &str); 15] = [
    ("html", "text/html; charset=utf-8"),
    ("css", "text/css; charset=utf-8"),
    ("js", "text/javascript; charset=utf-8"),
    ("mjs", "text/javascript; charset=utf-8"),
    ("json", "application/json"),
    ("svg", "image/svg+xml"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("ico", "image/x-icon"),
    ("woff", "font/woff"),
    ("woff2", "font/woff2"),
    ("txt", "text/plain; charset=utf-8"),
];

/// The `Content-Type` for a lower-case file extension, if it is on the allowlist.
fn mime_for(extension: &str) -> Option<&'static str> {
    MIME_TYPES.iter().find(|(known, _)| *known == extension).map(|(_, mime)| *mime)
}

/// Whether the **raw** request path is free of the encodings the frame route
/// refuses outright: an encoded slash (`%2f`), an encoded backslash (`%5c`), an
/// encoded NUL (`%00`), a literal backslash or NUL. The router decodes a path
/// capture once, so after that an encoded slash would look like a separator;
/// this has to look at the path as it arrived.
pub(crate) fn raw_path_is_safe(raw: &str) -> bool {
    if raw.contains(['\\', '\0']) {
        return false;
    }
    let lower = raw.to_ascii_lowercase();
    !["%2f", "%5c", "%00"].iter().any(|encoded| lower.contains(encoded))
}

/// The segments of a **decoded** path below `ui/`, or `None` when any rule is
/// broken: an empty path, an absolute path, an empty segment (`a//b`, a trailing
/// `/`), a `.` or `..` or any other dot-prefixed segment, a backslash, a control
/// character (NUL included), or a `:` (a Windows drive or stream name).
pub(crate) fn safe_segments(decoded: &str) -> Option<Vec<&str>> {
    if decoded.is_empty() || decoded.starts_with('/') {
        return None;
    }
    let mut segments = Vec::new();
    for segment in decoded.split('/') {
        let unsafe_segment = segment.is_empty()
            || segment.starts_with('.')
            || segment.contains(['\\', ':'])
            || segment.chars().any(char::is_control);
        if unsafe_segment {
            return None;
        }
        segments.push(segment);
    }
    Some(segments)
}

/// A file the frame route may serve.
pub struct UiAsset {
    /// The canonical path, inside `<plugin dir>/ui/`.
    pub path: PathBuf,
    /// The `Content-Type`.
    pub mime: &'static str,
    /// The plugin version, part of the `ETag`.
    pub version: String,
}

impl UiAsset {
    /// `"<plugin version>-<file mtime>-<file len>"`, from the metadata of the
    /// handle the route actually opened. The mtime is in milliseconds.
    #[must_use]
    pub fn etag(&self, meta: &fs::Metadata) -> String {
        let modified = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |elapsed| elapsed.as_millis());
        format!("\"{}-{modified}-{}\"", self.version, meta.len())
    }
}

/// Resolve `decoded` below `<plugin_dir>/ui/` for a plugin at `version`.
///
/// `None` unless the path obeys [`safe_segments`], its extension is on the
/// allowlist, and the file, once symlinks are resolved, is a regular file of at
/// most [`MAX_ASSET_BYTES`] that still lies inside `ui/` (and `ui/` inside the
/// plugin directory) and is reached through no dot-prefixed name.
pub(crate) fn resolve_in(plugin_dir: &Path, version: &str, decoded: &str) -> Option<UiAsset> {
    let segments = safe_segments(decoded)?;
    let extension = Path::new(segments.last()?).extension()?.to_str()?.to_ascii_lowercase();
    let mime = mime_for(&extension)?;

    let base = fs::canonicalize(plugin_dir).ok()?;
    let ui_root = fs::canonicalize(base.join("ui")).ok()?;
    if !ui_root.starts_with(&base) {
        return None;
    }
    let mut candidate = ui_root.clone();
    candidate.extend(&segments);
    let path = fs::canonicalize(candidate).ok()?;
    let relative = path.strip_prefix(&ui_root).ok()?;
    let plain = relative.components().all(
        |component| matches!(component, Component::Normal(name) if name.to_str().is_some_and(|n| !n.starts_with('.'))),
    );
    if !plain {
        return None;
    }
    let meta = fs::metadata(&path).ok()?;
    (meta.is_file() && meta.len() <= MAX_ASSET_BYTES).then(|| UiAsset { path, mime, version: version.to_string() })
}

/// Read and parse a JSON file of the package at `dir`. `relative` is a manifest
/// path (already refused when absolute or containing `..`); the file must still
/// resolve inside `dir` after symlinks, be a regular file, and stay under
/// [`MAX_PACKAGE_JSON_BYTES`].
pub(crate) fn read_package_json(dir: &Path, relative: &str) -> Option<Value> {
    let base = fs::canonicalize(dir).ok()?;
    let path = fs::canonicalize(base.join(relative)).ok()?;
    if !path.starts_with(&base) {
        return None;
    }
    let meta = fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAX_PACKAGE_JSON_BYTES {
        return None;
    }
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// `text` without control characters, cut at `max` chars.
pub(crate) fn sanitize_text(text: &str, max: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(max).collect()
}

impl PluginSystem {
    /// The CSP header value of every frame response.
    pub const FRAME_CSP: &'static str = FRAME_CSP;

    /// The largest file the frame route serves, in bytes.
    pub const MAX_UI_ASSET_BYTES: u64 = MAX_ASSET_BYTES;

    /// The six headers of every `200` and `304` of the frame route and of
    /// `bridge.js` (plan §18 *W6b seam freeze*). `etag` is already quoted.
    #[must_use]
    pub fn frame_headers(etag: &str) -> Vec<(HeaderName, HeaderValue)> {
        let mut headers = vec![
            (header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(FRAME_CSP)),
            (header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
            (header::REFERRER_POLICY, HeaderValue::from_static("no-referrer")),
            // `cross-origin`: a sandboxed frame has an opaque origin, so for it even
            // its own sub-assets are cross-origin, and `same-origin` blocks them.
            (HeaderName::from_static("cross-origin-resource-policy"), HeaderValue::from_static("cross-origin")),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ];
        // The ETag is built from a version, two numbers and quotes, so it is
        // always a valid value; the response is still safe without it.
        if let Ok(value) = HeaderValue::from_str(etag) {
            headers.push((header::ETAG, value));
        }
        headers
    }

    /// Whether the raw request path (as it arrived, still percent-encoded) is
    /// free of encoded slashes and backslashes, encoded or literal NULs and
    /// literal backslashes. The route checks it before it looks anything up.
    #[must_use]
    pub fn ui_path_is_safe(raw_path: &str) -> bool { raw_path_is_safe(raw_path) }

    /// The file `path` (percent-decoded once) of plugin `id`'s `ui/`
    /// directory, if the plugin is `enabled` and every rule of
    /// [`resolve_in`] holds. `None` is a `404`. Blocking filesystem work: call
    /// it from `spawn_blocking`.
    #[must_use]
    pub fn ui_asset(&self, id: &str, path: &str) -> Option<UiAsset> {
        let snapshot = self.registry().snapshot();
        let record = snapshot.get(id)?;
        if record.state() != PluginState::Enabled {
            return None;
        }
        let manifest = record.manifest.as_deref()?;
        resolve_in(&record.dir, &manifest.version.to_string(), path)
    }

    /// Everything the web UI renders for each plugin with a `ui:` part whose
    /// state is `enabled`, `disabled` or `failed` (the last two grey out).
    #[must_use]
    pub fn api_ui(&self) -> Vec<PluginUiDto> {
        self.registry().snapshot().plugins.iter().filter_map(|record| ui_entry(record)).collect()
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

const fn rpc_kind_dto(kind: RpcKind) -> PluginRpcKindDto {
    match kind {
        RpcKind::Read => PluginRpcKindDto::Read,
        RpcKind::Write => PluginRpcKindDto::Write,
    }
}

/// Whether the manifest has anything the web UI would render.
fn has_ui(manifest: &Manifest) -> bool {
    let ui = &manifest.ui;
    ui.config_section.is_some()
        || !ui.config_extensions.is_empty()
        || ui.config_tab.is_some()
        || !ui.contributions.is_empty()
        || !ui.rpc_exports.is_empty()
}

fn ui_entry(record: &PluginRecord) -> Option<PluginUiDto> {
    let manifest = record.manifest.as_deref().filter(|manifest| has_ui(manifest))?;
    let state = match record.state() {
        PluginState::Enabled => PluginStateDto::Enabled,
        PluginState::Disabled => PluginStateDto::Disabled,
        PluginState::Failed => PluginStateDto::Failed,
        _ => return None,
    };
    let ui = &manifest.ui;
    Some(PluginUiDto {
        id: record.id.to_string(),
        name: manifest.name.clone(),
        version: manifest.version.to_string(),
        state,
        failure: record.failure(),
        locales: ui.locales.clone(),
        config_section: ui.config_section.is_some(),
        config_extensions: ui
            .config_extensions
            .iter()
            .map(|extension| PluginConfigExtensionDto {
                section: extension.section.clone(),
                schema: extension.schema.clone(),
            })
            .collect(),
        config_tab: ui.config_tab.as_ref().map(|tab| {
            let (schema, element) = match &tab.source {
                ConfigTabSource::Schema(path) => (read_package_json(&record.dir, path), None),
                ConfigTabSource::Element(element) => (None, Some(element.clone())),
            };
            PluginConfigTabDto {
                title: tab.title.clone(),
                title_key: tab.title_key.clone(),
                icon: tab.icon.clone(),
                schema,
                element,
            }
        }),
        contributions: ui
            .contributions
            .iter()
            .map(|contribution| PluginContributionDto {
                slot: slot_dto(contribution.slot),
                title: contribution.title.clone(),
                title_key: contribution.title_key.clone(),
                icon: contribution.icon.clone(),
                element: contribution.element.clone(),
            })
            .collect(),
        rpc_exports: ui
            .rpc_exports
            .iter()
            .map(|export| PluginRpcExportDto {
                name: export.name.clone(),
                kind: rpc_kind_dto(export.kind),
                mcp: export.mcp,
                description: export.description.as_deref().map(|text| sanitize_text(text, MAX_DESCRIPTION_CHARS)),
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// A plugin dir with `ui/` holding the given files (relative to `ui/`).
    fn plugin(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        for (name, body) in files {
            let path = dir.path().join("ui").join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
        fs::write(dir.path().join("manifest.yml"), "secret manifest").unwrap();
        dir
    }

    fn resolve(dir: &TempDir, path: &str) -> Option<UiAsset> { resolve_in(dir.path(), "1.2.3", path) }

    #[test]
    fn a_plain_file_resolves_with_its_mime_type() {
        let dir = plugin(&[("index.html", "<p>"), ("assets/app.js", "1"), ("assets/logo.PNG", "x")]);
        let html = resolve(&dir, "index.html").unwrap();
        assert_eq!(html.mime, "text/html; charset=utf-8");
        assert!(html.path.starts_with(fs::canonicalize(dir.path().join("ui")).unwrap()));
        assert_eq!(resolve(&dir, "assets/app.js").unwrap().mime, "text/javascript; charset=utf-8");
        assert_eq!(resolve(&dir, "assets/logo.PNG").unwrap().mime, "image/png", "extensions compare lower-cased");
    }

    #[test]
    fn traversal_and_malformed_paths_are_refused() {
        let dir = plugin(&[("index.html", "<p>"), ("a/b.js", "1"), (".hidden.js", "x")]);
        let refused = [
            "",
            "/index.html",
            "../manifest.yml",
            "a/../index.html",
            "./index.html",
            "a/./b.js",
            "a//b.js",
            "a/b.js/",
            "..",
            ".",
            "..\\manifest.yml",
            "a\\b.js",
            "index.html\0",
            "C:/Windows/win.ini",
            "index.html::$DATA",
            ".hidden.js",
            "a/.b.js",
            "../../etc/passwd.txt",
        ];
        for path in refused {
            assert!(resolve(&dir, path).is_none(), "{path:?} must be refused");
        }
        assert!(resolve(&dir, "a/b.js").is_some(), "the control case resolves");
    }

    #[test]
    fn raw_paths_with_encoded_separators_or_nul_are_refused() {
        for raw in [
            "/plugins/x/ui/a%2fb.js",
            "/plugins/x/ui/a%2Fb.js",
            "/plugins/x/ui/a%5cb.js",
            "/plugins/x/ui/a%5Cb.js",
            "/plugins/x/ui/a%00.js",
            "/plugins/x/ui/a\\b.js",
            "/plugins/x/ui/a\0b.js",
            "/plugins/x%2f..%2fy/ui/a.js",
        ] {
            assert!(!raw_path_is_safe(raw), "{raw:?}");
        }
        for raw in ["/plugins/x/ui/index.html", "/plugins/x/ui/a%20b.js", "/plugins/x/ui/%2e%2e/a.js"] {
            // `%2e%2e` decodes to `..`, which `safe_segments` refuses on the decoded path.
            assert!(raw_path_is_safe(raw), "{raw:?}");
        }
        assert!(safe_segments("../a.js").is_none());
    }

    #[test]
    fn an_extension_off_the_allowlist_is_refused() {
        let dir = plugin(&[
            ("run.sh", "x"),
            ("data.xml", "x"),
            ("page.htm", "x"),
            ("noext", "x"),
            ("trailing.", "x"),
            ("map.js.map", "x"),
            ("wasm.wasm", "x"),
            ("ok.txt", "x"),
        ]);
        for path in ["run.sh", "data.xml", "page.htm", "noext", "trailing.", "map.js.map", "wasm.wasm"] {
            assert!(resolve(&dir, path).is_none(), "{path}");
        }
        assert!(resolve(&dir, "ok.txt").is_some());
    }

    #[test]
    fn every_allowlisted_extension_has_a_mime_type() {
        for extension in [
            "html", "css", "js", "mjs", "json", "svg", "png", "jpg", "jpeg", "gif", "webp", "ico", "woff", "woff2",
            "txt",
        ] {
            assert!(mime_for(extension).is_some(), "{extension}");
        }
        assert!(mime_for("php").is_none());
    }

    #[test]
    fn a_missing_file_a_directory_and_a_missing_ui_dir_are_refused() {
        let dir = plugin(&[("sub/x.js", "1")]);
        assert!(resolve(&dir, "nope.js").is_none());
        assert!(resolve(&dir, "sub.js").is_none());
        fs::create_dir(dir.path().join("ui/folder.js")).unwrap();
        assert!(resolve(&dir, "folder.js").is_none(), "a directory named like a script is not a file");
        let bare = TempDir::new().unwrap();
        assert!(resolve(&bare, "index.html").is_none());
    }

    #[test]
    fn a_file_over_the_cap_is_refused() {
        let dir = plugin(&[("small.js", "1")]);
        let big = fs::File::create(dir.path().join("ui/big.js")).unwrap();
        big.set_len(MAX_ASSET_BYTES + 1).unwrap();
        assert!(resolve(&dir, "big.js").is_none());
        let exact = fs::File::create(dir.path().join("ui/exact.js")).unwrap();
        exact.set_len(MAX_ASSET_BYTES).unwrap();
        assert!(resolve(&dir, "exact.js").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_ui_is_refused_and_one_inside_is_allowed() {
        use std::os::unix::fs::symlink;
        let dir = plugin(&[("real.js", "1"), ("sub/inner.js", "2"), (".dot.js", "3")]);
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("secret.txt"), "token").unwrap();
        let ui = dir.path().join("ui");
        symlink(outside.path().join("secret.txt"), ui.join("leak.txt")).unwrap();
        symlink(outside.path(), ui.join("linked")).unwrap();
        symlink(dir.path().join("manifest.yml"), ui.join("manifest.txt")).unwrap();
        symlink(ui.join("real.js"), ui.join("alias.js")).unwrap();
        symlink(ui.join(".dot.js"), ui.join("dotalias.js")).unwrap();

        assert!(resolve(&dir, "leak.txt").is_none(), "a file symlink out of ui/");
        assert!(resolve(&dir, "linked/secret.txt").is_none(), "a directory symlink out of ui/");
        assert!(resolve(&dir, "manifest.txt").is_none(), "a symlink to the plugin's own manifest");
        assert!(resolve(&dir, "alias.js").is_some(), "a symlink that stays inside ui/");
        assert!(resolve(&dir, "dotalias.js").is_none(), "a symlink onto a dotfile");
    }

    #[cfg(unix)]
    #[test]
    fn a_ui_dir_that_is_a_symlink_out_of_the_plugin_is_refused() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("index.html"), "x").unwrap();
        symlink(outside.path(), dir.path().join("ui")).unwrap();
        assert!(resolve(&dir, "index.html").is_none());
    }

    #[test]
    fn the_etag_is_version_mtime_and_length() {
        let dir = plugin(&[("index.html", "12345")]);
        let asset = resolve(&dir, "index.html").unwrap();
        let meta = fs::metadata(&asset.path).unwrap();
        let etag = asset.etag(&meta);
        assert!(etag.starts_with("\"1.2.3-") && etag.ends_with("-5\""), "{etag}");
    }

    #[test]
    fn the_frame_headers_carry_the_frozen_csp_and_the_other_five() {
        assert!(FRAME_CSP.ends_with("sandbox allow-scripts"));
        assert_eq!(
            FRAME_CSP,
            "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
             connect-src 'none'; frame-ancestors 'self'; sandbox allow-scripts"
        );
        let headers = PluginSystem::frame_headers("\"1.2.3-9-5\"");
        let get = |name: &str| headers.iter().find(|(key, _)| key.as_str() == name).map(|(_, v)| v.to_str().unwrap());
        assert_eq!(get("content-security-policy"), Some(FRAME_CSP));
        assert_eq!(get("x-content-type-options"), Some("nosniff"));
        assert_eq!(get("referrer-policy"), Some("no-referrer"));
        assert_eq!(get("cross-origin-resource-policy"), Some("cross-origin"));
        assert_eq!(get("cache-control"), Some("no-cache"));
        assert_eq!(get("etag"), Some("\"1.2.3-9-5\""));
        assert_eq!(headers.len(), 6);
    }

    #[test]
    fn package_json_is_read_only_from_inside_the_package() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("rpc")).unwrap();
        fs::write(dir.path().join("rpc/in.json"), r#"{"type":"object"}"#).unwrap();
        fs::write(dir.path().join("rpc/bad.json"), "not json").unwrap();
        assert_eq!(read_package_json(dir.path(), "rpc/in.json"), Some(serde_json::json!({"type": "object"})));
        assert!(read_package_json(dir.path(), "rpc/bad.json").is_none());
        assert!(read_package_json(dir.path(), "rpc/missing.json").is_none());
        assert!(read_package_json(dir.path(), "../outside.json").is_none());
    }

    #[test]
    fn descriptions_lose_control_characters_and_are_cut() {
        assert_eq!(sanitize_text("a\u{0}b\nc\u{7}d", 512), "abcd");
        assert_eq!(sanitize_text(&"x".repeat(600), 512).chars().count(), 512);
    }
}
