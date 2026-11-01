//! The package-level half of the install-time validation (plan §4 steps 4-6):
//! the files a manifest points at. Runs when a `.tlpx` is uploaded and again
//! on every load, so a package dropped in by hand is held to the same rules.
//!
//! * Step 4: every settings schema (`ui.config_section`, a schema
//!   `ui.config_tab`) and every `ui.config_extensions` fragment is a schema the
//!   host's validator can honour, and no secret property carries a `default`.
//! * Step 5: every declared locale file exists and is a flat string map, and
//!   every `title_key` resolves in `en`, the fallback locale.
//! * Step 6: every `ui.config_extensions[].section` is a built-in config section.

use super::{ConfigExtension, ConfigTab, ConfigTabSource, Manifest};
use crate::{
    manifest::secrets::secret_property_paths,
    settings::{schema_version, unsupported_constructs},
};
use serde_json::{Map, Value};
use std::{fs, io::Read, path::Path};

/// The web UI's `ConfigPage` ids, which `ui.config_extensions[].section` may name.
pub(crate) const KNOWN_CONFIG_SECTIONS: [&str; 15] = [
    "main",
    "api",
    "log",
    "schedules",
    "messaging",
    "webui",
    "reverse_proxy",
    "hdhomerun",
    "proxy",
    "ipcheck",
    "mcp",
    "panel",
    "recording",
    "metadata_update",
    "library",
];

/// The fallback locale of `ui/locales`.
const FALLBACK_LOCALE: &str = "en";
/// A schema or locale file larger than this is refused.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// The `ui/` entry the host reserves for the frame bridge.
const UI_RESERVED_DIR: &str = "_sdk";

/// The settings key under which `ui.config_extensions` values are stored.
pub(crate) const EXTENSION_KEY: &str = "ext";

/// The package-relative schema files that describe the plugin's own settings:
/// `ui.config_section`'s, then a schema `ui.config_tab`'s (when different).
pub(crate) fn settings_schema_paths(manifest: &Manifest) -> Vec<&str> {
    let mut paths = Vec::new();
    if let Some(section) = &manifest.ui.config_section {
        paths.push(section.schema.as_str());
    }
    if let Some(ConfigTab { source: ConfigTabSource::Schema(path), .. }) = &manifest.ui.config_tab {
        if !paths.contains(&path.as_str()) {
            paths.push(path.as_str());
        }
    }
    paths
}

/// The schema settings are saved against: the first of [`settings_schema_paths`].
pub(crate) fn settings_schema_path(manifest: &Manifest) -> Option<&str> {
    settings_schema_paths(manifest).into_iter().next()
}

/// Read a package JSON file, refusing anything over [`MAX_FILE_BYTES`].
fn read_json(dir: &Path, relative: &str, what: &str) -> Result<Value, String> {
    let path = dir.join(relative);
    let file = fs::File::open(&path).map_err(|err| format!("cannot read {what} `{relative}`: {err}"))?;
    let mut text = String::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|err| format!("cannot read {what} `{relative}`: {err}"))?;
    if text.len() as u64 > MAX_FILE_BYTES {
        return Err(format!("{what} `{relative}` is larger than {} KiB", MAX_FILE_BYTES / 1024));
    }
    serde_json::from_str(&text).map_err(|err| format!("{what} `{relative}` is not JSON: {err}"))
}

/// The path of a secret in `ext.<section>`, in the secret-path grammar.
fn extension_secret_path(section: &str, path: &str) -> String {
    if path == "(root)" {
        format!("{EXTENSION_KEY}.{section}")
    } else {
        format!("{EXTENSION_KEY}.{section}.{path}")
    }
}

/// The secret property paths of the plugin's whole settings object: the
/// schemas' own, and each extension's under `ext.<section>.`.
///
/// # Errors
/// A secret with a `default`, in any of them.
pub(crate) fn settings_secret_paths(schemas: &[Value], manifest: &Manifest) -> Result<Vec<String>, String> {
    let mut paths: Vec<String> = Vec::new();
    let mut push = |path: String| {
        if !paths.contains(&path) {
            paths.push(path);
        }
    };
    for schema in schemas {
        for path in secret_property_paths(schema).map_err(|err| err.message().to_string())? {
            push(path);
        }
    }
    for extension in &manifest.ui.config_extensions {
        for path in secret_property_paths(&extension.schema).map_err(|err| err.message().to_string())? {
            push(extension_secret_path(&extension.section, &path));
        }
    }
    Ok(paths)
}

