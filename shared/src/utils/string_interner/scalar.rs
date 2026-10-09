/// Convert an `f64` that reached `visit_f64` into a round-trip-safe string.
///
/// Special values are emitted as `"infinity"`, `"-infinity"`, and `"nan"`.
/// `serde_saphyr` re-serializes these ambiguous scalars quoted, so they
/// survive a YAML round-trip as strings.
///
/// This is a safety-net for paths that intentionally accept typed numeric
/// scalars and normalize them into strings.
#[inline]
pub(super) fn f64_to_str(v: f64) -> String {
    if v.is_infinite() {
        if v.is_sign_positive() {
            "infinity".to_owned()
        } else {
            "-infinity".to_owned()
        }
    } else if v.is_nan() {
        "nan".to_owned()
    } else {
        v.to_string()
    }
}

#[inline]
// This intentionally only normalizes the lowercase dot-prefixed spellings that
// serde_saphyr emits for special float scalars. Other YAML 1.1 variants such as
// `.Inf`, `.INF`, or `.NaN` are out of scope here because they are not produced
// by the current parser path.
pub(super) fn normalize_scalar_string(value: &str) -> &str {
    match value {
        ".inf" => "infinity",
        "-.inf" => "-infinity",
        ".nan" => "nan",
        _ => value,
    }
}

/// Returns `true` for values that should be treated as absent.
///
/// Covers:
/// - empty string (`""`)
/// - JSON/YAML null literals (`"null"`, `"~"`)
///
/// Generic, field-agnostic. Callers decide whether `true` maps to `""`
/// (for non-optional `Arc<str>`) or `None` (for `Option<Arc<str>>`).
/// Case-sensitive on purpose: provider-supplied `"NULL"` or `"Null"` are real
/// values and must be preserved verbatim.
#[inline]
pub fn is_nullish(value: &str) -> bool { value.is_empty() || value == "~" || value.eq_ignore_ascii_case("null") }
