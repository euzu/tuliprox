//! `plugin.<id>.<fn>(literal, ...)` terms: syntax, the call index, and evaluation from a
//! batch prepared before the evaluator runs.

use super::{get_filter, Filter, FilterBuilder, Leaf, NoPluginCalls, PluginCallBatch, PluginCallDecisions};
use crate::{
    foundation::value_provider::ValueProvider,
    model::{PlaylistItem, PlaylistItemHeader},
    utils::Internable,
};

fn mock(name: &str, group: &str) -> PlaylistItem {
    PlaylistItem { header: PlaylistItemHeader { name: name.into(), group: group.intern(), ..Default::default() } }
}

fn parse(text: &str) -> Filter { get_filter(text, None).unwrap_or_else(|e| panic!("{text} should parse: {e}")) }

fn accepts(text: &str) -> bool { get_filter(text, None).is_ok() }

fn eval_with<D: PluginCallDecisions>(filter: &Filter, item: &PlaylistItem, decisions: &D, index: usize) -> bool {
    filter.filter_with(&ValueProvider { pli: item, match_as_ascii: false }, decisions, index)
}

fn batch(items: usize, calls: &[(usize, &[bool])]) -> PluginCallBatch {
    let mut batch = PluginCallBatch::new(items);
    for (call, decisions) in calls {
        assert!(batch.set(*call, decisions.to_vec()), "decision vector of call {call} must fit the batch");
    }
    batch
}

fn args_of(filter: &Filter) -> Vec<String> {
    filter.plugin_calls().next().map(|call| call.args.to_vec()).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// Syntax
// ---------------------------------------------------------------------------------------------

#[test]
fn a_plugin_call_parses_into_a_leaf_with_literal_arguments() {
    let filter = parse(r#"plugin.my-plugin.name_contains("Sport", 12, -3.5, true, FALSE)"#);
    let calls: Vec<_> = filter.plugin_calls().collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].index, 0);
    assert_eq!(calls[0].plugin, "my-plugin");
    assert_eq!(calls[0].func, "name_contains");
    assert_eq!(calls[0].args, ["Sport", "12", "-3.5", "true", "false"]);
}

#[test]
fn a_plugin_call_may_have_no_arguments() {
    let filter = parse("plugin.p.f()");
    let calls: Vec<_> = filter.plugin_calls().collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].args, [] as [String; 0]);
}

