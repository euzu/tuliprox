#![allow(clippy::empty_docs)]

use crate::error::{info_err, info_err_res, TuliproxError};
use crate::foundation::mapper::EvalResult::{AnyValue, Failure, Named, Number, Undefined, Value};
use crate::foundation::value_provider::ValueAccessor;
use crate::model::{PatternTemplate, PlaylistItemType, TemplateValue};
use crate::utils::{Capitalize, Internable};
use log::{debug, trace};
use pest::iterators::{Pair, Pairs};
use pest::Parser;
use pest_derive::Parser;
use regex::Regex;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::fmt::Write;
use std::ops::Deref;
use std::str::FromStr;
use std::sync::Arc;

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
field = { ^"name" | ^"title" | ^"caption" | ^"group" | ^"id" | ^"chno" | ^"logo" | ^"logo_small" | ^"parent_code" | ^"audio_track" | ^"time_shift" | ^"rec" | ^"url" | ^"epg_channel_id" | ^"epg_id" | ^"genre" }
field_access = _{ "@" ~ field }
regex_source = _{ field_access | identifier }
regex_expr = { regex_source ~ regex_op ~ string_literal }
block_expr = { "{" ~ statements ~ "}" }
condition = { function_call | var_access | field_access }
assignment = { (field_access | identifier) ~ "=" ~ expression }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExprId(pub usize);

