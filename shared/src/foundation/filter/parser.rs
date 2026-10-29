use super::{
    apply_templates_to_pattern_single, BinaryOperator, CompiledRegex, Filter, FilterBuilder, ItemField, Leaf, NodeId,
    NumericOperator, PatternTemplate, PlaylistItemType, PresenceOperator, StringOperator,
};
use crate::{error::TuliproxError, utils::CONSTANTS};
use indexmap::IndexSet;
use log::{log_enabled, trace, Level};
use pest::{iterators::Pair, Parser};
use pest_derive::Parser;
use strum::IntoEnumIterator;

#[derive(Parser)]
#[grammar_inline = r#"
WHITESPACE = _{ " " | "\t" | "\r" | "\n"}
field = { ^"group" | ^"title" | ^"name" | ^"genre" | ^"url" | ^"input" | ^"caption" | ^"epgid"}
numeric_field = { ^"chno" | ^"quality" }
and = { ^"and" }
or = { ^"or" }
not = { ^"not" }
regexp = @{ "\"" ~ ( "\\\"" | (!"\"" ~ ANY) )* ~ "\"" }
number = @{ ASCII_DIGIT+ }
type_value = { ^"live" | ^"vod" | ^"movie" | ^"series" }
type_comparison = { ^"type" ~ "=" ~ type_value }
field_comparison_value = _{ regexp }
field_comparison = { field ~ "~" ~ field_comparison_value }
str_op = { "!=" | "=" | ^"contains" | ^"startswith" }
string_comparison = { field ~ str_op ~ regexp }
presence_negation = { ^"not" }
presence_op = { (^"is" ~ presence_negation?) | "!=" | "=" }
presence_comparison = { field ~ presence_op ~ ^"empty" }
num_op = { ">=" | "<=" | "!=" | ">" | "<" | "=" }
numeric_comparison = { numeric_field ~ num_op ~ number }
set_values = { regexp ~ ("," ~ regexp)* }
set_comparison = { field ~ ^"in" ~ "[" ~ set_values ~ "]" }
comparison = { field_comparison | type_comparison | numeric_comparison | set_comparison | presence_comparison | string_comparison }
bool_op = { and | or }
expr_group = { "(" ~ expr ~ ")" }
basic_expr = _{ comparison | expr_group }
not_expr = _{ not ~ basic_expr }
expr = { (not_expr | basic_expr) ~ (bool_op ~ (not_expr | basic_expr))* }
stmt = { expr ~ (bool_op ~ expr)* }
main = _{ SOI ~ stmt ~ EOI }
"#]
struct FilterParser;

/// Returns the next child of a rule.
///
/// The grammar fixes the child count of every comparison rule, so a missing child means the
/// grammar changed without this code following. It is reported as an error instead of
/// panicking, so a malformed internal state cannot abort the process.
fn next_rule<'i>(pairs: &mut pest::iterators::Pairs<'i, Rule>, what: &str) -> Result<Pair<'i, Rule>, TuliproxError> {
    pairs.next().ok_or_else(|| TuliproxError::FilterParse(format!("{what} is incomplete: missing a part")))
}

/// Rejects a filter that has grown past [`MAX_FILTER_NODES`].
fn check_node_budget(builder: &FilterBuilder) -> Result<(), String> {
    if builder.node_count() > MAX_FILTER_NODES {
        return Err(format!(
            "filter is too large: more than {MAX_FILTER_NODES} nodes, about {} terms in total are allowed",
            MAX_FILTER_NODES / 2
        ));
    }
    Ok(())
}

fn get_parser_item_field(expr: &Pair<Rule>) -> Result<ItemField, TuliproxError> {
    if expr.as_rule() == Rule::field {
        let field_text = expr.as_str();
        for item in ItemField::iter() {
            if field_text.eq_ignore_ascii_case(item.as_ref()) {
                return Ok(item);
            }
        }
    }
    Err(TuliproxError::FilterParse(format!("unknown field: {}", expr.as_str())))
}

