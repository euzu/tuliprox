use super::*;
use crate::model::PlaylistItemType;

#[test]
fn delta_absent_null_value_roundtrip() {
    let yaml = "schema_version: 1\nlive:\n  '12345':\n    name: A\n    logo: null\n";
    let (file, errors) = parse_input_override_file(yaml).expect("parse input override from yaml");
    assert_eq!(errors, &[] as &[FieldError]);
    let item = file.live.get("12345").expect("entry 12345");

    assert_eq!(item.name, FieldDelta::Set("A".to_string()));
    assert_eq!(item.logo, FieldDelta::Clear);
    assert!(item.title.is_keep());
    assert!(item.group.is_keep());
    assert!(item.logo_small.is_keep());
    assert!(item.epg_channel_id.is_keep());
    assert!(item.chno.is_keep());
    assert!(item.parent_code.is_keep());
    assert!(item.audio_track.is_keep());
    assert!(item.time_shift.is_keep());
    assert!(item.rec.is_keep());
    assert!(item.tmdb.is_keep());
    assert!(item.plot.is_keep());
    assert!(item.genre.is_keep());
    assert!(item.cast.is_keep());
    assert!(item.director.is_keep());
    assert!(item.release_date.is_keep());
    assert!(item.rating.is_keep());

    let serialized = serde_saphyr::to_string(&item).expect("serialize ItemOverride to yaml");
    assert_eq!(serialized, "logo: null\nname: A\n");
}

#[test]
fn hint_only_entry_is_empty() {
    let item = ItemOverride { hint: Some("DE | Vollprogramm / Das Erste".to_string()), ..Default::default() };
    assert!(item.is_empty());
}

#[test]
fn unknown_key_survives_patch_and_blocks_empty() {
    let mut item = ItemOverride::default();
    item.unknown.insert("epg_chanel_id".to_string(), serde_json::json!("typo"));
    assert!(!item.is_empty());

    let patch = ItemOverridePatch::default();
    item.apply_patch(&patch);

    assert_eq!(item.unknown.get("epg_chanel_id"), Some(&serde_json::json!("typo")));
    assert!(!item.is_empty());

    let yaml = serde_saphyr::to_string(&item).expect("serialize item with unknown");
    assert!(yaml.contains("epg_chanel_id: typo"));

    let repair_patch = ItemOverridePatch { remove_unknown: vec!["epg_chanel_id".to_string()], ..Default::default() };
    apply_patch(&mut item, &repair_patch);
    assert!(item.unknown.is_empty());
    assert!(item.is_empty());
}

#[test]
fn patch_json_null_rejected() {
    let json = r#"{"name": null}"#;
    let res = serde_json::from_str::<ItemOverridePatch>(json);
    assert!(res.is_err(), "JSON null should be rejected for patch field op");
}

