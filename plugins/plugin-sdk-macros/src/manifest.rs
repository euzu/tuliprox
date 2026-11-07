//! Reading the `hooks:` list of the plugin's `manifest.yml` at compile time.

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct HooksOnly {
    #[serde(default)]
    hooks: Vec<String>,
}

/// The manifest file a plugin crate is built against.
pub(crate) fn locate(relative: Option<&str>) -> Result<PathBuf, String> {
    let dir = std::env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is not set; build with cargo")?;
    Ok(Path::new(&dir).join(relative.unwrap_or("manifest.yml")))
}

/// The `hooks:` ids, in file order. Unknown top-level keys are ignored, as the
/// registry ignores them.
pub(crate) fn read_hooks(path: &Path) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("cannot read the plugin manifest `{}`: {err}", path.display()))?;
    parse_hooks(&text).map_err(|err| format!("cannot parse the plugin manifest `{}`: {err}", path.display()))
}

pub(crate) fn parse_hooks(text: &str) -> Result<Vec<String>, String> {
    let parsed: HooksOnly = serde_saphyr::from_str(text).map_err(|err| err.to_string())?;
    Ok(parsed.hooks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_are_read_and_other_keys_ignored() {
        let text = "id: demo\nname: Demo\nversion: 0.1.0\napi_version: 1\nhooks:\n  - playlist.after-filter\n  - events.on-event\nevents: [playlist.update]\n";
        assert_eq!(parse_hooks(text).unwrap(), vec!["playlist.after-filter", "events.on-event"]);
    }

    #[test]
    fn a_missing_hooks_key_is_an_empty_list() {
        assert_eq!(parse_hooks("id: demo\n").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn malformed_yaml_is_an_error() {
        assert!(parse_hooks("hooks: [unclosed").is_err());
    }
}
