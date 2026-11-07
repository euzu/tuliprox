//! First start of the server: a `config.yml` without a `plugins:` section gets a minimal one.
//!
//! The plugin engine exists only when the section says `enabled: true` at startup, and the
//! Plugin Manager's routes answer `404` without it. The section is appended as text, so the
//! comments and values the operator wrote stay as they are. A section the operator wrote
//! (even `enabled: false`) is never touched.
use log::{info, warn};
use std::path::Path;
use tokio::io::AsyncWriteExt;

const PLUGINS_SECTION: &str = "# WebAssembly plugins. Added on the first start so the Plugin Manager works;\n\
# set `enabled: false` to turn the plugin system off.\n\
plugins:\n  enabled: true\n";

/// A top-level `plugins` key, quoted or not. Comments and nested keys do not count.
fn has_plugins_section(text: &str) -> bool {
    text.lines().any(|line| {
        !line.starts_with([' ', '\t', '#', '-'])
            && line.split_once(':').is_some_and(|(key, _)| key.trim_end().trim_matches(['"', '\'']) == "plugins")
    })
}

/// The text to append to `text`, `None` when it has a `plugins:` section already.
fn missing_section(text: &str) -> Option<String> {
    if has_plugins_section(text) {
        return None;
    }
    let separator = match text {
        "" => "",
        text if text.ends_with("\n\n") => "",
        text if text.ends_with('\n') => "\n",
        _ => "\n\n",
    };
    Some(format!("{separator}{PLUGINS_SECTION}"))
}

/// Append the minimal `plugins:` section to the config file when it has none. A file that cannot
/// be read or written (read-only mount) is left alone: the server starts with the plugins off,
/// and the Plugin Manager says so.
pub async fn ensure_section(config_file_path: &str) {
    let path = Path::new(config_file_path);
    let Ok(text) = tokio::fs::read_to_string(path).await else {
        return;
    };
    let Some(addition) = missing_section(&text) else {
        return;
    };
    let appended = async {
        let mut file = tokio::fs::OpenOptions::new().append(true).open(path).await?;
        file.write_all(addition.as_bytes()).await?;
        file.flush().await
    }
    .await;
    match appended {
        Ok(()) => info!("Added a `plugins:` section to {config_file_path}; set `enabled: false` to turn plugins off"),
        Err(err) => warn!(
            "Could not add a `plugins:` section to {config_file_path}: {err}. Add `plugins:` with `enabled: true` \
             yourself to use the Plugin Manager"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_top_level_section_only() {
        assert!(has_plugins_section("api:\n  port: 1\nplugins:\n  enabled: false\n"));
        assert!(has_plugins_section("plugins: {}\n"));
        assert!(has_plugins_section("\"plugins\":\n  enabled: true\n"));
        assert!(!has_plugins_section("#plugins:\n#  enabled: false\n"));
        assert!(!has_plugins_section("api:\n  plugins: 1\n"));
        assert!(!has_plugins_section("- plugins: 1\n"));
        assert!(!has_plugins_section("plugins_dir: x\n"));
    }

    #[test]
    fn leaves_a_configured_section_alone() {
        assert_eq!(missing_section("plugins:\n  enabled: false\n"), None);
    }

    #[test]
    fn appends_after_a_blank_line() {
        let added = missing_section("api:\n  port: 1\n").unwrap();
        assert!(added.starts_with("\n# WebAssembly plugins."));
        assert!(added.ends_with("plugins:\n  enabled: true\n"));
        assert!(missing_section("api:\n  port: 1").unwrap().starts_with("\n\n#"));
        assert!(missing_section("api:\n  port: 1\n\n").unwrap().starts_with("# "));
        assert!(missing_section("").unwrap().starts_with("# "));
    }

    #[tokio::test]
    async fn appends_once_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yml");
        std::fs::write(&path, "# my comment\napi:\n  port: 1\n").unwrap();
        let path_str = path.to_str().unwrap();

        ensure_section(path_str).await;
        let once = std::fs::read_to_string(&path).unwrap();
        assert!(once.starts_with("# my comment\napi:\n  port: 1\n\n"));
        assert!(once.ends_with("plugins:\n  enabled: true\n"));

        ensure_section(path_str).await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), once);
    }
}
