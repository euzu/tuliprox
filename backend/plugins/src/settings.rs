//! `PUT /api/v1/plugins/{id}/settings` : schema
//! validation of the supported schema subset, the secret sentinel round-trip, and the
//! warnings of a save. The `init` re-run lives in `api.rs`.
//!
//! Pure over JSON values; nothing here reads a file or touches the registry.
//! Messages never echo a value: a rejected save must not leak a secret.

use crate::api::{is_env_reference, parse_secret_path, Step};
use serde_json::{Map, Value};
use shared::model::{PluginFieldErrorDto, PluginSettingsErrorDto, DEFAULT_PLUGIN_SETTINGS_VERSION, REDACTED_SECRET};

/// Deepest schema nesting followed, a guard against pathological packages.
const MAX_DEPTH: usize = shared::utils::PLUGIN_SCHEMA_MAX_DEPTH;

/// Schema keywords the validator does not implement. A schema that uses one is
/// refused: a constraint that is silently skipped would let bad settings in.
const UNSUPPORTED_KEYWORDS: [&str; 10] =
    ["oneOf", "anyOf", "allOf", "$ref", "if", "then", "else", "not", "dependentSchemas", "patternProperties"];

const TYPES: [&str; 7] = ["string", "number", "integer", "boolean", "null", "object", "array"];

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// `(root)` for the settings object itself, else the path unchanged.
fn shown(path: &str) -> &str {
    if path.is_empty() {
        "(root)"
    } else {
        path
    }
}

fn child(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

fn items_of(path: &str) -> String { format!("{path}[]") }

// ---------------------------------------------------------------------------
// Sentinel merge
// ---------------------------------------------------------------------------

/// Decide what one secret value becomes. Returns `false` when the value has to
/// be removed (a sentinel with nothing stored to keep).
fn resolve_leaf(slot: &mut Value, stored: Option<&Value>, literal: &mut bool) -> bool {
    let sentinel = matches!(slot, Value::String(text) if text == REDACTED_SECRET);
    if sentinel {
        return match stored.filter(|value| !value.is_null()) {
            Some(kept) => {
                *slot = kept.clone();
                true
            }
            None => false,
        };
    }
    match slot {
        Value::Null => {}
        Value::String(text) if text.is_empty() || is_env_reference(text) => {}
        _ => *literal = true,
    }
    true
}

fn merge_at(value: &mut Value, stored: Option<&Value>, steps: &[Step<'_>], literal: &mut bool) {
    let Some((step, rest)) = steps.split_first() else {
        return;
    };
    match (step, value) {
        (Step::Key(name), Value::Object(map)) => {
            let kept = stored.and_then(|stored| stored.get(*name));
            if let Some(slot) = map.get_mut(*name) {
                if rest.is_empty() {
                    if !resolve_leaf(slot, kept, literal) {
                        map.remove(*name);
                    }
                } else {
                    merge_at(slot, kept, rest, literal);
                }
            }
        }
        (Step::Any, Value::Object(map)) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                let kept = stored.and_then(|stored| stored.get(key.as_str()));
                let Some(slot) = map.get_mut(&key) else { continue };
                if rest.is_empty() {
                    if !resolve_leaf(slot, kept, literal) {
                        map.remove(&key);
                    }
                } else {
                    merge_at(slot, kept, rest, literal);
                }
            }
        }
        (Step::Items, Value::Array(items)) => {
            // Positional: element `i` takes the secret of stored element `i`.
            for (index, mut item) in std::mem::take(items).into_iter().enumerate() {
                let kept = stored.and_then(|stored| stored.get(index));
                if rest.is_empty() {
                    if resolve_leaf(&mut item, kept, literal) {
                        items.push(item);
                    }
                } else {
                    merge_at(&mut item, kept, rest, literal);
                    items.push(item);
                }
            }
        }
        _ => {}
    }
}

/// Apply the secret sentinel to `incoming`, in place.
///
/// At every secret path of `paths`, a value exactly equal to
/// [`REDACTED_SECRET`] is replaced with the value `stored` holds at the same
/// place (the **unresolved** value, so an `${env:VAR}` reference stays a
/// reference). With nothing stored there, the value is removed. Anything else
/// is kept as sent.
///
/// Returns the secret paths that now carry a literal the caller just sent, one
/// entry per path.
pub(crate) fn merge_secrets(incoming: &mut Value, stored: &Value, paths: &[String]) -> Vec<String> {
    let mut literals = Vec::new();
    for path in paths {
        let mut literal = false;
        merge_at(incoming, Some(stored), &parse_secret_path(path), &mut literal);
        if literal {
            literals.push(path.clone());
        }
    }
    literals
}

