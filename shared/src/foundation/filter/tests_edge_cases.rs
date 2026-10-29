//! Edge cases for the filter parser, the arena representation and the evaluator.
//!
//! These complement `tests.rs`: that file covers the main grammar and evaluation paths, this
//! one pins the behaviour of malformed input, of the arena API itself and of the operator
//! semantics that are easy to break while refactoring.

use super::{
    get_filter_detailed, Filter, FilterBuilder, Leaf, Node, NumericOperator, PresenceOperator, StringOperator,
};
use crate::{
    foundation::{filter::get_filter, value_provider::ValueProvider},
    model::{ItemField, PlaylistItem, PlaylistItemHeader, PlaylistItemType},
    utils::Internable,
};

fn mock(name: &str, group: &str) -> PlaylistItem {
    PlaylistItem { header: PlaylistItemHeader { name: name.into(), group: group.intern(), ..Default::default() } }
}

fn accepts(text: &str) -> bool { get_filter(text, None).is_ok() }

fn parse(text: &str) -> Filter { get_filter(text, None).unwrap_or_else(|e| panic!("{text} should parse: {e}")) }

fn root_of(filter: &Filter) -> Node { *filter.node(filter.root()).expect("root node exists") }

/// Renders a parsed filter back to text.
fn render(text: &str) -> String { parse(text).to_string() }

// ---------------------------------------------------------------------------------------------
// Malformed input: every case below must be rejected with an error, never a panic.
// ---------------------------------------------------------------------------------------------

