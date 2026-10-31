//! Secret properties of a plugin's `config-schema.json` (plan §9, *Plugin secrets*).
//!
//! A property is secret iff its schema has `"writeOnly": true` or
//! `"format": "password"`. A secret must not carry a `default`: a default is
//! shipped inside the package, so it would be a credential everybody can read.
//!
//! Pure over the parsed schema; this module reads no files.

use serde_json::Value;
use shared::error::TuliproxError;

/// Deepest schema nesting followed, a guard against pathological packages.
const MAX_DEPTH: usize = 32;

/// The paths of the secret properties of `schema`.
///
/// Paths are relative to the settings object: `token`, `credentials.password`,
/// `servers[].key` (`[]` for array items, `.*` for `additionalProperties`).
/// Local `$ref`s (`#/$defs/…`) are followed, `allOf`/`anyOf`/`oneOf` branches
/// are searched, and the same property reached twice is listed once.
///
/// # Errors
/// `TuliproxError::Plugin` naming every secret that has a `default`.
pub(crate) fn secret_property_paths(schema: &Value) -> Result<Vec<String>, TuliproxError> {
    let mut walk = Walk { root: schema, secrets: Vec::new(), defaulted: Vec::new(), refs: Vec::new() };
    walk.visit(schema, "", 0);
    if walk.defaulted.is_empty() {
        Ok(walk.secrets)
    } else {
        let list = walk.defaulted.iter().map(|path| format!("`{path}`")).collect::<Vec<_>>().join(", ");
        Err(TuliproxError::Plugin(format!("config schema: secret properties must not have a `default`: {list}")))
    }
}

struct Walk<'a> {
    root: &'a Value,
    secrets: Vec<String>,
    defaulted: Vec<String>,
    /// `$ref`s being followed, to stop a cycle.
    refs: Vec<&'a str>,
}

impl<'a> Walk<'a> {
    fn visit(&mut self, node: &'a Value, path: &str, depth: usize) {
        let Some(object) = node.as_object() else {
            return;
        };
        if depth > MAX_DEPTH {
            return;
        }

        let (secret, defaulted) = self.flags(node, 0);
        if secret {
            let shown = if path.is_empty() { "(root)" } else { path };
            push_unique(&mut self.secrets, shown);
            if defaulted {
                push_unique(&mut self.defaulted, shown);
            }
        }

        if let Some(properties) = object.get("properties").and_then(Value::as_object) {
            for (name, child) in properties {
                let child_path = if path.is_empty() { name.clone() } else { format!("{path}.{name}") };
                self.visit(child, &child_path, depth + 1);
            }
        }
        if let Some(items) = object.get("items") {
            self.visit(items, &format!("{path}[]"), depth + 1);
        }
        if let Some(extra) = object.get("additionalProperties").filter(|extra| extra.is_object()) {
            let extra_path = if path.is_empty() { "*".to_string() } else { format!("{path}.*") };
            self.visit(extra, &extra_path, depth + 1);
        }
        for keyword in ["allOf", "anyOf", "oneOf"] {
            if let Some(branches) = object.get(keyword).and_then(Value::as_array) {
                for branch in branches {
                    self.visit(branch, path, depth + 1);
                }
            }
        }
        if let Some(target) = self.resolve(object.get("$ref")) {
            if !self.refs.contains(&reference(object)) {
                self.refs.push(reference(object));
                self.visit(target, path, depth + 1);
                self.refs.pop();
            }
        }
    }

    /// `(secret, has default)` of `node` itself and of the schemas it `$ref`s,
    /// so `{"$ref": "#/$defs/token", "default": "x"}` and a defaulted target
    /// are both seen.
    fn flags(&self, node: &'a Value, depth: usize) -> (bool, bool) {
        let Some(object) = node.as_object() else {
            return (false, false);
        };
        let mut secret = object.get("writeOnly") == Some(&Value::Bool(true))
            || object.get("format").and_then(Value::as_str) == Some("password");
        let mut defaulted = object.contains_key("default");
        if depth < MAX_DEPTH {
            if let Some(target) = self.resolve(object.get("$ref")) {
                let (target_secret, target_default) = self.flags(target, depth + 1);
                secret |= target_secret;
                defaulted |= target_default;
            }
        }
        // a default only matters on something that is secret
        (secret, secret && defaulted)
    }