impl Deref for ExprId {
    type Target = usize;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MatchCaseKey {
    Identifier(String),
    AnyMatch,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchCase {
    pub keys: Vec<MatchCaseKey>,
    pub expression: ExprId,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MapCaseKey {
    Text(String),
    RangeFrom(f64),
    RangeTo(f64),
    RangeFull(f64, f64),
    RangeEq(f64),
    AnyMatch,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MapCase {
    pub keys: Vec<MapCaseKey>,
    pub expression: ExprId,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MapKey {
    Identifier(String),
    FieldAccess(String),
    VarAccess(String, String),
}


#[derive(Debug, Clone, PartialEq)]
pub enum ForEachKey {
    Identifier(String),
    VarAccess(String, String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ForEachExpr {
    pub key_var: Option<String>,
    pub value_var: Option<String>,
    pub expression: ExprId,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BuiltInFunction {
    Concat,
    Uppercase,
    Lowercase,
    Capitalize,
    Split,
    Trim,
    Print,
    ToNumber,
    First,
    Template,
    Replace,
    Pad,
    Format,
    AddFavourite,
}

impl FromStr for BuiltInFunction {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "concat" => Ok(Self::Concat),
            "capitalize" => Ok(Self::Capitalize),
            "lowercase" => Ok(Self::Lowercase),
            "uppercase" => Ok(Self::Uppercase),
            "split" => Ok(Self::Split),
            "trim" => Ok(Self::Trim),
            "print" => Ok(Self::Print),
            "number" => Ok(Self::ToNumber),
            "first" => Ok(Self::First),
            "template" => Ok(Self::Template),
            "replace" => Ok(Self::Replace),
            "pad" => Ok(Self::Pad),
            "format" => Ok(Self::Format),
            "add_favourite" => Ok(Self::AddFavourite),
            _ => info_err_res!("Unknown function {s}"),
        }
    }
}

impl Display for BuiltInFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let str = match &self {
            Self::Concat => "concat",
            Self::Capitalize => "capitalize",
            Self::Lowercase => "lowercase",
            Self::Uppercase => "uppercase",
            Self::Split => "split",
            Self::Trim => "trim",
            Self::Print => "print",
            Self::ToNumber => "number",
            Self::First => "first",
            Self::Template => "template",
            Self::Replace => "replace",
            Self::Pad => "pad",
            Self::Format => "format",
            Self::AddFavourite => "add_favourite",
        }.to_owned();
        write!(f, "{str}")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RegexSource {
    Identifier(String),
    Field(String),
}

#[derive(Debug, Clone)]
pub enum Expression {
    Identifier(String),
    StringLiteral(String),
    NumberLiteral(f64),
    FieldAccess(String),
    VarAccess(String, String),
    RegexExpr { field: RegexSource, pattern: String, re_pattern: Arc<Regex> },
    FunctionCall { name: BuiltInFunction, args: Vec<ExprId> },
    Assignment { target: AssignmentTarget, expr: ExprId },
    MatchBlock(Vec<MatchCase>),
    MapBlock { key: MapKey, cases: Vec<MapCase> },
    ForEachBlock { key: ForEachKey, expr: ForEachExpr },
    NullValue,
    Block(Vec<ExprId>),
}

impl PartialEq for Expression {
    fn eq(&self, other: &Self) -> bool {
        use Expression::*;
        match (self, other) {
            (Identifier(a), Identifier(b)) => a == b,
            (StringLiteral(a), StringLiteral(b)) => a == b,
            (NumberLiteral(a), NumberLiteral(b)) => a == b,
            (FieldAccess(a), FieldAccess(b)) => a == b,
            (VarAccess(a1, b1), VarAccess(a2, b2)) => a1 == a2 && b1 == b2,
            (
                RegexExpr { field: f1, pattern: p1, .. },
                RegexExpr { field: f2, pattern: p2, .. },
            ) => f1 == f2 && p1 == p2,
            (FunctionCall { name: n1, args: a1 }, FunctionCall { name: n2, args: a2 }) => n1 == n2 && a1 == a2,
            (Assignment { target: t1, expr: e1 }, Assignment { target: t2, expr: e2 }) => t1 == t2 && e1 == e2,
            (MatchBlock(m1), MatchBlock(m2)) => m1 == m2,
            (MapBlock { key: k1, cases: c1 }, MapBlock { key: k2, cases: c2 }) => k1 == k2 && c1 == c2,
            (ForEachBlock { key: k1, expr: c1 }, ForEachBlock { key: k2, expr: c2 }) => k1 == k2 && c1 == c2,
            (NullValue, NullValue) => true,
            (Block(b1), Block(b2)) => b1 == b2,
            _ => false,
        }
    }
}


#[derive(Debug, Clone, PartialEq)]
pub enum AssignmentTarget {
    Identifier(String),
    Field(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Expression(ExprId),
    Comment(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct MapperScript {
    pub expressions: Vec<Expression>,
    pub statements: Vec<Statement>,
}

impl MapperScript {
    pub fn eval(&self, setter: &mut ValueAccessor, templates: Option<&Vec<PatternTemplate>>) {
        let ctx = &mut MapperContext::new(&self.expressions, templates);
        self.eval_with_context(ctx, setter);
    }

    fn eval_with_context(&self, ctx: &mut MapperContext, setter: &mut ValueAccessor) {
        for stmt in &self.statements {
            stmt.eval(ctx, setter);
        }
    }

    pub fn get_expr_by_id(&self, id: usize) -> Option<&Expression> {
        self.expressions.get(id)
    }
}

impl ExprId {
    pub fn eval(self, ctx: &mut MapperContext, accessor: &mut ValueAccessor) -> EvalResult {
        let id = self.0;
        ctx.eval_expr_by_id(id, accessor)
    }
}

impl Statement {
    pub fn eval(&self, ctx: &mut MapperContext, setter: &mut ValueAccessor) {
        match self {
            Statement::Expression(expr_id) => {
                let result = expr_id.eval(ctx, setter);
                if let Failure(err) = &result {
                    debug!("{err}");
                    // } else {
                    //     trace!("Ignoring result {result:?}");
                }
            }
            Statement::Comment(_) => {}
        }
    }
}

impl MapperScript {
    fn validate(expressions: &Vec<Expression>, statements: &Vec<Statement>, templates: Option<&Vec<PatternTemplate>>) -> Result<(), TuliproxError> {
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

    pub fn parse(input: &str, templates: Option<&Vec<PatternTemplate>>) -> Result<Self, TuliproxError> {
        let mut parsed = MapperParser::parse(Rule::main, input).map_err(|e| info_err!("{e}"))?;
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
    fn parse_statement(pair: Pair<Rule>, expressions: &mut Vec<Expression>) -> Result<Option<Statement>, TuliproxError> {
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

    fn parse_assignment(pair: Pair<Rule>, expressions: &mut Vec<Expression>) -> Result<Option<Expression>, TuliproxError> {
        let mut inner = pair.into_inner();
        let name = inner.next().unwrap();
        let target = match name.as_rule() {
            Rule::identifier => AssignmentTarget::Identifier(name.as_str().to_string()),
            Rule::field => AssignmentTarget::Field(name.as_str().to_string()),
            _ => return info_err_res!("Assignment target isn't supported {}", name.as_str()),
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

    fn parse_match_case_key(pair: Pair<Rule>) -> Result<MatchCaseKey, TuliproxError> {
        let inner = pair.into_inner().next().unwrap();
        match inner.as_rule() {
            Rule::identifier => Ok(MatchCaseKey::Identifier(inner.as_str().to_string())),
            Rule::any_match => Ok(MatchCaseKey::AnyMatch),
            _ => info_err_res!("Unexpected match_key: {:?}", inner.as_rule()),
        }
    }

    fn parse_match_case(pair: Pair<Rule>, expressions: &mut Vec<Expression>) -> Result<Option<MatchCase>, TuliproxError> {
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
                    return info_err_res!("Unexpected match case key: _");
                }
                matches
            }
            _ => return info_err_res!("Unexpected match arm input: {:?}", first.as_rule()),
        };

        if let Some(expr) = MapperScript::parse_expression(inner.next().unwrap(), expressions)? {
            expressions.push(expr);
            let expr_id = ExprId(expressions.len() - 1);
            Ok(Some(MatchCase {
                keys: identifiers,
                expression: expr_id,
            }))
        } else {
            Ok(None)
        }
    }

    fn parse_map_case_key(pair: Pair<Rule>) -> Result<Vec<MapCaseKey>, TuliproxError> {
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
                        _ => return info_err_res!("Unexpected map key: {:?}", arm.as_rule()),
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
            _ => info_err_res!("Unexpected map key: {:?}", inner.as_rule()),
        }
    }

    fn parse_map_case(pair: Pair<Rule>, expressions: &mut Vec<Expression>) -> Result<Option<MapCase>, TuliproxError> {
        let mut inner = pair.into_inner();

        let first = inner.next().unwrap();

        let identifier = match first.as_rule() {
            Rule::map_case_key => {
                MapperScript::parse_map_case_key(first)?
            }
            _ => return info_err_res!("Unexpected match arm input: {:?}", first.as_rule()),
        };

        if let Some(expr) = MapperScript::parse_expression(inner.next().unwrap(), expressions)? {
            expressions.push(expr);
            let expr_id = ExprId(expressions.len() - 1);
            Ok(Some(MapCase {
                keys: identifier,
                expression: expr_id,
            }))
        } else {
            Ok(None)
        }
    }

    fn parse_expression(pair: Pair<Rule>, expressions: &mut Vec<Expression>) -> Result<Option<Expression>, TuliproxError> {
        match pair.as_rule() {
            Rule::assignment => {
                if let Some(expr) = MapperScript::parse_assignment(pair, expressions)? {
                    Ok(Some(expr))
                } else {
                    Ok(None)
                }
            }
            Rule::field => {
                Ok(Some(Expression::FieldAccess(pair.as_str().trim().to_string())))
            }
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
                    info_err_res!("Invalid number {raw}")
                }
            }

            Rule::regex_expr => {
                let mut inner = pair.into_inner();
                let first = inner.next().unwrap();
                let field = match first.as_rule() {
                    Rule::identifier => RegexSource::Identifier(first.as_str().to_string()),
                    Rule::field => RegexSource::Field(first.as_str().to_string()),
                    _ => return info_err_res!("Invalid regex source {}", first.as_str().to_string()),
                };
                let pattern_raw = inner.next().unwrap().as_str();
                let pattern = &pattern_raw[1..pattern_raw.len() - 1]; // Strip quotes
                match crate::model::REGEX_CACHE.get_or_compile(pattern) {
                    Ok(re) => Ok(Some(Expression::RegexExpr { field, pattern: pattern.to_string(), re_pattern: re })),
                    Err(_) => info_err_res!("Invalid regex {}", pattern),
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

            Rule::map_block => {
                Self::parse_map_block(pair.into_inner(), expressions)
            }

            Rule::for_each_block => {
                Self::parse_for_each_block(pair.into_inner(), expressions)
            }

            Rule::null => {
                Ok(Some(Expression::NullValue))
            }

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
            _ => info_err_res!("Unknown expression rule: {:?}", pair.as_rule()),
        }
    }

    fn parse_map_block(mut pairs: Pairs<Rule>, expressions: &mut Vec<Expression>) -> Result<Option<Expression>, TuliproxError> {
        let first = pairs.next().unwrap();
        let key = match first.as_rule() {
            Rule::map_key => {
                if let Some(map_key) = first.into_inner().next() {
                    match map_key.as_rule() {
                        Rule::field => {
                            MapKey::FieldAccess(map_key.as_str().trim().to_string())
                        }
                        Rule::var_access => {
                            let text = map_key.as_str();
                            if text.contains('.') {
                                let splitted: Vec<&str> = text.splitn(2, '.').collect();
                                MapKey::VarAccess(splitted[0].trim().to_string(), splitted[1].trim().to_string())
                            } else {
                                MapKey::Identifier(text.trim().to_string())
                            }
                        }
                        _ => return info_err_res!("Unexpected map case key: {:?}", map_key.as_rule()),
                    }
                } else {
                    return info_err_res!("Missing map case key");
                }
            }
            _ => return info_err_res!("Unexpected map case key: {:?}", first.as_rule()),
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

    fn parse_for_each_param(pair: Pair<Rule>) -> Result<Option<String>, TuliproxError> {
        let inner = pair.into_inner().next().unwrap();
        match inner.as_rule() {
            Rule::identifier => Ok(Some(inner.as_str().to_string())),
            Rule::any_match => Ok(None),
            _ => info_err_res!("Unexpected for_each_param: {:?}", inner.as_rule()),
        }
    }

    fn parse_for_each_params(pair: Pair<Rule>) -> Result<(Option<String>, Option<String>), TuliproxError> {
        let mut inner = pair.into_inner();
        let key = Self::parse_for_each_param(inner.next().unwrap())?;
        let val = Self::parse_for_each_param(inner.next().unwrap())?;

        if key.is_none() && val.is_none() {
            return info_err_res!("At least one parameter must be named in for_each loop");
        }

        Ok((key, val))
    }

    fn parse_for_each_block(mut pairs: Pairs<Rule>, expressions: &mut Vec<Expression>) -> Result<Option<Expression>, TuliproxError> {
        let first = pairs.next().unwrap();
        let key = match first.as_rule() {
            Rule::for_each_target_simple => {
                ForEachKey::Identifier(first.as_str().trim().to_string())
            }
            Rule::for_each_target_nested => {
                let text = first.as_str();
                let splitted: Vec<&str> = text.splitn(2, '.').collect();
                ForEachKey::VarAccess(splitted[0].trim().to_string(), splitted[1].trim().to_string())
            }
            _ => return info_err_res!("Unexpected for each target: {:?}", first.as_rule()),
        };

        if let Some(params_pair) = pairs.next() { // .for_each
            let (key_var, value_var) = Self::parse_for_each_params(params_pair)?;

            let expr_pair = pairs.next().unwrap();
            if let Some(expr) = MapperScript::parse_expression(expr_pair, expressions)? {
                expressions.push(expr);
                let expr_id = ExprId(expressions.len() - 1);
                return Ok(Some(Expression::ForEachBlock {
                    key,
                    expr: ForEachExpr {
                        key_var,
                        value_var,
                        expression: expr_id,
                    },
                }));
            }
        }

        Ok(None)
    }
}

pub struct MapperContext<'a> {
    expressions: &'a Vec<Expression>,
    variables: HashMap<String, EvalResult>,
    templates: Option<HashMap<String, &'a PatternTemplate>>,
}

impl<'a> MapperContext<'a> {
    fn new(expressions: &'a Vec<Expression>, templates: Option<&'a Vec<PatternTemplate>>) -> Self {
        Self {
            expressions,
            variables: HashMap::new(),
            templates: templates.and_then(|vec_templates| {
                if vec_templates.is_empty() {
                    None
                } else {
                    let mut hash_map = HashMap::new();
                    for template in vec_templates {
                        hash_map.insert(template.name.to_string(), template);
                    }
                    Some(hash_map)
                }
            }),
        }
    }

    fn get_template(&self, name: &str) -> Option<&str> {
        match self.templates.as_ref() {
            None => None,
            Some(templates) => templates.get(name).and_then(|&template| {
                match &template.value {
                    TemplateValue::Single(v) => Some(v.as_str()),
                    TemplateValue::Multi(_) => None,
                }
            })
        }
    }

    fn set_var(&mut self, name: &str, value: EvalResult) {
        self.variables.insert(name.to_string(), value);
    }

    fn has_var(&self, name: &str) -> bool {
        self.variables.contains_key(name)
    }

    fn get_var(&self, name: &str) -> &EvalResult {
        self.variables.get(name).unwrap_or(&Undefined)
    }

    fn eval_expr_by_id(&mut self, id: usize, accessor: &mut ValueAccessor) -> EvalResult {
        let Some(expr) = self.expressions.get(id) else { return Undefined };
        expr.eval(self, accessor)
    }

    fn validate_expr(&mut self, expr_id: ExprId, identifiers: &mut HashSet<String>) -> Result<(), TuliproxError> {
        let Some(expr) = self.expressions.get(expr_id.0) else { return info_err_res!("No matching expression found at index {}", expr_id.0) };
        match expr {
            Expression::Identifier(ident)
            | Expression::VarAccess(ident, _) => {
                if !identifiers.contains(ident.as_str()) {
                    return info_err_res!("Identifier unknown {}, {:?}", ident, expr);
                }
            }
            Expression::NullValue
            | Expression::FieldAccess(_)
            | Expression::StringLiteral(_)
            | Expression::NumberLiteral(_) => {}
            Expression::RegexExpr { field, pattern: _pattern, re_pattern: _re_pattern } => {
                match field {
                    RegexSource::Identifier(ident) => {
                        if !identifiers.contains(ident.as_str()) {
                            return info_err_res!("Regex identifier unknown {}, {:?}", ident, expr);
                        }
                    }
                    RegexSource::Field(_) => {}
                }
            }
            Expression::Assignment { target, expr } => {
                match target {
                    AssignmentTarget::Identifier(ident) => {
                        identifiers.insert(ident.to_string());
                    }
                    AssignmentTarget::Field(_) => {}
                }
                self.validate_expr(*expr, identifiers)?;
            }
            Expression::FunctionCall { name, args } => {
                if args.is_empty() {
                    return info_err_res!("Function needs at least one argument {:?}", name);
                }
                match name {
                    BuiltInFunction::ToNumber
                    | BuiltInFunction::Template
                    | BuiltInFunction::First
                    | BuiltInFunction::AddFavourite => {
                        if args.len() > 1 {
                            return info_err_res!("Function accepts only one argument {:?}, {} given", name, args.len());
                        }
                    }
                    BuiltInFunction::Split => {
                        if args.len() != 2 {
                            return info_err_res!("Function accepts two arguments {:?}, {} given", name, args.len());
                        }
                    }
                    BuiltInFunction::Replace => {
                        if args.len() != 3 {
                            return info_err_res!("Function accepts three arguments {:?}, {} given", name, args.len());
                        }
                    }
                    BuiltInFunction::Pad => {
                        if !(args.len() == 3 || args.len() == 4) {
                            return info_err_res!("Function accepts three or four arguments {:?}, {} given", name, args.len());
                        }
                    }
                    _ => {}
                }
                for expr_id in args {
                    self.validate_expr(*expr_id, identifiers)?;
                }
            }
            Expression::MatchBlock(cases) => {
                self.validate_match_block(identifiers, cases)?;
            }
            Expression::MapBlock { key, cases } => {
                self.validate_map_block(identifiers, key, cases)?;
            }
            Expression::ForEachBlock { key, expr } => {
                self.validate_for_each_block(identifiers, key, expr)?;
            }
            Expression::Block(expressions) => {
                for expr_id in expressions {
                    self.validate_expr(*expr_id, identifiers)?;
                }
            }
        }
        Ok(())
    }

    fn validate_match_block(&mut self, identifiers: &mut HashSet<String>, cases: &Vec<MatchCase>) -> Result<(), TuliproxError> {
        let mut case_keys = HashSet::new();
        for match_case in cases {
            let mut any_match_count = 0;
            let mut identifier_key = String::with_capacity(56);
            for identifier in &match_case.keys {
                match identifier {
                    MatchCaseKey::Identifier(ident) => {
                        if !identifiers.contains(ident.as_str()) {
                            return info_err_res!("Match case identifier unknown {}", ident);
                        }
                        identifier_key.push_str(ident.as_str());
                        identifier_key.push_str(", ");
                    }
                    MatchCaseKey::AnyMatch => {
                        any_match_count += 1;
                        if any_match_count > 1 {
                            return info_err_res!("Match case can only have one '_'");
                        }
                        identifier_key.push_str("_, ");
                    }
                }
            }
            if case_keys.contains(&identifier_key) {
                return info_err_res!("Duplicate case {}", identifier_key);
            }
            case_keys.insert(identifier_key);
            self.validate_expr(match_case.expression, identifiers)?;
        }
        Ok(())
    }

    fn validate_map_block(&mut self, identifiers: &mut HashSet<String>, key: &MapKey, cases: &Vec<MapCase>) -> Result<(), TuliproxError> {
        match key {
            MapKey::Identifier(ident)
            | MapKey::VarAccess(ident, _) => {
                if !identifiers.contains(ident.as_str()) {
                    return info_err_res!("Map key identifier unknown {}", ident);
                }
            }
            MapKey::FieldAccess(_) => {}
        }
        let mut case_keys = HashSet::new();
        let mut any_match_count = 0;
        for map_case in cases {
            for key in &map_case.keys {
                match key {
                    MapCaseKey::Text(value) => {
                        if case_keys.contains(value.as_str()) {
                            return info_err_res!("Duplicate case {}", value);
                        }
                        case_keys.insert(value.as_str());
                    }
                    MapCaseKey::RangeEq(_)
                    | MapCaseKey::RangeTo(_)
                    | MapCaseKey::RangeFrom(_) => {}
                    MapCaseKey::RangeFull(from, to) => {
                        if *from > *to {
                            return info_err_res!("Invalid range {from}..{to}");
                        }
                    }
                    MapCaseKey::AnyMatch => {
                        any_match_count += 1;
                        if any_match_count > 1 {
                            return info_err_res!("Map case can only have one '_'");
                        }
                    }
                }
            }
            self.validate_expr(map_case.expression, identifiers)?;
        }
        Ok(())
    }

    fn validate_for_each_block(&mut self, identifiers: &mut HashSet<String>, key: &ForEachKey, expr: &ForEachExpr) -> Result<(), TuliproxError> {
        match key {
            ForEachKey::Identifier(ident)
            | ForEachKey::VarAccess(ident, _) => {
                if !identifiers.contains(ident.as_str()) {
                    return info_err_res!("For each key identifier unknown {}", ident);
                }
            }
        }
        let mut local_identifiers = identifiers.clone();

        if let Some(key_var) = &expr.key_var {
            if local_identifiers.contains(key_var) {
                return info_err_res!("For each key variable shadows existing identifier {}", key_var);
            }
            local_identifiers.insert(key_var.clone());
        }

        if let Some(value_var) = &expr.value_var {
            if local_identifiers.contains(value_var) {
                return info_err_res!("For each value variable shadows existing identifier {}", value_var);
            }
            local_identifiers.insert(value_var.clone());
        }

        self.validate_expr(expr.expression, &mut local_identifiers)?;

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum EvalResult {
    Undefined,
    Value(String),
    Number(f64),
    Named(Vec<(String, String)>),
    AnyValue,
    Failure(String),
}

fn to_number(value: &str) -> EvalResult {
    match value.parse::<f64>() {
        Ok(num) => Number(num),
        Err(_) => Failure(format!("Invalid number: {value}")),
    }
}

fn compare_number(a: f64, b: f64) -> Ordering {
    let epsilon = 1e-3; // = 0.001

    if (a - b).abs() < epsilon {
        Ordering::Equal
    } else if a < b {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}

#[allow(clippy::cast_possible_truncation)]
fn format_number(num: f64) -> String {
    let epsilon = 1e-3; // = 0.001

    if num.fract().abs() < epsilon {
        format!("{}", num as i64)
    } else {
        format!("{num}")
    }
}

fn compare_tuple_vec(
    a: &[(String, String)],
    b: &[(String, String)],
) -> bool {
    fn to_map(vec: &[(String, String)]) -> HashMap<&str, &str> {
        vec.iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    to_map(a) == to_map(b)
}

fn match_number(num: f64, s: &str) -> bool {
    if let Ok(val) = s.parse::<f64>() {
        return compare_number(num, val) == Ordering::Equal;
    }
    false
}

fn cmp_number(num: f64, s: &str) -> Option<Ordering> {
    if let Ok(val) = s.parse::<f64>() {
        return Some(compare_number(num, val));
    }
    None
}


impl EvalResult {
    fn matches(&self, other: &EvalResult) -> bool {
        match (self, other) {
            (AnyValue, _) | (_, AnyValue) => true,
            (Value(a), Value(b)) => a == b,
            (Number(a), Value(b)) => match_number(*a, b),
            (Value(a), Number(b)) => match_number(*b, a),
            (Number(a), Number(b)) => compare_number(*a, *b) == Ordering::Equal,
            (Named(a), Named(b)) => compare_tuple_vec(a, b),
            _ => false,
        }
    }

    fn compare(&self, other: &EvalResult) -> Option<Ordering> {
        match (self, other) {
            (AnyValue, _) | (_, AnyValue) => Some(Ordering::Equal),
            (Value(a), Value(b)) => Some(a.cmp(b)),
            (Number(a), Value(b)) => cmp_number(*a, b),
            (Value(a), Number(b)) => match cmp_number(*b, a) {
                None => None,
                Some(ord) => {
                    match ord {
                        Ordering::Less => Some(Ordering::Greater),
                        Ordering::Equal => Some(Ordering::Equal),
                        Ordering::Greater => Some(Ordering::Less),
                    }
                }
            },
            (Number(a), Number(b)) => Some(compare_number(*a, *b)),
            (Named(a), Named(b)) => if compare_tuple_vec(a, b) { Some(Ordering::Equal) } else { None },
            _ => None,
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Failure(_))
    }
}

fn concat_args(args: &Vec<EvalResult>) -> Vec<Cow<'_, str>> {
    let mut result = vec![];

    for arg in args {
        match arg {
            Value(value) => result.push(Cow::Borrowed(value.as_str())),
            Number(value) => result.push(Cow::Owned(format_number(*value))),
            Named(pairs) => {
                for (i, (key, value)) in pairs.iter().enumerate() {
                    result.push(Cow::Borrowed(key.as_str()));
                    result.push(Cow::Borrowed(": "));
                    result.push(Cow::Borrowed(value.as_str()));
                    if i < pairs.len() - 1 {
                        result.push(Cow::Borrowed(", "));
                    }
                }
            }
            Undefined | AnyValue | Failure(_) => {}
        }
    }

    result
}

macro_rules! extract_evaluated_arg_value {
    ($evaluated_args:expr, $index:expr) => {{
        if $index >= $evaluated_args.len() {
            None
        } else {
            let evaluated_arg = &$evaluated_args[$index];
            match evaluated_arg {
                Value(value) => Some(value),
                Named(values) => values.first().map(|(_key, val)| val),
                _ => None,
            }
        }
    }};
}

macro_rules! extract_arg_value {
    ($evaluated_arg:expr) => {{
        match $evaluated_arg {
            Value(value) => Some(value),
            Named(values) => values.first().map(|(_key, val)| val),
            _ => None,
        }
    }};
}

impl Expression {
    #[allow(clippy::too_many_lines)]
    pub fn eval(&self, ctx: &mut MapperContext, accessor: &mut ValueAccessor) -> EvalResult {
        match self {
            Expression::NullValue => Undefined,
            Expression::Identifier(name) => {
                if ctx.has_var(name) {
                    ctx.get_var(name).clone()
                } else {
                    Failure(format!("Variable with name {name} not found."))
                }
            }
            Expression::FieldAccess(field) => {
                if let Some(val) = accessor.get(field) {
                    Value(val.to_string())
                } else {
                    Undefined
                }
            }
            Expression::VarAccess(name, field) => {
                match ctx.variables.get(name) {
                    None => Failure(format!("Variable with name {name} not found.")),
                    Some(value) => match value {
                        Undefined => Undefined,
                        Number(_) | Value(_) => Failure(format!("Variable with name {name} has no fields.")),
                        Named(values) => {
                            for (key, val) in values {
                                if key == field {
                                    return Value(val.to_string());
                                }
                            }
                            Failure(format!("Variable with name {name} has no field {field}."))
                        }
                        AnyValue | Failure(_) => value.clone(),
                    },
                }
            }
            Expression::StringLiteral(s) => Value(s.clone()),
            Expression::NumberLiteral(num) => Number(*num),
            Expression::RegexExpr { field, pattern: _pattern, re_pattern } => {
                let source = match field {
                    RegexSource::Identifier(ident) => {
                        match ctx.get_var(ident) {
                            Value(text) => Some(text.as_str().into()),
                            _ => None,
                        }
                    }
                    RegexSource::Field(field) => accessor.get(field),
                };
                if let Some(val) = source {
                    let mut values = vec![];
                    for caps in re_pattern.captures_iter(&val) {
                        // Positional groups
                        for i in 1..caps.len() {
                            if let Some(m) = caps.get(i) {
                                values.push((i.to_string(), m.as_str().to_string()));
                            }
                        }

                        // named groups
                        for name in re_pattern.capture_names().flatten() {
                            if let Some(m) = caps.name(name) {
                                values.push((name.to_string(), m.as_str().to_string()));
                            }
                        }
                    }
                    if values.is_empty() {
                        return Undefined;
                    } else if values.len() == 1 {
                        return Value(values[0].1.to_string());
                    }
                    return Named(values);
                }
                Undefined
            }
            Expression::Assignment { target, expr } => {
                let val = expr.eval(ctx, accessor);
                match target {
                    AssignmentTarget::Identifier(name) => {
                        ctx.set_var(name, val);
                        Undefined
                    }
                    AssignmentTarget::Field(name) => {
                        match val {
                            Value(content) => {
                                accessor.set(name, content.as_str());
                            }
                            Number(num) => {
                                accessor.set(name, format_number(num).as_str());
                            }
                            Named(pairs) => {
                                let mut result = String::with_capacity(128);
                                for (i, (key, value)) in pairs.iter().enumerate() {
                                    result.push_str(key);
                                    result.push_str(": ");
                                    result.push_str(value);
                                    if i < pairs.len() - 1 {
                                        result.push_str(", ");
                                    }
                                }
                                accessor.set(name, &result);
                            }
                            Undefined | AnyValue => {}
                            Failure(err) => {
                                return Failure(format!("Failed to set field {name} value: {err}"));
                            }
                        }
                        Undefined
                    }
                }
            }
            Expression::FunctionCall { name, args } => {
                let mut evaluated_args: Vec<EvalResult> = args.iter().map(|a| a.eval(ctx, accessor)).collect();
                for arg in &evaluated_args {
                    if arg.is_error() {
                        return Failure(format!("Function '{name:?}' failed: {}", if let Failure(msg) = arg { msg } else { "Unknown error" }));
                    }
                }
                evaluated_args.retain(|er| !matches!(er, Undefined | Failure(_) | AnyValue));
                if evaluated_args.is_empty() {
                    if matches!(name, BuiltInFunction::Print) {
                        trace!("[MapperScript] undefined");
                    }
                    Undefined
                } else {
                    match name {
                        BuiltInFunction::Concat => Value(concat_args(&evaluated_args).join("")),
                        BuiltInFunction::Uppercase => Value(concat_args(&evaluated_args).join(" ").to_uppercase()),
                        BuiltInFunction::Trim => Value(concat_args(&evaluated_args).iter().map(|s| s.trim()).collect::<Vec<_>>().join(" ").trim().to_string()),
                        BuiltInFunction::Lowercase => Value(concat_args(&evaluated_args).join(" ").to_lowercase()),
                        BuiltInFunction::Capitalize => Value(concat_args(&evaluated_args).iter().map(Capitalize::capitalize).collect::<Vec<_>>().join(" ")),
                        BuiltInFunction::Split => {
                            let string = extract_evaluated_arg_value!(evaluated_args, 0);
                            let pattern = extract_evaluated_arg_value!(evaluated_args, 1);

                            if let (Some(text), Some(pat)) = (string, pattern) {
                                match crate::model::REGEX_CACHE.get_or_compile(pat) {
                                    Ok(re) => Named(re.split(text).enumerate().map(|(i, s)| (i.to_string(), s.trim().to_string())).collect()),
                                    Err(e) => Failure(format!("Invalid regex pattern '{}': {}", pat, e)),
                                }
                            } else {
                                Undefined
                            }
                        }
                        BuiltInFunction::Print => {
                            trace!("[MapperScript] {}", concat_args(&evaluated_args).join(""));
                            Undefined
                        }
                        BuiltInFunction::ToNumber => {
                            let evaluated_arg = &evaluated_args[0];
                            match evaluated_arg {
                                Value(value) => {
                                    to_number(value)
                                }
                                _ => evaluated_arg.clone()
                            }
                        }
                        BuiltInFunction::First => {
                            match evaluated_args.first() {
                                Some(value) => {
                                    match value {
                                        Named(values) => {
                                            match values.first() {
                                                None => Undefined,
                                                Some((_key, val)) => Value(val.to_string()),
                                            }
                                        }
                                        _ => value.clone()
                                    }
                                }
                                None => Undefined,
                            }
                        }
                        BuiltInFunction::Template => {
                            let value = extract_evaluated_arg_value!(evaluated_args, 0);
                            if let Some(val) = value {
                                match ctx.get_template(val) {
                                    Some(v) => Value(v.to_string()),
                                    None => Undefined,
                                }
                            } else {
                                Undefined
                            }
                        }
                        BuiltInFunction::Replace => {
                            let value = extract_evaluated_arg_value!(evaluated_args, 0);
                            let pattern = extract_evaluated_arg_value!(evaluated_args, 1);
                            let substring = extract_evaluated_arg_value!(evaluated_args, 2);

                            if let (Some(text), Some(pat), Some(subst)) = (value, pattern, substring) {
                                Value(text.replace(pat, subst))
                            } else {
                                evaluated_args[0].clone()
                            }
                        }
                        BuiltInFunction::Pad => {
                            let value = match &evaluated_args[0] {
                                Number(value) => Some(value.to_string()),
                                Value(value) => Some(value.clone()),
                                Named(values) => values.first().map(|(_key, val)| val.clone()),
                                _ => None,
                            };
                            let width = match &evaluated_args[1] {
                                Number(value) => {
                                    if value.is_nan() || value.is_infinite() {
                                        0
                                    } else {
                                        value.abs().min(usize::MAX as f64) as usize
                                    }
                                }
                                Value(value) => value.parse::<usize>().ok().unwrap_or(0),
                                Named(values) => values.first().and_then(|(_key, val)| val.parse::<usize>().ok()).unwrap_or(0),
                                _ => 0,
                            };

                            let fill = match &evaluated_args[2] {
                                Number(value) => Some(value.to_string()),
                                Value(value) => Some(value.clone()),
                                Named(values) => values.first().map(|(_key, val)| val.clone()),
                                _ => None,
                            };

                            let align = extract_evaluated_arg_value!(evaluated_args, 3); // "<", ">", "^"

                            if let Some(text) = value {
                                let fill_char = fill.and_then(|s| s.chars().next()).unwrap_or(' ');

                                let padded = if width <= text.len() {
                                    text.clone()
                                } else {
                                    let pad = width - text.len();
                                    if let Some(al) = align {
                                        match al.as_str() {
                                            "^" => {
                                                let left = pad / 2;
                                                let right = pad - left;
                                                format!(
                                                    "{}{}{}",
                                                    fill_char.to_string().repeat(left),
                                                    text,
                                                    fill_char.to_string().repeat(right)
                                                )
                                            }
                                            "<" => format!("{}{}", text, fill_char.to_string().repeat(pad)),
                                            _ => format!("{}{}", fill_char.to_string().repeat(pad), text),
                                        }
                                    } else {
                                        format!("{}{}", fill_char.to_string().repeat(pad), text)
                                    }
                                };

                                Value(padded)
                            } else {
                                Undefined
                            }
                        }
                        BuiltInFunction::Format => {
                            let fmt_pattern = extract_evaluated_arg_value!(evaluated_args, 0);

                            if let Some(fmt_str) = fmt_pattern {
                                let args: Vec<Option<&String>> = evaluated_args.iter()
                                    .skip(1)
                                    .map(|v| extract_arg_value!(v))
                                    .collect();

                                let mut formatted = String::new();
                                let mut arg_iter = args.iter();
                                let mut chars = fmt_str.chars().peekable();

                                while let Some(ch) = chars.next() {
                                    if ch == '{' && chars.peek() == Some(&'}') {
                                        chars.next();
                                        if let Some(Some(arg)) = arg_iter.next() {
                                            write!(formatted, "{}", arg).unwrap();
                                        } else {
                                            formatted.push_str("{}");
                                        }
                                    } else {
                                        formatted.push(ch);
                                    }
                                }

                                Value(formatted)
                            } else {
                                Undefined
                            }
                        }
                        BuiltInFunction::AddFavourite => {
                            let group_name = extract_evaluated_arg_value!(evaluated_args, 0);
                            if let Some(group) = group_name {
                                let item_type = accessor.pli.header.item_type;
                                if item_type != PlaylistItemType::Series && item_type != PlaylistItemType::LocalSeries {
                                    let mut pli = accessor.pli.clone();
                                    pli.header.group = group.intern();
                                    pli.header.uuid = crate::utils::create_alias_uuid(&accessor.pli.header.uuid, group);
                                    accessor.virtual_items.push((group.clone(), pli));
                                }
                            }
                            Undefined
                        }
                    }
                }
            }
            Expression::MatchBlock(cases) => {
                for match_case in cases {
                    let mut case_keys = vec![];
                    for case_key in &match_case.keys {
                        match case_key {
                            MatchCaseKey::Identifier(ident) => {
                                if !ctx.has_var(ident) {
                                    return Failure(format!("Match case invalid! Variable with name {ident} not found."));
                                }
                                case_keys.push(ctx.get_var(ident).clone());
                            }
                            MatchCaseKey::AnyMatch => case_keys.push(AnyValue),
                        }
                    }

                    let mut match_count = 0;
                    let case_keys_len = case_keys.len();
                    for case_key in case_keys {
                        match case_key {
                            Value(_)
                            | Number(_)
                            | Named(_)
                            | AnyValue => match_count += 1,
                            Undefined | Failure(_) => {}
                        }
                    }
                    if match_count == case_keys_len {
                        return match_case.expression.eval(ctx, accessor);
                    }
                }
                Undefined
            }
            Expression::MapBlock { key, cases } => {
                let key_value = match key {
                    MapKey::Identifier(ident) => {
                        if !ctx.has_var(ident) {
                            return Failure(format!("Map expression invalid! Variable with name {ident} not found."));
                        }
                        ctx.get_var(ident).clone()
                    }
                    MapKey::FieldAccess(field) => {
                        if let Some(val) = accessor.get(field) {
                            Value(val.to_string())
                        } else {
                            Undefined
                        }
                    }
                    MapKey::VarAccess(name, field) => {
                        match ctx.variables.get(name) {
                            None => Failure(format!("Variable with name {name} not found.")),
                            Some(value) => match value {
                                Undefined => Undefined,
                                Number(_) | Value(_) => Failure(format!("Variable with name {name} has no fields.")),
                                Named(values) => {
                                    for (key, val) in values {
                                        if key == field {
                                            return Value(val.to_string());
                                        }
                                    }
                                    Failure(format!("Variable with name {name} has no field {field}."))
                                }
                                AnyValue | Failure(_) => value.clone(),
                            },
                        }
                    }
                };

                for map_case in cases {
                    let mut matches = false;
                    for key in &map_case.keys {
                        if match key {
                            MapCaseKey::Text(value) => key_value.matches(&Value(value.to_string())),
                            MapCaseKey::AnyMatch => true,
                            MapCaseKey::RangeFrom(num) => {
                                match key_value.compare(&Number(*num)) {
                                    None => false,
                                    Some(ord) => match ord {
                                        Ordering::Less => false,
                                        Ordering::Equal | Ordering::Greater => true,
                                    }
                                }
                            }
                            MapCaseKey::RangeTo(num) => {
                                match key_value.compare(&Number(*num)) {
                                    None => false,
                                    Some(ord) => match ord {
                                        Ordering::Equal | Ordering::Less => true,
                                        Ordering::Greater => false,
                                    }
                                }
                            }
                            MapCaseKey::RangeFull(from, to) => {
                                match key_value.compare(&Number(*from)) {
                                    None => false,
                                    Some(ord) => match ord {
                                        Ordering::Less => false,
                                        Ordering::Equal | Ordering::Greater => {
                                            match key_value.compare(&Number(*to)) {
                                                None => false,
                                                Some(ord) => match ord {
                                                    Ordering::Equal | Ordering::Less => true,
                                                    Ordering::Greater => false,
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            MapCaseKey::RangeEq(num) => {
                                match key_value.compare(&Number(*num)) {
                                    None => false,
                                    Some(ord) => match ord {
                                        Ordering::Equal => true,
                                        Ordering::Less | Ordering::Greater => false,
                                    }
                                }
                            }
                        } {
                            matches = true;
                            break;
                        }
                    }

                    if matches {
                        return map_case.expression.eval(ctx, accessor);
                    }
                }
                Undefined
            }
            Expression::ForEachBlock { key, expr } => {
                let key_value = match key {
                    ForEachKey::Identifier(ident) => {
                        if !ctx.has_var(ident) {
                            return Failure(format!("For each expression invalid! Variable with name {ident} not found."));
                        }
                        let v = ctx.get_var(ident);
                        match v {
                            Named(_) | AnyValue | Failure(_) => v.clone(),
                            Undefined => Undefined,
                            _ => Failure(format!("Variable with name {ident} must be a Named list.")),
                        }
                    }
                    ForEachKey::VarAccess(name, field) => {
                        match ctx.variables.get(name) {
                            None => Failure(format!("Variable with name {name} not found.")),
                            Some(value) => match value {
                                AnyValue | Failure(_) => value.clone(),
                                Named(values) => {
                                    let filtered: Vec<(String, String)> = values.iter()
                                        .filter(|(k, _)| k == field)
                                        .map(|(k, v)| (k.clone(), v.clone()))
                                        .collect();
                                    if filtered.is_empty() {
                                        Undefined
                                    } else {
                                        Named(filtered)
                                    }
                                }
                                Undefined => Undefined,
                                _ => Failure(format!("Variable with name {name} must be a Named list.")),
                            },
                        }
                    }
                };

                let values = match key_value {
                    Named(key_value) => key_value,
                    Failure(_) => return key_value,
                    _ => Vec::new()
                };
                for (k, val) in values {
                    if let Some(key_var) = &expr.key_var {
                        ctx.set_var(key_var, EvalResult::Value(k));
                    }
                    if let Some(value_var) = &expr.value_var {
                        ctx.set_var(value_var, EvalResult::Value(val));
                    }
                    expr.expression.eval(ctx, accessor);
                }
                Undefined
            }
            Expression::Block(expressions) => {
                let mut result = Undefined;
                for expr in expressions {
                    result = expr.eval(ctx, accessor);
                }
                result
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PlaylistItem, PlaylistItemHeader, SeriesStreamProperties, StreamProperties, VideoStreamDetailProperties, VideoStreamProperties};

    #[test]
    fn test_mapper_dsl_eval() {
        let dsl = r#"
            coast = @Caption ~ "(?i)\b(EAST|WEST)\b"
            quality = @Caption ~ "(?i)\b([FUSL]?HD|SD|4K|1080p|720p|3840p)\b"
            quality = uppercase(quality)
            quality = map quality {
                       "SHD" => "SD",
                       "LHD" => "HD",
                       "720p" => "HD",
                       "1080p" => "FHD",
                       "4K" => "UHD",
                       "3840p" => "UHD",
                        _ => quality,
            }
            coast_quality = match {
                (coast, quality) => concat(capitalize(coast), " ", uppercase(quality)),
                coast => concat(capitalize(coast), " HD"),
                quality => concat("East ", uppercase(quality)),
            }
            @Caption = concat("US: TNT", " ", coast_quality)
            @Group = "United States - Entertainment"
    "#;

        let mapper = MapperScript::parse(dsl, None).expect("Parsing failed");
        println!("Program: {mapper:?}");
        let mut channels: Vec<PlaylistItem> = vec![
            ("D", "HD"), ("A", "FHD"), ("Z", ""), ("K", "HD"), ("B", "HD"), ("A", "HD"),
            ("K", "SHD"), ("C", "LHD"), ("L", "FHD"), ("R", "UHD"), ("T", "SD"), ("A", "FHD"),
        ].into_iter().map(|(name, quality)| PlaylistItem { header: PlaylistItemHeader { title: format!("Chanel {name} [{quality}]").into(), ..Default::default() } }).collect::<Vec<PlaylistItem>>();

        for pli in &mut channels {
            let mut accessor = ValueAccessor {
                pli,
                virtual_items: vec![],
                match_as_ascii: false,
            };
            mapper.eval(&mut accessor, None);
            println!("Result: {pli:?}");
        }


        // ctx.fields.insert("Caption".to_string(), "US: TNT East LHD bubble".to_string());
        //
        // for stmt in &program.statements {
        //     //let res = stmt.eval(&mut ctx);
        //     println!("Statement Result: {:?}", res);
        // }
        //
        // println!("Result variable: {:?}", ctx.variables.get("result"));
        // assert_eq!(ctx.variables.get("result").unwrap(), "US: TNT East HD");
    }

    #[test]
    fn test_complex() {
        let script = r#"
        print("LOCAL")
            coast = @Caption ~ "!COAST!"
            quality = uppercase(@Caption ~ "!QUALITY!")

            quality = map quality {
              "SHD" | "SD"           => "SD",
              "LHD" | "720P" | "HD"  => "HD",
              "FHD" | "1080P"        => "FHD",
              "UHD" | "4K" | "3840P" => "UHD",
              _ => quality,
            }

            coast_quality = match {
                (coast, quality) => concat(capitalize(coast), " ", uppercase(quality)),
                quality => uppercase(quality),
                _ => "HD",
            }

            network = uppercase(first(@Caption ~ "(?i)\b(CBS|NBC|FOX|ABC|PBS|CW|UNIVISION)\b"))
            station = map network {
              "CBS" => @Caption ~ "(?i)\b(WINK|WFOR)\b",
              "NBC" => @Caption ~ "(?i)\b(WBBH|WTVJ)\b",
              "FOX" => @Caption ~ "(?i)\b(WFTX|WSVM)\b",
              "ABC" => @Caption ~ "(?i)\b(WZVN|WPLG)\b",
              "PBS" => @Caption ~ "(?i)\b(WGCU|WPBT)\b",
              "CW" => @Caption ~ "(?i)\b(WINK|WSFL)\b",
              "UNIVISION" => @Caption ~ "(?i)\b(WUVF|WLTV)\b",
              _ => null,
            }

            match {
              station => {
                station = uppercase(station)
                @Caption = map station {
                  "WINK" => concat("!US_CBS_FM_PREFIX!", " ", coast_quality),
                  "WBBH" => concat("!US_NBC_FM_PREFIX!", " ", coast_quality),
                  "WFTX" => concat("!US_FOX_FM_PREFIX!", " ", coast_quality),
                  "WZVN" => concat("!US_ABC_FM_PREFIX!", " ", coast_quality),
                  "WGCU" => concat("!US_PBS_FM_PREFIX!", " ", coast_quality),
                  "WUVF" => concat("!US_UNIVISION_FM_PREFIX!", " ", coast_quality),

                  "WFOR" => concat("!US_CBS_MIA_PREFIX!", " ", coast_quality),
                  "WTVJ" => concat("!US_NBC_MIA_PREFIX!", " ", coast_quality),
                  "WSVM" => concat("!US_FOX_MIA_PREFIX!", " ", coast_quality),
                  "WPLG" => concat("!US_ABC_MIA_PREFIX!", " ", coast_quality),
                  "WPBT" => concat("!US_PBS_MIA_PREFIX!", " ", coast_quality),
                  "WSFL" => concat("!US_CW_MIA_PREFIX!", " ", coast_quality),
                  "WLTV" => concat("!US_UNIVISION_MIA_PREFIX!", " ", coast_quality),

                  _ => concat(network, " ", station, " ", coast_quality),
                }

                @Group = concat("🇺🇸 > USA - ", network, " Locals")
              }
            }
        "#;
        let mapper = MapperScript::parse(script, None).expect("Parsing failed");
        println!("Program: {mapper:?}")
    }

    #[test]
    fn test_mapper_format() {
        let dsl = r#"
            @Name = pad(1000, 10, 0);
            @Title = format("Hello {} how {}", "a", "b");
        "#;

        let mapper = MapperScript::parse(dsl, None).expect("Parsing failed");
        let mut channels: Vec<PlaylistItem> = vec![
            ("D", "HD"),
        ].into_iter().map(|(name, quality)| PlaylistItem { header: PlaylistItemHeader { title: format!("Chanel {name} [{quality}]").into(), ..Default::default() } }).collect::<Vec<PlaylistItem>>();

        for pli in &mut channels {
            let mut accessor = ValueAccessor {
                pli,
                virtual_items: vec![],
                match_as_ascii: false,
            };
            mapper.eval(&mut accessor, None);
            println!("Result: {pli:?}");
        }
    }

    #[test]
    fn test_mapper_add_favourite() {
        use crate::model::PlaylistItemType;
        let dsl = r#"
            add_favourite("My Favs");
        "#;

        let mapper = MapperScript::parse(dsl, None).expect("Parsing failed");

        // Test with Video (should work)
        let mut video = PlaylistItem {
            header: PlaylistItemHeader {
                name: "Movie 1".to_string().into(),
                item_type: PlaylistItemType::Video,
                ..Default::default()
            }
        };
        let mut accessor = ValueAccessor { pli: &mut video, virtual_items: vec![], match_as_ascii: false };
        mapper.eval(&mut accessor, None);
        assert_eq!(accessor.virtual_items.len(), 1);
        assert_eq!(&*accessor.virtual_items[0].1.header.group, "My Favs");

        // Test with SeriesInfo (should work)
        let mut series_info = PlaylistItem {
            header: PlaylistItemHeader {
                name: "Series 1".to_string().into(),
                item_type: PlaylistItemType::SeriesInfo,
                ..Default::default()
            }
        };
        let mut accessor = ValueAccessor { pli: &mut series_info, virtual_items: vec![], match_as_ascii: false };
        mapper.eval(&mut accessor, None);
        assert_eq!(accessor.virtual_items.len(), 1);

        // Test with Series episode (should NOT work)
        let mut episode = PlaylistItem {
            header: PlaylistItemHeader {
                name: "Episode 1".to_string().into(),
                item_type: PlaylistItemType::Series,
                ..Default::default()
            }
        };
        let mut accessor = ValueAccessor { pli: &mut episode, virtual_items: vec![], match_as_ascii: false };
        mapper.eval(&mut accessor, None);
        assert_eq!(accessor.virtual_items.len(), 0);
    }

    #[test]
    fn test_mapper_split_loop() {
        use crate::model::PlaylistItemType;
        let dsl = r#"
            genres = split(@Genre, ",")
            print(genres)
            genres.for_each((_, gen) => {
                    add_favourite(concat("Genre - ", gen))
                })
        "#;

        let mapper = MapperScript::parse(dsl, None).expect("Parsing failed");

        // Test with Video (should work)
        let mut video = PlaylistItem {
            header: PlaylistItemHeader {
                name: "Movie 1".to_string().into(),
                item_type: PlaylistItemType::Video,
                additional_properties: Some(StreamProperties::Video(Box::new(VideoStreamProperties {
                    details: Some(VideoStreamDetailProperties {
                        genre: Some("A, B, C".intern()),
                        ..VideoStreamDetailProperties::default()
                    }),
                    ..VideoStreamProperties::default()
                }))),
                ..Default::default()
            }
        };
        let mut accessor = ValueAccessor { pli: &mut video, virtual_items: vec![], match_as_ascii: false };
        mapper.eval(&mut accessor, None);
        assert_eq!(accessor.virtual_items.len(), 3);
        assert_eq!(&*accessor.virtual_items[0].1.header.group, "Genre - A");
        assert_eq!(&*accessor.virtual_items[1].1.header.group, "Genre - B");
        assert_eq!(&*accessor.virtual_items[2].1.header.group, "Genre - C");

        // Test with SeriesInfo (should work)
        let mut series_info = PlaylistItem {
            header: PlaylistItemHeader {
                name: "Series 1".to_string().into(),
                item_type: PlaylistItemType::SeriesInfo,
                additional_properties: Some(StreamProperties::Series(Box::new(SeriesStreamProperties {
                    genre: Some("A, B, C".intern()),
                    ..SeriesStreamProperties::default()
                }))),
                ..Default::default()
            }
        };
        let mut accessor = ValueAccessor { pli: &mut series_info, virtual_items: vec![], match_as_ascii: false };
        mapper.eval(&mut accessor, None);
        assert_eq!(accessor.virtual_items.len(), 3);
    }
}