fn get_parser_regexp(expr: &Pair<Rule>, templates: Option<&[PatternTemplate]>) -> Result<CompiledRegex, TuliproxError> {
    if expr.as_rule() == Rule::regexp {
        let full_str = expr.as_str();
        let parsed_text = &full_str[1..full_str.len() - 1];
        let regstr = apply_templates_to_pattern_single(parsed_text, templates)?;
        let regexp = crate::model::REGEX_CACHE
            .get_or_compile(regstr.as_str())
            .map_err(|_| TuliproxError::RegexCompile(regstr.clone()))?;
        if log_enabled!(Level::Trace) {
            trace!("Created regex: {regstr}");
        }
        return Ok(CompiledRegex { restr: regstr, re: regexp });
    }
    Err(TuliproxError::FilterParse(format!("unknown field: {}", expr.as_str())))
}

fn get_parser_field_comparison(expr: Pair<Rule>, templates: Option<&[PatternTemplate]>) -> Result<Leaf, TuliproxError> {
    let mut expr_inner = expr.into_inner();
    let field = get_parser_item_field(&next_rule(&mut expr_inner, "field comparison")?)?;
    let regexp = get_parser_regexp(&next_rule(&mut expr_inner, "field comparison")?, templates)?;
    Ok(Leaf::FieldComparison(field, regexp))
}

fn get_parser_string_value(expr: &Pair<Rule>, templates: Option<&[PatternTemplate]>) -> Result<String, TuliproxError> {
    if expr.as_rule() == Rule::regexp {
        let full_str = expr.as_str();
        let parsed_text = &full_str[1..full_str.len() - 1];
        let resolved = apply_templates_to_pattern_single(parsed_text, templates)?;
        return Ok(resolved.replace("\\\"", "\""));
    }
    Err(TuliproxError::FilterParse(format!("expected string literal: {}", expr.as_str())))
}

fn get_parser_string_operator(expr: &Pair<Rule>) -> Result<StringOperator, TuliproxError> {
    let text = expr.as_str();
    if text == "=" {
        Ok(StringOperator::Eq)
    } else if text == "!=" {
        Ok(StringOperator::NotEq)
    } else if text.eq_ignore_ascii_case("contains") {
        Ok(StringOperator::Contains)
    } else if text.eq_ignore_ascii_case("startswith") {
        Ok(StringOperator::StartsWith)
    } else {
        Err(TuliproxError::FilterParse(format!("unknown string operator: {text}")))
    }
}

fn get_parser_numeric_operator(expr: &Pair<Rule>) -> Result<NumericOperator, TuliproxError> {
    match expr.as_str() {
        "=" => Ok(NumericOperator::Eq),
        "!=" => Ok(NumericOperator::NotEq),
        ">" => Ok(NumericOperator::Greater),
        ">=" => Ok(NumericOperator::GreaterOrEqual),
        "<" => Ok(NumericOperator::Less),
        "<=" => Ok(NumericOperator::LessOrEqual),
        other => Err(TuliproxError::FilterParse(format!("unknown numeric operator: {other}"))),
    }
}

fn get_parser_string_comparison(
    expr: Pair<Rule>,
    templates: Option<&[PatternTemplate]>,
) -> Result<Leaf, TuliproxError> {
    let mut expr_inner = expr.into_inner();
    let field = get_parser_item_field(&next_rule(&mut expr_inner, "string comparison")?)?;
    let op = get_parser_string_operator(&next_rule(&mut expr_inner, "string comparison")?)?;
    let value = get_parser_string_value(&next_rule(&mut expr_inner, "string comparison")?, templates)?;
    Ok(Leaf::StringComparison(field, op, value))
}

fn get_parser_presence_comparison(expr: Pair<Rule>) -> Result<Leaf, TuliproxError> {
    let mut expr_inner = expr.into_inner();
    let field = get_parser_item_field(
        &expr_inner
            .next()
            .ok_or_else(|| TuliproxError::FilterParse("presence comparison is missing a field".to_string()))?,
    )?;
    let operator = expr_inner
        .next()
        .ok_or_else(|| TuliproxError::FilterParse("presence comparison is missing an operator".to_string()))?;
    let op = if operator.as_str() == "!=" || operator.as_str().split_whitespace().count() > 1 {
        PresenceOperator::IsNotEmpty
    } else {
        PresenceOperator::IsEmpty
    };
    Ok(Leaf::PresenceComparison(field, op))
}

