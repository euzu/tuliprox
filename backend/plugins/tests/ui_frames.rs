//! The plugin UI frame files and the contribution list, through `PluginSystem`
//! (plan §18 *W6b seam freeze* "Frame serving"). The route's headers are
//! asserted here from the same function the route uses; the route itself is
//! tested in `app`.

mod common;
mod ui_support;

use shared::model::{PluginSlotDto, PluginStateDto};
use tuliprox_plugins::PluginSystem;
use ui_support::{stage, ID};

#[test]
fn an_enabled_plugins_files_are_served_with_their_mime_type() {
    let Some(staged) = stage(Some(true)) else { return };
    let html = staged.system.ui_asset(ID, "index.html").expect("index.html");
    assert_eq!(html.mime, "text/html; charset=utf-8");
    assert!(html.path.starts_with(staged.plugin_dir.canonicalize().unwrap().join("ui")));
    assert_eq!(staged.system.ui_asset(ID, "assets/app.js").expect("app.js").mime, "text/javascript; charset=utf-8");
}

#[test]
fn traversal_dotfiles_and_unknown_types_are_a_404() {
    let Some(staged) = stage(Some(true)) else { return };
    for path in [
        "",
        "/index.html",
        "../manifest.yml",
        "../plugin.wasm",
        "assets/../index.html",
        "./index.html",
        "assets//app.js",
        "assets/",
        "..\\manifest.yml",
        "assets\\app.js",
        "index.html\0",
        ".secret.js",
        "assets/../.secret.js",
        "data.xml",
        "missing.html",
        "assets",
    ] {
        assert!(staged.system.ui_asset(ID, path).is_none(), "{path:?} must be a 404");
    }
}

#[test]
fn raw_paths_with_encoded_separators_are_refused_before_any_lookup() {
    for raw in [
        "/plugins/probe-bare/ui/assets%2fapp.js",
        "/plugins/probe-bare/ui/assets%2Fapp.js",
        "/plugins/probe-bare/ui/assets%5capp.js",
        "/plugins/probe-bare/ui/%2e%2e%2fmanifest.yml",
        "/plugins/probe-bare/ui/index.html%00.png",
    ] {
        assert!(!PluginSystem::ui_path_is_safe(raw), "{raw}");
    }
    assert!(PluginSystem::ui_path_is_safe("/plugins/probe-bare/ui/assets/app.js"));
}

#[cfg(unix)]
#[test]
fn a_symlink_that_leaves_ui_is_a_404() {
    use std::os::unix::fs::symlink;
    let Some(staged) = stage(Some(true)) else { return };
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("token.txt"), "secret").unwrap();
    let ui = staged.plugin_dir.join("ui");
    symlink(outside.path().join("token.txt"), ui.join("leak.txt")).unwrap();
    symlink(outside.path(), ui.join("linked")).unwrap();
    symlink(staged.plugin_dir.join("manifest.yml"), ui.join("manifest.txt")).unwrap();

    assert!(staged.system.ui_asset(ID, "leak.txt").is_none());
    assert!(staged.system.ui_asset(ID, "linked/token.txt").is_none());
    assert!(staged.system.ui_asset(ID, "manifest.txt").is_none());
    assert!(staged.system.ui_asset(ID, "index.html").is_some(), "the control case still resolves");
}

#[test]
fn only_an_enabled_plugin_is_served() {
    let Some(disabled) = stage(Some(false)) else { return };
    assert!(disabled.system.ui_asset(ID, "index.html").is_none(), "a disabled plugin");
    let Some(unlisted) = stage(None) else { return };
    assert!(unlisted.system.ui_asset(ID, "index.html").is_none(), "a plugin with no config entry");
    let Some(enabled) = stage(Some(true)) else { return };
    assert!(enabled.system.ui_asset("other-plugin", "index.html").is_none(), "an unknown id");
    assert!(enabled.system.ui_asset("../probe-bare", "index.html").is_none(), "an id that is a path");
    assert!(enabled.system.ui_asset(ID, "index.html").is_some());
}

#[test]
fn the_frame_headers_are_the_frozen_six() {
    let headers = PluginSystem::frame_headers("\"0.1.0-1-2\"");
    let get =
        |name: &str| headers.iter().find(|(key, _)| key.as_str() == name).map(|(_, value)| value.to_str().unwrap());
    assert_eq!(headers.len(), 6);
    assert_eq!(get("content-security-policy"), Some(PluginSystem::FRAME_CSP));
    assert_eq!(
        PluginSystem::FRAME_CSP,
        "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
         connect-src 'none'; frame-ancestors 'self'; sandbox allow-scripts"
    );
    assert_eq!(get("x-content-type-options"), Some("nosniff"));
    assert_eq!(get("referrer-policy"), Some("no-referrer"));
    assert_eq!(get("cross-origin-resource-policy"), Some("cross-origin"));
    assert_eq!(get("cache-control"), Some("no-cache"));
    assert_eq!(get("etag"), Some("\"0.1.0-1-2\""));
}

#[test]
fn the_etag_follows_the_version_the_file_and_its_length() {
    let Some(staged) = stage(Some(true)) else { return };
    let asset = staged.system.ui_asset(ID, "assets/app.js").unwrap();
    let meta = std::fs::metadata(&asset.path).unwrap();
    let etag = asset.etag(&meta);
    assert!(etag.starts_with("\"0.1.0-") && etag.ends_with(&format!("-{}\"", meta.len())), "{etag}");
}

#[test]
fn the_ui_list_shows_enabled_and_disabled_plugins_but_not_unlisted_ones() {
    let Some(enabled) = stage(Some(true)) else { return };
    let list = enabled.system.api_ui();
    assert_eq!(list.len(), 1);
    assert_eq!((list[0].id.as_str(), list[0].state), (ID, PluginStateDto::Enabled));
    assert_eq!(list[0].contributions.len(), 1);
    assert_eq!(list[0].contributions[0].slot, PluginSlotDto::Menu);
    assert_eq!(list[0].contributions[0].element, "ui/index.html");
    assert_eq!(list[0].rpc_exports, Vec::new());

    let Some(disabled) = stage(Some(false)) else { return };
    assert_eq!(disabled.system.api_ui()[0].state, PluginStateDto::Disabled, "greyed out, not hidden");

    let Some(unlisted) = stage(None) else { return };
    assert_eq!(unlisted.system.api_ui(), Vec::new());
}
