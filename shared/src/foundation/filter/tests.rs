use super::{Filter, FilterBuilder, Leaf, Node, StringOperator};
use crate::{
    foundation::{filter::get_filter, prepare_templates, value_provider::ValueProvider},
    model::{ItemField, PatternTemplate, PlaylistItem, PlaylistItemHeader, PlaylistItemType, TemplateValue},
    utils::{Internable, CONSTANTS},
};
use std::borrow::Cow;

/// Longest chain the node budget admits; a term costs a leaf and a chain node.
fn max_chain_terms() -> usize { super::parser::MAX_FILTER_NODES / 2 }

fn chain_of(terms: usize) -> String {
    (0..terms).map(|i| format!(r#"Group ~ "^NOPE{i}$""#)).collect::<Vec<_>>().join(" OR ")
}

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
fn test_filter_template_with_full_expression_keeps_regex_unanchored() {
    let regex = r#"Name ~ "(?i)(CNBC|Fox News Channel|The Weather Channel)""#;
    let mut templates = vec![PatternTemplate {
        name: "NEWS".to_string(),
        value: TemplateValue::Single(regex.to_string()),
        placeholder: String::new(),
    }];
    let prepared = prepare_templates(&mut templates).expect("templates should prepare");

    let direct = get_filter(regex, None).expect("direct filter should parse");
    let templated = get_filter("!NEWS!", Some(&prepared)).expect("templated filter should parse");
    assert_eq!(format!("{direct}"), regex);
    assert_eq!(format!("{templated}"), regex);

    let channel = create_mock_pli("US: Fox News Channel HD", "News");
    assert!(templated.filter(&ValueProvider { pli: &channel, match_as_ascii: false }));
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

/// The `expr` rule iterates over its operands instead of recursing once per
/// `AND`/`OR`, so a long chain must not exhaust the parser stack. Before the
/// change chains of a few dozen terms failed with `stack limit reached` on the
/// static musl build.
#[test]
fn test_filter_long_chain_parses() {
    for terms in [200usize, max_chain_terms()] {
        let filter_text = chain_of(terms);
        // Parsing only: evaluation still walks the tree recursively and is covered by
        // its own test at the longest chain the parser accepts.
        get_filter(&filter_text, None).expect("long chain parses");
    }
}

/// A filter larger than the node budget is rejected while it is still being built, so the
/// memory is never allocated. The message names the budget instead of dumping the filter.
#[test]
fn test_filter_beyond_the_node_budget_is_rejected() {
    let filter_text = chain_of(max_chain_terms() + 1);
    let err = get_filter(&filter_text, None).expect_err("over-large filter is rejected");
    let message = err.to_string();
    assert!(message.contains("too large"), "unexpected message: {message}");
    assert!(message.contains("nodes"), "message should name the unit: {message}");
    // The generic parse-error path appends the whole filter text below this line; the
    // budget diagnostic itself stays short.
    let first_line = message.lines().next().unwrap_or_default();
    assert!(first_line.len() < 200, "budget diagnostic must stay short: {}", first_line.len());
}

/// The budget counts the whole filter, not one expression. Without that, nesting would
/// multiply it: parentheses may nest 256 deep, so a per-expression limit would admit
/// hundreds of thousands of terms.
#[test]
fn test_node_budget_counts_nested_groups() {
    let per_group = max_chain_terms() / 4;
    let groups = 4usize;
    let parts: Vec<String> = (0..groups).map(|_| format!("({})", chain_of(per_group))).collect();
    let filter_text = parts.join(" OR ");
    let err = get_filter(&filter_text, None).expect_err("the total budget must count every group");
    let message = err.to_string();
    assert!(message.contains("too large"), "unexpected message: {message}");

    // The same number of terms spread over groups *within* the budget is still accepted.
    let per_group = max_chain_terms() / 8;
    let parts: Vec<String> = (0..groups).map(|_| format!("({})", chain_of(per_group))).collect();
    get_filter(&parts.join(" OR "), None).expect("a filter within the budget parses");
}

/// Evaluation walks the tree recursively, so it is limited by stack depth per
/// term rather than by the parser. The longest chain the parser accepts must still
/// be safe to evaluate on a worker thread stack.
#[test]
fn test_filter_chain_evaluates() {
    let terms = max_chain_terms();
    let filter_text = chain_of(terms);
    let filter = get_filter(&filter_text, None).expect("chain parses");
    let item = create_mock_pli("Channel", "Group");
    let provider = ValueProvider { pli: &item, match_as_ascii: false };
    // Every term is a non-match, so evaluation walks the whole chain.
    assert!(!filter.filter(&provider));
}

/// `NOT` binds to the operand that follows it, exactly as before the grammar change.
#[test]
fn test_filter_long_not_chain_parses() {
    let terms = 500usize;
    let filter_text = (0..terms).map(|i| format!(r#"NOT Name = "n{i}""#)).collect::<Vec<_>>().join(" AND ");
    let filter = get_filter(&filter_text, None).expect("long NOT chain parses");

    let matching = create_mock_pli("n0", "Group");
    let provider = ValueProvider { pli: &matching, match_as_ascii: false };
    assert!(!filter.filter(&provider), "one negated term must reject the item");

    let other = create_mock_pli("elsewhere", "Group");
    let provider = ValueProvider { pli: &other, match_as_ascii: false };
    assert!(filter.filter(&provider), "all negated terms hold for another name");
}

/// Resolves the root node of a parsed filter.
fn root_of(filter: &Filter) -> Node {
    let root = filter.root();
    *filter.node(root).expect("root node exists")
}

/// Chains group from the right, as the previous recursive grammar did. A left fold
/// would turn `a AND b OR c` into `(a AND b) OR c` and silently change results.
#[test]
fn test_filter_chain_associativity_is_right() {
    let and_then_or = get_filter(r#"Name = "a" AND Name = "b" OR Group = "c""#, None).expect("filter parses");
    match root_of(&and_then_or) {
        Node::And(lhs, rhs) => {
            assert!(matches!(and_then_or.node(lhs), Some(Node::Leaf(_))));
            assert!(matches!(and_then_or.node(rhs), Some(Node::Or(..))));
        }
        other => panic!("expected `a AND (b OR c)`, got {other:?}"),
    }

    let or_then_and = get_filter(r#"Name = "a" OR Name = "b" AND Group = "c""#, None).expect("filter parses");
    match root_of(&or_then_and) {
        Node::Or(lhs, rhs) => {
            assert!(matches!(or_then_and.node(lhs), Some(Node::Leaf(_))));
            assert!(matches!(or_then_and.node(rhs), Some(Node::And(..))));
        }
        other => panic!("expected `a OR (b AND c)`, got {other:?}"),
    }
}

/// `NOT` applies to one operand, not to the rest of the chain.
#[test]
fn test_filter_not_binds_to_single_operand() {
    let filter = get_filter(r#"NOT Name = "a" AND Name = "b""#, None).expect("filter parses");
    match root_of(&filter) {
        Node::And(lhs, rhs) => {
            assert!(matches!(filter.node(lhs), Some(Node::Not(_))));
            assert!(matches!(filter.node(rhs), Some(Node::Leaf(_))));
        }
        other => panic!("expected `(NOT a) AND b`, got {other:?}"),
    }
    // Doubled NOT stays a syntax error.
    assert!(get_filter(r#"NOT NOT Name = "a""#, None).is_err());
}

/// `Display` walks the tree with an explicit stack. The tree is built directly here,
/// because the parser bounds chain length for evaluation — a separate concern. The previous
/// recursive implementation already overflowed a 2 MiB thread at 2000 terms of this shape.
#[test]
fn test_filter_display_renders_deep_tree_without_recursion() {
    // Right-nested, the shape the parser produces. The grammar groups from the right, so no
    // parentheses are needed and none must be emitted, whatever the length.
    let terms = 10_000usize;
    let mut builder = FilterBuilder::new();
    let leaf = builder.leaf(Leaf::StringComparison(ItemField::Name, StringOperator::Eq, "leaf".to_string()));
    let mut root = leaf;
    for _ in 0..terms {
        root = builder.or(leaf, root);
    }
    let filter = builder.finish(root);

    let rendered = filter.to_string();
    assert_eq!(rendered.matches(" OR ").count(), terms);
    assert_eq!(
        rendered,
        format!(r#"Name = "leaf" OR {}"#, rendered.strip_prefix(r#"Name = "leaf" OR "#).expect("prefix"))
    );
    assert!(!rendered.contains('('), "a right-nested chain needs no parentheses");
}

/// Evaluation continues on an explicit stack once the recursive depth budget is used up, so a
/// long chain no longer needs stack proportional to its length. A 256 KiB thread is far below
/// what the recursive evaluator needed for this length; the chain stays well under the drop
/// capacity of such a thread, which is the remaining recursive step.
#[test]
fn test_filter_evaluates_deep_chain_on_small_stack() {
    let text = chain_of(1_000);
    let item = create_mock_pli("Channel", "Group");
    let rendered = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || {
            let filter = get_filter(&text, None).expect("chain parses");
            // Every term is a non-match, so evaluation walks the whole chain.
            (filter.filter(&ValueProvider { pli: &item, match_as_ascii: false }), filter.to_string().len())
        })
        .expect("spawn")
        .join()
        .expect("evaluation finishes");
    assert!(!rendered.0);
    assert!(rendered.1 > 0);
}

/// Each `expr_group` re-enters the parser, and pest 2.9.5 does not probe the stack on musl,
/// so nesting is bounded before the recursion can exhaust a worker thread's stack.
#[test]
fn test_filter_nesting_beyond_limit_is_rejected() {
    // The documented boundary: exactly this many levels parse, one more does not.
    let allowed = super::parser::MAX_NESTING_DEPTH;
    let accepted = format!("{}Name = \"a\"{}", "(".repeat(allowed), ")".repeat(allowed));
    get_filter(&accepted, None).expect("nesting up to the documented limit parses");

    let rejected = format!("{}Name = \"a\"{}", "(".repeat(allowed + 1), ")".repeat(allowed + 1));
    let err = get_filter(&rejected, None).expect_err("nesting beyond the limit is rejected");
    let message = err.to_string();
    assert!(message.contains("nested too deeply"), "unexpected message: {message}");
    assert!(message.contains(&allowed.to_string()), "message should name the limit: {message}");
}

/// A rejected over-long filter must not be echoed into the log in full.
#[test]
fn test_filter_parse_error_abbreviates_long_input() {
    let text = chain_of(max_chain_terms() + 1);
    let err = get_filter(&text, None).expect_err("chain is rejected");
    let message = err.to_string();
    assert!(message.len() < 500, "error must stay short, was {} bytes", message.len());
    assert!(message.contains("characters"), "error should state the full length: {message}");
}

/// A filter is an arena, so dropping a deep chain is a single buffer release. The previous
/// boxed representation recursed once per term in the generated `Drop` and needed roughly
/// 48 bytes of stack per term.
#[test]
fn test_filter_drops_deep_chain_on_small_stack() {
    let text = chain_of(5_000);
    let dropped = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || {
            let filter = get_filter(&text, None).expect("chain parses");
            drop(filter);
        })
        .expect("spawn")
        .join();
    assert!(dropped.is_ok(), "dropping a deep chain must not touch the stack");
}