/// The warning of a secret saved as a literal.
pub(crate) fn literal_secret_warning(path: &str) -> String {
    format!("secret `{path}` is stored as a literal; prefer ${{env:VAR}}")
}

fn collect_secrets(value: &Value, steps: &[Step<'_>], out: &mut Vec<String>) {
    let Some((step, rest)) = steps.split_first() else {
        if let Value::String(secret) = value {
            if !secret.is_empty() && !out.contains(secret) {
                out.push(secret.clone());
            }
        }
        return;
    };
    match (step, value) {
        (Step::Key(name), Value::Object(map)) => {
            if let Some(child) = map.get(*name) {
                collect_secrets(child, rest, out);
            }
        }
        (Step::Any, Value::Object(map)) => map.values().for_each(|child| collect_secrets(child, rest, out)),
        (Step::Items, Value::Array(items)) => items.iter().for_each(|item| collect_secrets(item, rest, out)),
        _ => {}
    }
}

/// The string values found at the secret `paths` of `settings`. Call it on the
/// **resolved** settings to get what has to be redacted from log lines.
pub(crate) fn secret_values(settings: &Value, paths: &[String]) -> Vec<String> {
    let mut secrets = Vec::new();
    for path in paths {
        collect_secrets(settings, &parse_secret_path(path), &mut secrets);
    }
    secrets
}

/// `text` with every secret in `secrets` replaced by [`REDACTED_SECRET`].
pub(crate) fn redact(text: &str, secrets: &[String]) -> String {
    secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .fold(text.to_string(), |text, secret| text.replace(secret.as_str(), REDACTED_SECRET))
}

// ---------------------------------------------------------------------------
// Settings versions
// ---------------------------------------------------------------------------

/// The settings-schema `version` of `schema`: its top-level integer
/// `"version"` (at least 1), or 1 when it declares none.
///
/// # Errors
/// `version` is present and not an integer of at least 1.
pub(crate) fn schema_version(schema: &Value) -> Result<u32, String> {
    match schema.get("version") {
        None => Ok(DEFAULT_PLUGIN_SETTINGS_VERSION),
        Some(version) => version
            .as_u64()
            .and_then(|version| u32::try_from(version).ok())
            .filter(|version| *version >= 1)
            .ok_or_else(|| "`version` must be an integer of at least 1".to_string()),
    }
}

/// The settings `value` as the plugin's own schema sees them: without the
/// `ext` object, which holds `ui.config_extensions` values.
fn own_settings(settings: &Map<String, Value>) -> Value {
    let mut own = settings.clone();
    own.remove(crate::manifest::package::EXTENSION_KEY);
    Value::Object(own)
}

/// Why settings written for `stored_version` cannot be used under a schema at
/// `current_version`: only when the versions differ, and the settings fail the
/// schema. Paths and messages only, never a value.
pub(crate) fn version_conflict(
    schema: Option<&Value>,
    current_version: u32,
    stored_version: u32,
    settings: &Map<String, Value>,
) -> Option<String> {
    if stored_version == current_version {
        return None;
    }
    let error = check_settings(schema, &own_settings(settings)).err()?;
    let mut problems: Vec<String> =
        error.fields.iter().take(3).map(|field| format!("{}: {}", field.path, field.message)).collect();
    if problems.is_empty() {
        problems.push(error.error);
    }
    Some(format!(
        "the stored settings (version {stored_version}) do not match the settings schema (version {current_version}): {}; \
         save the settings to continue",
        problems.join(", ")
    ))
}

/// The `settings_review` text of a plugin whose stored settings are at
/// `stored` while its package's schema is at `current`, or `None` when nothing
/// needs a look.
///
/// `can_migrate` is whether the package exports `settings.migrate` (`None`
/// while the package is not loaded, which gives nothing to say yet);
/// `failure` is why the last migration attempt failed, if it did.
pub(crate) fn review_text(
    stored: u32,
    current: u32,
    can_migrate: Option<bool>,
    failure: Option<&str>,
) -> Option<String> {
    match stored.cmp(&current) {
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Greater => Some(format!(
            "the stored settings are at version {stored}, newer than this package's settings schema (version \
             {current}), as after a rollback; review them and save"
        )),
        std::cmp::Ordering::Less => match (can_migrate?, failure) {
            (false, _) => Some(format!(
                "the settings schema moved from version {stored} to {current} and this plugin has no migration; \
                 review the settings and save"
            )),
            (true, Some(reason)) => Some(format!(
                "migrating the settings from version {stored} to {current} failed ({reason}); the stored settings \
                 were kept; review them and save"
            )),
            (true, None) => None,
        },
    }
}

/// Pairs of `(resolved, original)` for every string of `original` that
/// `${env:VAR}` resolution changed, found by walking both values together.
pub(crate) fn reference_pairs(original: &Value, resolved: &Value) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    collect_pairs(original, resolved, &mut pairs);
    pairs
}