fn get_parser_numeric_comparison(expr: Pair<Rule>) -> Result<Leaf, TuliproxError> {
    let mut expr_inner = expr.into_inner();
    let field_pair = next_rule(&mut expr_inner, "numeric comparison")?;
    let field_text = field_pair.as_str();
    let field = if field_text.eq_ignore_ascii_case("chno") {
        ItemField::Chno
    } else if field_text.eq_ignore_ascii_case("quality") {
        ItemField::Quality
    } else {
        return Err(TuliproxError::FilterParse(format!("unknown numeric field: {field_text}")));
    };
    let op = get_parser_numeric_operator(&next_rule(&mut expr_inner, "numeric comparison")?)?;
    let value_pair = next_rule(&mut expr_inner, "numeric comparison")?;
    let value = value_pair
        .as_str()
        .parse::<u32>()
        .map_err(|_| TuliproxError::FilterParse(format!("invalid number: {}", value_pair.as_str())))?;
    Ok(Leaf::NumericComparison(field, op, value))
}

fn get_parser_set_comparison(expr: Pair<Rule>, templates: Option<&[PatternTemplate]>) -> Result<Leaf, TuliproxError> {
    let mut expr_inner = expr.into_inner();
    let field = get_parser_item_field(&next_rule(&mut expr_inner, "IN comparison")?)?;
    let values_pair = next_rule(&mut expr_inner, "IN comparison")?;
    let mut values = Vec::new();
    for value in values_pair.into_inner() {
        values.push(get_parser_string_value(&value, templates)?);
    }
    if values.is_empty() {
        return Err(TuliproxError::FilterParse("empty value list in IN comparison".to_string()));
    }
    Ok(Leaf::SetComparison(field, values))
}

fn get_filter_item_type(text_item_type: &str) -> Option<PlaylistItemType> {
    if text_item_type.eq_ignore_ascii_case("live") {
        Some(PlaylistItemType::Live)
    } else if text_item_type.eq_ignore_ascii_case(Filter::VOD)
        || text_item_type.eq_ignore_ascii_case("video")
        || text_item_type.eq_ignore_ascii_case(Filter::MOVIE)
    {
        Some(PlaylistItemType::Video)
    } else if text_item_type.eq_ignore_ascii_case("series") {
        Some(PlaylistItemType::Series)
    } else if text_item_type.eq_ignore_ascii_case("series-info") {
        // this is necessarry to avoid series and series-info confusion in filter!
        // we can now use series  for filtering series and series-info (series-info are categories)
        Some(PlaylistItemType::Series)
    } else {
        None
    }
}

fn get_parser_type_comparison(expr: Pair<Rule>) -> Result<Leaf, TuliproxError> {
    let expr_inner = expr.into_inner();
    let text_item_type = expr_inner.as_str();
    let item_type = get_filter_item_type(text_item_type);
    item_type.map_or_else(
        || Err(TuliproxError::FilterParse(format!("can't parse item type: {text_item_type}"))),
        |itype| Ok(Leaf::TypeComparison(ItemField::Type, itype)),
    )
}

/// Largest filter accepted, counted in arena nodes.
///
/// This is not a stack bound: a filter is an arena, so parsing, rendering, evaluating and
/// dropping a chain are loops over indices and do not grow with its length. It bounds the
/// memory and time one filter may cost, because the text arrives either from a configuration
/// file or from the endpoints that validate user supplied filters.
///
/// Measured on the release profile with a warm regex cache, one node costs about 100 bytes of
/// heap and about 1 microsecond to parse, so this budget stays near 2 MB and 20 ms. A filter
/// made of many *distinct* regexes costs more than that in the compiled patterns themselves,
/// which the regex cache holds for the process lifetime; that cost is proportional to the
/// number of distinct patterns in the configuration, not to this budget.
///
/// The budget is counted over the whole filter, not per expression. A per-expression limit
/// cannot bound the total: parentheses may nest 256 deep, so 255 groups of 5 000 terms each
/// would pass a per-expression check while building 1.27 million nodes.
///
/// A term costs one node while the expression is collected and one more when it is folded into
/// the chain, so this admits about 10 000 terms, far more than any hand-written filter.
pub(super) const MAX_FILTER_NODES: usize = 20_000;

