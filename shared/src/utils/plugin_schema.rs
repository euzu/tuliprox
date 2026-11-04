//! Validation of a value against a plugin's JSON Schema (the supported
//! draft 2020-12 **subset**).
//!
//! One implementation for both sides: the server checks a settings save and an
//! rpc payload with it, the web UI checks a `type: plugin` input's or output's
//! `plugin_config` before the form is applied, so both report the same
//! violations under the same paths.
//!
//! Paths use the secret-path grammar: `a`, `a.b`, `a[]` for the elements of the
//! list `a`, and `(root)` for the value itself. Messages never echo the value:
//! a rejected save must not leak a secret.
//!
//! Keywords checked: `type` (a name or a list of names), `enum`, `const`,
//! `minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum`, `multipleOf`,
//! `minLength`, `maxLength`, `pattern`, `format` (`email`, `uri`, `hostname`,
//! `date-time`, `date`, `time`, `duration`; other formats are annotations),
//! `required`, `properties`, `additionalProperties`, `items`, `minItems`,
//! `maxItems`, `uniqueItems`. Keywords starting with `x-` are UI hints and never
//! change what is valid, except `x-pattern-message`, the text shown when
//! `pattern` does not match.

use crate::model::PluginFieldErrorDto;
use regex::Regex;
use serde_json::{Map, Value};
use std::sync::LazyLock;

/// Deepest schema nesting followed, a guard against pathological packages.
pub const PLUGIN_SCHEMA_MAX_DEPTH: usize = 32;

/// The path used for the value as a whole.
pub const PLUGIN_SCHEMA_ROOT_PATH: &str = "(root)";

/// The `format` values [`validate_plugin_schema`] checks.
pub const PLUGIN_SCHEMA_FORMATS: [&str; 7] = ["email", "uri", "hostname", "date-time", "date", "time", "duration"];

/// `(root)` for the value itself, else the path unchanged.
fn shown(path: &str) -> &str {
    if path.is_empty() {
        PLUGIN_SCHEMA_ROOT_PATH
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

fn is_integer(value: &Value) -> bool {
    value.is_i64()
        || value.is_u64()
        || value.as_f64().is_some_and(|number| number.is_finite() && (number - number.round()).abs() < f64::EPSILON)
}

fn type_matches(name: &str, value: &Value) -> bool {
    match name {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => is_integer(value),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        _ => false,
    }
}

fn article(name: &str) -> String {
    match name {
        "object" | "integer" | "array" => format!("an {name}"),
        "null" => "null".to_string(),
        other => format!("a {other}"),
    }
}

// ---------------------------------------------------------------------------
// Formats
// ---------------------------------------------------------------------------

static EMAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[^@\s]+@[^@\s.]+(\.[^@\s.]+)+$").expect("email pattern compiles"));
static URI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z][A-Za-z0-9+.\-]*:[^\s]+$").expect("uri pattern compiles"));
static HOSTNAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?i)[a-z0-9]([a-z0-9\-]{0,61}[a-z0-9])?(\.[a-z0-9]([a-z0-9\-]{0,61}[a-z0-9])?)*\.?$")
        .expect("hostname pattern compiles")
});
static DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{4}-(0[1-9]|1[0-2])-(0[1-9]|[12]\d|3[01])$").expect("date pattern compiles"));
static TIME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([01]\d|2[0-3]):[0-5]\d:([0-5]\d|60)(\.\d+)?([Zz]|[+\-]([01]\d|2[0-3]):[0-5]\d)$")
        .expect("time pattern compiles")
});
static DURATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^P(?:\d+W|(?:\d+Y)?(?:\d+M)?(?:\d+D)?(?:T(?:\d+H)?(?:\d+M)?(?:\d+(?:\.\d+)?S)?)?)$")
        .expect("duration pattern compiles")
});