/// The fragments of `section`, merged into one object schema: the union of
/// their `properties` and `required`, so a value saved under `ext.<section>`
/// is checked against everything every fragment asked for.
pub(crate) fn extension_schema(extensions: &[ConfigExtension], section: &str) -> Option<Value> {
    let fragments: Vec<&ConfigExtension> = extensions.iter().filter(|extension| extension.section == section).collect();
    let first = fragments.first()?;
    if fragments.len() == 1 {
        return Some(first.schema.clone());
    }
    let mut properties = Map::new();
    let mut required: Vec<Value> = Vec::new();
    for fragment in &fragments {
        if let Some(Value::Object(map)) = fragment.schema.get("properties") {
            properties.extend(map.iter().map(|(key, value)| (key.clone(), value.clone())));
        }
        if let Some(Value::Array(names)) = fragment.schema.get("required") {
            for name in names {
                if !required.contains(name) {
                    required.push(name.clone());
                }
            }
        }
    }
    let mut merged = Map::new();
    merged.insert("type".to_string(), Value::String("object".to_string()));
    merged.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        merged.insert("required".to_string(), Value::Array(required));
    }
    Some(Value::Object(merged))
}

fn check_schema(schema: &Value, label: &str, problems: &mut Vec<String>) {
    if !schema.is_object() {
        problems.push(format!("{label} must be a JSON Schema object"));
        return;
    }
    let unsupported = unsupported_constructs(schema);
    if !unsupported.is_empty() {
        problems.push(format!("{label} uses constructs outside the supported subset: {}", unsupported.join(", ")));
    }
    if let Err(problem) = schema_version(schema) {
        problems.push(format!("{label}: {problem}"));
    }
}

/// A locale file: a flat map of string to string.
fn read_locale(dir: &Path, locale: &str) -> Result<Map<String, Value>, String> {
    let relative = format!("ui/locales/{locale}.json");
    match read_json(dir, &relative, "locale file")? {
        Value::Object(map) if map.values().all(Value::is_string) => Ok(map),
        Value::Object(_) => Err(format!("locale file `{relative}` must map keys to strings only")),
        _ => Err(format!("locale file `{relative}` must be a JSON object")),
    }
}