#[test]
fn malformed_filters_are_rejected() {
    let cases = [
        ("empty", ""),
        ("whitespace only", "   \n\t "),
        ("single NOT", "NOT"),
        ("dangling OR", r#"Name = "a" OR"#),
        ("dangling AND", r#"Name = "a" AND"#),
        ("leading OR", r#"OR Name = "a""#),
        ("leading AND", r#"AND Name = "a""#),
        ("doubled OR", r#"Name = "a" OR OR Name = "b""#),
        ("doubled AND", r#"Name = "a" AND AND Name = "b""#),
        ("unclosed paren", r#"(Name = "a""#),
        ("extra closing paren", r#"Name = "a")"#),
        ("empty group", "()"),
        ("empty IN list", r"Group IN []"),
        ("doubled NOT", r#"NOT NOT Name = "a""#),
        ("missing opening quote", r"Name = a"),
        ("missing value", "Name ="),
        ("unknown field", r#"Foo = "a""#),
        ("unknown operator", r#"Name LIKE "a""#),
        ("non numeric chno value", r"Chno >= x"),
        ("chno exceeds u32", r"Chno >= 4294967296"),
        ("type without value", r#"Name = "a" AND Type ="#),
        ("unsupported type value", r"Type = unsupported"),
        ("only a boolean operator", "AND"),
        ("operator instead of operand", r#"Name = "a" OR AND Name = "b""#),
    ];
    for (label, text) in cases {
        let result = get_filter_detailed(text, None);
        assert!(result.is_err(), "{label}: {text:?} must be rejected");
    }
}

/// The position of a syntax error has to point into the source, including on later lines.
#[test]
fn syntax_errors_report_their_position() {
    let (_, position) = get_filter_detailed("", None).expect_err("empty filter is rejected");
    assert_eq!(position.map(|p| (p.line, p.column)), Some((1, 1)));

    let (_, position) =
        get_filter_detailed("Name = \"a\"\nOR\nGroup ~ unquoted", None).expect_err("unquoted regex is rejected");
    let position = position.expect("syntax errors carry a position");
    assert_eq!(position.line, 3, "error must point at the third line");
    assert!(position.column > 1, "error must point inside the line");
}

/// A semantic error has no position, and its message must not repeat the error category.
#[test]
fn semantic_errors_have_no_position_and_one_prefix() {
    let (error, position) = get_filter_detailed(r"Chno >= 4294967296", None).expect_err("value out of range");
    assert!(position.is_none(), "a semantic error has no source position");
    let message = error.to_string();
    assert!(message.contains("invalid number"), "unexpected message: {message}");
    assert_eq!(message.matches("filter parse error").count(), 1, "prefix must appear once: {message}");
}

// ---------------------------------------------------------------------------------------------
// Accepted input: values that look like syntax but are data.
// ---------------------------------------------------------------------------------------------

#[test]
fn keywords_inside_a_quoted_value_are_not_operators() {
    // `OR`, `AND`, `NOT` and parentheses inside a regex belong to the pattern.
    assert!(accepts(r#"Group ~ "a OR b""#));
    assert!(accepts(r#"Group ~ "a AND b""#));
    assert!(accepts(r#"Group ~ "NOT a""#));
    assert!(accepts(r#"Group ~ "^(a|b)$""#));
    assert!(accepts(r#"Group ~ "((a))""#));
    assert_eq!(render(r#"Group ~ "a OR b""#), r#"Group ~ "a OR b""#);

    // One leaf, not two: a quoted operator must not split the expression.
    let filter = parse(r#"Group ~ "a OR b""#);
    assert!(matches!(root_of(&filter), Node::Leaf(_)), "quoted OR must stay inside one leaf");
}

#[test]
fn escaped_quotes_survive_a_round_trip() {
    assert_eq!(render(r#"Name = "say \"hi\"""#), r#"Name = "say \"hi\"""#);
    assert_eq!(render(r#"Group ~ "a\"b""#), r#"Group ~ "a\"b""#);
    assert_eq!(render(r#"Group IN ["a\"b"]"#), r#"Group IN ["a\"b"]"#);
}

#[test]
fn keywords_and_fields_are_case_insensitive() {
    assert_eq!(
        render(r#"name = "a" and group contains "b" or not title = "c""#),
        r#"Name = "a" AND Group CONTAINS "b" OR NOT Title = "c""#
    );
    assert_eq!(render(r"EpgId IS not EMPTY"), "EpgId IS NOT EMPTY");
    assert_eq!(render(r"type = LIVE"), "Type = live");
    assert_eq!(render(r"CHNO >= 5"), "Chno >= 5");
}

#[test]
fn whitespace_and_line_breaks_between_tokens_are_ignored() {
    assert_eq!(render("  Name   =   \"a\"   AND    Group   =   \"b\"  "), r#"Name = "a" AND Group = "b""#);
    assert_eq!(render("Name = \"a\"\nOR\nGroup = \"b\""), r#"Name = "a" OR Group = "b""#);
    assert_eq!(render("Name\t=\t\"a\"\r\nAND\tGroup\t=\t\"b\""), r#"Name = "a" AND Group = "b""#);
}

#[test]
fn non_ascii_values_survive_a_round_trip() {
    assert_eq!(render(r#"Group ~ "^(Ä|Ö|Ü).*" OR Name = "Köln""#), r#"Group ~ "^(Ä|Ö|Ü).*" OR Name = "Köln""#);
}

/// Repeated parsing of rendered output must be stable: parse -> render -> parse -> render.
#[test]
fn rendering_is_idempotent() {
    let cases = [
        r#"Name = "a""#,
        r#"Name = "a" AND Name = "b" OR Group = "c""#,
        r#"NOT (Name = "a" OR Group = "b") AND Chno >= 5"#,
        r#"Group IN ["x", "y"] OR Type = movie"#,
        r#"EpgId IS NOT EMPTY AND Caption STARTSWITH "DE:""#,
        r#"((Name = "a"))"#,
    ];
    for text in cases {
        let once = render(text);
        let twice = render(&once);
        assert_eq!(once, twice, "second render must equal the first for {text}");
        // Structures must match too, not only their text.
        assert_eq!(parse(text), parse(&once), "reparsing the rendered filter must yield the same tree");
    }
}

// ---------------------------------------------------------------------------------------------
// Type and set comparisons: aliases and normalization.
// ---------------------------------------------------------------------------------------------

#[test]
fn type_aliases_normalize_to_their_family() {
    // `vod` is an alias for `movie`; the rendered form is always the canonical one.
    assert_eq!(render("Type = vod"), "Type = movie");
    assert_eq!(render("Type = movie"), "Type = movie");
    assert_eq!(render("Type = series"), "Type = series");
    assert_eq!(render("Type = live"), "Type = live");
    // The grammar only accepts the four canonical values, so other spellings never reach the
    // alias mapping.
    for rejected in ["Type = video", "Type = series-info", "Type = unsupported", "Type = Live TV"] {
        assert!(!accepts(rejected), "{rejected} is not part of the grammar");
    }
}

/// `Quality` compares against the derived quality tier and rejects non numeric input.
#[test]
fn quality_accepts_only_numeric_operands() {
    for op in [">=", "<=", "!=", ">", "<", "="] {
        assert!(accepts(&format!("Quality {op} 3")), "Quality {op} 3 should parse");
    }
    assert!(!accepts("Quality >= hd"));
}

/// A set keeps duplicates and order; an empty set is rejected by the grammar.
#[test]
fn set_comparison_preserves_order_and_duplicates() {
    assert_eq!(render(r#"Group IN ["a", "a", "b"]"#), r#"Group IN ["a", "a", "b"]"#);
    assert!(accepts(r#"Group IN ["only"]"#));
}

// ---------------------------------------------------------------------------------------------
// Evaluation: operator truth tables and negation.
// ---------------------------------------------------------------------------------------------

/// `AND`/`OR`/`NOT` over every combination of operand results.
#[test]
fn boolean_operators_follow_the_truth_table() {
    let matching = mock("hit", "hit");
    let other = mock("miss", "miss");
    // `matching` is (group "hit", name "hit"), `other` is (group "miss", name "miss").
    // P = Group = "hit" is (true, false), Q = Group = "miss" is (false, true),
    // R = Name = "hit" is (true, false).
    let cases = [
        (r#"Group = "hit""#, true, false),                                          // P
        (r#"Group = "miss""#, false, true),                                         // Q
        (r#"Group = "hit" AND Name = "hit""#, true, false),                         // P AND R
        (r#"Group = "hit" AND Group = "miss""#, false, false),                      // P AND Q
        (r#"Group = "miss" OR Name = "hit""#, true, true),                          // Q OR R
        (r#"Group = "hit" OR Group = "miss""#, true, true),                         // P OR Q
        (r#"NOT Group = "hit""#, false, true),                                      // NOT P
        (r#"NOT Group = "miss""#, true, false),                                     // NOT Q
        (r#"NOT (Group = "hit" OR Group = "miss")"#, false, false),                 // NOT (P OR Q)
        (r#"NOT (Group = "hit" AND Group = "miss")"#, true, true),                  // NOT (P AND Q)
        (r#"(Group = "hit" OR Group = "miss") AND NOT Name = "hit""#, false, true), // (P OR Q) AND NOT R
        (r#"NOT Group = "hit" OR Group = "hit""#, true, true),                      // NOT P OR P
    ];
    for (text, expected_for_matching, expected_for_other) in cases {
        let filter = parse(text);
        let got_matching = filter.filter(&ValueProvider { pli: &matching, match_as_ascii: false });
        assert_eq!(got_matching, expected_for_matching, "{text} against a matching item");
        let got_other = filter.filter(&ValueProvider { pli: &other, match_as_ascii: false });
        assert_eq!(got_other, expected_for_other, "{text} against another item");
    }
}

/// A numeric comparison against a non numeric field is a non-match, not a parse error, and it
/// must not leak into the surrounding expression.
#[test]
fn numeric_comparison_on_a_foreign_field_is_false() {
    let item = mock("channel", "group");
    let builder_leaf = Leaf::NumericComparison(ItemField::Name, NumericOperator::Greater, 0);
    let mut builder = FilterBuilder::new();
    let leaf = builder.leaf(builder_leaf);
    let root = builder.not(leaf);
    let filter = builder.finish(root);
    // NOT(false) is true, so the leaf must evaluate to false instead of short-circuiting out.
    assert!(filter.filter(&ValueProvider { pli: &item, match_as_ascii: false }));
}

#[test]
fn presence_and_string_operators_agree_on_empty_values() {
    let item = mock("", "");
    let provider = ValueProvider { pli: &item, match_as_ascii: false };
    assert!(parse("Name IS EMPTY").filter(&provider));
    assert!(!parse("Name IS NOT EMPTY").filter(&provider));
    assert!(parse(r#"Name = """#).filter(&provider), "an empty value matches an empty string");
    assert!(!parse(r#"Name CONTAINS "x""#).filter(&provider));
}

/// `Caption` falls back to `Name`, and only where the grammar says so.
#[test]
fn caption_uses_title_then_name() {
    let mut title_only = mock("name-value", "group");
    title_only.header.title = "title-value".intern();
    let provider = ValueProvider { pli: &title_only, match_as_ascii: false };
    assert!(parse(r#"Caption = "title-value""#).filter(&provider));
    assert!(parse(r#"Caption = "name-value""#).filter(&provider), "caption falls back to name");
    assert!(!parse(r#"Title = "name-value""#).filter(&provider), "title does not fall back to name");
}

/// `Type` compares against the normalized family, including the series/info aliases.
#[test]
fn type_comparison_matches_normalized_families() {
    for (item_type, live, video, series) in [
        (PlaylistItemType::Live, true, false, false),
        (PlaylistItemType::Video, false, true, false),
        (PlaylistItemType::LocalVideo, false, true, false),
        (PlaylistItemType::Series, false, false, true),
        (PlaylistItemType::SeriesInfo, false, false, true),
        (PlaylistItemType::LocalSeries, false, false, true),
    ] {
        let mut item = mock("c", "g");
        item.header.item_type = item_type;
        let provider = ValueProvider { pli: &item, match_as_ascii: false };
        assert_eq!(parse("Type = live").filter(&provider), live, "{item_type:?} vs live");
        assert_eq!(parse("Type = movie").filter(&provider), video, "{item_type:?} vs movie");
        assert_eq!(parse("Type = series").filter(&provider), series, "{item_type:?} vs series");
    }
}

// ---------------------------------------------------------------------------------------------
// The arena API itself.
// ---------------------------------------------------------------------------------------------

#[test]
fn builder_allocates_children_before_their_parent() {
    let mut builder = FilterBuilder::new();
    let leaf = builder.leaf(Leaf::PresenceComparison(ItemField::Name, PresenceOperator::IsEmpty));
    let group = builder.group(leaf);
    let root = builder.not(group);

    assert!(leaf.index() < group.index(), "a group is allocated after its child");
    assert!(group.index() < root.index(), "a negation is allocated after its child");
    assert_eq!(builder.finish(root).node_count(), 3);
}

/// Appending copies the whole tree, so the source can be dropped afterwards.
#[test]
fn append_copies_a_tree_between_filters() {
    let source = parse(r#"Name = "a" OR Group = "b""#);
    let mut builder = FilterBuilder::new();
    let copied = builder.append(&source).expect("copy");
    let combined = builder.append(&parse(r#"Name = "a""#)).expect("copy");
    let root = builder.and(copied, combined);
    let combined = builder.finish(root);

    // Structure first: the copy keeps the OR subtree as the left side of the AND.
    match root_of(&combined) {
        Node::And(left, right) => {
            assert!(matches!(combined.node(left), Some(Node::Or(..))), "left side is the copied OR");
            assert!(matches!(combined.node(right), Some(Node::Leaf(_))), "right side is the appended leaf");
        }
        other => panic!("expected an AND root, got {other:?}"),
    }
    // Two leaves plus the OR from the copy, plus the appended leaf and the AND.
    assert_eq!(combined.node_count(), 5);

    drop(source);
    // The copy is independent of the dropped source.
    let item = mock("a", "other");
    assert!(combined.filter(&ValueProvider { pli: &item, match_as_ascii: false }));
}

/// `Display` parenthesises exactly where the right-associative grammar would otherwise
/// regroup: a binary left operand, and a binary or negated operand of `NOT`. Rendering must be
/// stable and keep the meaning, because the rendered text is what a saved configuration
/// stores for a target filter.
#[test]
fn display_parenthesises_where_regrouping_would_happen() {
    // Right-nested, as the parser produces it: no parentheses are needed and none are added.
    let parsed = parse(r#"Name = "a" OR Group = "b" AND Title = "c""#);
    assert_eq!(parsed.to_string(), r#"Name = "a" OR Group = "b" AND Title = "c""#);

    // A binary left operand would be regrouped without parentheses.
    let mut builder = FilterBuilder::new();
    let left = builder.append(&parse(r#"Name = "a" OR Group = "b""#)).expect("copy");
    let right = builder.append(&parse(r#"Title = "c""#)).expect("copy");
    let root = builder.and(left, right);
    let filter = builder.finish(root);
    assert_eq!(filter.to_string(), r#"(Name = "a" OR Group = "b") AND Title = "c""#);

    // `NOT` binds to one operand, so a binary child must be parenthesised as well.
    let mut builder = FilterBuilder::new();
    let inner_left = builder.append(&parse(r#"Name = "a""#)).expect("copy");
    let inner_right = builder.append(&parse(r#"Group = "b""#)).expect("copy");
    let inner = builder.and(inner_left, inner_right);
    let root = builder.not(inner);
    assert_eq!(builder.finish(root).to_string(), r#"NOT (Name = "a" AND Group = "b")"#);

    // A doubled negation would otherwise render as `NOT NOT`, which does not parse.
    let mut builder = FilterBuilder::new();
    let leaf = builder.leaf(Leaf::StringComparison(ItemField::Name, StringOperator::Eq, "a".to_string()));
    let once = builder.not(leaf);
    let root = builder.not(once);
    assert_eq!(builder.finish(root).to_string(), r#"NOT (NOT Name = "a")"#);
}

/// Rendering a tree that `Display` parenthesised must parse again, keep its text and keep its
/// meaning. `Group` nodes are insignificant for evaluation, so the structure may differ.
#[test]
fn parenthesised_rendering_round_trips_in_meaning() {
    let mut builder = FilterBuilder::new();
    let left = builder.append(&parse(r#"Name = "a" OR Group = "b""#)).expect("copy");
    let right = builder.append(&parse(r#"Title = "c""#)).expect("copy");
    let root = builder.and(left, right);
    let original = builder.finish(root);

    let rendered = original.to_string();
    let reparsed = parse(&rendered);
    assert_eq!(reparsed.to_string(), rendered, "rendering must be stable across a reparse");

    for item in [mock("a", "q"), mock("x", "b"), mock("x", "q")] {
        let provider = ValueProvider { pli: &item, match_as_ascii: false };
        assert_eq!(
            original.filter(&provider),
            reparsed.filter(&provider),
            "the reparse must evaluate like the original"
        );
    }
}

/// Appending into a builder that already has nodes must not alias the source indices.
#[test]
fn append_after_local_nodes_keeps_indices_consistent() {
    let mut builder = FilterBuilder::new();
    let own = builder.leaf(Leaf::PresenceComparison(ItemField::Name, PresenceOperator::IsEmpty));
    let copied = builder.append(&parse(r#"Group = "x""#)).expect("copy");
    let root = builder.or(own, copied);
    let filter = builder.finish(root);

    assert_eq!(filter.to_string(), r#"Name IS EMPTY OR Group = "x""#);
    assert_eq!(filter.node_count(), 3);
}

#[test]
fn equality_compares_structure_not_identity() {
    let a = parse(r#"Name = "x" AND Group = "y""#);
    let b = parse(r#"Name = "x" AND Group = "y""#);
    let c = parse(r#"Name = "x" OR Group = "y""#);
    assert_eq!(a, b, "two parses of the same text are equal");
    assert_ne!(a, c, "a different operator is not equal");

    let cloned = a.clone();
    assert_eq!(a, cloned, "a clone equals its source");
    // The clone owns a separate arena; evaluating both must stay independent.
    let item = mock("x", "y");
    let provider = ValueProvider { pli: &item, match_as_ascii: false };
    assert!(a.filter(&provider));
    assert!(cloned.filter(&provider));
}

/// Identifiers belong to the arena that issued them; a foreign id must not resolve.
#[test]
fn foreign_node_ids_do_not_resolve() {
    let mut a = FilterBuilder::new();
    let foreign = a.leaf(Leaf::PresenceComparison(ItemField::Name, PresenceOperator::IsEmpty));
    let filter_a = a.finish(foreign);

    let mut b = FilterBuilder::new();
    let local = b.leaf(Leaf::PresenceComparison(ItemField::Name, PresenceOperator::IsEmpty));
    let filter_b = b.finish(local);

    assert!(filter_b.node(foreign).is_none(), "a foreign node id must not resolve");
    assert!(filter_b.node(filter_b.root()).is_some());
    let foreign_leaf = match *filter_a.node(filter_a.root()).expect("node") {
        Node::Leaf(id) => id,
        other => panic!("expected a leaf, got {other:?}"),
    };
    assert!(filter_b.leaf(foreign_leaf).is_none(), "a foreign leaf id must not resolve");
}

#[test]
fn default_filter_matches_every_group() {
    let filter = Filter::default();
    let item = mock("anything", "any-group");
    let provider = ValueProvider { pli: &item, match_as_ascii: false };
    assert!(filter.filter(&provider));
    assert_eq!(filter.to_string(), r#"(Group ~ ".*")"#);
}

/// `Clone` must deep-copy the arena. A shallow copy would leave the clone pointing at freed
/// nodes once the source is dropped.
#[test]
fn clone_survives_dropping_the_source() {
    let filter = parse(r#"Name = "x" OR Chno >= 5"#);
    let cloned = filter.clone();
    let expected = filter.to_string();
    drop(filter);

    assert_eq!(cloned.to_string(), expected);
    let item = mock("x", "g");
    assert!(cloned.filter(&ValueProvider { pli: &item, match_as_ascii: false }));
}

// ---------------------------------------------------------------------------------------------
// Deep structures: the arena makes these safe, so they are cheap to assert.
// ---------------------------------------------------------------------------------------------

/// Rendering and evaluating a long alternating chain exercises both operator arms and the
/// iterative fallback, without the limits the boxed representation needed.
#[test]
fn long_alternating_chain_renders_and_evaluates() {
    let terms = 2_000usize;
    let mut builder = FilterBuilder::new();
    let leaf = builder.leaf(Leaf::NumericComparison(ItemField::Chno, NumericOperator::GreaterOrEqual, u32::MAX));
    let mut root = leaf;
    for i in 0..terms {
        root = if i % 2 == 0 { builder.or(root, leaf) } else { builder.and(root, leaf) };
    }
    let filter = builder.finish(root);

    let rendered = filter.to_string();
    assert_eq!(rendered.matches(" OR ").count() + rendered.matches(" AND ").count(), terms);
    let item = mock("c", "g");
    assert!(!filter.filter(&ValueProvider { pli: &item, match_as_ascii: false }));
}

/// Nesting below the parser limit still round-trips; the limit itself is covered in `tests.rs`.
#[test]
fn nesting_below_the_limit_round_trips() {
    let depth = 64usize;
    let text = format!("{}Name = \"a\"{}", "(".repeat(depth), ")".repeat(depth));
    let filter = parse(&text);
    assert_eq!(filter.to_string(), text);

    // Every level must be a group node.
    let mut node_id = filter.root();
    for _ in 0..depth {
        match *filter.node(node_id).expect("node exists") {
            Node::Group(inner) => node_id = inner,
            other => panic!("expected a group at depth, got {other:?}"),
        }
    }
    assert!(matches!(*filter.node(node_id).expect("leaf"), Node::Leaf(_)));
}

/// Right-nesting of chains is preserved, which is what keeps `Display` and the tree in sync.
#[test]
fn chains_are_right_nested() {
    let filter = parse(r#"Name = "a" OR Name = "b" OR Name = "c""#);
    let left_leaf = |id| matches!(filter.node(id), Some(Node::Leaf(_)));
    match root_of(&filter) {
        Node::Or(left, right) => {
            assert!(left_leaf(left), "the left operand of the topmost OR is a leaf");
            match *filter.node(right).expect("right node") {
                Node::Or(inner_left, inner_right) => {
                    assert!(left_leaf(inner_left));
                    assert!(left_leaf(inner_right));
                }
                other => panic!("expected the chain to continue on the right, got {other:?}"),
            }
        }
        other => panic!("expected an OR root, got {other:?}"),
    }
}

/// Every binary operator variant is reachable from the parser and maps to the expected node.
#[test]
fn both_operators_map_to_their_node() {
    let and = parse(r#"Name = "a" AND Group = "b""#);
    assert!(matches!(root_of(&and), Node::And(..)));
    let or = parse(r#"Name = "a" OR Group = "b""#);
    assert!(matches!(root_of(&or), Node::Or(..)));
}

// ---------------------------------------------------------------------------------------------
// Short-circuiting: `and` must not evaluate the right operand once the left one is false, and
// `or` must not once the left one is true. The observable proof is the trace log: a leaf that
// is not evaluated emits no message.
// ---------------------------------------------------------------------------------------------

static TRACE_LINES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
static LOGGER_INIT: std::sync::Once = std::sync::Once::new();

/// Serialises the trace tests. They share one process-wide logger and one capture buffer, so
/// a test that clears the buffer while another is asserting would make the assertions flaky.
static TRACE_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Hands out the serialisation guard and clears the capture buffer.
///
/// The guard must be held for as long as the trace is inspected.
fn capture_trace() -> std::sync::MutexGuard<'static, ()> {
    let guard = TRACE_SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    LOGGER_INIT.call_once(|| {
        let _ = log::set_logger(&TraceCapture);
        log::set_max_level(log::LevelFilter::Trace);
    });
    if let Ok(mut lines) = TRACE_LINES.lock() {
        lines.clear();
    }
    guard
}

struct TraceCapture;

impl log::Log for TraceCapture {
    fn enabled(&self, _: &log::Metadata) -> bool { true }
    fn log(&self, record: &log::Record) {
        if let Ok(mut lines) = TRACE_LINES.lock() {
            lines.push(record.args().to_string());
        }
    }
    fn flush(&self) {}
}

/// Whether the captured trace mentions `sentinel`. Sentinels are unique per assertion, so
/// output from other threads cannot produce a false positive.
fn trace_mentions(sentinel: &str) -> bool {
    TRACE_LINES.lock().is_ok_and(|lines| lines.iter().any(|line| line.contains(sentinel)))
}

#[test]
fn and_stops_evaluating_once_the_left_side_is_false() {
    let _trace = capture_trace();
    let item = mock("name", "sc_false_side");
    let filter = parse(r#"Group = "sc_miss" AND Group = "sc_never_reached""#);
    assert!(!filter.filter(&ValueProvider { pli: &item, match_as_ascii: false }));

    assert!(trace_mentions("sc_miss"), "the left operand must be evaluated");
    assert!(!trace_mentions("sc_never_reached"), "the right operand must not be evaluated");
}

#[test]
fn and_evaluates_the_right_side_when_the_left_side_holds() {
    let _trace = capture_trace();
    let item = mock("name", "sc_true_side");
    let filter = parse(r#"Group = "sc_true_side" AND Group = "sc_reached""#);
    assert!(!filter.filter(&ValueProvider { pli: &item, match_as_ascii: false }));

    assert!(trace_mentions("sc_true_side"));
    assert!(trace_mentions("sc_reached"), "a true left operand must let the right one run");
}

#[test]
fn or_stops_evaluating_once_the_left_side_holds() {
    let _trace = capture_trace();
    let item = mock("name", "sc_or_left");
    let filter = parse(r#"Group = "sc_or_left" OR Group = "sc_or_never_reached""#);
    assert!(filter.filter(&ValueProvider { pli: &item, match_as_ascii: false }));

    assert!(trace_mentions("sc_or_left"));
    assert!(!trace_mentions("sc_or_never_reached"), "the right operand must not be evaluated");
}

#[test]
fn or_evaluates_the_right_side_when_the_left_side_fails() {
    let _trace = capture_trace();
    let item = mock("name", "sc_or_right");
    let filter = parse(r#"Group = "sc_or_absent" OR Group = "sc_or_right""#);
    assert!(filter.filter(&ValueProvider { pli: &item, match_as_ascii: false }));

    assert!(trace_mentions("sc_or_absent"));
    assert!(trace_mentions("sc_or_right"));
}

/// The iterative fallback that takes over past the recursive budget must short-circuit as
/// well. The chain is long enough that the decision is taken in the iterative walk.
#[test]
fn iterative_fallback_still_short_circuits() {
    let _trace = capture_trace();
    // 600 OR terms; the item matches the term at 300, a sentinel term follows at 400.
    let terms: Vec<String> = (0..600).map(|i| format!(r#"Group = "sc_chain_{i}""#)).collect();
    let filter = parse(&terms.join(" OR "));

    let item = mock("name", "sc_chain_300");
    assert!(filter.filter(&ValueProvider { pli: &item, match_as_ascii: false }));

    assert!(trace_mentions("sc_chain_299"), "terms before the match are evaluated");
    assert!(trace_mentions("sc_chain_300"), "the matching term is evaluated");
    assert!(!trace_mentions("sc_chain_400"), "terms after the match must not be evaluated");
}

/// A long chain that matches nothing must be walked to its end, through both paths.
#[test]
fn iterative_fallback_walks_the_whole_chain_when_nothing_matches() {
    let _trace = capture_trace();
    let terms: Vec<String> = (0..600).map(|i| format!(r#"Group = "sc_full_{i}""#)).collect();
    let filter = parse(&terms.join(" OR "));

    let item = mock("name", "unrelated");
    assert!(!filter.filter(&ValueProvider { pli: &item, match_as_ascii: false }));

    assert!(trace_mentions("sc_full_0"));
    assert!(trace_mentions("sc_full_599"), "the last term must be reached");
}

// ---------------------------------------------------------------------------------------------
// Project rule: no panicking constructs in production code.
// ---------------------------------------------------------------------------------------------

/// Guards the rule that `unwrap`, `expect`, `panic!`, `unreachable!`, `todo!` and
/// `unimplemented!` stay out of production code. The sources are embedded at compile time, so
/// the check cannot drift from the files it protects and does not depend on the test's working
/// directory. Comment lines are skipped, because prose may name these macros.
#[test]
fn filter_production_code_contains_no_panicking_constructs() {
    const SOURCES: [(&str, &str); 5] = [
        ("parser.rs", include_str!("parser.rs")),
        ("eval.rs", include_str!("eval.rs")),
        ("expression.rs", include_str!("expression.rs")),
        ("templates.rs", include_str!("templates.rs")),
        ("value_provider.rs", include_str!("../value_provider.rs")),
    ];
    // `matches!`/`assert` are not listed: they cannot panic outside tests. `debug_assert` is
    // compiled out in release builds, so it is not a production panic either.
    const FORBIDDEN: [&str; 6] = [".unwrap()", ".expect(", "panic!", "unreachable!", "todo!", "unimplemented!"];

    let mut violations = Vec::new();
    for (name, source) in SOURCES {
        for (number, line) in source.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with('*') {
                continue;
            }
            for pattern in FORBIDDEN {
                if line.contains(pattern) {
                    violations.push(format!("{name}:{}: {pattern}", number + 1));
                }
            }
        }
    }
    assert!(violations.is_empty(), "panicking constructs in production code:\n{}", violations.join("\n"));
}

/// A filter can be finished with an id from another builder, because `finish` takes any
/// `NodeId`. Copying such a filter must report `None` instead of indexing out of bounds or
/// mapping the foreign id to whatever sits at its index here.
#[test]
fn append_refuses_a_filter_with_an_unresolved_root() {
    let leaf = |builder: &mut FilterBuilder| {
        builder.leaf(Leaf::PresenceComparison(ItemField::Name, PresenceOperator::IsEmpty))
    };

    // Id beyond this filter's own length.
    let mut foreign = FilterBuilder::new();
    let mut foreign_id = leaf(&mut foreign);
    for _ in 0..5 {
        foreign_id = leaf(&mut foreign);
    }
    let mut builder = FilterBuilder::new();
    let _own = leaf(&mut builder);
    let out_of_range = builder.finish(foreign_id);
    let mut target = FilterBuilder::new();
    assert!(target.append(&out_of_range).is_none(), "an out-of-range root must be refused");

    // Id whose index is valid here, but belongs to another arena.
    let mut foreign = FilterBuilder::new();
    let in_range = leaf(&mut foreign);
    let mut builder = FilterBuilder::new();
    let _own = leaf(&mut builder);
    let mismatched = builder.finish(in_range);
    let mut target = FilterBuilder::new();
    assert!(target.append(&mismatched).is_none(), "a foreign root must be refused, not remapped");

    // A filter this crate built still copies.
    let mut target = FilterBuilder::new();
    let copied = target.append(&parse(r#"Name = "a" OR Group = "b""#)).expect("copy");
    assert_eq!(target.finish(copied).to_string(), r#"Name = "a" OR Group = "b""#);
}