fn collect_pairs(original: &Value, resolved: &Value, pairs: &mut Vec<(String, String)>) {
    match (original, resolved) {
        (Value::String(before), Value::String(after)) => {
            if before != after && !after.is_empty() && !pairs.iter().any(|(known, _)| known == after) {
                pairs.push((after.clone(), before.clone()));
            }
        }
        (Value::Object(before), Value::Object(after)) => {
            for (key, value) in before {
                if let Some(counterpart) = after.get(key) {
                    collect_pairs(value, counterpart, pairs);
                }
            }
        }
        (Value::Array(before), Value::Array(after)) => {
            for (value, counterpart) in before.iter().zip(after) {
                collect_pairs(value, counterpart, pairs);
            }
        }
        _ => {}
    }
}

/// Put the `${env:VAR}` references back: every string of `value` that equals a
/// resolved value in `pairs` becomes the original text again, so a migration
/// that saw resolved secrets does not write them into `config.yml`.
pub(crate) fn restore_references(value: &mut Value, pairs: &[(String, String)]) {
    match value {
        Value::String(text) => {
            if let Some((_, original)) = pairs.iter().find(|(resolved, _)| resolved == text) {
                text.clone_from(original);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| restore_references(item, pairs)),
        Value::Object(map) => map.values_mut().for_each(|item| restore_references(item, pairs)),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Schema subset
// ---------------------------------------------------------------------------

/// The constructs of `schema` this validator cannot honour, each as
/// `` `keyword` at `path` ``. Empty when the whole schema is in the supported schema subset.
pub(crate) fn unsupported_constructs(schema: &Value) -> Vec<String> {
    let mut found = Vec::new();
    scan(schema, "", 0, &mut found);
    found.extend(shared::utils::plugin_schema_pattern_problems(schema));
    found
}

fn scan(node: &Value, path: &str, depth: usize, found: &mut Vec<String>) {
    let Some(object) = node.as_object() else {
        return;
    };
    if depth > MAX_DEPTH {
        return;
    }
    for keyword in UNSUPPORTED_KEYWORDS {
        if object.contains_key(keyword) {
            found.push(format!("`{keyword}` at `{}`", shown(path)));
        }
    }
    match object.get("type") {
        Some(Value::String(name)) if !TYPES.contains(&name.as_str()) => {
            found.push(format!("`type: {name}` at `{}`", shown(path)));
        }
        Some(Value::Array(names)) => {
            for name in names.iter().filter_map(Value::as_str).filter(|name| !TYPES.contains(name)) {
                found.push(format!("`type: {name}` at `{}`", shown(path)));
            }
        }
        _ => {}
    }
    if let Some(Value::Object(properties)) = object.get("properties") {
        for (key, property) in properties {
            scan(property, &child(path, key), depth + 1, found);
        }
    }
    if let Some(items) = object.get("items").filter(|items| items.is_object()) {
        scan(items, &items_of(path), depth + 1, found);
    }
    if let Some(extra) = object.get("additionalProperties").filter(|extra| extra.is_object()) {
        scan(extra, &child(path, "*"), depth + 1, found);
    }
}

/// Every schema violation of `settings`, with paths in the secret-path
/// grammar. The schema must be inside the subset (see
/// [`unsupported_constructs`]); constructs outside it are not checked.
pub(crate) fn validate(schema: &Value, settings: &Value) -> Vec<PluginFieldErrorDto> {
    shared::utils::validate_plugin_schema(schema, settings)
}

/// Accept or reject `settings` against the plugin's `schema`.
///
/// No schema accepts any object. A schema outside the supported schema subset is refused
/// whole, naming each construct, because skipping a constraint would let bad
/// settings through. Otherwise every violation is collected at once.
pub(crate) fn check_settings(schema: Option<&Value>, settings: &Value) -> Result<(), PluginSettingsErrorDto> {
    let Some(schema) = schema else {
        return Ok(());
    };
    let unsupported = unsupported_constructs(schema);
    if !unsupported.is_empty() {
        return Err(PluginSettingsErrorDto {
            error: format!("the settings schema uses constructs this host cannot validate: {}", unsupported.join(", ")),
            fields: Vec::new(),
        });
    }
    let fields = validate(schema, settings);
    if fields.is_empty() {
        Ok(())
    } else {
        Err(PluginSettingsErrorDto { error: "the settings do not match the schema".to_string(), fields })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn paths(list: &[&str]) -> Vec<String> { list.iter().map(|path| (*path).to_string()).collect() }

    fn fields(schema: &Value, settings: &Value) -> Vec<(String, String)> {
        validate(schema, settings).into_iter().map(|field| (field.path, field.message)).collect()
    }

    fn paths_of(schema: &Value, settings: &Value) -> Vec<String> {
        // `serde_json::Map` order depends on a feature flag: compare sorted.
        let mut paths: Vec<String> = fields(schema, settings).into_iter().map(|(path, _)| path).collect();
        paths.sort();
        paths
    }

    // --- settings versions ---------------------------------------------------

    #[test]
    fn the_schema_version_is_the_top_level_integer_and_defaults_to_one() {
        assert_eq!(schema_version(&json!({"type": "object"})), Ok(1));
        assert_eq!(schema_version(&json!({"type": "object", "version": 3})), Ok(3));
        // a property called `version` is a setting, not the schema version
        assert_eq!(schema_version(&json!({"properties": {"version": {"type": "string"}}})), Ok(1));
        for bad in [json!(0), json!(-1), json!("2"), json!(1.5), json!(null), json!(5_000_000_000_u64)] {
            assert!(schema_version(&json!({"version": bad})).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_schema_subset_accepts_a_top_level_version() {
        let schema = json!({"type": "object", "version": 2, "properties": {"a": {"type": "string"}}});
        assert_eq!(unsupported_constructs(&schema), Vec::<String>::new());
        assert!(check_settings(Some(&schema), &json!({"a": "x"})).is_ok());
    }

    fn map(value: &Value) -> Map<String, Value> { value.as_object().cloned().unwrap() }

    #[test]
    fn a_version_conflict_needs_different_versions_and_a_schema_failure() {
        let schema = json!({"type": "object", "version": 2, "required": ["endpoint"], "additionalProperties": false,
            "properties": {"endpoint": {"type": "string"}}});
        let old = map(&json!({"host": "secret-looking-value"}));
        // same version: never a conflict, even for settings the schema rejects
        assert_eq!(version_conflict(Some(&schema), 2, 2, &old), None);
        let message = version_conflict(Some(&schema), 2, 1, &old).unwrap();
        assert!(message.contains("version 1") && message.contains("version 2") && message.contains("endpoint"));
        assert!(!message.contains("secret-looking-value"), "a value is never echoed");
        // old settings that still validate keep running
        assert_eq!(version_conflict(Some(&schema), 2, 1, &map(&json!({"endpoint": "a"}))), None);
        assert!(version_conflict(Some(&schema), 1, 2, &old).is_some(), "a downgrade checks too");
        assert_eq!(version_conflict(None, 2, 1, &old), None, "no schema accepts any object");
    }

    #[test]
    fn the_ext_object_is_not_checked_against_the_own_schema() {
        let schema = json!({"type": "object", "additionalProperties": false, "properties": {"a": {"type": "string"}}});
        assert_eq!(version_conflict(Some(&schema), 2, 1, &map(&json!({"a": "x", "ext": {"s": {"k": 1}}}))), None);
    }

    #[test]
    fn review_text_follows_the_freeze() {
        assert_eq!(review_text(2, 2, Some(true), None), None);
        assert_eq!(review_text(1, 2, Some(true), None), None, "a pending migration needs no review yet");
        assert_eq!(review_text(1, 2, None, None), None, "not loaded: nothing to say");
        let no_export = review_text(1, 2, Some(false), None).unwrap();
        assert!(no_export.contains("no migration") && no_export.contains('1') && no_export.contains('2'));
        let failed = review_text(1, 3, Some(true), Some("boom")).unwrap();
        assert!(failed.contains("boom") && failed.contains("kept"));
        let newer = review_text(4, 2, Some(false), None).unwrap();
        assert!(newer.contains("version 4") && newer.contains("rollback"));
        assert!(review_text(4, 2, None, None).is_some(), "a downgrade does not depend on the export");
    }

    #[test]
    fn env_references_come_back_after_a_migration() {
        let original = json!({"token": "${env:T}", "list": ["${env:L}", "plain"], "n": 1, "same": "x"});
        let resolved = json!({"token": "tok-1", "list": ["list-1", "plain"], "n": 1, "same": "x"});
        let pairs = reference_pairs(&original, &resolved);
        assert_eq!(pairs.len(), 2);
        // the plugin renamed and moved the values
        let mut migrated = json!({"auth": {"key": "tok-1"}, "all": ["list-1", "plain", "tok-1"], "n": 1});
        restore_references(&mut migrated, &pairs);
        assert_eq!(migrated, json!({"auth": {"key": "${env:T}"}, "all": ["${env:L}", "plain", "${env:T}"], "n": 1}));
        // an unset variable resolves to an empty string: nothing to map back
        assert_eq!(reference_pairs(&json!({"a": "${env:UNSET}"}), &json!({"a": ""})), Vec::<(String, String)>::new());
    }

    // --- validator: each supported construct --------------------------------

    #[test]
    fn no_schema_accepts_any_object() {
        assert!(check_settings(None, &json!({"anything": [1, {"x": null}]})).is_ok());
    }

    #[test]
    fn string_number_integer_boolean_types_are_checked() {
        let schema = json!({"type": "object", "properties": {
            "s": {"type": "string"}, "n": {"type": "number"}, "i": {"type": "integer"}, "b": {"type": "boolean"}
        }});
        assert_eq!(validate(&schema, &json!({"s": "x", "n": 1.5, "i": 3, "b": true})), Vec::<_>::new());
        assert_eq!(paths_of(&schema, &json!({"s": 1, "n": "1", "i": 1.5, "b": "yes"})), ["b", "i", "n", "s"]);
        // 2.0 is an integer in JSON Schema
        assert_eq!(validate(&schema, &json!({"i": 2.0})), Vec::<_>::new());
    }

    #[test]
    fn enum_must_match_a_member() {
        let schema = json!({"type": "object", "properties": {"mode": {"type": "string", "enum": ["fast", "slow"]}}});
        assert_eq!(validate(&schema, &json!({"mode": "fast"})), Vec::<_>::new());
        let found = fields(&schema, &json!({"mode": "medium"}));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "mode");
        assert!(found[0].1.contains("\"fast\""));
        assert!(!found[0].1.contains("medium"), "a rejected value is never echoed");
    }

    #[test]
    fn minimum_and_maximum_bound_numbers() {
        let schema =
            json!({"type": "object", "properties": {"port": {"type": "integer", "minimum": 1, "maximum": 10}}});
        assert_eq!(validate(&schema, &json!({"port": 1})), Vec::<_>::new());
        assert_eq!(validate(&schema, &json!({"port": 10})), Vec::<_>::new());
        assert_eq!(paths_of(&schema, &json!({"port": 0})), ["port"]);
        assert_eq!(paths_of(&schema, &json!({"port": 11})), ["port"]);
    }

    #[test]
    fn arrays_of_scalars_check_each_element() {
        let schema = json!({"type": "object", "properties": {"tags": {"type": "array", "items": {"type": "string"}}}});
        assert_eq!(validate(&schema, &json!({"tags": ["a", "b"]})), Vec::<_>::new());
        let found = fields(&schema, &json!({"tags": ["a", 2, "c", 4]}));
        assert_eq!(found.iter().map(|(path, _)| path.as_str()).collect::<Vec<_>>(), ["tags[]", "tags[]"]);
        assert!(found[0].1.starts_with("element 1:"));
        assert!(found[1].1.starts_with("element 3:"));
        assert_eq!(paths_of(&schema, &json!({"tags": "a"})), ["tags"]);
    }

    #[test]
    fn nested_objects_report_dotted_paths() {
        let schema = json!({"type": "object", "properties": {
            "credentials": {"type": "object", "properties": {"user": {"type": "string"}, "port": {"type": "integer"}}}
        }});
        assert_eq!(validate(&schema, &json!({"credentials": {"user": "me", "port": 1}})), Vec::<_>::new());
        assert_eq!(
            paths_of(&schema, &json!({"credentials": {"user": 1, "port": "x"}})),
            ["credentials.port", "credentials.user"]
        );
        assert_eq!(paths_of(&schema, &json!({"credentials": []})), ["credentials"]);
    }

    #[test]
    fn required_properties_must_be_present() {
        let schema = json!({"type": "object", "required": ["token", "region"], "properties": {
            "token": {"type": "string"}, "region": {"type": "string"},
            "inner": {"type": "object", "required": ["k"], "properties": {"k": {"type": "string"}}}
        }});
        assert_eq!(paths_of(&schema, &json!({})), ["region", "token"]);
        assert_eq!(paths_of(&schema, &json!({"token": "t", "region": "r", "inner": {}})), ["inner.k"]);
        assert_eq!(validate(&schema, &json!({"token": "t", "region": "r"})), Vec::<_>::new());
    }

    #[test]
    fn additional_properties_false_rejects_unknown_keys() {
        let schema = json!({"type": "object", "additionalProperties": false, "properties": {
            "known": {"type": "string"},
            "inner": {"type": "object", "additionalProperties": false, "properties": {"a": {"type": "string"}}}
        }});
        assert_eq!(validate(&schema, &json!({"known": "x", "inner": {"a": "b"}})), Vec::<_>::new());
        assert_eq!(paths_of(&schema, &json!({"known": "x", "extra": 1, "inner": {"b": 1}})), ["extra", "inner.b"]);
        // open by default
        assert_eq!(validate(&json!({"type": "object", "properties": {}}), &json!({"extra": 1})), Vec::<_>::new());
    }

    #[test]
    fn additional_properties_schema_checks_extra_values() {
        let schema = json!({"type": "object", "additionalProperties": {"type": "string"}});
        assert_eq!(validate(&schema, &json!({"a": "x"})), Vec::<_>::new());
        assert_eq!(paths_of(&schema, &json!({"a": 1})), ["*"]);
    }

    #[test]
    fn every_violation_is_collected_at_once() {
        let schema = json!({"type": "object", "required": ["a"], "additionalProperties": false, "properties": {
            "b": {"type": "integer", "minimum": 5}, "c": {"type": "boolean"}
        }});
        assert_eq!(paths_of(&schema, &json!({"b": 1, "c": "no", "z": 0})), ["a", "b", "c", "z"]);
    }

    #[test]
    fn the_root_must_be_an_object_when_the_schema_says_so() {
        let schema = json!({"type": "object"});
        assert_eq!(paths_of(&schema, &json!([1])), ["(root)"]);
    }

    #[test]
    fn env_references_are_accepted_only_where_a_string_is_allowed() {
        let schema = json!({"type": "object", "properties": {
            "token": {"type": "string", "writeOnly": true},
            "port": {"type": "integer"},
            "flag": {"type": "boolean"},
            "mode": {"type": "string", "enum": ["a", "b"]},
            "names": {"type": "array", "items": {"type": "string"}},
        }});
        let settings = json!({
            "token": "${env:TOKEN}", "port": "${env:PORT}", "flag": "${env:FLAG}",
            "mode": "${env:MODE}", "names": ["${env:N}"],
        });
        assert_eq!(paths_of(&schema, &settings), ["flag", "mode", "port"]);
        assert_eq!(validate(&schema, &json!({"token": "${env:TOKEN}", "names": ["${env:N}"]})), Vec::<_>::new());
    }

    // --- validator: each unsupported construct ------------------------------

    #[test]
    fn every_unsupported_construct_is_refused_by_name() {
        for keyword in
            ["oneOf", "anyOf", "allOf", "$ref", "if", "then", "else", "not", "dependentSchemas", "patternProperties"]
        {
            let schema = json!({"type": "object", "properties": {"a": {"type": "string"}}, keyword: {}});
            let error = check_settings(Some(&schema), &json!({"a": "x"})).expect_err(keyword);
            assert!(error.error.contains(&format!("`{keyword}`")), "{keyword}: {}", error.error);
            assert!(error.error.contains("`(root)`"), "{}", error.error);
            assert_eq!(error.fields, Vec::<_>::new());
        }
    }

    #[test]
    fn unsupported_constructs_are_found_in_nested_positions() {
        let schema = json!({"type": "object", "properties": {
            "inner": {"type": "object", "properties": {"x": {"oneOf": []}}},
            "list": {"type": "array", "items": {"$ref": "#/$defs/x"}},
            "extra": {"type": "object", "additionalProperties": {"not": {}}},
        }});
        let found = unsupported_constructs(&schema);
        assert!(found.contains(&"`oneOf` at `inner.x`".to_string()), "{found:?}");
        assert!(found.contains(&"`$ref` at `list[]`".to_string()), "{found:?}");
        assert!(found.contains(&"`not` at `extra.*`".to_string()), "{found:?}");
    }

    #[test]
    fn a_property_that_is_named_like_a_keyword_is_not_a_construct() {
        let schema = json!({"type": "object", "properties": {"not": {"type": "string"}, "if": {"type": "string"}}});
        assert_eq!(unsupported_constructs(&schema), Vec::<String>::new());
        assert!(check_settings(Some(&schema), &json!({"not": "x"})).is_ok());
    }

    #[test]
    fn an_unknown_type_is_refused() {
        let schema = json!({"type": "object", "properties": {"a": {"type": "date"}}});
        let error = check_settings(Some(&schema), &json!({})).unwrap_err();
        assert!(error.error.contains("`type: date` at `a`"), "{}", error.error);
    }

    #[test]
    fn a_pattern_that_does_not_compile_is_refused() {
        let schema = json!({"type": "object", "properties": {"a": {"type": "string", "pattern": "(open"}}});
        let error = check_settings(Some(&schema), &json!({})).unwrap_err();
        assert!(error.error.contains("`pattern` at `a`"), "{}", error.error);
    }

    #[test]
    fn string_list_and_number_keywords_are_enforced_on_save() {
        let schema = json!({"type": "object", "properties": {
            "name": {"type": "string", "maxLength": 3, "format": "hostname"},
            "tags": {"type": "array", "items": {"type": "string"}, "uniqueItems": true, "maxItems": 2},
            "ratio": {"type": "number", "exclusiveMaximum": 1}
        }});
        assert!(check_settings(Some(&schema), &json!({"name": "abc", "tags": ["a"], "ratio": 0.5})).is_ok());
        assert_eq!(
            paths_of(&schema, &json!({"name": "abcd", "tags": ["a", "a"], "ratio": 1})),
            ["name", "ratio", "tags"]
        );
    }

    #[test]
    fn violations_come_back_as_field_errors() {
        let schema = json!({"type": "object", "required": ["a"]});
        let error = check_settings(Some(&schema), &json!({})).unwrap_err();
        assert_eq!(
            error.fields,
            vec![PluginFieldErrorDto { path: "a".to_string(), message: "is required".to_string() }]
        );
    }

    // --- sentinel merge ------------------------------------------------------

    #[test]
    fn a_sentinel_keeps_the_stored_value() {
        let stored = json!({"token": "hunter2", "region": "eu"});
        let mut incoming = json!({"token": REDACTED_SECRET, "region": "us"});
        let literals = merge_secrets(&mut incoming, &stored, &paths(&["token"]));
        assert_eq!(incoming, json!({"token": "hunter2", "region": "us"}));
        assert!(literals.is_empty(), "a kept value is not a new literal");
    }

    #[test]
    fn a_sentinel_keeps_a_stored_env_reference_verbatim() {
        let stored = json!({"token": "${env:SPORTS_KEY}"});
        let mut incoming = json!({"token": REDACTED_SECRET});
        assert_eq!(merge_secrets(&mut incoming, &stored, &paths(&["token"])), Vec::<String>::new());
        assert_eq!(incoming, json!({"token": "${env:SPORTS_KEY}"}));
    }

    #[test]
    fn a_sentinel_with_nothing_stored_is_absent() {
        let mut incoming = json!({"token": REDACTED_SECRET, "other": REDACTED_SECRET});
        let literals = merge_secrets(&mut incoming, &json!({}), &paths(&["token"]));
        // only the secret path is touched: `other` is not a secret
        assert_eq!(incoming, json!({"other": REDACTED_SECRET}));
        assert_eq!(literals, Vec::<String>::new());

        let mut incoming = json!({"token": REDACTED_SECRET});
        merge_secrets(&mut incoming, &json!({"token": null}), &paths(&["token"]));
        assert_eq!(incoming, json!({}));
    }

    #[test]
    fn an_env_reference_is_stored_verbatim_without_a_warning() {
        let mut incoming = json!({"token": "${env:TULIPROX_PLUGIN_TEST_UNSET_C}"});
        let literals = merge_secrets(&mut incoming, &json!({"token": "old"}), &paths(&["token"]));
        assert_eq!(incoming, json!({"token": "${env:TULIPROX_PLUGIN_TEST_UNSET_C}"}));
        assert_eq!(literals, Vec::<String>::new());
    }

    #[test]
    fn a_new_literal_is_reported_once_per_path() {
        let mut incoming = json!({"token": "fresh", "pass": "${env:P}", "empty": ""});
        let literals = merge_secrets(&mut incoming, &json!({}), &paths(&["token", "pass", "empty"]));
        assert_eq!(literals, ["token"]);
        assert_eq!(incoming["token"], "fresh");
        assert_eq!(
            literal_secret_warning("token"),
            "secret `token` is stored as a literal; prefer ${env:VAR}".to_string()
        );
    }

    #[test]
    fn a_literal_in_a_mixed_reference_string_counts_as_a_literal() {
        let mut incoming = json!({"token": "pre-${env:X}"});
        assert_eq!(merge_secrets(&mut incoming, &json!({}), &paths(&["token"])), ["token"]);
    }

    #[test]
    fn nested_array_and_wildcard_sentinels_follow_the_position() {
        let stored = json!({
            "credentials": {"password": "pw"},
            "servers": [{"key": "k0"}, {"key": "${env:K1}"}],
            "extra": {"x": "ex"},
        });
        let mut incoming = json!({
            "credentials": {"password": REDACTED_SECRET},
            "servers": [{"key": REDACTED_SECRET}, {"key": REDACTED_SECRET}, {"key": REDACTED_SECRET}, {"key": "new"}],
            "extra": {"x": REDACTED_SECRET, "y": REDACTED_SECRET},
        });
        let literals =
            merge_secrets(&mut incoming, &stored, &paths(&["credentials.password", "servers[].key", "extra.*"]));
        assert_eq!(
            incoming,
            json!({
                "credentials": {"password": "pw"},
                "servers": [{"key": "k0"}, {"key": "${env:K1}"}, {}, {"key": "new"}],
                "extra": {"x": "ex"},
            })
        );
        assert_eq!(literals, ["servers[].key"]);
    }

    #[test]
    fn a_secret_that_is_an_array_element_is_dropped_without_a_stored_value() {
        let mut incoming = json!({"matrix": [[REDACTED_SECRET, "b"]]});
        merge_secrets(&mut incoming, &json!({"matrix": [["a"]]}), &paths(&["matrix[][]"]));
        assert_eq!(incoming, json!({"matrix": [["a", "b"]]}));
        let mut incoming = json!({"matrix": [[REDACTED_SECRET, "b"]]});
        merge_secrets(&mut incoming, &json!({}), &paths(&["matrix[][]"]));
        assert_eq!(incoming, json!({"matrix": [["b"]]}));
    }

    // --- redaction -----------------------------------------------------------

    #[test]
    fn secret_values_and_redaction() {
        let settings = json!({"token": "hunter2", "servers": [{"key": "k1"}, {"key": ""}], "name": "n"});
        let secrets = secret_values(&settings, &paths(&["token", "servers[].key"]));
        assert_eq!(secrets, ["hunter2", "k1"]);
        assert_eq!(
            redact("bad token hunter2 / k1", &secrets),
            format!("bad token {REDACTED_SECRET} / {REDACTED_SECRET}")
        );
        assert_eq!(redact("nothing", &[]), "nothing");
    }
}
