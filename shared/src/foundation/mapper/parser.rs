use super::{
    eval::to_number, AssignmentTarget, BuiltInFunction, ExprId, Expression, ForEachExpr, ForEachKey, MapCase,
    MapCaseKey, MapKey, MapperContext, MapperScript, MatchCase, MatchCaseKey, RegexSource, Statement,
};
use crate::{error::TuliproxError, foundation::mapper::EvalResult::Number, model::PatternTemplate};
use pest::{
    iterators::{Pair, Pairs},
    Parser,
};
use pest_derive::Parser;
use std::{collections::HashSet, str::FromStr};

#[derive(Parser)]
#[grammar_inline = r##"
WHITESPACE = _{ " " | "\t"}
regex_op =  _{ "~" }
null = { "null" }
identifier = @{ !null ~ (ASCII_ALPHANUMERIC | "_")+ }
var_access = { identifier ~ ("." ~ identifier)? }
string_literal = @{ "\"" ~ ( "\\\\" | "\\\"" | "\\n" | "\\t" | "\\r" | (!"\"" ~ ANY) )* ~ "\"" }
number = @{ "-"? ~ ASCII_DIGIT+ ~ ("." ~ ASCII_DIGIT+)? }
number_range_from = { number ~ ".." }
number_range_to = { ".." ~ number }
number_range_full = { number ~ ".." ~ number }
number_range_eq = { number }
number_range = _{ number_range_full | number_range_from | number_range_to | number_range_eq}
read_write_field = _{ ^"name" | ^"title" | ^"caption" | ^"group" | ^"id" | ^"chno" | ^"logo" | ^"logo_small" | ^"parent_code" | ^"audio_track" | ^"time_shift" | ^"rec" | ^"url" | ^"epg_channel_id" | ^"epg_id" | ^"genre" }
read_only_field = _{ ^"input" | ^"type" }
field = { read_write_field | read_only_field }
assignment_field = { read_write_field }
field_access = _{ "@" ~ field }
regex_source = _{ field_access | identifier }
regex_expr = { regex_source ~ regex_op ~ string_literal }
block_expr = { "{" ~ statements ~ "}" }
condition = { function_call | var_access | field_access }
assignment = { (("@" ~ assignment_field) | identifier) ~ "=" ~ expression }
expression = { assignment | map_block | match_block | for_each_block | function_call | regex_expr | string_literal | number | var_access | field_access | null | block_expr }
function_name = { "concat" | "uppercase" | "lowercase" | "capitalize" | "split" | "trim" | "print" | "number" | "first" | "template" | "replace" | "pad" | "format" | "add_favourite" }
function_call = { function_name ~ "(" ~ (expression ~ ("," ~ expression)*)? ~ ")" }
any_match = { "_" }
match_case_key = { any_match | identifier }
match_case_key_list = { match_case_key ~ ("," ~ match_case_key)* }
match_case = { match_case_key_list ~ "=>" ~ expression | "(" ~ match_case_key_list ~ ")" ~ "=>" ~ expression }
match_block = { "match" ~  "{" ~ NEWLINE* ~ (match_case ~ ("," ~ NEWLINE* ~ match_case)*)? ~ ","? ~ NEWLINE* ~ "}" }
map_case_key_list = { string_literal ~ ("|" ~ string_literal)* }
map_case_key = { any_match | number_range | map_case_key_list }
map_case = { map_case_key ~ "=>" ~ expression }
map_key = { var_access | field_access  }
map_block = { "map" ~ map_key ~ "{" ~ NEWLINE* ~ (map_case ~ ("," ~ NEWLINE* ~ map_case)*)? ~ ","? ~ NEWLINE* ~ "}" }
for_each_param = { any_match | identifier }
for_each_params = { "(" ~ for_each_param ~ "," ~ for_each_param ~ ")" }
for_each_target_nested = { identifier ~ "." ~ identifier }
for_each_target_simple = { identifier }
for_each_block = { 
    (for_each_target_nested ~ ^".for_each" ~ "(" ~ for_each_params ~ "=>" ~ expression ~ ")") | 
    (for_each_target_simple ~ ^".for_each" ~ "(" ~ for_each_params ~ "=>" ~ expression ~ ")") 
}
statement = _{ expression }
comment = _{ "#" ~ (!NEWLINE ~ ANY)* }
statement_reparator = _{ ";" | NEWLINE }
statements = _{ (statement_reparator* ~ (statement | comment))* ~ statement_reparator* }
main = { SOI ~ statements? ~ EOI }
"##]
struct MapperParser;

