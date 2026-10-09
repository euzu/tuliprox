// These serde helper modules deliberately re-use the parent module's imports.
// Clippy's expansion of this glob names private sibling modules and does not
// compile, so the glob is kept.
#[allow(clippy::wildcard_imports)]
use super::*;

#[derive(Debug, serde::Deserialize)]
struct ArcStrHolder {
    #[serde(default, with = "arc_str_serde")]
    value: Arc<str>,
}

#[derive(Debug, serde::Deserialize)]
struct OptArcStrHolder {
    #[serde(default, with = "arc_str_option_serde")]
    value: Option<Arc<str>>,
}

#[test]
fn arc_str_serde_preserves_yaml_infinity_literal_as_string() {
    let parsed: ArcStrHolder = serde_saphyr::from_str("value: infinity\n").unwrap();
    assert_eq!(parsed.value.as_ref(), "infinity");
}

#[test]
fn arc_str_serde_preserves_yaml_numeric_like_word_as_string() {
    let parsed: ArcStrHolder = serde_saphyr::from_str("value: 01abc\n").unwrap();
    assert_eq!(parsed.value.as_ref(), "01abc");
}

#[test]
fn arc_str_serde_accepts_json_integer() {
    let parsed: ArcStrHolder = serde_json::from_str(r#"{"value":1285728}"#).unwrap();
    assert_eq!(parsed.value.as_ref(), "1285728");
}

#[test]
fn arc_str_option_serde_accepts_json_integer() {
    let parsed: OptArcStrHolder = serde_json::from_str(r#"{"value":8169}"#).unwrap();
    assert_eq!(parsed.value.as_deref(), Some("8169"));
}

#[test]
fn arc_str_option_serde_accepts_json_string() {
    let parsed: OptArcStrHolder = serde_json::from_str(r#"{"value":"8169"}"#).unwrap();
    assert_eq!(parsed.value.as_deref(), Some("8169"));
}

#[test]
fn arc_str_option_serde_maps_empty_string_to_none() {
    let parsed: OptArcStrHolder = serde_json::from_str(r#"{"value":""}"#).unwrap();
    assert_eq!(parsed.value, None);
}

#[test]
fn arc_str_serde_normalizes_json_scientific_notation_numbers() {
    let parsed: ArcStrHolder = serde_json::from_str(r#"{"value":1e2}"#).unwrap();
    assert_eq!(parsed.value.as_ref(), "100");
}

#[test]
fn arc_str_serde_normalizes_yaml_special_float_scalars() {
    let parsed_inf: ArcStrHolder = serde_saphyr::from_str("value: .inf\n").unwrap();
    let parsed_neg_inf: ArcStrHolder = serde_saphyr::from_str("value: -.inf\n").unwrap();
    let parsed_nan: ArcStrHolder = serde_saphyr::from_str("value: .nan\n").unwrap();

    assert_eq!(parsed_inf.value.as_ref(), "infinity");
    assert_eq!(parsed_neg_inf.value.as_ref(), "-infinity");
    assert_eq!(parsed_nan.value.as_ref(), "nan");
}

#[test]
fn arc_str_option_serde_normalizes_yaml_special_float_scalars() {
    let parsed_inf: OptArcStrHolder = serde_saphyr::from_str("value: .inf\n").unwrap();
    let parsed_neg_inf: OptArcStrHolder = serde_saphyr::from_str("value: -.inf\n").unwrap();
    let parsed_nan: OptArcStrHolder = serde_saphyr::from_str("value: .nan\n").unwrap();

    assert_eq!(parsed_inf.value.as_deref(), Some("infinity"));
    assert_eq!(parsed_neg_inf.value.as_deref(), Some("-infinity"));
    assert_eq!(parsed_nan.value.as_deref(), Some("nan"));
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct NullishArcStrHolder {
    #[serde(default, with = "arc_str_null_is_none_serde")]
    value: Arc<str>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct NullishOptArcStrHolder {
    #[serde(default, with = "arc_str_null_is_none_option_serde")]
    value: Option<Arc<str>>,
}

#[test]
fn null_is_none_arc_str_treats_literal_null_string_as_empty() {
    let parsed: NullishArcStrHolder = serde_json::from_str(r#"{"value":"null"}"#).unwrap();
    assert!(parsed.value.is_empty());
}

#[test]
fn null_is_none_arc_str_treats_yaml_null_scalar_as_empty() {
    let parsed: NullishArcStrHolder = serde_saphyr::from_str("value: null\n").unwrap();
    assert!(parsed.value.is_empty());
    let parsed_tilde: NullishArcStrHolder = serde_saphyr::from_str("value: ~\n").unwrap();
    assert!(parsed_tilde.value.is_empty());
}

#[test]
fn null_is_none_arc_str_treats_json_null_as_empty() {
    let parsed: NullishArcStrHolder = serde_json::from_str(r#"{"value":null}"#).unwrap();
    assert!(parsed.value.is_empty());
}

#[test]
fn null_is_none_arc_str_preserves_real_extensions() {
    for ext in ["mkv", "mp4", "ts", "avi"] {
        let json = format!(r#"{{"value":"{ext}"}}"#);
        let parsed: NullishArcStrHolder = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.value.as_ref(), ext, "extension {ext} must survive");
    }
}

#[test]
fn null_is_none_option_treats_literal_null_string_as_none() {
    let parsed: NullishOptArcStrHolder = serde_json::from_str(r#"{"value":"null"}"#).unwrap();
    assert_eq!(parsed.value, None);
}

#[test]
fn null_is_none_option_treats_json_null_as_none() {
    let parsed: NullishOptArcStrHolder = serde_json::from_str(r#"{"value":null}"#).unwrap();
    assert_eq!(parsed.value, None);
}

#[test]
fn null_is_none_option_treats_empty_string_as_none() {
    let parsed: NullishOptArcStrHolder = serde_json::from_str(r#"{"value":""}"#).unwrap();
    assert_eq!(parsed.value, None);
}

#[test]
fn null_is_none_option_preserves_real_extensions() {
    for ext in ["mkv", "mp4", "ts"] {
        let json = format!(r#"{{"value":"{ext}"}}"#);
        let parsed: NullishOptArcStrHolder = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.value.as_deref(), Some(ext), "extension {ext} must survive");
    }
}

#[test]
fn null_is_none_option_serializes_none_and_literal_null_as_json_null() {
    let none_value: NullishOptArcStrHolder = NullishOptArcStrHolder { value: None };
    assert_eq!(serde_json::to_string(&none_value).unwrap(), r#"{"value":null}"#);

    let literal_null: NullishOptArcStrHolder = NullishOptArcStrHolder { value: Some("null".into()) };
    assert_eq!(serde_json::to_string(&literal_null).unwrap(), r#"{"value":null}"#);

    let empty: NullishOptArcStrHolder = NullishOptArcStrHolder { value: Some("".into()) };
    assert_eq!(serde_json::to_string(&empty).unwrap(), r#"{"value":null}"#);
}

#[test]
fn null_is_none_arc_str_roundtrips_real_extensions_byte_for_byte() {
    let holder = NullishArcStrHolder { value: "mkv".into() };
    let json = serde_json::to_string(&holder).unwrap();
    assert_eq!(json, r#"{"value":"mkv"}"#);
    let back: NullishArcStrHolder = serde_json::from_str(&json).unwrap();
    assert_eq!(back.value.as_ref(), "mkv");
}