    /// The schema a local `$ref` (`#`, `#/$defs/name`, `#/definitions/name`) points at.
    fn resolve(&self, reference: Option<&'a Value>) -> Option<&'a Value> {
        let pointer = reference?.as_str()?.strip_prefix('#')?;
        self.root.pointer(pointer)
    }
}

fn reference(object: &serde_json::Map<String, Value>) -> &str {
    object.get("$ref").and_then(Value::as_str).unwrap_or_default()
}

fn push_unique(list: &mut Vec<String>, value: &str) {
    if !list.iter().any(|existing| existing == value) {
        list.push(value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Sorted, so the tests hold whether or not `serde_json` keeps key order.
    fn secrets(schema: &Value) -> Vec<String> {
        let mut paths = secret_property_paths(schema).expect("no defaulted secret");
        paths.sort();
        paths
    }

    #[test]
    fn write_only_and_password_format_are_secret() {
        let schema = json!({
            "type": "object",
            "properties": {
                "token": {"type": "string", "writeOnly": true},
                "api_key": {"type": "string", "format": "password"},
                "city": {"type": "string"},
                "verbose": {"type": "boolean", "writeOnly": false},
                "email": {"type": "string", "format": "email"}
            }
        });
        assert_eq!(secrets(&schema), ["api_key", "token"]);
    }

    #[test]
    fn write_only_must_be_the_boolean_true() {
        let schema = json!({"properties": {"a": {"writeOnly": "true"}, "b": {"writeOnly": 1}}});
        assert_eq!(secrets(&schema).len(), 0);
    }

    #[test]
    fn a_schema_without_secrets_has_none() {
        assert_eq!(secrets(&json!({})).len(), 0);
        assert_eq!(
            secrets(&json!({"type": "object", "properties": {"a": {"type": "string", "default": "x"}}})).len(),
            0
        );
        assert_eq!(secrets(&json!(true)).len(), 0);
        assert_eq!(secrets(&json!("nonsense")).len(), 0);
    }

    #[test]
    fn nested_properties_arrays_and_additional_properties_get_paths() {
        let schema = json!({
            "properties": {
                "credentials": {"properties": {"password": {"writeOnly": true}, "user": {"type": "string"}}},
                "servers": {"type": "array", "items": {"properties": {"key": {"format": "password"}}}},
                "tokens": {"type": "array", "items": {"writeOnly": true}},
                "headers": {"type": "object", "additionalProperties": {"writeOnly": true}}
            }
        });
        assert_eq!(secrets(&schema), ["credentials.password", "headers.*", "servers[].key", "tokens[]"]);
    }

    #[test]
    fn combinator_branches_are_searched() {
        let schema = json!({
            "allOf": [{"properties": {"a": {"writeOnly": true}}}],
            "oneOf": [{"properties": {"b": {"format": "password"}}}, {"properties": {"c": {"type": "string"}}}],
            "anyOf": [{"properties": {"a": {"writeOnly": true}}}]
        });
        assert_eq!(secrets(&schema), ["a", "b"], "a property reached twice is listed once");
    }

    #[test]
    fn local_refs_are_followed() {
        let schema = json!({
            "$defs": {"secret": {"type": "string", "writeOnly": true}, "plain": {"type": "string"}},
            "properties": {"token": {"$ref": "#/$defs/secret"}, "name": {"$ref": "#/$defs/plain"}, "gone": {"$ref": "#/$defs/missing"}}
        });
        assert_eq!(secrets(&schema), ["token"]);
    }

    #[test]
    fn definitions_keyword_and_external_refs() {
        let schema = json!({
            "definitions": {"s": {"format": "password"}},
            "properties": {"a": {"$ref": "#/definitions/s"}, "b": {"$ref": "https://example.com/schema.json"}}
        });
        assert_eq!(secrets(&schema), ["a"]);
    }

    #[test]
    fn reference_cycles_terminate() {
        let schema = json!({
            "$defs": {"node": {"properties": {"next": {"$ref": "#/$defs/node"}, "key": {"writeOnly": true}}}},
            "properties": {"tree": {"$ref": "#/$defs/node"}},
            "$ref": "#"
        });
        let found = secrets(&schema);
        assert!(found.contains(&"tree.key".to_string()), "{found:?}");
    }

    #[test]
    fn a_secret_root_is_reported_as_root() {
        assert_eq!(secrets(&json!({"writeOnly": true})), ["(root)"]);
    }

    #[test]
    fn a_default_on_a_secret_is_an_error_naming_every_offender() {
        let schema = json!({
            "properties": {
                "token": {"writeOnly": true, "default": "hunter2"},
                "key": {"format": "password", "default": ""},
                "nested": {"properties": {"pw": {"writeOnly": true, "default": null}}},
                "fine": {"writeOnly": true},
                "region": {"default": "eu"}
            }
        });
        let message = secret_property_paths(&schema).unwrap_err().to_string();
        assert!(
            message.contains("`token`") && message.contains("`key`") && message.contains("`nested.pw`"),
            "{message}"
        );
        assert!(!message.contains("`fine`") && !message.contains("`region`"), "{message}");
    }

    #[test]
    fn a_default_reached_through_a_ref_is_an_error() {
        let on_target = json!({
            "$defs": {"s": {"writeOnly": true, "default": "x"}},
            "properties": {"token": {"$ref": "#/$defs/s"}}
        });
        assert!(secret_property_paths(&on_target).is_err());
        let on_referrer = json!({
            "$defs": {"s": {"writeOnly": true}},
            "properties": {"token": {"$ref": "#/$defs/s", "default": "x"}}
        });
        assert!(secret_property_paths(&on_referrer).is_err());
    }
}