impl MapperScript {
    pub(super) fn validate(
        expressions: &[Expression],
        statements: &[Statement],
        templates: Option<&[PatternTemplate]>,
    ) -> Result<(), TuliproxError> {
        let ctx = &mut MapperContext::new(expressions, templates);

        let mut identifiers: HashSet<String> = HashSet::new();
        for stmt in statements {
            match stmt {
                Statement::Expression(expr) => {
                    ctx.validate_expr(*expr, &mut identifiers)?;
                }
                Statement::Comment(_) => {}
            }
        }
        Ok(())
    }

    pub fn parse(input: &str, templates: Option<&[PatternTemplate]>) -> Result<Self, TuliproxError> {
        let mut parsed = MapperParser::parse(Rule::main, input).map_err(|e| TuliproxError::Mapper(format!("{e}")))?;
        let program_pair = parsed.next().unwrap();
        let mut statements = Vec::new();
        let mut expressions = Vec::new();
        for stmt_pair in program_pair.into_inner() {
            if let Some(stmt) = Self::parse_statement(stmt_pair, &mut expressions)? {
                statements.push(stmt);
            }
        }

        MapperScript::validate(&expressions, &statements, templates)?;
        Ok(Self { expressions, statements })
    }
    pub(super) fn parse_statement(
        pair: Pair<Rule>,
        expressions: &mut Vec<Expression>,
    ) -> Result<Option<Statement>, TuliproxError> {
        match pair.as_rule() {
            Rule::expression => {
                if let Some(expr) = MapperScript::parse_expression(pair, expressions)? {
                    expressions.push(expr);
                    let expr_id = ExprId(expressions.len() - 1);
                    Ok(Some(Statement::Expression(expr_id)))
                } else {
                    Ok(None)
                }
            }
            Rule::comment => Ok(Some(Statement::Comment(pair.as_str().trim().to_string()))),

            _ => {
                // error!("Unknown statement rule: {:?}", pair.as_rule());
                Ok(None)
            }
        }
    }

    pub(super) fn parse_assignment(
        pair: Pair<Rule>,
        expressions: &mut Vec<Expression>,
    ) -> Result<Option<Expression>, TuliproxError> {
        let mut inner = pair.into_inner();
        let name = inner.next().unwrap();
        let target = match name.as_rule() {
            Rule::identifier => AssignmentTarget::Identifier(name.as_str().to_string()),
            Rule::assignment_field => AssignmentTarget::Field(name.as_str().to_string()),
            _ => return Err(TuliproxError::Mapper(format!("Assignment target isn't supported {}", name.as_str()))),
        };
        let next = inner.next().unwrap();
        if let Some(expr) = MapperScript::parse_expression(next, expressions)? {
            expressions.push(expr);
            let expr_id = ExprId(expressions.len() - 1);
            Ok(Some(Expression::Assignment { target, expr: expr_id }))
        } else {
            Ok(None)
        }
    }

    pub(super) fn parse_match_case_key(pair: Pair<Rule>) -> Result<MatchCaseKey, TuliproxError> {
        let inner = pair.into_inner().next().unwrap();
        match inner.as_rule() {
            Rule::identifier => Ok(MatchCaseKey::Identifier(inner.as_str().to_string())),
            Rule::any_match => Ok(MatchCaseKey::AnyMatch),
            _ => Err(TuliproxError::Mapper(format!("Unexpected match_key: {:?}", inner.as_rule()))),
        }
    }

