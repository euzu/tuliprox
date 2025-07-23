#![allow(clippy::empty_docs)]

use pest_derive::Parser;
use std::borrow::Cow;
use enum_iterator::all;
use indexmap::IndexSet;
use log::{error, log_enabled, trace, Level};
use pest::iterators::Pair;
use pest::Parser;
use std::cmp::Ordering;
use std::collections::HashMap;
use crate::error::{create_tuliprox_error_result, TuliproxError, TuliproxErrorKind};
use crate::info_err;
pub use crate::model::{ItemField, PatternTemplate, PlaylistItem, PlaylistItemType, FieldGetAccessor, FieldSetAccessor, TemplateValue};
use crate::utils::{DirectedGraph, CONSTANTS};

pub fn get_field_value(pli: &PlaylistItem, field: ItemField) -> String {
    let header = &pli.header;
    let value = match field {
        ItemField::Group => header.group.to_string(),
        ItemField::Name => header.name.to_string(),
        ItemField::Title => header.title.to_string(),
        ItemField::Url => header.url.to_string(),
        ItemField::Input => header.input_name.to_string(),
        ItemField::Type => header.item_type.to_string(),
        ItemField::Caption => if header.title.is_empty() { header.name.to_string() } else { header.title.to_string() },
    };
    value.to_string()
}

pub fn set_field_value(pli: &mut PlaylistItem, field: ItemField, value: String) -> bool {
    let header = &mut pli.header;
    match field {
        ItemField::Group => header.group = value,
        ItemField::Name => header.name = value,
        ItemField::Title => header.title = value,
        ItemField::Url => header.url = value,
        ItemField::Input => header.input_name = value,
        ItemField::Caption => {
            header.title.clone_from(&value);
            header.name = value;
        }
        ItemField::Type => {},
    }
    true
}

pub struct ValueProvider<'a> {
    pub pli: &'a PlaylistItem,
}

impl ValueProvider<'_> {
    pub fn get(&self, field: &str) -> Option<Cow<str>> {
        self.pli.header.get_field(field)
    }
}

pub struct ValueAccessor<'a> {
    pub pli: &'a mut PlaylistItem,
}