/// Longest nesting of parentheses accepted in one filter.
///
/// Deepest parenthesis nesting accepted in one filter.
///
/// `pest` builds its parse tree with one nested call per group, before any of this crate's
/// code runs, and pest 2.9.5 does not probe the stack on musl, the shipped target. Measured on
/// a 2 MiB thread (the stack of a Tokio worker, of a `spawn_blocking` closure and of a test
/// thread) `((((...))))` overflows at 444 levels in a debug build and 1818 in a release build,
/// so the input is rejected from its text before parsing. A hand-written filter nests two or
/// three levels.
pub(super) const MAX_NESTING_DEPTH: usize = 256;

/// Builds one expression into `builder` from the flat `expr` rule and returns its root.
///
/// The grammar produces a single operand sequence instead of recursing once per operator,
/// so a deep `AND`/`OR` chain never nests the parser stack. The tree is folded from the
/// right, because that is the grouping the previous recursive grammar produced:
/// `A AND B OR C` stays `A AND (B OR C)`, not `(A AND B) OR C`.
fn get_parser_expression(
    expr: Pair<Rule>,
    templates: Option<&[PatternTemplate]>,
    errors: &mut Vec<String>,
    depth: usize,
    builder: &mut FilterBuilder,
) -> Result<NodeId, String> {
    // `get_filter_detailed` rejects deep nesting from the input text before pest parses, so
    // this is a second, cheap guard in case a caller reaches the expression builder another way.
    if depth > MAX_NESTING_DEPTH {
        return Err(format!("filter is nested too deeply: more than {MAX_NESTING_DEPTH} parentheses"));
    }
    check_node_budget(builder)?;
    let mut operands: Vec<NodeId> = Vec::new();
    let mut operators: Vec<BinaryOperator> = Vec::new();
    let mut negate = false;

    for pair in expr.into_inner() {
        let parsed = match pair.as_rule() {
            Rule::field_comparison => match get_parser_field_comparison(pair, templates) {
                Ok(leaf) => Some(builder.leaf(leaf)),
                Err(err) => {
                    errors.push(err.message().to_string());
                    None
                }
            },
            Rule::type_comparison => match get_parser_type_comparison(pair) {
                Ok(leaf) => Some(builder.leaf(leaf)),
                Err(err) => {
                    errors.push(err.message().to_string());
                    None
                }
            },
            Rule::string_comparison => match get_parser_string_comparison(pair, templates) {
                Ok(leaf) => Some(builder.leaf(leaf)),
                Err(err) => {
                    errors.push(err.message().to_string());
                    None
                }
            },
            Rule::presence_comparison => match get_parser_presence_comparison(pair) {
                Ok(leaf) => Some(builder.leaf(leaf)),
                Err(err) => {
                    errors.push(err.message().to_string());
                    None
                }
            },
            Rule::numeric_comparison => match get_parser_numeric_comparison(pair) {
                Ok(leaf) => Some(builder.leaf(leaf)),
                Err(err) => {
                    errors.push(err.message().to_string());
                    None
                }
            },
            Rule::set_comparison => match get_parser_set_comparison(pair, templates) {
                Ok(leaf) => Some(builder.leaf(leaf)),
                Err(err) => {
                    errors.push(err.message().to_string());
                    None
                }
            },
            // `comparison` is a named rule and reaches this arm; `expr` never appears as a direct
            // child of `expr` (only inside `expr_group`), so it is not matched here.
            // A comparison adds exactly one level of recursion and is not nesting, so it does
            // not consume budget: the depth counts parentheses only.
            Rule::comparison => Some(get_parser_expression(pair, templates, errors, depth, builder)?),
            Rule::expr_group => {
                let inner = pair.into_inner().next().ok_or_else(|| "empty expression group".to_string())?;
                let node = get_parser_expression(inner, templates, errors, depth + 1, builder)?;
                Some(builder.group(node))
            }
            Rule::not => {
                negate = true;
                None
            }
            Rule::bool_op => {
                let op = pair.into_inner().next().ok_or_else(|| "missing boolean operator".to_string())?;
                match get_parser_binary_op(&op) {
                    Ok(binop) => operators.push(binop),
                    Err(err) => errors.push(format!("{err}")),
                }
                None
            }
            _ => {
                errors.push(format!("did not expect rule: {pair:?}"));
                None
            }
        };

        if let Some(node) = parsed {
            let negated = std::mem::take(&mut negate);
            operands.push(if negated { builder.not(node) } else { node });
            // Checked per operand so an over-long filter is rejected before it is built, not
            // after the memory is already allocated.
            check_node_budget(builder)?;
        }
    }

    if operands.is_empty() {
        return Err(format!("Invalid Filter, could not parse {errors:?}"));
    }
    // A failed operand is already recorded in `errors`; the filter is rejected below
    // either way, so a count mismatch never builds a partial tree.
    if operators.len() + 1 != operands.len() {
        return Err(format!("incomplete filter expression: {operands:?}, {errors:?}"));
    }
    let mut acc = operands.pop().ok_or_else(|| "empty filter expression".to_string())?;
    for op in operators.into_iter().rev() {
        let Some(lhs) = operands.pop() else {
            return Err("incomplete filter expression".to_string());
        };
        acc = match op {
            BinaryOperator::And => builder.and(lhs, acc),
            BinaryOperator::Or => builder.or(lhs, acc),
        };
    }
    // Folding adds one chain node per operator, so the budget is checked again once the
    // expression is a tree.
    check_node_budget(builder)?;

    Ok(acc)
}