    pub(super) fn parse_match_case(
        pair: Pair<Rule>,
        expressions: &mut Vec<Expression>,
    ) -> Result<Option<MatchCase>, TuliproxError> {
        let mut inner = pair.into_inner();

        let first = inner.next().unwrap();

        let identifiers = match first.as_rule() {
            Rule::match_case_key => {
                vec![MapperScript::parse_match_case_key(first)?]
            }
            Rule::match_case_key_list => {
                let mut matches = vec![];
                for arm in first.into_inner() {
                    if arm.as_rule() != Rule::WHITESPACE {
                        match MapperScript::parse_match_case_key(arm)? {
                            MatchCaseKey::Identifier(ident) => matches.push(MatchCaseKey::Identifier(ident)),
                            MatchCaseKey::AnyMatch => matches.push(MatchCaseKey::AnyMatch),
                        }
                    }
                }
                // we don't allow inside multi match keys AnyMatch
                if matches.len() > 1 && matches.iter().filter(|&m| matches!(m, &MatchCaseKey::AnyMatch)).count() > 0 {
                    return Err(TuliproxError::Mapper("Unexpected match case key: _".to_string()));
                }
                matches
            }
            _ => return Err(TuliproxError::Mapper(format!("Unexpected match arm input: {:?}", first.as_rule()))),
        };

        if let Some(expr) = MapperScript::parse_expression(inner.next().unwrap(), expressions)? {
            expressions.push(expr);
            let expr_id = ExprId(expressions.len() - 1);
            Ok(Some(MatchCase { keys: identifiers, expression: expr_id }))
        } else {
            Ok(None)
        }
    }

    pub(super) fn parse_map_case_key(pair: Pair<Rule>) -> Result<Vec<MapCaseKey>, TuliproxError> {
        let inner = pair.into_inner().next().unwrap();
        match inner.as_rule() {
            Rule::map_case_key_list => {
                let mut matches = vec![];
                for arm in inner.into_inner() {
                    match arm.as_rule() {
                        Rule::string_literal => {
                            let raw = arm.as_str().to_string();
                            // remove quotes
                            let content = &raw[1..raw.len() - 1];
                            matches.push(MapCaseKey::Text(content.to_string()));
                        }
                        _ => return Err(TuliproxError::Mapper(format!("Unexpected map key: {:?}", arm.as_rule()))),
                    }
                }
                Ok(matches)
            }
            Rule::number_range_full => {
                let mut inner = inner.into_inner();
                let start = inner.next().unwrap().as_str().parse::<f64>().unwrap();
                let end = inner.next().unwrap().as_str().parse::<f64>().unwrap();
                Ok(vec![MapCaseKey::RangeFull(start, end)])
            }
            Rule::number_range_from => {
                let mut inner = inner.into_inner();
                let start = inner.next().unwrap().as_str().parse::<f64>().unwrap();
                Ok(vec![MapCaseKey::RangeFrom(start)])
            }
            Rule::number_range_to => {
                let mut inner = inner.into_inner();
                let to = inner.next().unwrap().as_str().parse::<f64>().unwrap();
                Ok(vec![MapCaseKey::RangeTo(to)])
            }
            Rule::number_range_eq => {
                let mut inner = inner.into_inner();
                let num = inner.next().unwrap().as_str().parse::<f64>().unwrap();
                Ok(vec![MapCaseKey::RangeEq(num)])
            }
            Rule::any_match => Ok(vec![MapCaseKey::AnyMatch]),
            _ => Err(TuliproxError::Mapper(format!("Unexpected map key: {:?}", inner.as_rule()))),
        }
    }

    pub(super) fn parse_map_case(
        pair: Pair<Rule>,
        expressions: &mut Vec<Expression>,
    ) -> Result<Option<MapCase>, TuliproxError> {
        let mut inner = pair.into_inner();

        let first = inner.next().unwrap();

        let identifier = match first.as_rule() {
            Rule::map_case_key => MapperScript::parse_map_case_key(first)?,
            _ => return Err(TuliproxError::Mapper(format!("Unexpected match arm input: {:?}", first.as_rule()))),
        };

        if let Some(expr) = MapperScript::parse_expression(inner.next().unwrap(), expressions)? {
            expressions.push(expr);
            let expr_id = ExprId(expressions.len() - 1);
            Ok(Some(MapCase { keys: identifier, expression: expr_id }))
        } else {
            Ok(None)
        }
    }