impl ValueAccessor<'_> {
    pub fn get(&self, field: &str) -> Option<Cow<str>> {
        self.pli.header.get_field(field)
    }

    pub fn set(&mut self, field: &str, value: &str) {
        if self.pli.header.set_field(field, value) {
            trace!("Property {field} set to {value}");
        } else {
            error!("Can't set unknown field {field} set to {value}");
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompiledRegex {
    pub restr: String,
    pub re: regex::Regex,
}
impl PartialEq for CompiledRegex {
    fn eq(&self, other: &Self) -> bool {
        self.restr == other.restr
    }
}

#[derive(Parser)]
#[grammar_inline = r#"
WHITESPACE = _{ " " | "\t" | "\r" | "\n"}
field = { ^"group" | ^"title" | ^"name" | ^"url" | ^"input" | ^"caption"}
and = { ^"and" }
or = { ^"or" }
not = { ^"not" }
regexp = @{ "\"" ~ ( "\\\"" | (!"\"" ~ ANY) )* ~ "\"" }
type_value = { ^"live" | ^"vod" | ^"series" }
type_comparison = { ^"type" ~ "=" ~ type_value }
field_comparison_value = _{ regexp }
field_comparison = { field ~ "~" ~ field_comparison_value }
comparison = { field_comparison | type_comparison }
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

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UnaryOperator {
    Not,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BinaryOperator {
    And,
    Or,
}

impl BinaryOperator {
    const OP_OR: &'static str = "OR";
    const OP_AND: &'static str = "AND";
}

impl std::fmt::Display for BinaryOperator {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", match *self {
            Self::Or => Self::OP_OR,
            Self::And => Self::OP_AND,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    Group(Box<Filter>),
    FieldComparison(ItemField, CompiledRegex),
    TypeComparison(ItemField, PlaylistItemType),
    UnaryExpression(UnaryOperator, Box<Filter>),
    BinaryExpression(Box<Filter>, BinaryOperator, Box<Filter>),
}

fn get_caption<'a>(provider: &'a ValueProvider<'a>, rewc: &'a CompiledRegex) -> (bool, Cow<'a, str>) {
    if let Some(value) = provider.get("title") {
        if rewc.re.is_match(&value) {
            return (true, value);
        }
    }

    if let Some(value) = provider.get("title") {
        if rewc.re.is_match(&value) {
            return (true, value);
        }
    }
    (false, Cow::Borrowed(""))
}

impl Filter {
    pub fn filter(&self, provider: &ValueProvider) -> bool {
        match self {
            Self::FieldComparison(field, rewc) => {
                let (is_match, value) = if field == &ItemField::Caption {
                    get_caption(provider, rewc)
               } else if let Some(value) = provider.get(field.as_str()) {
                    (rewc.re.is_match(&value), value)
                } else {
                    (false, Cow::Borrowed(""))
                };
                if log_enabled!(Level::Trace) {
                    if is_match {
                        trace!("Match found: {rewc:?} {} => {field}='{value}'", &rewc.restr);
                    } else {
                        trace!("Match failed: {self}: {rewc:?} {} => {field}='{value}'", &rewc.restr);
                    }
                }
                is_match
            }
            Self::TypeComparison(field, item_type) => {
                if let Some(value) = provider.get(field.as_str()) {
                    get_filter_item_type(&value).is_some_and(|pli_type| {
                        let is_match = pli_type.eq(item_type);
                        if log_enabled!(Level::Trace) {
                            if is_match {
                                trace!("Match found: {field:?} {value}");
                            } else {
                                trace!("Match failed: {self}: {field:?} {value}");
                            }
                        }
                        is_match
                    })
                } else {
                    false
                }
            }
            Self::Group(expr) => expr.filter(provider),
            Self::UnaryExpression(op, expr) => match op {
                UnaryOperator::Not => !expr.filter(provider),
            },
            Self::BinaryExpression(left, op, right) => match op {
                BinaryOperator::And => {
                    left.filter(provider) && right.filter(provider)
                }
                BinaryOperator::Or => {
                    left.filter(provider) || right.filter(provider)
                }
            },
        }
    }
}

impl Filter {
    const LIVE: &'static str = "live";
    const VOD: &'static str = "vod";
    const SERIES: &'static str = "series";
    const UNSUPPORTED: &'static str = "unsupported";
}

impl std::fmt::Display for Filter {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::FieldComparison(field, rewc) => {
                write!(f, "{} ~ \"{}\"", field, String::from(&rewc.restr))
            }
            Self::TypeComparison(field, item_type) => {
                write!(f, "{} = {}", field, match item_type {
                    PlaylistItemType::Live => Self::LIVE,
                    PlaylistItemType::Video => Self::VOD,
                    PlaylistItemType::Series | PlaylistItemType::SeriesInfo => Self::SERIES, // yes series-info is handled as series in filter
                    _ => Self::UNSUPPORTED
                })
            }
            Self::Group(stmt) => {
                write!(f, "({stmt})")
            }
            Self::UnaryExpression(op, expr) => {
                let flt = match op {
                    UnaryOperator::Not => format!("NOT {expr}"),
                };
                write!(f, "{flt}")
            }
            Self::BinaryExpression(left, op, right) => {
                write!(f, "{left} {op} {right}")
            }
        }
    }
}

fn get_parser_item_field(expr: &Pair<Rule>) -> Result<ItemField, TuliproxError> {
    if expr.as_rule() == Rule::field {
        let field_text = expr.as_str();
        for item in all::<ItemField>() {
            if field_text.eq_ignore_ascii_case(item.to_string().as_str()) {
                return Ok(item);
            }
        }
    }
    create_tuliprox_error_result!(TuliproxErrorKind::Info, "unknown field: {}", expr.as_str())
}

fn get_parser_regexp(
    expr: &Pair<Rule>,
    templates: Option<&Vec<PatternTemplate>>,
) -> Result<CompiledRegex, TuliproxError> {
    if expr.as_rule() == Rule::regexp {
        let mut parsed_text = String::from(expr.as_str());
        parsed_text.pop();
        parsed_text.remove(0);
        let regstr = apply_templates_to_pattern_single(&parsed_text, templates)?;
        let re = regex::Regex::new(regstr.as_str());
        if re.is_err() {
            return create_tuliprox_error_result!(TuliproxErrorKind::Info, "cant parse regex: {}", regstr);
        }
        let regexp = re.unwrap();
        if log_enabled!(Level::Trace) {
            trace!("Created regex: {regstr}");
        }
        return Ok(CompiledRegex {
            restr: regstr,
            re: regexp,
        });
    }
    create_tuliprox_error_result!(TuliproxErrorKind::Info, "unknown field: {}", expr.as_str())
}