/// Plan §4 steps 4-6 over the files of the package at `dir`.
///
/// Returns the secret property paths of the settings (schemas and
/// extensions), or **every** problem found.
pub(crate) fn check_package(dir: &Path, manifest: &Manifest) -> Result<Vec<String>, Vec<String>> {
    let mut problems = Vec::new();

    // `ui/_sdk/` is where the host serves the frame bridge (`_sdk/bridge.js`).
    if fs::symlink_metadata(dir.join("ui").join(UI_RESERVED_DIR)).is_ok() {
        problems.push(format!("`ui/{UI_RESERVED_DIR}` is reserved for the host and must not be in the package"));
    }

    // Step 4: settings schemas and extension fragments.
    let mut schemas = Vec::new();
    for relative in settings_schema_paths(manifest) {
        match read_json(dir, relative, "config schema") {
            Ok(schema) => {
                check_schema(&schema, &format!("config schema `{relative}`"), &mut problems);
                if !manifest.ui.config_extensions.is_empty() && schema.pointer("/properties/ext").is_some() {
                    problems.push(format!(
                        "config schema `{relative}` must not declare `{EXTENSION_KEY}`: it holds `ui.config_extensions` values"
                    ));
                }
                schemas.push(schema);
            }
            Err(problem) => problems.push(problem),
        }
    }
    for extension in &manifest.ui.config_extensions {
        check_schema(&extension.schema, &format!("ui.config_extensions `{}` schema", extension.section), &mut problems);
    }
    let secret_paths = match settings_secret_paths(&schemas, manifest) {
        Ok(paths) => paths,
        Err(problem) => {
            problems.push(problem);
            Vec::new()
        }
    };

    // Step 5: locale files, and the title keys resolving in the fallback locale.
    let mut fallback: Option<Map<String, Value>> = None;
    for locale in &manifest.ui.locales {
        match read_locale(dir, locale) {
            Ok(strings) if locale == FALLBACK_LOCALE => fallback = Some(strings),
            Ok(_) => {}
            Err(problem) => problems.push(problem),
        }
    }
    let tab_key = manifest.ui.config_tab.iter().filter_map(|tab| tab.title_key.as_deref());
    let slot_keys = manifest.ui.contributions.iter().filter_map(|contribution| contribution.title_key.as_deref());
    for key in tab_key.chain(slot_keys) {
        if !manifest.ui.locales.iter().any(|locale| locale == FALLBACK_LOCALE) {
            problems.push(format!("title_key `{key}` needs the `{FALLBACK_LOCALE}` locale in `ui.locales`"));
        } else if fallback.as_ref().is_some_and(|strings| !strings.contains_key(key)) {
            problems.push(format!("title_key `{key}` is missing from `ui/locales/{FALLBACK_LOCALE}.json`"));
        }
    }

    // Step 6: extensions target built-in sections.
    for section in manifest.unknown_config_sections(&KNOWN_CONFIG_SECTIONS) {
        problems.push(format!(
            "ui.config_extensions section `{section}` is not a built-in config section; known sections are {}",
            KNOWN_CONFIG_SECTIONS.join(", ")
        ));
    }

    if problems.is_empty() {
        Ok(secret_paths)
    } else {
        Err(problems)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BASE: &str = "name: Demo\nversion: 0.1.0\napi_version: 1\n";

    fn manifest(extra: &str) -> Manifest { Manifest::parse(&format!("id: demo\n{BASE}{extra}"), "demo").unwrap() }

    fn package(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, content) in files {
            let target = dir.path().join(path);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, content).unwrap();
        }
        dir
    }

    #[test]
    fn a_package_file_under_ui_sdk_is_refused() {
        let manifest = manifest("");
        let dir = package(&[("ui/_sdk/bridge.js", "evil()")]);
        let problems = check_package(dir.path(), &manifest).unwrap_err();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("ui/_sdk"), "{problems:?}");
        let clean = package(&[("ui/index.html", "<p>")]);
        assert!(check_package(clean.path(), &manifest).is_ok());
    }

    #[test]
    fn extension_sections_must_be_built_in() {
        let manifest = manifest(
            "ui:\n  config_extensions:\n    - section: webhooks\n      schema: {type: object}\n    - section: reverse_proxy\n      schema: {type: object}\n",
        );
        let dir = package(&[]);
        let problems = check_package(dir.path(), &manifest).unwrap_err();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("`webhooks`"), "{problems:?}");
    }

    #[test]
    fn a_known_section_passes() {
        let manifest = manifest("ui:\n  config_extensions:\n    - section: messaging\n      schema: {type: object}\n");
        assert_eq!(check_package(package(&[]).path(), &manifest).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn unsupported_schema_constructs_and_secret_defaults_are_problems() {
        let manifest = manifest("ui:\n  config_section:\n    schema: config-schema.json\n");
        let dir = package(&[(
            "config-schema.json",
            r#"{"type":"object","properties":{"a":{"oneOf":[]},"token":{"type":"string","writeOnly":true,"default":"x"}}}"#,
        )]);
        let problems = check_package(dir.path(), &manifest).unwrap_err();
        assert!(problems.iter().any(|problem| problem.contains("oneOf")), "{problems:?}");
        assert!(problems.iter().any(|problem| problem.contains("token")), "{problems:?}");
    }

    #[test]
    fn a_settings_schema_version_must_be_a_positive_integer() {
        let manifest = manifest("ui:\n  config_section:\n    schema: config-schema.json\n");
        let ok = package(&[("config-schema.json", r#"{"type":"object","version":2}"#)]);
        assert!(check_package(ok.path(), &manifest).is_ok());
        for bad in [r#""2""#, "0", "1.5"] {
            let dir = package(&[("config-schema.json", &format!(r#"{{"type":"object","version":{bad}}}"#))]);
            let problems = check_package(dir.path(), &manifest).unwrap_err();
            assert!(problems.iter().any(|problem| problem.contains("`version` must be")), "{bad}: {problems:?}");
        }
    }

    #[test]
    fn extension_secrets_are_listed_under_their_section() {
        let manifest = manifest(
            "ui:\n  config_extensions:\n    - section: proxy\n      schema:\n        type: object\n        properties:\n          key: {type: string, writeOnly: true}\n",
        );
        let paths = check_package(package(&[]).path(), &manifest).unwrap();
        assert_eq!(paths, ["ext.proxy.key"]);
    }

    #[test]
    fn a_secret_default_in_an_extension_is_a_problem() {
        let manifest = manifest(
            "ui:\n  config_extensions:\n    - section: proxy\n      schema:\n        type: object\n        properties:\n          key: {type: string, writeOnly: true, default: abc}\n",
        );
        let problems = check_package(package(&[]).path(), &manifest).unwrap_err();
        assert!(problems.iter().any(|problem| problem.contains("key")), "{problems:?}");
    }

    #[test]
    fn locale_files_must_exist_be_flat_and_hold_the_title_keys() {
        let extra = "ui:\n  locales: [en, de, fr]\n  contributions:\n    - slot: menu\n      title: T\n      title_key: menu.title\n      element: ui/a.html\n";
        let manifest = manifest(extra);
        // en lacks the key, de is nested, fr is missing
        let dir = package(&[("ui/locales/en.json", r#"{"other":"x"}"#), ("ui/locales/de.json", r#"{"a":{"b":"c"}}"#)]);
        let problems = check_package(dir.path(), &manifest).unwrap_err();
        assert!(problems.iter().any(|problem| problem.contains("menu.title") && problem.contains("en.json")));
        assert!(problems.iter().any(|problem| problem.contains("de.json") && problem.contains("strings")));
        assert!(problems.iter().any(|problem| problem.contains("fr.json")));

        let ok = package(&[
            ("ui/locales/en.json", r#"{"menu.title":"x"}"#),
            ("ui/locales/de.json", "{}"),
            ("ui/locales/fr.json", "{}"),
        ]);
        assert!(check_package(ok.path(), &manifest).is_ok());
    }

    #[test]
    fn a_title_key_needs_the_en_locale() {
        let extra = "ui:\n  locales: [de]\n  contributions:\n    - slot: menu\n      title: T\n      title_key: menu.title\n      element: ui/a.html\n";
        let dir = package(&[("ui/locales/de.json", r#"{"menu.title":"x"}"#)]);
        let problems = check_package(dir.path(), &manifest(extra)).unwrap_err();
        assert!(problems.iter().any(|problem| problem.contains("`en` locale")), "{problems:?}");
    }

    #[test]
    fn a_schema_must_not_declare_the_reserved_ext_key() {
        let extra = "ui:\n  config_section:\n    schema: s.json\n  config_extensions:\n    - section: proxy\n      schema: {type: object}\n";
        let dir = package(&[("s.json", r#"{"type":"object","properties":{"ext":{"type":"string"}}}"#)]);
        let problems = check_package(dir.path(), &manifest(extra)).unwrap_err();
        assert!(problems.iter().any(|problem| problem.contains("`ext`")), "{problems:?}");
    }

    #[test]
    fn fragments_of_one_section_merge() {
        let extra = "ui:\n  config_extensions:\n    - section: proxy\n      schema: {type: object, properties: {a: {type: string}}, required: [a]}\n    - section: proxy\n      schema: {type: object, properties: {b: {type: integer}}}\n";
        let manifest = manifest(extra);
        let merged = extension_schema(&manifest.ui.config_extensions, "proxy").unwrap();
        assert_eq!(
            merged,
            json!({"type":"object","properties":{"a":{"type":"string"},"b":{"type":"integer"}},"required":["a"]})
        );
        assert!(extension_schema(&manifest.ui.config_extensions, "main").is_none());
    }
}