    pub(super) fn parse_expression(
        pair: Pair<Rule>,
        expressions: &mut Vec<Expression>,
    ) -> Result<Option<Expression>, TuliproxError> {
        match pair.as_rule() {
            Rule::assignment => {
                if let Some(expr) = MapperScript::parse_assignment(pair, expressions)? {
                    Ok(Some(expr))
                } else {
                    Ok(None)
                }
            }
            Rule::field => Ok(Some(Expression::FieldAccess(pair.as_str().trim().to_string()))),
            Rule::var_access => {
                let text = pair.as_str();
                if text.contains('.') {
                    let splitted: Vec<&str> = text.splitn(2, '.').collect();
                    Ok(Some(Expression::VarAccess(splitted[0].trim().to_string(), splitted[1].trim().to_string())))
                } else {
                    Ok(Some(Expression::Identifier(text.trim().to_string())))
                }
            }

            Rule::string_literal => {
                let raw = pair.as_str();
                // remove quotes
                let content = &raw[1..raw.len() - 1];
                Ok(Some(Expression::StringLiteral(content.to_string())))
            }

            Rule::number => {
                let raw = pair.as_str();
                if let Number(val) = to_number(raw) {
                    Ok(Some(Expression::NumberLiteral(val)))
                } else {
                    Err(TuliproxError::Mapper(format!("Invalid number {raw}")))
                }
            }

            Rule::regex_expr => {
                let mut inner = pair.into_inner();
                let first = inner.next().unwrap();
                let field = match first.as_rule() {
                    Rule::identifier => RegexSource::Identifier(first.as_str().to_string()),
                    Rule::field => RegexSource::Field(first.as_str().to_string()),
                    _ => return Err(TuliproxError::RegexCompile(first.as_str().to_string())),
                };
                let pattern_raw = inner.next().unwrap().as_str();
                let pattern = &pattern_raw[1..pattern_raw.len() - 1]; // Strip quotes
                match crate::model::REGEX_CACHE.get_or_compile(pattern) {
                    Ok(re) => Ok(Some(Expression::RegexExpr { field, pattern: pattern.to_string(), re_pattern: re })),
                    Err(_) => Err(TuliproxError::RegexCompile(pattern.to_string())),
                }
            }

            Rule::function_call => {
                let mut inner = pair.into_inner();
                let fn_name = inner.next().unwrap().as_str().to_string();
                let mut args = vec![];
                for arg in inner {
                    if let Some(expr) = MapperScript::parse_expression(arg, expressions)? {
                        expressions.push(expr);
                        let expr_id = ExprId(expressions.len() - 1);
                        args.push(expr_id);
                    }
                }
                let name = BuiltInFunction::from_str(&fn_name)?;
                Ok(Some(Expression::FunctionCall { name, args }))
            }

            Rule::match_block => {
                let case_pairs = pair.into_inner();
                let mut cases = vec![];
                for case in case_pairs {
                    if let Some(expr) = MapperScript::parse_match_case(case, expressions)? {
                        cases.push(expr);
                    }
                }
                if cases.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(Expression::MatchBlock(cases)))
                }
            }

            Rule::map_block => Self::parse_map_block(pair.into_inner(), expressions),

            Rule::for_each_block => Self::parse_for_each_block(pair.into_inner(), expressions),

            Rule::null => Ok(Some(Expression::NullValue)),