#[test]
fn the_keyword_is_case_insensitive_and_whitespace_is_free() {
    assert!(accepts(r#"PLUGIN.p.f( "a" ,  "b" )"#));
    assert!(accepts("plugin.p.f (\n\"a\"\n)"));
    // but the name parts are a single token
    assert!(!accepts(r#"plugin . p . f("a")"#));
    assert!(!accepts(r#"plugin.p. f("a")"#));
}

#[test]
fn an_escaped_quote_in_a_string_argument_is_unescaped() {
    assert_eq!(args_of(&parse(r#"plugin.p.f("say \"hi\"")"#)), [r#"say "hi""#]);
}

#[test]
fn parentheses_and_commas_inside_a_string_argument_are_data() {
    assert_eq!(args_of(&parse(r#"plugin.p.f("a,(b)", "c")"#)), ["a,(b)", "c"]);
}

#[test]
fn identifiers_take_letters_digits_underscore_and_dash() {
    assert!(accepts(r#"plugin.Abc_1-x.fn-2_b("a")"#));
    assert!(!accepts(r#"plugin..f("a")"#));
    assert!(!accepts(r#"plugin.p.("a")"#));
    assert!(!accepts(r#"plugin.p.f.g("a")"#));
    assert!(!accepts(r#"plugin.p("a")"#));
    assert!(!accepts(r#"plugin.p$.f("a")"#));
}

#[test]
fn only_literal_arguments_are_accepted() {
    // a nested call
    assert!(!accepts(r#"plugin.p.f(plugin.q.g("a"))"#));
    // a field reference
    assert!(!accepts("plugin.p.f(name)"));
    assert!(!accepts(r#"plugin.p.f(name, "a")"#));
    // a bare word, a missing separator, a trailing or leading comma, a missing closing parenthesis
    assert!(!accepts("plugin.p.f(abc)"));
    assert!(!accepts(r#"plugin.p.f("a" "b")"#));
    assert!(!accepts(r#"plugin.p.f("a",)"#));
    assert!(!accepts(r#"plugin.p.f(,"a")"#));
    assert!(!accepts(r#"plugin.p.f("a""#));
    // a number with an exponent or a boolean glued to a word is not a literal
    assert!(!accepts("plugin.p.f(1e5)"));
    assert!(!accepts("plugin.p.f(truex)"));
}

#[test]
fn a_plugin_call_is_a_comparison_among_the_others() {
    assert!(accepts(r#"Group = "News" AND plugin.p.f("a") OR NOT plugin.q.g()"#));
    assert!(accepts(r#"(plugin.p.f("a") OR Name ~ "x") AND Chno > 3"#));
    // a call is not a field: it cannot take an operator
    assert!(!accepts(r#"plugin.p.f("a") = "b""#));
    assert!(!accepts(r#"plugin.p.f("a") ~ "b""#));
}

#[test]
fn calls_are_numbered_in_term_order_and_keep_their_own_arguments() {
    let filter = parse(r#"plugin.a.x("1") AND (Name ~ "n" OR plugin.b.y("2")) AND plugin.a.x("3")"#);
    let calls: Vec<_> = filter.plugin_calls().map(|c| (c.index, c.plugin.to_string(), c.args.to_vec())).collect();
    assert_eq!(
        calls,
        vec![
            (0, "a".to_string(), vec!["1".to_string()]),
            (1, "b".to_string(), vec!["2".to_string()]),
            (2, "a".to_string(), vec!["3".to_string()]),
        ]
    );
}

#[test]
fn a_filter_without_plugin_terms_has_no_plugin_calls() {
    assert_eq!(parse(r#"Name ~ "a" AND Chno > 3"#).plugin_calls().count(), 0);
}

#[test]
fn a_rendered_plugin_call_parses_back_to_the_same_call() {
    let filter = parse(r#"NOT plugin.p.f("a b", 12, true) AND Name ~ "x""#);
    let again = parse(&filter.to_string());
    assert_eq!(filter, again);
    assert_eq!(filter.to_string(), again.to_string());
}

#[test]
fn nesting_depth_still_guards_filters_with_plugin_calls() {
    let allowed = super::parser::MAX_NESTING_DEPTH;
    let nested = |depth: usize| format!("{}plugin.p.f(\"a\"){}", "(".repeat(depth), ")".repeat(depth));
    // the call's own parentheses count as one level, so the guard trips one group early
    assert!(accepts(&nested(allowed - 1)));
    assert!(!accepts(&nested(allowed)));
    assert!(!accepts(&nested(allowed + 100)));
    // parentheses inside a quoted argument are data and cost nothing
    let quoted = format!("plugin.p.f(\"{}\")", "(".repeat(allowed * 4));
    assert!(accepts(&quoted));
}

#[test]
fn a_long_chain_of_plugin_calls_evaluates_without_recursing() {
    let terms = 5_000;
    let text = (0..terms).map(|i| format!(r#"plugin.p.f("{i}")"#)).collect::<Vec<_>>().join(" OR ");
    let filter = parse(&text);
    assert_eq!(filter.plugin_calls().count(), terms);
    let item = mock("a", "b");
    assert!(!eval_with(&filter, &item, &NoPluginCalls, 0));
    let mut decisions = PluginCallBatch::new(1);
    assert!(decisions.set(terms - 1, vec![true]));
    assert!(eval_with(&filter, &item, &decisions, 0));
}

// ---------------------------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------------------------

#[test]
fn no_plugin_calls_answers_false_and_filter_uses_it() {
    let filter = parse(r#"plugin.p.f("a")"#);
    let item = mock("a", "b");
    assert!(!eval_with(&filter, &item, &NoPluginCalls, 0));
    assert!(!filter.filter(&ValueProvider { pli: &item, match_as_ascii: false }));
    let negated = parse(r#"NOT plugin.p.f("a")"#);
    assert!(negated.filter(&ValueProvider { pli: &item, match_as_ascii: false }));
}

#[test]
fn a_batch_decides_per_item_and_per_call() {
    let filter = parse(r#"plugin.p.f("a") AND NOT plugin.q.g("b")"#);
    let items = [mock("x", "g"), mock("y", "g"), mock("z", "g")];
    let decisions = batch(3, &[(0, &[true, true, false]), (1, &[false, true, false])]);
    let kept: Vec<bool> = items.iter().enumerate().map(|(i, item)| eval_with(&filter, item, &decisions, i)).collect();
    assert_eq!(kept, [true, false, false]);
}

#[test]
fn plugin_terms_combine_with_built_in_terms() {
    let filter = parse(r#"Group = "News" AND plugin.p.f("a")"#);
    let items = [mock("x", "News"), mock("y", "Sport"), mock("z", "News")];
    let decisions = batch(3, &[(0, &[true, true, false])]);
    let kept: Vec<bool> = items.iter().enumerate().map(|(i, item)| eval_with(&filter, item, &decisions, i)).collect();
    assert_eq!(kept, [true, false, false]);
}

#[test]
fn a_missing_call_an_out_of_range_item_and_a_length_mismatch_are_false() {
    let filter = parse(r#"plugin.p.f("a") OR plugin.q.g("b")"#);
    let item = mock("x", "g");
    // call 1 has no vector
    let decisions = batch(2, &[(0, &[false, false])]);
    assert!(!eval_with(&filter, &item, &decisions, 0));
    // item past the end of the vector
    let decisions = batch(2, &[(0, &[true, true])]);
    assert!(eval_with(&filter, &item, &decisions, 1));
    assert!(!eval_with(&filter, &item, &decisions, 2));
    // a vector of the wrong length is refused and the call stays false
    let mut decisions = PluginCallBatch::new(2);
    assert!(!decisions.set(0, vec![true]));
    assert!(!decisions.set(0, vec![true, true, true]));
    assert!(!eval_with(&filter, &item, &decisions, 0));
    // a call index the filter never had
    assert!(!decisions.decision(99, 0));
}

#[test]
fn a_deep_filter_with_plugin_terms_takes_the_iterative_path() {
    // deeper than the recursive budget, so the explicit stack evaluates the tail
    let leaf = || Leaf::PluginCall { call: 0, plugin: "p".into(), func: "f".into(), args: vec![] };
    let mut builder = FilterBuilder::new();
    let mut root = builder.leaf(leaf());
    for _ in 0..2_000 {
        let next = builder.leaf(leaf());
        root = builder.and(next, root);
    }
    let filter = builder.finish(root);
    assert_eq!(filter.plugin_calls().count(), 2_001);
    let item = mock("x", "g");
    assert!(!eval_with(&filter, &item, &NoPluginCalls, 0));
    let mut decisions = PluginCallBatch::new(1);
    for call in 0..2_001 {
        assert!(decisions.set(call, vec![true]));
    }
    assert!(eval_with(&filter, &item, &decisions, 0));
    assert!(decisions.set(1_000, vec![false]));
    assert!(!eval_with(&filter, &item, &decisions, 0));
}

// ---------------------------------------------------------------------------------------------
// Existing filters
// ---------------------------------------------------------------------------------------------

#[test]
fn existing_filters_evaluate_the_same_through_filter_and_filter_with() {
    let item = mock("Channel HD", "Sports");
    let provider = ValueProvider { pli: &item, match_as_ascii: false };
    for text in [
        r#"Name ~ "HD""#,
        r#"Group = "Sports" AND NOT Name CONTAINS "SD""#,
        r#"Group = "News" OR (Name ~ "^Channel" AND Chno = 0)"#,
        r#"Name ~ "nope""#,
    ] {
        let filter = parse(text);
        assert_eq!(filter.plugin_calls().count(), 0, "{text}");
        assert_eq!(filter.filter(&provider), filter.filter_with(&provider, &NoPluginCalls, 0), "{text}");
    }
    assert!(parse(r#"Name ~ "HD""#).filter(&provider));
    assert!(!parse(r#"Name ~ "nope""#).filter(&provider));
}

#[test]
fn field_names_that_start_with_plugin_are_not_plugin_calls() {
    assert!(!accepts(r#"pluginx = "a""#));
    assert!(!accepts(r#"plugin = "a""#));
}