fn get_parser_field_comparison(
    expr: Pair<Rule>,
    templates: Option<&Vec<PatternTemplate>>,
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

fn get_filter_item_type(text_item_type: &str) -> Option<PlaylistItemType> {
    if text_item_type.eq_ignore_ascii_case("live") {
        Some(PlaylistItemType::Live)
    } else if text_item_type.eq_ignore_ascii_case("vod")
        || text_item_type.eq_ignore_ascii_case("video")
        || text_item_type.eq_ignore_ascii_case("movie")
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
    item_type.map_or_else(|| create_tuliprox_error_result!(TuliproxErrorKind::Info, "cant parse item type: {text_item_type}"),
                          |itype| Ok(Filter::TypeComparison(ItemField::Type, itype)))
}

macro_rules! handle_expr {
    ($bop: expr, $uop: expr, $stmts: expr, $exp: expr) => {{
        let result = match $bop {
            Some(binop) => {
                let lhs = $stmts.pop().unwrap();
                $bop = None;
                Filter::BinaryExpression(Box::new(lhs), binop.clone(), Box::new($exp))
            }
            _ => match $uop {
                Some(unop) => {
                    $uop = None;
                    Filter::UnaryExpression(unop.clone(), Box::new($exp))
                }
                _ => $exp,
            },
        };
        $stmts.push(result);
    }};
}

fn get_parser_expression(
    expr: Pair<Rule>,
    templates: Option<&Vec<PatternTemplate>>,
    errors: &mut Vec<String>,
) -> Result<Filter, String> {
    let mut stmts = Vec::with_capacity(128);
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
            Rule::comparison | Rule::expr => {
                match get_parser_expression(pair, templates, errors) {
                    Ok(expr) => handle_expr!(bop, uop, stmts, expr),
                    Err(err) => return Err(err),
                }
            }
            Rule::expr_group => {
                match get_parser_expression(pair.into_inner().next().unwrap(), templates, errors) {
                    Ok(expr) => handle_expr!(bop, uop, stmts, Filter::Group(Box::new(expr))),
                    Err(err) => return Err(err),
                }
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
        _ => create_tuliprox_error_result!(
            TuliproxErrorKind::Info,
            "Unknown binary operator {}",
            expr.as_str()
        ),
    }
}

pub fn get_filter(
    filter_text: &str,
    templates: Option<&Vec<PatternTemplate>>,
) -> Result<Filter, TuliproxError> {
    let source = apply_templates_to_pattern_single(filter_text, templates)?;

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
                                Rule::expr => {
                                    match get_parser_expression(expr, templates, &mut errors) {
                                        Ok(expr) => {
                                            match &op {
                                                Some(binop) => {
                                                    result = Some(Filter::BinaryExpression(
                                                        Box::new(result.unwrap()),
                                                        *binop,
                                                        Box::new(expr),
                                                    ));
                                                    op = None;
                                                }
                                                _ => result = Some(expr),
                                            }
                                        }
                                        Err(err) => errors.push(err),
                                    }
                                }
                                Rule::bool_op => {
                                    match get_parser_binary_op(&expr.into_inner().next().unwrap()) {
                                        Ok(binop) => {
                                            op = Some(binop);
                                        }
                                        Err(err) => {
                                            errors.push(err.to_string());
                                        }
                                    }
                                }
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
                errors.push(format!("Unable to parse filter: {}", &filter_text));
                return Err(info_err!(errors.join("\n")));
            }

            result.map_or_else(
                || {
                    create_tuliprox_error_result!(
                        TuliproxErrorKind::Info,
                        "Unable to parse filter: {}",
                        &filter_text
                    )
                },
                Ok,
            )
        }
        Err(err) => create_tuliprox_error_result!(TuliproxErrorKind::Info, "{}", err),
    }
}

