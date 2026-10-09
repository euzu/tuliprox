use super::{
    apply_templates_to_pattern_single, BinaryOperator, CompiledRegex, Filter, ItemField, NumericOperator,
    PatternTemplate, PlaylistItemType, PresenceOperator, StringOperator, UnaryOperator,
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
expr = {
  not_expr ~ (bool_op ~ expr)?
  | basic_expr ~ (bool_op ~ expr)*
}
stmt = { expr ~ (bool_op ~ expr)* }
main = _{ SOI ~ stmt ~ EOI }
"#]
struct FilterParser;

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
        let re = crate::model::REGEX_CACHE.get_or_compile(regstr.as_str());
        if re.is_err() {
            return Err(TuliproxError::RegexCompile(regstr));
        }
        let regexp = re.unwrap();
        if log_enabled!(Level::Trace) {
            trace!("Created regex: {regstr}");
        }
        return Ok(CompiledRegex { restr: regstr, re: regexp });
    }
    Err(TuliproxError::FilterParse(format!("unknown field: {}", expr.as_str())))
}

fn get_parser_field_comparison(
    expr: Pair<Rule>,
    templates: Option<&[PatternTemplate]>,
) -> Result<Filter, TuliproxError> {
    let mut expr_inner = expr.into_inner();
    match get_parser_item_field(&expr_inner.next().unwrap()) {
        Ok(field) => match get_parser_regexp(&expr_inner.next().unwrap(), templates) {
            Ok(regexp) => Ok(Filter::FieldComparison(field, regexp)),
            Err(err) => Err(err),
        },
        Err(err) => Err(err),
    }
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
) -> Result<Filter, TuliproxError> {
    let mut expr_inner = expr.into_inner();
    let field = get_parser_item_field(&expr_inner.next().unwrap())?;
    let op = get_parser_string_operator(&expr_inner.next().unwrap())?;
    let value = get_parser_string_value(&expr_inner.next().unwrap(), templates)?;
    Ok(Filter::StringComparison(field, op, value))
}

fn get_parser_presence_comparison(expr: Pair<Rule>) -> Result<Filter, TuliproxError> {
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
    Ok(Filter::PresenceComparison(field, op))
}

fn get_parser_numeric_comparison(expr: Pair<Rule>) -> Result<Filter, TuliproxError> {
    let mut expr_inner = expr.into_inner();
    let field_pair = expr_inner.next().unwrap();
    let field_text = field_pair.as_str();
    let field = if field_text.eq_ignore_ascii_case("chno") {
        ItemField::Chno
    } else if field_text.eq_ignore_ascii_case("quality") {
        ItemField::Quality
    } else {
        return Err(TuliproxError::FilterParse(format!("unknown numeric field: {field_text}")));
    };
    let op = get_parser_numeric_operator(&expr_inner.next().unwrap())?;
    let value_pair = expr_inner.next().unwrap();
    let value = value_pair
        .as_str()
        .parse::<u32>()
        .map_err(|_| TuliproxError::FilterParse(format!("invalid number: {}", value_pair.as_str())))?;
    Ok(Filter::NumericComparison(field, op, value))
}