fn get_parser_binary_op(expr: &Pair<Rule>) -> Result<BinaryOperator, TuliproxError> {
    match expr.as_rule() {
        Rule::and => Ok(BinaryOperator::And),
        Rule::or => Ok(BinaryOperator::Or),
        _ => Err(TuliproxError::FilterParse(format!("Unknown binary operator {}", expr.as_str()))),
    }
}

fn unresolved_template_placeholders(input: &str) -> Vec<String> {
    let mut placeholders = IndexSet::new();
    for captures in CONSTANTS.re_template_var.captures_iter(input) {
        if let Some(inner) = captures.get(1) {
            let value = inner.as_str();
            if !value.is_empty() {
                placeholders.insert(format!("!{value}!"));
            }
        }
    }
    placeholders.into_iter().collect()
}

/// Deepest parenthesis nesting in `filter_text`, ignoring parentheses inside a quoted regex.
///
/// pest itself recurses per `expr_group` while building the parse tree, before any of this
/// crate's code runs, and pest 2.9.5 does not probe the stack on musl. The nesting has to be
/// rejected from the input text, because by the time the parse tree exists it may already be
/// too late to walk it.
fn max_nesting(filter_text: &str) -> usize {
    let mut depth = 0usize;
    let mut deepest = 0usize;
    let mut in_value = false;
    let mut escaped = false;
    for ch in filter_text.chars() {
        if in_value {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_value = false;
            }
            continue;
        }
        match ch {
            '"' => in_value = true,
            '(' => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            ')' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    deepest
}

/// Shortens a filter for an error message. A filter can be an expanded template chain of
/// tens of thousands of characters, which must not end up in one log line. The cut happens on
/// a character boundary, so multi-byte values stay intact.
fn abbreviated(filter_text: &str) -> String {
    const LIMIT: usize = 160;
    let mut end = filter_text.len().min(LIMIT);
    while end < filter_text.len() && !filter_text.is_char_boundary(end) {
        end -= 1;
    }
    if end < filter_text.len() {
        format!("{}... ({} characters)", &filter_text[..end], filter_text.chars().count())
    } else {
        filter_text.to_string()
    }
}

/// 1-based line/column of a filter syntax error, when the parser can locate it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterParsePosition {
    pub line: usize,
    pub column: usize,
}