fn build_dependency_graph(
    templates: &Vec<PatternTemplate>,
) -> Result<DirectedGraph<String>, TuliproxError> {
    let mut graph = DirectedGraph::<String>::new();
    for template in templates {
        graph.add_node(&template.name);
        let mut handle_template_value = |value| {
            CONSTANTS.re_template_var
                .captures_iter(value)
                .filter(|caps| caps.len() > 1)
                .filter_map(|caps| caps.get(1))
                .map(|caps| String::from(caps.as_str()))
                .for_each(|e| {
                    graph.add_node(&e);
                    graph.add_edge(&template.name, &e);
                });
        };
        match &template.value {
            TemplateValue::Single(value) => handle_template_value(value),
            TemplateValue::Multi(values) => values.iter().for_each(|value| handle_template_value(value)),
        }
    }
    let cycles = graph.find_cycles();
    for cyclic in &cycles {
        error!(
            "Cyclic template dependencies detected [{}]",
            cyclic.join(" <-> ")
        );
    }
    if !cycles.is_empty() {
        return create_tuliprox_error_result!(
            TuliproxErrorKind::Info,
            "Cyclic dependencies in templates detected!"
        );
    }
    Ok(graph)
}

pub fn prepare_templates(templates: &mut Vec<PatternTemplate>) -> Result<Vec<PatternTemplate>, TuliproxError> {

    let graph = build_dependency_graph(templates)?;
    let mut template_values = HashMap::new();
    let mut template_map = HashMap::with_capacity(templates.len());

    for item in templates.iter_mut() {
        item.prepare();
        template_values.insert(item.name.clone(), item.value.clone());
        template_map.insert(item.name.clone(), item);
    }

    if let Some(dependencies) = graph.get_dependencies() {
        if let Some(sorted) = graph.topological_sort() {
            for template_name in sorted {
                if let Some(depends_on) = dependencies.get(&template_name) {
                    let mut templ_value = template_values.get(&template_name).unwrap().clone();
                    for dep_templ_name in depends_on {
                        let dep_value = template_values.get(dep_templ_name).ok_or_else(|| info_err!(format!("Failed to load template {dep_templ_name}")))?;
                        let dep_templ = template_map.get_mut(dep_templ_name).unwrap();
                        templ_value = match dep_value {
                            TemplateValue::Single(dep_val) => {
                                match templ_value {
                                    TemplateValue::Single(templ_val) => {
                                        if templ_val.contains(&dep_templ.placeholder) {
                                            TemplateValue::Single(templ_val.replace(&dep_templ.placeholder, dep_val.as_str()))
                                        } else {
                                            TemplateValue::Single(templ_val)
                                        }
                                    }
                                    TemplateValue::Multi(templ_vals) => {
                                        let mut new_values = vec![];
                                        for val in templ_vals {
                                            if val.contains(&dep_templ.placeholder) {
                                                new_values.push(val.replace(&dep_templ.placeholder, dep_val.as_str()));
                                            } else {
                                                new_values.push(val);
                                            }
                                        }
                                        TemplateValue::Multi(new_values)
                                    }
                                }
                            }
                            TemplateValue::Multi(dep_vals) => {
                                match templ_value {
                                    TemplateValue::Single(templ_val) => {
                                        let mut new_values = vec![];
                                        for dep_val in dep_vals {
                                            if templ_val.contains(&dep_templ.placeholder) {
                                                new_values.push(templ_val.replace(&dep_templ.placeholder, dep_val.as_str()));
                                            } else {
                                                new_values.push(templ_val.clone());
                                            }
                                        }
                                        TemplateValue::Multi(new_values)
                                    }
                                    TemplateValue::Multi(templ_vals) => {
                                        let mut new_values = vec![];
                                        for dep_val in dep_vals {
                                            for templ_val in &templ_vals {
                                                if templ_val.contains(&dep_templ.placeholder) {
                                                    new_values.push(templ_val.replace(&dep_templ.placeholder, dep_val.as_str()));
                                                } else {
                                                    new_values.push(templ_val.clone());
                                                }
                                            }
                                        }
                                        TemplateValue::Multi(new_values)
                                    }
                                }
                            }
                        };
                    }
                    template_values.insert(template_name.clone(), templ_value);
                }
            }

            for (k, v) in template_values {
                let template = template_map.get_mut(&k).unwrap();
                template.value = v;
            }
        }
    }
    let result: Vec<PatternTemplate> = template_map.iter_mut().map(|(_, t)| t.clone()).collect();
    Ok(result)
}