fn is_date_time(text: &str) -> bool {
    match text.split_once(['T', 't', ' ']) {
        Some((date, time)) => DATE.is_match(date) && TIME.is_match(time),
        None => false,
    }
}

fn is_duration(text: &str) -> bool {
    // `P` and `PT` alone match the grammar but name no duration.
    DURATION.is_match(text) && text != "P" && !text.ends_with('T')
}

/// Whether `text` is a valid `format`; an unknown format accepts anything.
#[must_use]
pub fn plugin_schema_format_matches(format: &str, text: &str) -> bool {
    match format {
        "email" => EMAIL.is_match(text),
        "uri" => URI.is_match(text),
        "hostname" => text.len() <= 253 && HOSTNAME.is_match(text),
        "date-time" => is_date_time(text),
        "date" => DATE.is_match(text),
        "time" => TIME.is_match(text),
        "duration" => is_duration(text),
        _ => true,
    }
}

fn format_message(format: &str) -> String {
    match format {
        "email" => "must be an email address".to_string(),
        "uri" => "must be a URI with a scheme, like https://…".to_string(),
        "hostname" => "must be a host name".to_string(),
        "date-time" => "must be a date and time, like 2026-01-31T18:00:00Z".to_string(),
        "date" => "must be a date, like 2026-01-31".to_string(),
        "time" => "must be a time, like 18:00:00Z".to_string(),
        "duration" => "must be an ISO 8601 duration, like PT30M".to_string(),
        other => format!("must be a valid {other}"),
    }
}

// ---------------------------------------------------------------------------
// Schema problems the validator cannot work around
// ---------------------------------------------------------------------------

/// The `pattern`s of `schema` that are not valid regular expressions, each as
/// `` `pattern` at `path`: reason ``. A package with one is refused: a pattern
/// the host cannot compile is a constraint it would silently skip.
#[must_use]
pub fn plugin_schema_pattern_problems(schema: &Value) -> Vec<String> {
    let mut found = Vec::new();
    scan_patterns(schema, "", 0, &mut found);
    found
}

