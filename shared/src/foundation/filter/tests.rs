use crate::{
    foundation::{filter::get_filter, prepare_templates, value_provider::ValueProvider},
    model::{ItemField, PatternTemplate, PlaylistItem, PlaylistItemHeader, PlaylistItemType, TemplateValue},
    utils::{Internable, CONSTANTS},
};
use std::borrow::Cow;

fn create_mock_pli(name: &str, group: &str) -> PlaylistItem {
    PlaylistItem { header: PlaylistItemHeader { name: name.into(), group: group.intern(), ..Default::default() } }
}

/// Parse `input` and assert that `Display` round-trips back to `input`
/// byte-for-byte. Used by the three filter-parser tests below that only
/// exercise the parser and its `Display` impl, not the runtime evaluator.
fn assert_filter_round_trip(input: &str) {
    match get_filter(input, None) {
        Ok(filter) => assert_eq!(format!("{filter}"), input),
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn test_filter_1() {
    assert_filter_round_trip(
        r#"(Group ~ "A" OR Group ~ "B") AND (Name ~ "C" OR Name ~ "D" OR Name ~ "E") OR (NOT (Title ~ "F") AND NOT Title ~ "K")"#,
    );
}

#[test]
fn test_filter_2() {
    assert_filter_round_trip(
        r#"Group ~ "d" AND ((Name ~ "e" AND NOT ((Name ~ "c" OR Name ~ "f"))) OR (Name ~ "a" OR Name ~ "b"))"#,
    );
}

#[test]
fn test_filter_3() {
    assert_filter_round_trip(
        r#"Group ~ "d" AND ((Name ~ "e" AND NOT ((Name ~ "c" OR Name ~ "f"))) OR (Name ~ "a" OR Name ~ "b")) AND (Type = movie)"#,
    );
}

#[test]
fn test_filter_string_ops_round_trip() {
    assert_filter_round_trip(
        r#"Group = "Sports" AND Title != "News" OR Name CONTAINS "HD" AND Caption STARTSWITH "DE:""#,
    );
}

#[test]
fn test_filter_numeric_round_trip() { assert_filter_round_trip(r"Chno >= 100 AND Chno < 200 OR NOT Chno = 7"); }

#[test]
fn test_filter_set_round_trip() { assert_filter_round_trip(r#"Group IN ["Sports", "News"] AND NOT (Name IN ["A"])"#); }

#[test]
fn test_filter_presence_round_trip_and_aliases() {
    assert_filter_round_trip("EpgId IS EMPTY");
    assert_filter_round_trip("EpgId IS NOT EMPTY");

    assert_eq!(get_filter("EpgId = EMPTY", None).expect("alias parses").to_string(), "EpgId IS EMPTY");
    assert_eq!(get_filter("EpgId != EMPTY", None).expect("alias parses").to_string(), "EpgId IS NOT EMPTY");
}

#[test]
fn test_filter_presence_eval() {
    let empty = get_filter("EpgId IS EMPTY", None).expect("empty filter parses");
    let populated = get_filter("EpgId IS NOT EMPTY", None).expect("populated filter parses");
    let mut item = create_mock_pli("Channel", "Group");
    let provider = ValueProvider { pli: &item, match_as_ascii: false };
    assert!(empty.filter(&provider));
    assert!(!populated.filter(&provider));

    item.header.epg_channel_id = Some("channel.epg".intern());
    let provider = ValueProvider { pli: &item, match_as_ascii: false };
    assert!(!empty.filter(&provider));
    assert!(populated.filter(&provider));

    item.header.epg_channel_id = Some("".intern());
    let provider = ValueProvider { pli: &item, match_as_ascii: false };
    assert!(empty.filter(&provider));
    assert!(!populated.filter(&provider));
}

#[test]
fn test_filter_string_ops_eval() {
    let flt = r#"Group = "sports" AND Name CONTAINS "hd" AND NOT (Name STARTSWITH "x")"#;
    let filter = get_filter(flt, None).expect("filter parses");
    let matching = create_mock_pli("Channel HD", "Sports");
    assert!(filter.filter(&ValueProvider { pli: &matching, match_as_ascii: false }));
    let other = create_mock_pli("Channel", "Sports");
    assert!(!filter.filter(&ValueProvider { pli: &other, match_as_ascii: false }));
}

#[test]
fn test_filter_set_eval() {
    let filter = get_filter(r#"Group IN ["Sports", "News"]"#, None).expect("filter parses");
    let matching = create_mock_pli("A", "news");
    assert!(filter.filter(&ValueProvider { pli: &matching, match_as_ascii: false }));
    let other = create_mock_pli("B", "Movies");
    assert!(!filter.filter(&ValueProvider { pli: &other, match_as_ascii: false }));
}

#[test]
fn test_filter_caption_string_ops_fall_back_to_name() {
    // title and name differ; only the name matches
    let mut pli = create_mock_pli("Channel HD", "Sports");
    pli.header.title = "Some Title".into();
    let provider = ValueProvider { pli: &pli, match_as_ascii: false };

    let contains = get_filter(r#"Caption CONTAINS "hd""#, None).expect("filter parses");
    assert!(contains.filter(&provider));

    let starts = get_filter(r#"Caption STARTSWITH "channel""#, None).expect("filter parses");
    assert!(starts.filter(&provider));

    let eq = get_filter(r#"Caption = "channel hd""#, None).expect("filter parses");
    assert!(eq.filter(&provider));
    // NotEq is the negation of Eq-on-either
    let not_eq = get_filter(r#"Caption != "channel hd""#, None).expect("filter parses");
    assert!(!not_eq.filter(&provider));

    let set = get_filter(r#"Caption IN ["Channel HD"]"#, None).expect("filter parses");
    assert!(set.filter(&provider));

    let no_match = get_filter(r#"Caption CONTAINS "missing""#, None).expect("filter parses");
    assert!(!no_match.filter(&provider));
}

#[test]
fn test_filter_chno_eval() {
    let filter = get_filter("Chno >= 10 AND Chno <= 20", None).expect("filter parses");
    let mut pli = create_mock_pli("A", "G");
    pli.header.chno = 15;
    assert!(filter.filter(&ValueProvider { pli: &pli, match_as_ascii: false }));
    pli.header.chno = 5;
    assert!(!filter.filter(&ValueProvider { pli: &pli, match_as_ascii: false }));
}

#[test]
fn test_filter_quality_round_trip_and_eval() {
    assert_filter_round_trip(r"Quality >= 3 AND NOT Quality = 5");

    let filter = get_filter("Quality >= 3", None).expect("filter parses");
    let mut fhd = create_mock_pli("X", "G");
    fhd.header.title = "News FHD".into();
    assert!(filter.filter(&ValueProvider { pli: &fhd, match_as_ascii: false }));
    let mut hd = create_mock_pli("X", "G");
    hd.header.title = "News HD".into();
    assert!(!filter.filter(&ValueProvider { pli: &hd, match_as_ascii: false }));
    let unknown = create_mock_pli("News", "G");
    assert!(!filter.filter(&ValueProvider { pli: &unknown, match_as_ascii: false }));
}

#[test]
fn test_filter_4() {
    let flt = r#"NOT (Name ~ ".*24/7.*" AND Group ~ "^US.*")"#;
    match get_filter(flt, None) {
        Ok(filter) => {
            assert_eq!(format!("{filter}"), flt);
            let channels = [
                create_mock_pli("24/7: Cars", "FR Channels"),
                create_mock_pli("24/7: Cars", "US Channels"),
                create_mock_pli("Entertainment", "US Channels"),
            ];
            let filtered: Vec<&PlaylistItem> = channels
                .iter()
                .filter(|&chan| {
                    let provider = ValueProvider { pli: chan, match_as_ascii: false };
                    filter.filter(&provider)
                })
                .collect();
            assert_eq!(filtered.len(), 2);
            assert!(filtered.iter().any(|&chan| {
                let group = chan.header.group.to_string();
                let name = chan.header.name.to_string();
                name.eq("24/7: Cars") && group.eq("FR Channels")
            }));
            assert!(filtered.iter().any(|&chan| {
                let group = chan.header.group.to_string();
                let name = chan.header.name.to_string();
                name.eq("Entertainment") && group.eq("US Channels")
            }));
            assert!(!filtered.iter().any(|&chan| {
                let group = chan.header.group.to_string();
                let name = chan.header.name.to_string();
                name.eq("24/7: Cars") && group.eq("US Channels")
            }));
        }
        Err(e) => {
            panic!("{e}")
        }
    }
}

#[test]
fn test_filter_5() {
    let flt =
        r#"NOT (Name ~ "NC" OR Group ~ "GA") AND (Name ~ "NA" AND Group ~ "GA") OR (Name ~ "NB" AND Group ~ "GB")"#;
    match get_filter(flt, None) {
        Ok(filter) => {
            assert_eq!(format!("{filter}"), flt);
            let channels = [
                create_mock_pli("NA", "GA"),
                create_mock_pli("NB", "GB"),
                create_mock_pli("NA", "GB"),
                create_mock_pli("NB", "GA"),
                create_mock_pli("NC", "GA"),
                create_mock_pli("NA", "GC"),
            ];
            let filtered: Vec<&PlaylistItem> = channels
                .iter()
                .filter(|&chan| {
                    let provider = ValueProvider { pli: chan, match_as_ascii: false };
                    filter.filter(&provider)
                })
                .collect();
            assert_eq!(filtered.len(), 1);
        }
        Err(e) => {
            panic!("{e}")
        }
    }
}

#[test]
fn test_filter_6() {
    let flt = r####"
            Group ~ "^EU \| FRANCE.*"
            OR  Input ~ "hello"
            OR  Group ~ "^VOD \| FR.*"
            OR  Group ~ "\[FR\].*"
            OR  Group ~ "^SRS \| FR.*"
            AND NOT (Group ~ ".* LQ.*"
            OR Title ~ ".* LQ.*"
            OR Group ~ ".* SD.*"
            OR Title ~ ".* SD.*"
            OR Group ~ ".* HD.*"
            OR Title ~ ".* HD.*"
            OR Group ~ "(?i).*sport.*"
            OR Group ~ "(?i).*DAZN.*"
            OR Group ~ "(?i).*EQUIPE.*"
            OR Group ~ "DOM TOM.*"
            OR Group ~ "(?i).*PLUTO.*"
            OR Title ~ "(?i).*GOLD.*"
            OR Title ~ "###.*")"####;

    match get_filter(flt, None) {
        Ok(filter) => {
            let result = CONSTANTS.re_whitespace.replace_all(flt, " ");
            assert_eq!(format!("{filter}"), result.trim());
        }
        Err(e) => {
            panic!("{e}")
        }
    }
}

#[test]
fn test_filter_7() {
    let flt = r#"NOT (Name ~ ".*24/7.*")"#;
    match get_filter(flt, None) {
        Ok(filter) => {
            assert_eq!(format!("{filter}"), flt);
            let channels = [
                create_mock_pli("24/7: Cars", "FR Channels"),
                create_mock_pli("24/7: Cars", "US Channels"),
                create_mock_pli("Entertainment", "US Channels"),
            ];
            let filtered: Vec<&PlaylistItem> = channels
                .iter()
                .filter(|&chan| {
                    let provider = ValueProvider { pli: chan, match_as_ascii: false };
                    filter.filter(&provider)
                })
                .collect();
            assert_eq!(filtered.len(), 1);
            assert!(filtered.iter().any(|&chan| {
                let group = chan.header.group.to_string();
                let name = chan.header.name.to_string();
                name.eq("Entertainment") && group.eq("US Channels")
            }));
        }
        Err(e) => {
            panic!("{e}")
        }
    }
}

#[test]
fn test_filter_match_as_ascii() {
    let flt = r#"Name ~ "Cinema""#;
    match get_filter(flt, None) {
        Ok(filter) => {
            let chan = create_mock_pli("Cinéma", "Some Group");

            // Without match_as_ascii (should fail)
            let provider_fail = ValueProvider { pli: &chan, match_as_ascii: false };
            assert!(!filter.filter(&provider_fail));

            // With match_as_ascii (should succeed)
            let provider_success = ValueProvider { pli: &chan, match_as_ascii: true };
            assert!(filter.filter(&provider_success));
        }
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn filter_value_borrows_stored_field() {
    let channel = create_mock_pli("Channel", "Group");
    let provider = ValueProvider { pli: &channel, match_as_ascii: false };

    assert!(matches!(provider.get_filter_value(ItemField::Name), Some(Cow::Borrowed("Channel"))));
}

#[test]
fn missing_genre_does_not_match_empty_pattern() {
    let filter = get_filter(r#"Genre ~ "^$""#, None).expect("filter should parse");
    let channel = create_mock_pli("Channel", "Group");
    let provider = ValueProvider { pli: &channel, match_as_ascii: false };

    assert!(!filter.filter(&provider));
}

#[test]
fn caption_checks_name_when_title_does_not_match() {
    let filter = get_filter(r#"Caption ~ "Channel""#, None).expect("filter should parse");
    let mut channel = create_mock_pli("Channel", "Group");
    channel.header.title = "Different title".intern();
    let provider = ValueProvider { pli: &channel, match_as_ascii: false };

    assert!(filter.filter(&provider));
}

#[test]
fn type_filter_matches_normalized_type_families() {
    let live_filter = get_filter("Type = live", None).expect("live filter should parse");
    let series_filter = get_filter("Type = series", None).expect("series filter should parse");
    let mut channel = create_mock_pli("Channel", "Group");

    channel.header.item_type = PlaylistItemType::LiveHls;
    assert!(live_filter.filter(&ValueProvider { pli: &channel, match_as_ascii: false }));

    channel.header.item_type = PlaylistItemType::LocalSeriesInfo;
    assert!(series_filter.filter(&ValueProvider { pli: &channel, match_as_ascii: false }));
}

#[test]
fn test_filter_unknown_template_placeholder_reports_error() {
    let err = get_filter("!UNKNOWN_FILTER!", None).expect_err("unknown placeholder should fail");
    let msg = err.to_string();
    assert!(msg.contains("Unknown template placeholder(s) in filter"));
    assert!(msg.contains("!UNKNOWN_FILTER!"));
}

#[test]
fn prepare_templates_does_not_duplicate_unrelated_multi_entries() {
    let mut templates = vec![
        PatternTemplate {
            name: "A".to_string(),
            value: TemplateValue::Multi(vec!["a1".to_string(), "a2".to_string()]),
            placeholder: String::new(),
        },
        PatternTemplate {
            name: "SEQ".to_string(),
            value: TemplateValue::Multi(vec!["!A!".to_string(), "literal".to_string()]),
            placeholder: String::new(),
        },
    ];

    let prepared = prepare_templates(&mut templates).expect("templates should prepare");
    let seq = prepared.iter().find(|template| template.name == "SEQ").expect("SEQ template");

    assert_eq!(seq.value, TemplateValue::Multi(vec!["a1".to_string(), "a2".to_string(), "literal".to_string()]));
}

#[test]
fn prepare_templates_concatenates_sequence_templates_without_cartesian_blowup() {
    let mut templates = vec![
        PatternTemplate {
            name: "UK".to_string(),
            value: TemplateValue::Multi(vec!["uk1".to_string(), "uk2".to_string()]),
            placeholder: String::new(),
        },
        PatternTemplate {
            name: "US".to_string(),
            value: TemplateValue::Multi(vec!["us1".to_string(), "us2".to_string(), "us3".to_string()]),
            placeholder: String::new(),
        },
        PatternTemplate {
            name: "SEQ".to_string(),
            value: TemplateValue::Multi(vec!["!UK!".to_string(), "!US!".to_string(), "adult".to_string()]),
            placeholder: String::new(),
        },
    ];

    let prepared = prepare_templates(&mut templates).expect("templates should prepare");
    let seq = prepared.iter().find(|template| template.name == "SEQ").expect("SEQ template");

    assert_eq!(
        seq.value,
        TemplateValue::Multi(vec![
            "uk1".to_string(),
            "uk2".to_string(),
            "us1".to_string(),
            "us2".to_string(),
            "us3".to_string(),
            "adult".to_string(),
        ])
    );
}

#[test]
fn prepare_templates_promotes_single_template_to_multi_for_multi_dependency() {
    let mut templates = vec![
        PatternTemplate {
            name: "BASE".to_string(),
            value: TemplateValue::Multi(vec!["a".to_string(), "b".to_string()]),
            placeholder: String::new(),
        },
        PatternTemplate {
            name: "URL".to_string(),
            value: TemplateValue::Single("!BASE!/stream".to_string()),
            placeholder: String::new(),
        },
    ];

    let prepared = prepare_templates(&mut templates).expect("templates should prepare");
    let url = prepared.iter().find(|template| template.name == "URL").expect("URL template");

    assert_eq!(url.value, TemplateValue::Multi(vec!["a/stream".to_string(), "b/stream".to_string()]));
}