#[test]
fn patch_op_tagged_roundtrip() {
    let op_set = OverrideFieldOp::Set("test_value".to_string());
    let json_set = serde_json::to_string(&op_set).expect("serialize set");
    assert_eq!(json_set, r#"{"op":"set","value":"test_value"}"#);
    let parsed_set: OverrideFieldOp<String> = serde_json::from_str(&json_set).expect("deserialize set");
    assert_eq!(parsed_set, op_set);

    let op_clear: OverrideFieldOp<String> = OverrideFieldOp::Clear;
    let json_clear = serde_json::to_string(&op_clear).expect("serialize clear");
    assert_eq!(json_clear, r#"{"op":"clear"}"#);
    let parsed_clear: OverrideFieldOp<String> = serde_json::from_str(&json_clear).expect("deserialize clear");
    assert_eq!(parsed_clear, op_clear);

    let op_reset: OverrideFieldOp<String> = OverrideFieldOp::Reset;
    let json_reset = serde_json::to_string(&op_reset).expect("serialize reset");
    assert_eq!(json_reset, r#"{"op":"reset"}"#);
    let parsed_reset: OverrideFieldOp<String> = serde_json::from_str(&json_reset).expect("deserialize reset");
    assert_eq!(parsed_reset, op_reset);
}

#[test]
fn patch_reset_removes_key() {
    let mut item = ItemOverride { name: FieldDelta::Set("Current Name".to_string()), ..Default::default() };

    let patch = ItemOverridePatch { name: Some(OverrideFieldOp::Reset), ..Default::default() };
    item.apply_patch(&patch);

    assert!(item.name.is_keep());
    assert!(item.is_empty());

    let yaml = serde_saphyr::to_string(&item).expect("serialize");
    assert!(!yaml.contains("name:"));
}

#[test]
fn patch_clear_writes_null() {
    let mut item = ItemOverride::default();
    let patch = ItemOverridePatch { logo: Some(OverrideFieldOp::Clear), ..Default::default() };
    item.apply_patch(&patch);

    assert_eq!(item.logo, FieldDelta::Clear);
    assert!(!item.is_empty());

    let yaml = serde_saphyr::to_string(&item).expect("serialize");
    assert!(yaml.contains("logo: null"));
}

#[test]
fn validate_rejects_chno_zero_rating_11_bad_url_bad_date() {
    let item = ItemOverride {
        chno: FieldDelta::Set(0),
        rating: FieldDelta::Set(11.0),
        logo: FieldDelta::Set("not-a-url".to_string()),
        release_date: FieldDelta::Set("invalid-date".to_string()),
        ..Default::default()
    };

    let errors = item.validate(OverrideItemKind::Vod);
    let fields: Vec<&str> = errors.iter().map(|e| e.field.as_str()).collect();

    assert!(fields.contains(&"chno"), "chno=0 or chno on vod must be reported");
    assert!(fields.contains(&"rating"), "rating=11.0 must be reported");
    assert!(fields.contains(&"logo"), "not-a-url must be reported");
    assert!(fields.contains(&"release_date"), "invalid-date must be reported");

    let live_item = ItemOverride { chno: FieldDelta::Set(0), ..Default::default() };
    let live_errors = validate(&live_item, OverrideItemKind::Live);
    assert_eq!(live_errors.len(), 1);
    assert_eq!(live_errors[0].field, "chno");
    assert_eq!(live_errors[0].message_key, "error.invalid_channel_no");
}

#[test]
fn validate_rejects_chno_on_vod() {
    let item = ItemOverride { chno: FieldDelta::Set(100), ..Default::default() };

    let errors = item.validate(OverrideItemKind::Vod);
    assert!(errors.iter().any(|e| e.field == "chno"));
}

#[test]
fn item_kind_from_playlist_item_type() {
    assert_eq!(OverrideItemKind::from(PlaylistItemType::Live), OverrideItemKind::Live);
    assert_eq!(OverrideItemKind::from(PlaylistItemType::LiveHls), OverrideItemKind::Live);
    assert_eq!(OverrideItemKind::from(PlaylistItemType::LiveDash), OverrideItemKind::Live);
    assert_eq!(OverrideItemKind::from(PlaylistItemType::LiveUnknown), OverrideItemKind::Live);
    assert_eq!(OverrideItemKind::from(PlaylistItemType::Catchup), OverrideItemKind::Live);

    assert_eq!(OverrideItemKind::from(PlaylistItemType::Video), OverrideItemKind::Vod);
    assert_eq!(OverrideItemKind::from(PlaylistItemType::LocalVideo), OverrideItemKind::Vod);

    assert_eq!(OverrideItemKind::from(PlaylistItemType::Series), OverrideItemKind::Series);
    assert_eq!(OverrideItemKind::from(PlaylistItemType::SeriesInfo), OverrideItemKind::Series);
    assert_eq!(OverrideItemKind::from(PlaylistItemType::LocalSeries), OverrideItemKind::Series);
    assert_eq!(OverrideItemKind::from(PlaylistItemType::LocalSeriesInfo), OverrideItemKind::Series);
}

#[test]
fn parse_future_schema_is_unsupported() {
    let yaml = "schema_version: 2\nlive: {}\n";
    let res = parse_input_override_file(yaml);
    assert!(matches!(res, Err(OverrideParseError::UnsupportedSchema(2))));
}

#[test]
fn parse_unknown_key_reported_rest_applied() {
    let yaml = r#"
schema_version: 1
live:
  "12345":
    name: "Das Erste HD"
    epg_chanel_id: "typo"
"#;
    let (file, errors) = parse_input_override_file(yaml).expect("parse input override");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].field, "epg_chanel_id");
    assert_eq!(errors[0].entry.as_deref(), Some("live.12345"));

    let item = file.live.get("12345").expect("entry 12345");
    assert_eq!(item.name, FieldDelta::Set("Das Erste HD".to_string()));
    assert_eq!(item.unknown.get("epg_chanel_id"), Some(&serde_json::json!("typo")));
}

#[test]
fn parse_wrong_type_field_reports_and_keeps_entry() {
    let yaml = r#"
schema_version: 1
live:
  "12345":
    name: "Das Erste HD"
    chno: "abc"
"#;
    let (file, errors) = parse_input_override_file(yaml).expect("parse input override");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].field, "chno");
    assert_eq!(errors[0].entry.as_deref(), Some("live.12345"));

    let item = file.live.get("12345").expect("entry 12345");
    assert_eq!(item.name, FieldDelta::Set("Das Erste HD".to_string()));
    assert!(item.chno.is_keep());
    assert_eq!(item.unknown.get("chno"), Some(&serde_json::json!("abc")));

    let serialized = serde_saphyr::to_string(&item).expect("serialize");
    assert!(serialized.contains("chno: abc") || serialized.contains("chno: \"abc\""));
}