/// Like [`get_filter`], but reports the syntax-error position when available
/// (semantic errors such as invalid regex values have no position).
pub fn get_filter_detailed(
    filter_text: &str,
    templates: Option<&[PatternTemplate]>,
) -> Result<Filter, (TuliproxError, Option<FilterParsePosition>)> {
    let source = match apply_templates_to_pattern_single(filter_text, templates) {
        Ok(source) => source,
        Err(err) => return Err((err, None)),
    };
    let unresolved_placeholders = unresolved_template_placeholders(&source);
    if !unresolved_placeholders.is_empty() {
        return Err((
            TuliproxError::FilterParse(format!(
                "Unknown template placeholder(s) in filter: {}",
                unresolved_placeholders.join(", ")
            )),
            None,
        ));
    }

    let nesting = max_nesting(&source);
    if nesting > MAX_NESTING_DEPTH {
        return Err((
            TuliproxError::FilterParse(format!(
                "filter is nested too deeply: {nesting} parentheses, at most {MAX_NESTING_DEPTH} are allowed"
            )),
            None,
        ));
    }

    match FilterParser::parse(Rule::main, &source) {
        Ok(pairs) => {
            let mut errors = Vec::new();
            let mut builder = FilterBuilder::new();
            let mut result: Option<NodeId> = None;
            let mut op: Option<BinaryOperator> = None;
            for pair in pairs {
                match pair.as_rule() {
                    Rule::stmt => {
                        for expr in pair.into_inner() {
                            match expr.as_rule() {
                                Rule::expr => {
                                    match get_parser_expression(expr, templates, &mut errors, 0, &mut builder) {
                                        Ok(node) => match op.take() {
                                            Some(binop) => {
                                                // The grammar keeps every operator inside one `expr`, so this
                                                // fold is not reached; it mirrors the expression-level grouping
                                                // should a future grammar split a statement again.
                                                if let Some(lhs) = result {
                                                    result = Some(match binop {
                                                        BinaryOperator::And => builder.and(lhs, node),
                                                        BinaryOperator::Or => builder.or(lhs, node),
                                                    });
                                                } else {
                                                    errors.push("missing left operand".to_string());
                                                }
                                            }
                                            None => result = Some(node),
                                        },
                                        Err(err) => errors.push(err),
                                    }
                                }
                                Rule::bool_op => match expr.into_inner().next() {
                                    Some(op_pair) => match get_parser_binary_op(&op_pair) {
                                        Ok(binop) => {
                                            op = Some(binop);
                                        }
                                        Err(err) => {
                                            errors.push(err.message().to_string());
                                        }
                                    },
                                    None => errors.push("missing boolean operator".to_string()),
                                },
                                _ => {
                                    errors.push(format!("unknown expression {expr:?}"));
                                }
                            }
                        }
                    }
                    Rule::EOI => {}
                    _ => {
                        errors.push(format!("unknown: {}", pair.as_str()));
                    }
                }
            }

            if !errors.is_empty() {
                errors.push(format!("Unable to parse filter: {}", abbreviated(filter_text)));
                return Err((TuliproxError::FilterParse(errors.join("\n")), None));
            }

            result.map_or_else(
                || {
                    Err((
                        TuliproxError::FilterParse(format!("Unable to parse filter: {}", abbreviated(filter_text))),
                        None,
                    ))
                },
                |root| Ok(builder.finish(root)),
            )
        }
        Err(err) => {
            let (line, column) = match err.line_col {
                pest::error::LineColLocation::Pos((line, column))
                | pest::error::LineColLocation::Span((line, column), _) => (line, column),
            };
            // Only the reason and the position are reported. pest's `Display` also renders the
            // offending source line, which for a large filter would put the whole filter into
            // the log on every reload attempt.
            let reason = err.variant.message();
            Err((
                TuliproxError::FilterParse(format!("syntax error at line {line}, column {column}: {reason}")),
                Some(FilterParsePosition { line, column }),
            ))
        }
    }
}

pub fn get_filter(filter_text: &str, templates: Option<&[PatternTemplate]>) -> Result<Filter, TuliproxError> {
    get_filter_detailed(filter_text, templates).map_err(|(err, _)| err)
}