fn scan_patterns(node: &Value, path: &str, depth: usize, found: &mut Vec<String>) {
    let Some(object) = node.as_object() else { return };
    if depth > PLUGIN_SCHEMA_MAX_DEPTH {
        return;
    }
    match object.get("pattern") {
        Some(Value::String(pattern)) => {
            if let Err(err) = Regex::new(pattern) {
                let reason = err.to_string().lines().last().unwrap_or_default().trim().to_string();
                found.push(format!("`pattern` at `{}` is not a valid regular expression: {reason}", shown(path)));
            }
        }
        Some(_) => found.push(format!("`pattern` at `{}` must be a string", shown(path))),
        None => {}
    }
    if let Some(Value::Object(properties)) = object.get("properties") {
        for (key, property) in properties {
            scan_patterns(property, &child(path, key), depth + 1, found);
        }
    }
    if let Some(items) = object.get("items").filter(|items| items.is_object()) {
        scan_patterns(items, &items_of(path), depth + 1, found);
    }
    if let Some(extra) = object.get("additionalProperties").filter(|extra| extra.is_object()) {
        scan_patterns(extra, &child(path, "*"), depth + 1, found);
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

struct Violations(Vec<PluginFieldErrorDto>);

impl Violations {
    fn add(&mut self, path: &str, message: impl Into<String>) {
        self.0.push(PluginFieldErrorDto { path: shown(path).to_string(), message: message.into() });
    }
}

fn number_of(schema: &Map<String, Value>, key: &str) -> Option<f64> { schema.get(key).and_then(Value::as_f64) }

fn count_of(schema: &Map<String, Value>, key: &str) -> Option<u64> { schema.get(key).and_then(Value::as_u64) }

/// Check the type, `enum` and `const` of one value. Returns `false` when the
/// value is already rejected, so nothing more specific is reported for it.
fn check_type(schema: &Map<String, Value>, value: &Value, path: &str, out: &mut Violations) -> bool {
    let declared: Vec<&str> = match schema.get("type") {
        Some(Value::String(name)) => vec![name.as_str()],
        Some(Value::Array(names)) => names.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if !declared.is_empty() && !declared.iter().any(|name| type_matches(name, value)) {
        let names: Vec<String> = declared.iter().map(|name| article(name)).collect();
        out.add(path, format!("must be {}", names.join(" or ")));
        return false;
    }
    if let Some(Value::Array(allowed)) = schema.get("enum") {
        if !allowed.contains(value) {
            let list: Vec<String> = allowed.iter().map(Value::to_string).collect();
            out.add(path, format!("must be one of {}", list.join(", ")));
            return false;
        }
    }
    if let Some(expected) = schema.get("const") {
        if expected != value {
            out.add(path, format!("must be {expected}"));
            return false;
        }
    }
    true
}

fn check_number(schema: &Map<String, Value>, number: f64, path: &str, out: &mut Violations) {
    if let Some(minimum) = number_of(schema, "minimum") {
        if number < minimum {
            out.add(path, format!("must be at least {minimum}"));
        }
    }
    if let Some(maximum) = number_of(schema, "maximum") {
        if number > maximum {
            out.add(path, format!("must be at most {maximum}"));
        }
    }
    if let Some(minimum) = number_of(schema, "exclusiveMinimum") {
        if number <= minimum {
            out.add(path, format!("must be greater than {minimum}"));
        }
    }
    if let Some(maximum) = number_of(schema, "exclusiveMaximum") {
        if number >= maximum {
            out.add(path, format!("must be less than {maximum}"));
        }
    }
    if let Some(step) = number_of(schema, "multipleOf").filter(|step| *step > 0.0) {
        let quotient = number / step;
        if (quotient - quotient.round()).abs() > 1e-9 {
            out.add(path, format!("must be a multiple of {step}"));
        }
    }
}

fn check_string(schema: &Map<String, Value>, text: &str, path: &str, out: &mut Violations) {
    let length = text.chars().count() as u64;
    if let Some(minimum) = count_of(schema, "minLength") {
        if length < minimum {
            let unit = if minimum == 1 { "character" } else { "characters" };
            out.add(path, format!("must be at least {minimum} {unit} long"));
        }
    }
    if let Some(maximum) = count_of(schema, "maxLength") {
        if length > maximum {
            let unit = if maximum == 1 { "character" } else { "characters" };
            out.add(path, format!("must be at most {maximum} {unit} long"));
        }
    }
    if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
        // An invalid pattern is refused when the package is checked; here it is skipped.
        if let Ok(regex) = Regex::new(pattern) {
            if !regex.is_match(text) {
                let message = schema
                    .get("x-pattern-message")
                    .and_then(Value::as_str)
                    .filter(|message| !message.trim().is_empty())
                    .map_or_else(|| format!("must match the pattern {pattern}"), str::to_string);
                out.add(path, message);
            }
        }
    }
    if let Some(format) = schema.get("format").and_then(Value::as_str) {
        if !plugin_schema_format_matches(format, text) {
            out.add(path, format_message(format));
        }
    }
}

fn check_array(schema: &Map<String, Value>, items: &[Value], path: &str, out: &mut Violations) {
    let count = items.len() as u64;
    if let Some(minimum) = count_of(schema, "minItems") {
        if count < minimum {
            let unit = if minimum == 1 { "entry" } else { "entries" };
            out.add(path, format!("must have at least {minimum} {unit}"));
        }
    }
    if let Some(maximum) = count_of(schema, "maxItems") {
        if count > maximum {
            let unit = if maximum == 1 { "entry" } else { "entries" };
            out.add(path, format!("must have at most {maximum} {unit}"));
        }
    }
    if schema.get("uniqueItems").and_then(Value::as_bool) == Some(true)
        && items.iter().enumerate().any(|(index, item)| items[..index].contains(item))
    {
        out.add(path, "must not contain the same entry twice");
    }
}

fn validate_node(schema: &Value, value: &Value, path: &str, depth: usize, out: &mut Violations) {
    let object = match schema {
        Value::Bool(false) => {
            out.add(path, "is not allowed");
            return;
        }
        Value::Object(object) if depth <= PLUGIN_SCHEMA_MAX_DEPTH => object,
        _ => return,
    };
    if !check_type(object, value, path, out) {
        return;
    }
    match value {
        Value::Number(_) => {
            if let Some(number) = value.as_f64() {
                check_number(object, number, path, out);
            }
        }
        Value::String(text) => check_string(object, text, path, out),
        Value::Object(map) => validate_object(object, map, path, depth, out),
        Value::Array(items) => {
            check_array(object, items, path, out);
            if let Some(item_schema) = object.get("items").filter(|items| items.is_object()) {
                let element_path = items_of(path);
                for (index, item) in items.iter().enumerate() {
                    let mut inner = Violations(Vec::new());
                    validate_node(item_schema, item, &element_path, depth + 1, &mut inner);
                    for mut violation in inner.0 {
                        violation.message = format!("element {index}: {}", violation.message);
                        out.0.push(violation);
                    }
                }
            }
        }
        Value::Null | Value::Bool(_) => {}
    }
}

fn validate_object(
    schema: &Map<String, Value>,
    map: &Map<String, Value>,
    path: &str,
    depth: usize,
    out: &mut Violations,
) {
    let properties = schema.get("properties").and_then(Value::as_object);
    if let Some(Value::Array(required)) = schema.get("required") {
        for key in required.iter().filter_map(Value::as_str).filter(|key| !map.contains_key(*key)) {
            out.add(&child(path, key), "is required");
        }
    }
    for (key, value) in map {
        let key_path = child(path, key);
        if let Some(property) = properties.and_then(|properties| properties.get(key)) {
            validate_node(property, value, &key_path, depth + 1, out);
            continue;
        }
        match schema.get("additionalProperties") {
            Some(Value::Bool(false)) => out.add(&key_path, "is not a known setting"),
            Some(extra @ Value::Object(_)) => validate_node(extra, value, &child(path, "*"), depth + 1, out),
            _ => {}
        }
    }
}

/// Every violation of `value` against `schema`, with paths in the secret-path
/// grammar. Constructs outside the supported subset are not checked; the server
/// refuses a package whose schema uses one.
#[must_use]
pub fn validate_plugin_schema(schema: &Value, value: &Value) -> Vec<PluginFieldErrorDto> {
    let mut out = Violations(Vec::new());
    validate_node(schema, value, "", 0, &mut out);
    out.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn messages(schema: &Value, value: &Value) -> Vec<(String, String)> {
        validate_plugin_schema(schema, value).into_iter().map(|error| (error.path, error.message)).collect()
    }

    fn rejected(schema: &Value, value: &Value) -> bool { !validate_plugin_schema(schema, value).is_empty() }

    #[test]
    fn string_lengths_patterns_and_custom_pattern_messages() {
        let schema = json!({"type": "object", "properties": {
            "code": {"type": "string", "minLength": 2, "maxLength": 3, "pattern": "^[A-Z]+$"},
            "slug": {"type": "string", "pattern": "^[a-z-]+$", "x-pattern-message": "lowercase letters and dashes"}}});
        assert!(!rejected(&schema, &json!({"code": "AB"})));
        assert_eq!(
            messages(&schema, &json!({"code": "A"})),
            [("code".into(), "must be at least 2 characters long".into())]
        );
        assert_eq!(
            messages(&schema, &json!({"code": "ABCD"})),
            [("code".into(), "must be at most 3 characters long".into())]
        );
        assert!(rejected(&schema, &json!({"code": "ab"})));
        assert_eq!(messages(&schema, &json!({"slug": "No"})), [("slug".into(), "lowercase letters and dashes".into())]);
        // Lengths count characters, not bytes.
        assert!(!rejected(&json!({"type": "string", "maxLength": 2}), &json!("äö")));
    }

    #[test]
    fn formats_are_checked_and_unknown_formats_accept_anything() {
        let cases = [
            ("email", "me@example.com", "me@example"),
            ("uri", "https://example.com/x", "example.com"),
            ("hostname", "media.example.com", "bad_host"),
            ("date-time", "2026-01-31T18:00:00Z", "2026-01-31"),
            ("date", "2026-01-31", "2026-13-01"),
            ("time", "18:00:00+01:00", "18:00"),
            ("duration", "PT30M", "30m"),
        ];
        for (format, good, bad) in cases {
            let schema = json!({"type": "string", "format": format});
            assert!(!rejected(&schema, &json!(good)), "{format} {good}");
            assert!(rejected(&schema, &json!(bad)), "{format} {bad}");
        }
        assert!(!rejected(&json!({"type": "string", "format": "color"}), &json!("anything")));
        assert!(!plugin_schema_format_matches("duration", "P"));
        assert!(!plugin_schema_format_matches("duration", "PT"));
    }

    #[test]
    fn number_bounds_and_steps() {
        let schema = json!({"type": "number", "exclusiveMinimum": 0, "exclusiveMaximum": 1, "multipleOf": 0.25});
        assert!(!rejected(&schema, &json!(0.5)));
        assert!(rejected(&schema, &json!(0)));
        assert!(rejected(&schema, &json!(1)));
        assert!(rejected(&schema, &json!(0.3)));
        let integer = json!({"type": "integer", "minimum": 1, "maximum": 10, "multipleOf": 5});
        assert!(!rejected(&integer, &json!(5)));
        assert!(rejected(&integer, &json!(7)));
        assert!(rejected(&integer, &json!(15)));
    }

    #[test]
    fn list_counts_and_uniqueness() {
        let schema =
            json!({"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 2, "uniqueItems": true});
        assert!(!rejected(&schema, &json!(["a", "b"])));
        assert!(rejected(&schema, &json!([])));
        assert!(rejected(&schema, &json!(["a", "b", "c"])));
        assert_eq!(
            messages(&schema, &json!(["a", "a"])),
            [("(root)".into(), "must not contain the same entry twice".into())]
        );
    }

    #[test]
    fn nullable_types_accept_null() {
        let schema = json!({"type": "object", "properties": {"name": {"type": ["string", "null"], "minLength": 2}}});
        assert!(!rejected(&schema, &json!({"name": null})));
        assert!(rejected(&schema, &json!({"name": "a"})));
        assert!(rejected(&schema, &json!({"name": 1})));
    }

    #[test]
    fn const_and_deep_nesting() {
        let schema = json!({"type": "object", "properties": {"a": {"type": "object", "properties": {
            "b": {"type": "object", "required": ["c"], "properties": {"c": {"const": "x"}}}}}}});
        assert!(!rejected(&schema, &json!({"a": {"b": {"c": "x"}}})));
        assert_eq!(messages(&schema, &json!({"a": {"b": {}}})), [("a.b.c".into(), "is required".into())]);
        assert_eq!(messages(&schema, &json!({"a": {"b": {"c": "y"}}})), [("a.b.c".into(), "must be \"x\"".into())]);
    }

    #[test]
    fn invalid_patterns_are_reported_with_their_path() {
        let schema = json!({"type": "object", "properties": {
            "ok": {"type": "string", "pattern": "^a+$"},
            "bad": {"type": "string", "pattern": "(unclosed"},
            "list": {"type": "array", "items": {"type": "string", "pattern": 3}}}});
        let problems = plugin_schema_pattern_problems(&schema);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().any(|problem| problem.starts_with("`pattern` at `bad`")));
        assert!(problems.iter().any(|problem| problem == "`pattern` at `list[]` must be a string"));
    }
}
