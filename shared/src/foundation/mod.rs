mod filter;
mod mapper;
mod value_provider;

pub use filter::{
    apply_templates_to_pattern, apply_templates_to_pattern_single, format_decimal_tenths, get_filter,
    get_filter_detailed, get_target_processing_filter, get_target_processing_filter_detailed, prepare_templates,
    BinaryOperator, CompiledRegex, Filter, FilterBuilder, FilterParsePosition, Leaf, LeafId, NoPluginCalls, Node,
    NodeId, NumericOperator, PluginCallBatch, PluginCallDecisions, PluginCallRef, PresenceOperator, StringOperator,
};
pub use mapper::*;
pub use value_provider::*;

/// Asserts that a `.pest` documentation file matches the `#[grammar_inline = ...]` block of its
/// parser source.
///
/// Extracts the inline grammar from `parser_source` and compares it with `pest_source`.
/// `expected_pest_path` is relative to `shared/src/foundation/` and appears in the failure message.
#[cfg(test)]
pub(crate) fn assert_pest_matches_inline_grammar(parser_source: &str, pest_source: &str, expected_pest_path: &str) {
    let key = "grammar_inline";
    let key_pos = parser_source.find(key).expect("parser source must define grammar_inline");
    let after_key = &parser_source[key_pos + key.len()..];
    let eq_pos = after_key.find('=').expect("grammar_inline must be followed by '='");
    let after_eq = after_key[eq_pos + 1..].trim_start();
    let raw = after_eq.strip_prefix('r').expect("grammar_inline literal must be a raw string");
    let hashes = raw.chars().take_while(|&c| c == '#').count();
    let after_hashes = &raw[hashes..];
    let content = after_hashes.strip_prefix('"').expect("grammar_inline raw string must open with quote");
    let closing = format!("\"{}", "#".repeat(hashes));
    let end = content.find(&closing).expect("grammar_inline raw string must have matching closing delimiter");
    let inline_grammar = &content[..end];

    let normalize = |s: &str| -> String { s.trim().lines().map(str::trim_end).collect::<Vec<_>>().join("\n") };

    assert_eq!(
        normalize(pest_source),
        normalize(inline_grammar),
        "shared/src/foundation/{expected_pest_path} must match grammar_inline in its parser source"
    );
}