pub fn apply_templates_to_pattern(
    pattern: &str,
    templates_list: Option<&Vec<PatternTemplate>>,
    allow_multi: bool,
) -> Result<TemplateValue, TuliproxError> {
    let mut new_pattern = TemplateValue::Single(pattern.to_string());

    if let Some(templates) = templates_list {
        for template in templates {
            match &template.value {
                TemplateValue::Single(val) => {
                    match new_pattern {
                        TemplateValue::Single(ref mut pat) => {
                            let replaced = pat.replace(&template.placeholder, val);
                            if replaced != *pat {
                                *pat = replaced;
                            }
                        }
                        TemplateValue::Multi(ref mut pats) => {
                            for pat in pats.iter_mut() {
                                let replaced = pat.replace(&template.placeholder, val);
                                if replaced != *pat {
                                    *pat = replaced;
                                }
                            }
                        }
                    }
                }

                TemplateValue::Multi(ref multi_vals) => {
                    new_pattern = match &new_pattern {
                        TemplateValue::Single(pat) => {
                            let mut new_values = IndexSet::new();
                            for val in multi_vals {
                                if pat.contains(&template.placeholder) {
                                    new_values.insert(pat.replace(&template.placeholder, val));
                                } else {
                                    new_values.insert(pat.clone());
                                }
                            }
                            TemplateValue::Multi(new_values.into_iter().collect())
                        }
                        TemplateValue::Multi(pats) => {
                            let mut new_values = IndexSet::new();
                            for val in multi_vals {
                                for pat in pats {
                                    if pat.contains(&template.placeholder) {
                                        new_values.insert(pat.replace(&template.placeholder, val));
                                    } else {
                                        new_values.insert(pat.clone());
                                    }
                                }
                            }
                            TemplateValue::Multi(new_values.into_iter().collect())
                        }
                    };
                }
            }
        }
    }

    if !allow_multi {
        match &new_pattern {
            TemplateValue::Single(_) => {}
            TemplateValue::Multi(multi_vals) => {
                match multi_vals.len().cmp(&1) {
                    Ordering::Less => {
                        return create_tuliprox_error_result!(TuliproxErrorKind::Info, "Empty multi value templates are not supported for pattern! {pattern}");
                    }
                    Ordering::Equal => {
                        new_pattern = TemplateValue::Single(multi_vals.first().unwrap().to_owned());
                    }
                    Ordering::Greater => {
                        return create_tuliprox_error_result!(TuliproxErrorKind::Info, "Multi value templates are not supported for pattern! {pattern}");
                    }
                }
            }
        }
    }

    Ok(new_pattern)
}

pub fn apply_templates_to_pattern_single(pattern: &str, templates: Option<&Vec<PatternTemplate>>) -> Result<String, TuliproxError> {
    match apply_templates_to_pattern(pattern, templates, false)? {
        TemplateValue::Single(value) => Ok(value),
        TemplateValue::Multi(_) => create_tuliprox_error_result!(TuliproxErrorKind::Info, "Multi value templates are not supported for pattern!"),
    }
}

#[cfg(test)]
mod tests {
    use crate::foundation::filter::{get_filter, ValueProvider};
    use crate::model::{PlaylistItem, PlaylistItemHeader};
    use crate::utils::CONSTANTS;