fn get_parser_set_comparison(expr: Pair<Rule>, templates: Option<&[PatternTemplate]>) -> Result<Filter, TuliproxError> {
    let mut expr_inner = expr.into_inner();
    let field = get_parser_item_field(&expr_inner.next().unwrap())?;
    let values_pair = expr_inner.next().unwrap();
    let mut values = Vec::new();
    for value in values_pair.into_inner() {
        values.push(get_parser_string_value(&value, templates)?);
    }
    if values.is_empty() {
        return Err(TuliproxError::FilterParse("empty value list in IN comparison".to_string()));
    }
    Ok(Filter::SetComparison(field, values))
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

fn get_parser_type_comparison(expr: Pair<Rule>) -> Result<Filter, TuliproxError> {
    let expr_inner = expr.into_inner();
    let text_item_type = expr_inner.as_str();
    let item_type = get_filter_item_type(text_item_type);
    item_type.map_or_else(
        || Err(TuliproxError::FilterParse(format!("can't parse item type: {text_item_type}"))),
        |itype| Ok(Filter::TypeComparison(ItemField::Type, itype)),
    )
}

fn get_parser_expression(
    expr: Pair<Rule>,
    templates: Option<&[PatternTemplate]>,
    errors: &mut Vec<String>,
) -> Result<Filter, String> {
    let mut stmts = Vec::new();
    let pairs = expr.into_inner();
    let mut bop: Option<BinaryOperator> = None;
    let mut uop: Option<UnaryOperator> = None;

    for pair in pairs {
        match pair.as_rule() {
            Rule::field_comparison => {
                let comp_res = get_parser_field_comparison(pair, templates);
                match comp_res {
                    Ok(comp) => handle_expr!(bop, uop, stmts, comp),
                    Err(err) => errors.push(err.to_string()),
                }
            }
            Rule::type_comparison => {
                let comp_res = get_parser_type_comparison(pair);
                match comp_res {
                    Ok(comp) => handle_expr!(bop, uop, stmts, comp),
                    Err(err) => errors.push(err.to_string()),
                }
            }
            Rule::string_comparison => match get_parser_string_comparison(pair, templates) {
                Ok(comp) => handle_expr!(bop, uop, stmts, comp),
                Err(err) => errors.push(err.to_string()),
            },
            Rule::presence_comparison => match get_parser_presence_comparison(pair) {
                Ok(comp) => handle_expr!(bop, uop, stmts, comp),
                Err(err) => errors.push(err.to_string()),
            },
            Rule::numeric_comparison => match get_parser_numeric_comparison(pair) {
                Ok(comp) => handle_expr!(bop, uop, stmts, comp),
                Err(err) => errors.push(err.to_string()),
            },
            Rule::set_comparison => match get_parser_set_comparison(pair, templates) {
                Ok(comp) => handle_expr!(bop, uop, stmts, comp),
                Err(err) => errors.push(err.to_string()),
            },
            Rule::comparison | Rule::expr => {
                let expr = get_parser_expression(pair, templates, errors)?;
                handle_expr!(bop, uop, stmts, expr);
            }
            Rule::expr_group => {
                let expr = get_parser_expression(pair.into_inner().next().unwrap(), templates, errors)?;
                handle_expr!(bop, uop, stmts, Filter::Group(Box::new(expr)));
            }
            Rule::not => {
                uop = Some(UnaryOperator::Not);
            }
            Rule::bool_op => match get_parser_binary_op(&pair.into_inner().next().unwrap()) {
                Ok(binop) => {
                    bop = Some(binop);
                }
                Err(err) => {
                    errors.push(format!("{err}"));
                }
            },
            _ => {
                errors.push(format!("did not expect rule: {pair:?}"));
            }
        }
    }
    if stmts.is_empty() {
        return Err(format!("Invalid Filter, could not parse {errors:?}"));
    }
    if stmts.len() > 1 {
        return Err(format!("did not expect multiple rule: {stmts:?}, {errors:?}"));
    }

    Ok(stmts.pop().unwrap())
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

    match FilterParser::parse(Rule::main, &source) {
        Ok(pairs) => {
            let mut errors = Vec::new();
            let mut result: Option<Filter> = None;
            let mut op: Option<BinaryOperator> = None;
            for pair in pairs {
                match pair.as_rule() {
                    Rule::stmt => {
                        for expr in pair.into_inner() {
                            match expr.as_rule() {
                                Rule::expr => match get_parser_expression(expr, templates, &mut errors) {
                                    Ok(expr) => match &op {
                                        Some(binop) => {
                                            result = Some(Filter::BinaryExpression(
                                                Box::new(result.unwrap()),
                                                *binop,
                                                Box::new(expr),
                                            ));
                                            op = None;
                                        }
                                        _ => result = Some(expr),
                                    },
                                    Err(err) => errors.push(err),
                                },
                                Rule::bool_op => match get_parser_binary_op(&expr.into_inner().next().unwrap()) {
                                    Ok(binop) => {
                                        op = Some(binop);
                                    }
                                    Err(err) => {
                                        errors.push(err.to_string());
                                    }
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
                errors.push(format!("Unable to parse filter: {filter_text}"));
                return Err((TuliproxError::FilterParse(errors.join("\n")), None));
            }

            result.map_or_else(
                || Err((TuliproxError::FilterParse(format!("Unable to parse filter: {filter_text}")), None)),
                Ok,
            )
        }
        Err(err) => {
            let (line, column) = match err.line_col {
                pest::error::LineColLocation::Pos((line, column))
                | pest::error::LineColLocation::Span((line, column), _) => (line, column),
            };
            Err((TuliproxError::FilterParse(format!("{err}")), Some(FilterParsePosition { line, column })))
        }
    }
}

pub fn get_filter(filter_text: &str, templates: Option<&[PatternTemplate]>) -> Result<Filter, TuliproxError> {
    get_filter_detailed(filter_text, templates).map_err(|(err, _)| err)
}