#[test]
fn parse_target_override_file_structure() {
    let yaml = r#"
schema_version: 1
inputs:
  provider-a:
    live:
      "12345":
        _hint: "DE | Vollprogramm / Das Erste"
        group: "Favoriten"
"#;
    let (file, errors) = parse_target_override_file(yaml).expect("parse target override");
    assert_eq!(errors, &[] as &[FieldError]);
    assert_eq!(file.schema_version, 1);

    let sections = file.inputs.get("provider-a").expect("provider-a");
    let item = sections.live.get("12345").expect("entry 12345");
    assert_eq!(item.hint.as_deref(), Some("DE | Vollprogramm / Das Erste"));
    assert_eq!(item.group, FieldDelta::Set("Favoriten".to_string()));
}

#[test]
fn parse_schema_version_out_of_range_and_invalid() {
    // Missing schema_version
    let res = parse_input_override_file("live: {}\n");
    assert_eq!(res, Err(OverrideParseError::Malformed("missing schema_version".to_string())));

    // Not an integer (string)
    let res = parse_input_override_file("schema_version: \"1\"\nlive: {}\n");
    assert_eq!(res, Err(OverrideParseError::Malformed("schema_version must be an integer".to_string())));

    // Not an integer (float)
    let res = parse_input_override_file("schema_version: 1.5\nlive: {}\n");
    assert_eq!(res, Err(OverrideParseError::Malformed("schema_version must be an integer".to_string())));

    // Negative integer
    let res = parse_input_override_file("schema_version: -1\nlive: {}\n");
    assert_eq!(res, Err(OverrideParseError::UnsupportedSchema(u32::MAX)));

    // Out of u32 range
    let res = parse_input_override_file("schema_version: 9999999999999\nlive: {}\n");
    assert_eq!(res, Err(OverrideParseError::UnsupportedSchema(u32::MAX)));
}

#[test]
fn parse_structural_problems_make_file_malformed() {
    // Unknown root key in input file
    let res = parse_input_override_file("schema_version: 1\nlvie: {}\n");
    assert_eq!(res, Err(OverrideParseError::Malformed("unknown root key 'lvie'".to_string())));

    // Section not a mapping
    let res = parse_input_override_file("schema_version: 1\nlive: \"not a map\"\n");
    assert_eq!(res, Err(OverrideParseError::Malformed("live: expected mapping".to_string())));

    // Entry not a mapping
    let res = parse_input_override_file("schema_version: 1\nlive:\n  '12345': 42\n");
    assert_eq!(res, Err(OverrideParseError::Malformed("live.12345: expected mapping".to_string())));

    // Target file unknown root key
    let res = parse_target_override_file("schema_version: 1\ninput: {}\n");
    assert_eq!(res, Err(OverrideParseError::Malformed("unknown root key 'input'".to_string())));

    // Target file inputs not a mapping
    let res = parse_target_override_file("schema_version: 1\ninputs: \"bad\"\n");
    assert_eq!(res, Err(OverrideParseError::Malformed("inputs: expected mapping".to_string())));

    // Target file input entry not a mapping
    let res = parse_target_override_file("schema_version: 1\ninputs:\n  provider-a: 42\n");
    assert_eq!(res, Err(OverrideParseError::Malformed("inputs.provider-a: expected mapping".to_string())));

    // Target file entry not a mapping
    let res = parse_target_override_file("schema_version: 1\ninputs:\n  provider-a:\n    live:\n      '12345': 42\n");
    assert_eq!(res, Err(OverrideParseError::Malformed("inputs.provider-a.live.12345: expected mapping".to_string())));
}

#[test]
fn patch_deny_unknown_fields() {
    let json = r#"{"name": {"op":"set","value":"A"}, "unknown_field": 123}"#;
    let res = serde_json::from_str::<ItemOverridePatch>(json);
    assert!(res.is_err(), "Patch must reject unknown fields");
}