            Rule::expression => {
                let inner = pair.into_inner().next().unwrap();
                MapperScript::parse_expression(inner, expressions)
            }
            Rule::block_expr => {
                let inner = pair.into_inner();
                let mut block_expressions = vec![];
                for expr in inner {
                    if let Some(expr) = MapperScript::parse_expression(expr, expressions)? {
                        expressions.push(expr);
                        let expr_id = ExprId(expressions.len() - 1);
                        block_expressions.push(expr_id);
                    }
                }
                Ok(Some(Expression::Block(block_expressions)))
            }
            _ => Err(TuliproxError::Mapper(format!("Unknown expression rule: {:?}", pair.as_rule()))),
        }
    }

    pub(super) fn parse_map_block(
        mut pairs: Pairs<Rule>,
        expressions: &mut Vec<Expression>,
    ) -> Result<Option<Expression>, TuliproxError> {
        let first = pairs.next().unwrap();
        let key = match first.as_rule() {
            Rule::map_key => {
                if let Some(map_key) = first.into_inner().next() {
                    match map_key.as_rule() {
                        Rule::field => MapKey::FieldAccess(map_key.as_str().trim().to_string()),
                        Rule::var_access => {
                            let text = map_key.as_str();
                            if text.contains('.') {
                                let splitted: Vec<&str> = text.splitn(2, '.').collect();
                                MapKey::VarAccess(splitted[0].trim().to_string(), splitted[1].trim().to_string())
                            } else {
                                MapKey::Identifier(text.trim().to_string())
                            }
                        }
                        _ => {
                            return Err(TuliproxError::Mapper(format!(
                                "Unexpected map case key: {:?}",
                                map_key.as_rule()
                            )))
                        }
                    }
                } else {
                    return Err(TuliproxError::Mapper("Missing map case key".to_string()));
                }
            }
            _ => return Err(TuliproxError::Mapper(format!("Unexpected map case key: {:?}", first.as_rule()))),
        };
        let mut cases = vec![];
        for case in pairs {
            if let Some(map_case) = MapperScript::parse_map_case(case, expressions)? {
                cases.push(map_case);
            }
        }
        if cases.is_empty() {
            Ok(None)
        } else {
            Ok(Some(Expression::MapBlock { key, cases }))
        }
    }

    pub(super) fn parse_for_each_param(pair: Pair<Rule>) -> Result<Option<String>, TuliproxError> {
        let inner = pair.into_inner().next().unwrap();
        match inner.as_rule() {
            Rule::identifier => Ok(Some(inner.as_str().to_string())),
            Rule::any_match => Ok(None),
            _ => Err(TuliproxError::Mapper(format!("Unexpected for_each_param: {:?}", inner.as_rule()))),
        }
    }

    pub(super) fn parse_for_each_params(pair: Pair<Rule>) -> Result<(Option<String>, Option<String>), TuliproxError> {
        let mut inner = pair.into_inner();
        let key = Self::parse_for_each_param(inner.next().unwrap())?;
        let val = Self::parse_for_each_param(inner.next().unwrap())?;

        if key.is_none() && val.is_none() {
            return Err(TuliproxError::Mapper("At least one parameter must be named in for_each loop".to_string()));
        }

        Ok((key, val))
    }

    pub(super) fn parse_for_each_block(
        mut pairs: Pairs<Rule>,
        expressions: &mut Vec<Expression>,
    ) -> Result<Option<Expression>, TuliproxError> {
        let first = pairs.next().unwrap();
        let key = match first.as_rule() {
            Rule::for_each_target_simple => ForEachKey::Identifier(first.as_str().trim().to_string()),
            Rule::for_each_target_nested => {
                let text = first.as_str();
                let splitted: Vec<&str> = text.splitn(2, '.').collect();
                ForEachKey::VarAccess(splitted[0].trim().to_string(), splitted[1].trim().to_string())
            }
            _ => return Err(TuliproxError::Mapper(format!("Unexpected for each target: {:?}", first.as_rule()))),
        };

        if let Some(params_pair) = pairs.next() {
            // .for_each
            let (key_var, value_var) = Self::parse_for_each_params(params_pair)?;

            let expr_pair = pairs.next().unwrap();
            if let Some(expr) = MapperScript::parse_expression(expr_pair, expressions)? {
                expressions.push(expr);
                let expr_id = ExprId(expressions.len() - 1);
                return Ok(Some(Expression::ForEachBlock {
                    key,
                    expr: ForEachExpr { key_var, value_var, expression: expr_id },
                }));
            }
        }

        Ok(None)
    }
}