    fn create_mock_pli(name: &str, group: &str) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                name: name.to_string(),
                group: group.to_string(),
                ..Default::default()
            },
        }
    }

    #[test]
    fn test_filter_1() {
        let flt1 = r#"(Group ~ "A" OR Group ~ "B") AND (Name ~ "C" OR Name ~ "D" OR Name ~ "E") OR (NOT (Title ~ "F") AND NOT Title ~ "K")"#;
        match get_filter(flt1, None) {
            Ok(filter) => {
                assert_eq!(format!("{filter}"), flt1);
            }
            Err(e) => {
                panic!("{}", e)
            }
        }
    }

    #[test]
    fn test_filter_2() {
        let flt2 = r#"Group ~ "d" AND ((Name ~ "e" AND NOT ((Name ~ "c" OR Name ~ "f"))) OR (Name ~ "a" OR Name ~ "b"))"#;
        match get_filter(flt2, None) {
            Ok(filter) => {
                assert_eq!(format!("{filter}"), flt2);
            }
            Err(e) => {
                panic!("{}", e)
            }
        }
    }

    #[test]
    fn test_filter_3() {
        let flt = r#"Group ~ "d" AND ((Name ~ "e" AND NOT ((Name ~ "c" OR Name ~ "f"))) OR (Name ~ "a" OR Name ~ "b")) AND (Type = vod)"#;
        match get_filter(flt, None) {
            Ok(filter) => {
                assert_eq!(format!("{filter}"), flt);
            }
            Err(e) => {
                panic!("{}", e)
            }
        }
    }

    #[test]
    fn test_filter_4() {
        let flt = r#"NOT (Name ~ ".*24/7.*" AND Group ~ "^US.*")"#;
        match get_filter(flt, None) {
            Ok(filter) => {
                assert_eq!(format!("{filter}"), flt);
                let channels = vec![
                    create_mock_pli("24/7: Cars", "FR Channels"),
                    create_mock_pli("24/7: Cars", "US Channels"),
                    create_mock_pli("Entertainment", "US Channels"),
                ];
                let filtered: Vec<&PlaylistItem> = channels
                    .iter()
                    .filter(|&chan| {
                        let provider = ValueProvider {
                            pli: chan,
                        };
                        filter.filter(&provider)
                    })
                    .collect();
                assert_eq!(filtered.len(), 2);
                assert!(
                    filtered.iter().any(|&chan| {
                        let group = chan.header.group.to_string();
                        let name = chan.header.name.to_string();
                        name.eq("24/7: Cars") && group.eq("FR Channels")
                    })
                );
                assert!(
                    filtered.iter().any(|&chan| {
                        let group = chan.header.group.to_string();
                        let name = chan.header.name.to_string();
                        name.eq("Entertainment") && group.eq("US Channels")
                    })
                );
                assert!(
                    !filtered.iter().any(|&chan| {
                        let group = chan.header.group.to_string();
                        let name = chan.header.name.to_string();
                        name.eq("24/7: Cars") && group.eq("US Channels")
                    })
                );
            }
            Err(e) => {
                panic!("{}", e)
            }
        }
    }

    #[test]
    fn test_filter_5() {
        let flt = r#"NOT (Name ~ "NC" OR Group ~ "GA") AND (Name ~ "NA" AND Group ~ "GA") OR (Name ~ "NB" AND Group ~ "GB")"#;
        match get_filter(flt, None) {
            Ok(filter) => {
                assert_eq!(format!("{filter}"), flt);
                let channels = vec![
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
                        let provider = ValueProvider {
                            pli: chan,
                        };
                        filter.filter(&provider)
                    })
                    .collect();
                assert_eq!(filtered.len(), 1);
            }
            Err(e) => {
                panic!("{}", e)
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
                panic!("{}", e)
            }
        }
    }

    #[test]
    fn test_filter_7() {
        let flt = r#"NOT (Name ~ ".*24/7.*")"#;
        match get_filter(flt, None) {
            Ok(filter) => {
                assert_eq!(format!("{filter}"), flt);
                let channels = vec![
                    create_mock_pli("24/7: Cars", "FR Channels"),
                    create_mock_pli("24/7: Cars", "US Channels"),
                    create_mock_pli("Entertainment", "US Channels"),
                ];
                let filtered: Vec<&PlaylistItem> = channels
                    .iter()
                    .filter(|&chan| {
                        let provider = ValueProvider {
                            pli: chan,
                        };
                        filter.filter(&provider)
                    })
                    .collect();
                assert_eq!(filtered.len(), 1);
                assert!(
                    filtered.iter().any(|&chan| {
                        let group = chan.header.group.to_string();
                        let name = chan.header.name.to_string();
                        name.eq("Entertainment") && group.eq("US Channels")
                    })
                );
            }
            Err(e) => {
                panic!("{}", e)
            }
        }
    }
}