#[test]
fn serialized_output_sorted_with_hint_first_and_unknown_roundtrip() {
    let yaml = r#"
schema_version: 1
live:
  "12345":
    _hint: "My Hint"
    name: "A"
    aaa_typo: 1
    nested_obj:
      key: "val"
    nested_seq:
      - 1
      - 2
"#;
    let (file, errors) = parse_input_override_file(yaml).expect("parse input override");
    assert_eq!(errors.len(), 3); // aaa_typo, nested_obj, nested_seq are unknown fields
    let item = file.live.get("12345").expect("entry 12345");

    let serialized = serde_saphyr::to_string(&item).expect("serialize");
    let lines: Vec<&str> = serialized.lines().map(str::trim).filter(|l| !l.is_empty()).collect();

    // Verify _hint is first line
    assert!(lines[0].starts_with("_hint:"));

    // Verify aaa_typo comes before name
    let pos_aaa = lines.iter().position(|l| l.starts_with("aaa_typo:")).expect("aaa_typo");
    let pos_name = lines.iter().position(|l| l.starts_with("name:")).expect("name");
    assert!(pos_aaa < pos_name, "aaa_typo must serialize before name");

    // Test that nested unknown value round-trips through re-parse
    let full_doc = format!(
        "schema_version: 1\nlive:\n  '12345':\n{}",
        serialized.lines().map(|l| format!("    {l}\n")).collect::<String>()
    );
    let (reparsed_file, _) = parse_input_override_file(&full_doc).expect("re-parse");
    let reparsed_item = reparsed_file.live.get("12345").expect("reparsed item");
    assert_eq!(reparsed_item.unknown.get("nested_obj"), Some(&serde_json::json!({"key": "val"})));
    assert_eq!(reparsed_item.unknown.get("nested_seq"), Some(&serde_json::json!([1, 2])));
}

#[test]
fn field_error_with_entry_and_parse_errors() {
    let yaml = r#"
schema_version: 1
inputs:
  provider-a:
    live:
      "12345":
        epg_chanel_id: "typo"
"#;
    let (_, errors) = parse_target_override_file(yaml).expect("parse target override");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].entry.as_deref(), Some("provider-a.live.12345"));
    assert_eq!(errors[0].field, "epg_chanel_id");

    // validate() leaves entry as None
    let item = ItemOverride { chno: FieldDelta::Set(0), ..Default::default() };
    let val_errors = item.validate(OverrideItemKind::Live);
    assert_eq!(val_errors.len(), 1);
    assert_eq!(val_errors[0].entry, None);
}

#[test]
fn prune_empty_removes_empty_entries() {
    let mut sections = InputOverrideSections::default();

    // 1. empty entry
    sections.live.insert("empty".to_string(), ItemOverride::default());
    // 2. hint-only entry
    sections
        .live
        .insert("hint_only".to_string(), ItemOverride { hint: Some("hint".to_string()), ..Default::default() });
    // 3. non-empty delta entry
    sections.live.insert(
        "valid_delta".to_string(),
        ItemOverride { name: FieldDelta::Set("Channel".to_string()), ..Default::default() },
    );
    // 4. non-empty unknown field entry
    let mut item_with_unknown = ItemOverride::default();
    item_with_unknown.unknown.insert("typo".to_string(), serde_json::json!(1));
    sections.vod.insert("has_unknown".to_string(), item_with_unknown);
    // 5. hint-only on series
    sections
        .series
        .insert("hint_series".to_string(), ItemOverride { hint: Some("series".to_string()), ..Default::default() });

    sections.prune_empty();

    assert_eq!(sections.live.len(), 1);
    assert!(sections.live.contains_key("valid_delta"));
    assert_eq!(sections.vod.len(), 1);
    assert!(sections.vod.contains_key("has_unknown"));
    assert_eq!(sections.series.len(), 0);
}

#[test]
fn validate_rejects_url_without_host() {
    let item = ItemOverride { logo: FieldDelta::Set("http:foo".to_string()), ..Default::default() };
    let errors = item.validate(OverrideItemKind::Live);
    assert!(errors.iter().any(|e| e.field == "logo" && e.message_key == "error.invalid_url"));
}

#[test]
fn parse_chno_and_rating_null_sets_clear() {
    let yaml = r#"
schema_version: 1
vod:
  "12345":
    chno: null
    rating: null
"#;
    let (file, errors) = parse_input_override_file(yaml).expect("parse input override");
    assert_eq!(errors, &[] as &[FieldError]);
    let item = file.vod.get("12345").expect("entry 12345");
    assert_eq!(item.chno, FieldDelta::Clear);
    assert_eq!(item.rating, FieldDelta::Clear);
}

#[test]
fn parse_malformed_yaml_returns_malformed() {
    let yaml = ": invalid: [yaml syntax";
    let res = parse_input_override_file(yaml);
    assert!(matches!(res, Err(OverrideParseError::Malformed(_))));
}

#[test]
fn target_file_same_id_two_inputs_stays_separate() {
    let yaml = r#"
schema_version: 1
inputs:
  provider-a:
    live:
      "12345":
        group: "Group A"
  provider-b:
    live:
      "12345":
        group: "Group B"
"#;
    let (file, errors) = parse_target_override_file(yaml).expect("parse target override");
    assert_eq!(errors, &[] as &[FieldError]);

    let item_a = file.inputs.get("provider-a").unwrap().live.get("12345").unwrap();
    let item_b = file.inputs.get("provider-b").unwrap().live.get("12345").unwrap();
    assert_eq!(item_a.group, FieldDelta::Set("Group A".to_string()));
    assert_eq!(item_b.group, FieldDelta::Set("Group B".to_string()));
}
