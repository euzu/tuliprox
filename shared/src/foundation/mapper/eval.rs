use super::{
    AssignmentTarget, BuiltInFunction, ExprId, Expression, ForEachKey, MapCaseKey, MapKey, MapperContext, MapperScript,
    MappingDiagnostic, MappingOutcome, MatchCaseKey, RegexSource, ValueAccessor,
};
use crate::{
    foundation::mapper::EvalResult::{AnyValue, Failure, Named, Number, Undefined, Value},
    model::{PatternTemplate, PlaylistItemType},
    utils::{deunicode_string, normalize_spaces, slugify, Capitalize, Internable},
};
use log::trace;
use regex::Regex;
use std::{borrow::Cow, cmp::Ordering, collections::HashMap};

impl MapperScript {
    pub fn eval(&self, setter: &mut ValueAccessor, templates: Option<&[PatternTemplate]>) -> MappingOutcome {
        let initial_change_count = setter.changed_fields.len();
        let initial_item_count = setter.virtual_items.len();
        let ctx = &mut MapperContext::new(&self.expressions, templates);
        let diagnostics = self.eval_with_context(ctx, setter);
        MappingOutcome {
            changed_fields: setter.changed_fields[initial_change_count..].to_vec(),
            emitted_items: setter.virtual_items.len().saturating_sub(initial_item_count),
            diagnostics,
        }
    }

    pub(super) fn eval_with_context(
        &self,
        ctx: &mut MapperContext,
        setter: &mut ValueAccessor,
    ) -> Vec<MappingDiagnostic> {
        self.statements
            .iter()
            .enumerate()
            .filter_map(|(statement, stmt)| {
                stmt.eval(ctx, setter).map(|message| MappingDiagnostic { statement, message })
            })
            .collect()
    }

    pub fn get_expr_by_id(&self, id: usize) -> Option<&Expression> { self.expressions.get(id) }
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

pub(super) fn to_number(value: &str) -> EvalResult {
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

fn format_number(num: f64) -> String {
    let epsilon = 1e-3; // = 0.001

    if num.fract().abs() < epsilon {
        format!("{}", num as i64)
    } else {
        format!("{num}")
    }
}

fn compare_tuple_vec(a: &[(String, String)], b: &[(String, String)]) -> bool {
    fn to_map(vec: &[(String, String)]) -> HashMap<&str, &str> {
        vec.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
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
    pub(super) fn compare(&self, other: &EvalResult) -> Option<Ordering> {
        match (self, other) {
            (AnyValue, _) | (_, AnyValue) => Some(Ordering::Equal),
            (Value(a), Value(b)) => Some(a.cmp(b)),
            (Number(a), Value(b)) => cmp_number(*a, b),
            (Value(a), Number(b)) => match cmp_number(*b, a) {
                None => None,
                Some(ord) => match ord {
                    Ordering::Less => Some(Ordering::Greater),
                    Ordering::Equal => Some(Ordering::Equal),
                    Ordering::Greater => Some(Ordering::Less),
                },
            },
            (Number(a), Number(b)) => Some(compare_number(*a, *b)),
            (Named(a), Named(b)) => {
                if compare_tuple_vec(a, b) {
                    Some(Ordering::Equal)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    pub fn is_error(&self) -> bool { matches!(self, Failure(_)) }

    fn is_usable(&self) -> bool {
        match self {
            Value(value) => !value.trim().is_empty(),
            Number(_) => true,
            Named(values) => !values.is_empty(),
            Undefined | AnyValue | Failure(_) => false,
        }
    }
}

fn function_failure(name: &BuiltInFunction, value: &EvalResult) -> EvalResult {
    Failure(format!("Function '{name:?}' failed: {}", if let Failure(msg) = value { msg } else { "Unknown error" }))
}

fn concat_args(args: &[EvalResult]) -> Vec<Cow<'_, str>> {
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

fn title_case_args(text: &EvalResult, acronyms: &[&str]) -> EvalResult {
    match text {
        Value(_) | Number(_) | Named(_) => Value(
            concat_args(std::slice::from_ref(text))
                .iter()
                .map(|s| s.title_case_with_acronyms(acronyms))
                .collect::<Vec<_>>()
                .join(" "),
        ),
        Undefined | AnyValue | Failure(_) => Undefined,
    }
}

fn pad_text_value(arg: &EvalResult) -> Option<String> {
    match arg {
        Number(value) => Some(value.to_string()),
        _ => text_arg(arg).map(String::from),
    }
}

/// Text view of an argument for a positional parameter; a `Named` value contributes its first entry.
fn text_arg(arg: &EvalResult) -> Option<&str> {
    match arg {
        Value(value) => Some(value.as_str()),
        Named(values) => values.first().map(|(_key, val)| val.as_str()),
        Undefined | Number(_) | AnyValue | Failure(_) => None,
    }
}

/// String view of an argument for a format placeholder; numbers keep their formatted representation.
fn format_arg(arg: &EvalResult) -> Option<String> {
    match arg {
        Number(value) => Some(format_number(*value)),
        _ => text_arg(arg).map(String::from),
    }
}

fn pad_width_value(arg: &EvalResult) -> Option<usize> {
    match arg {
        Number(value) => {
            Some(if value.is_nan() || value.is_infinite() { 0 } else { value.abs().min(usize::MAX as f64) as usize })
        }
        Value(value) => Some(value.parse::<usize>().ok().unwrap_or(0)),
        Named(values) => Some(values.first().and_then(|(_key, val)| val.parse::<usize>().ok()).unwrap_or(0)),
        Undefined | AnyValue | Failure(_) => None,
    }
}

/// Builds the padded value from arguments that keep their declared positions.
///
/// `pad(text, width, fill[, align])` is positional, so an argument that evaluated to `Undefined` must leave a gap
/// instead of shifting the following arguments into earlier parameter slots.
fn pad_values(args: &[EvalResult]) -> EvalResult {
    let Some(text) = args.first().and_then(pad_text_value) else {
        return Undefined;
    };
    let Some(width) = args.get(1).and_then(pad_width_value) else {
        return Undefined;
    };
    let Some(fill) = args.get(2).and_then(pad_text_value) else {
        return Undefined;
    };

    if width <= text.len() {
        return Value(text);
    }

    let fill_str = fill.chars().next().unwrap_or(' ').to_string();
    let count = width - text.len();
    match args.get(3).and_then(pad_text_value).as_deref() {
        Some("^") => {
            let left = count / 2;
            let right = count - left;
            Value(format!("{}{text}{}", fill_str.repeat(left), fill_str.repeat(right)))
        }
        Some("<") => Value(format!("{text}{}", fill_str.repeat(count))),
        _ => Value(format!("{}{text}", fill_str.repeat(count))),
    }
}

/// Replaces `pattern` with `substitution` in `text`, keeping every parameter in its declared position.
fn replace_values(args: &[EvalResult]) -> EvalResult {
    let (Some(text), Some(pattern), Some(substitution)) =
        (args.first().and_then(text_arg), args.get(1).and_then(text_arg), args.get(2).and_then(text_arg))
    else {
        // An unknown pattern or substitution leaves the first argument as it was.
        return args.first().cloned().unwrap_or(Undefined);
    };
    Value(text.replace(pattern, substitution))
}

/// Substitutes the positional arguments for the `{}` placeholders of the format string.
fn format_values(args: &[EvalResult]) -> EvalResult {
    let Some(format_string) = args.first().and_then(text_arg) else {
        return Undefined;
    };

    let mut formatted = String::with_capacity(format_string.len());
    let mut arguments = args.iter().skip(1);
    let mut chars = format_string.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '{' && chars.peek() == Some(&'}') {
            chars.next();
            // An unknown argument keeps its placeholder, so the later arguments stay in their own positions.
            match arguments.next().and_then(format_arg) {
                Some(argument) => formatted.push_str(&argument),
                None => formatted.push_str("{}"),
            }
        } else {
            formatted.push(ch);
        }
    }
    Value(formatted)
}

/// Evaluates function arguments without compacting them, so each argument keeps its declared position.
fn eval_args_in_place(
    name: &BuiltInFunction,
    args: &[ExprId],
    ctx: &mut MapperContext,
    accessor: &mut ValueAccessor,
) -> Result<Vec<EvalResult>, EvalResult> {
    let mut values = Vec::with_capacity(args.len());
    for &arg in args {
        let value = arg.eval(ctx, accessor);
        if value.is_error() {
            return Err(function_failure(name, &value));
        }
        values.push(value);
    }
    Ok(values)
}

pub(super) fn eval_regex(source: &str, re_pattern: &Regex, match_as_ascii: bool) -> EvalResult {
    let source = if match_as_ascii { deunicode_string(source) } else { Cow::Borrowed(source) };
    let mut values = vec![];

    let Some(caps) = re_pattern.captures(&source) else {
        return Undefined;
    };

    for i in 1..caps.len() {
        if let Some(value) = caps.get(i) {
            values.push((i.to_string(), value.as_str().to_string()));
        }
    }

    for name in re_pattern.capture_names().flatten() {
        if let Some(value) = caps.name(name) {
            values.push((name.to_string(), value.as_str().to_string()));
        }
    }

    match values.len() {
        0 => Undefined,
        1 => values.pop().map_or(Undefined, |(_, value)| Value(value)),
        _ => Named(values),
    }
}

fn matches_text(value: &EvalResult, expected: &str) -> bool {
    match value {
        AnyValue => true,
        Value(actual) => actual == expected,
        Number(actual) => match_number(*actual, expected),
        Undefined | Named(_) | Failure(_) => false,
    }
}

impl Expression {
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
            Expression::VarAccess(name, field) => match ctx.variables.get(name) {
                None => Failure(format!("Variable with name {name} not found.")),
                Some(value) => match value {
                    Undefined => Undefined,
                    Value(value) if field == "1" => Value(value.clone()),
                    Number(_) | Value(_) => Failure(format!("Variable with name {name} has no fields.")),
                    Named(values) => {
                        for (key, val) in values {
                            if key == field {
                                return Value(val.clone());
                            }
                        }
                        Failure(format!("Variable with name {name} has no field {field}."))
                    }
                    AnyValue | Failure(_) => value.clone(),
                },
            },
            Expression::StringLiteral(s) => Value(s.clone()),
            Expression::ArrayLiteral(values) => {
                Named(values.iter().enumerate().map(|(i, value)| (i.to_string(), value.clone())).collect())
            }
            Expression::NumberLiteral(num) => Number(*num),
            Expression::RegexExpr { field, pattern: _pattern, re_pattern } => match field {
                RegexSource::Identifier(ident) => match ctx.get_var(ident) {
                    Value(text) => eval_regex(text, re_pattern, accessor.match_as_ascii),
                    _ => Undefined,
                },
                RegexSource::Field(field) => accessor
                    .get(field)
                    .map_or(Undefined, |value| eval_regex(&value, re_pattern, accessor.match_as_ascii)),
            },
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
                if matches!(name, BuiltInFunction::Coalesce) {
                    for arg in args {
                        let value = arg.eval(ctx, accessor);
                        if value.is_error() {
                            return function_failure(name, &value);
                        }
                        if value.is_usable() {
                            return value;
                        }
                    }
                    return Undefined;
                }

                if matches!(name, BuiltInFunction::TitleCase) {
                    // Evaluate the arguments here instead of using the compacted list below, so the acronym options keep
                    // their original position and an undefined text argument cannot turn the options into the text.
                    let Some(text_expr) = args.first() else {
                        return Undefined;
                    };
                    let text = text_expr.eval(ctx, accessor);
                    if text.is_error() {
                        return function_failure(name, &text);
                    }
                    let Some(options_expr) = args.get(1) else {
                        return title_case_args(&text, &[]);
                    };
                    let options = options_expr.eval(ctx, accessor);
                    if options.is_error() {
                        return function_failure(name, &options);
                    }
                    return match &options {
                        Named(values) => {
                            let acronyms = values.iter().map(|(_, value)| value.as_str()).collect::<Vec<_>>();
                            title_case_args(&text, &acronyms)
                        }
                        _ => title_case_args(&text, &[]),
                    };
                }
                // Pad, Replace, and Format address their arguments by position. They evaluate the arguments in place
                // instead of the compacted list below, so an argument that evaluated to undefined keeps its slot
                // instead of pulling the later arguments forward.
                let positional: Option<fn(&[EvalResult]) -> EvalResult> = match name {
                    BuiltInFunction::Pad => Some(pad_values),
                    BuiltInFunction::Replace => Some(replace_values),
                    BuiltInFunction::Format => Some(format_values),
                    _ => None,
                };
                if let Some(evaluate) = positional {
                    return match eval_args_in_place(name, args, ctx, accessor) {
                        Ok(values) => evaluate(&values),
                        Err(failure) => failure,
                    };
                }

                let mut evaluated_args: Vec<EvalResult> = args.iter().map(|a| a.eval(ctx, accessor)).collect();
                for arg in &evaluated_args {
                    if arg.is_error() {
                        return function_failure(name, arg);
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
                        BuiltInFunction::Trim => Value(
                            concat_args(&evaluated_args)
                                .iter()
                                .map(|s| s.trim())
                                .collect::<Vec<_>>()
                                .join(" ")
                                .trim()
                                .to_string(),
                        ),
                        BuiltInFunction::Lowercase => Value(concat_args(&evaluated_args).join(" ").to_lowercase()),
                        BuiltInFunction::Capitalize => Value(
                            concat_args(&evaluated_args)
                                .iter()
                                .map(Capitalize::capitalize)
                                .collect::<Vec<_>>()
                                .join(" "),
                        ),
                        BuiltInFunction::Slug => {
                            Value(concat_args(&evaluated_args).iter().map(|s| slugify(s)).collect::<Vec<_>>().join(" "))
                        }
                        BuiltInFunction::NormalizeSpaces => Value(
                            concat_args(&evaluated_args)
                                .iter()
                                .map(|s| normalize_spaces(s))
                                .collect::<Vec<_>>()
                                .join(" "),
                        ),
                        BuiltInFunction::Deunicode => Value(
                            concat_args(&evaluated_args)
                                .iter()
                                .map(|s| deunicode::deunicode(s))
                                .collect::<Vec<_>>()
                                .join(" "),
                        ),
                        // TitleCase evaluates its arguments above to keep the acronym options in their original position.
                        BuiltInFunction::TitleCase => Undefined,
                        // Coalesce short-circuits argument evaluation above; this arm keeps the match exhaustive.
                        BuiltInFunction::Coalesce => Undefined,
                        BuiltInFunction::Split => {
                            let string = extract_evaluated_arg_value!(evaluated_args, 0);
                            let pattern = extract_evaluated_arg_value!(evaluated_args, 1);

                            if let (Some(text), Some(pat)) = (string, pattern) {
                                match crate::model::REGEX_CACHE.get_or_compile(pat) {
                                    Ok(re) => Named(
                                        re.split(text)
                                            .enumerate()
                                            .map(|(i, s)| (i.to_string(), s.trim().to_string()))
                                            .collect(),
                                    ),
                                    Err(e) => Failure(format!("Invalid regex pattern '{pat}': {e}")),
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
                                Value(value) => to_number(value),
                                _ => evaluated_arg.clone(),
                            }
                        }
                        BuiltInFunction::First => match evaluated_args.first() {
                            Some(value) => match value {
                                Named(values) => match values.first() {
                                    None => Undefined,
                                    Some((_key, val)) => Value(val.clone()),
                                },
                                _ => value.clone(),
                            },
                            None => Undefined,
                        },
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
                        // Pad, Replace, and Format evaluate their arguments in place above so that every parameter
                        // keeps the position it was written in.
                        BuiltInFunction::Pad | BuiltInFunction::Replace | BuiltInFunction::Format => Undefined,
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
                                    return Failure(format!(
                                        "Match case invalid! Variable with name {ident} not found."
                                    ));
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
                            Value(_) | Number(_) | Named(_) | AnyValue => match_count += 1,
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
                        let mut val = ctx.get_var(ident).clone();
                        if accessor.match_as_ascii {
                            if let Value(text) = val {
                                val = Value(deunicode_string(&text).into_owned());
                            }
                        }
                        val
                    }
                    MapKey::FieldAccess(field) => {
                        if let Some(mut val) = accessor.get(field) {
                            if accessor.match_as_ascii {
                                val = deunicode_string(&val).into_owned().into();
                            }
                            Value(val.to_string())
                        } else {
                            Undefined
                        }
                    }
                    MapKey::VarAccess(name, field) => match ctx.variables.get(name) {
                        None => Failure(format!("Variable with name {name} not found.")),
                        Some(value) => match value {
                            Undefined => Undefined,
                            Value(value) if field == "1" => {
                                let value = if accessor.match_as_ascii {
                                    deunicode_string(value).into_owned()
                                } else {
                                    value.clone()
                                };
                                Value(value)
                            }
                            Number(_) | Value(_) => Failure(format!("Variable with name {name} has no fields.")),
                            Named(values) => {
                                for (key, val) in values {
                                    if key == field {
                                        let mut res_val = val.clone();
                                        if accessor.match_as_ascii {
                                            res_val = deunicode_string(&res_val).into_owned();
                                        }
                                        return Value(res_val);
                                    }
                                }
                                Failure(format!("Variable with name {name} has no field {field}."))
                            }
                            AnyValue | Failure(_) => value.clone(),
                        },
                    },
                };

                for map_case in cases {
                    let mut matches = false;
                    for key in &map_case.keys {
                        if match key {
                            MapCaseKey::Text(value) => matches_text(&key_value, value),
                            MapCaseKey::AnyMatch => true,
                            MapCaseKey::RangeFrom(num) => match key_value.compare(&Number(*num)) {
                                None => false,
                                Some(ord) => match ord {
                                    Ordering::Less => false,
                                    Ordering::Equal | Ordering::Greater => true,
                                },
                            },
                            MapCaseKey::RangeTo(num) => match key_value.compare(&Number(*num)) {
                                None => false,
                                Some(ord) => match ord {
                                    Ordering::Equal | Ordering::Less => true,
                                    Ordering::Greater => false,
                                },
                            },
                            MapCaseKey::RangeFull(from, to) => match key_value.compare(&Number(*from)) {
                                None => false,
                                Some(ord) => match ord {
                                    Ordering::Less => false,
                                    Ordering::Equal | Ordering::Greater => match key_value.compare(&Number(*to)) {
                                        None => false,
                                        Some(ord) => match ord {
                                            Ordering::Equal | Ordering::Less => true,
                                            Ordering::Greater => false,
                                        },
                                    },
                                },
                            },
                            MapCaseKey::RangeEq(num) => match key_value.compare(&Number(*num)) {
                                None => false,
                                Some(ord) => match ord {
                                    Ordering::Equal => true,
                                    Ordering::Less | Ordering::Greater => false,
                                },
                            },
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
                            return Failure(format!(
                                "For each expression invalid! Variable with name {ident} not found."
                            ));
                        }
                        let v = ctx.get_var(ident);
                        match v {
                            Named(_) | AnyValue | Failure(_) => v.clone(),
                            Undefined => Undefined,
                            _ => Failure(format!("Variable with name {ident} must be a Named list.")),
                        }
                    }
                    ForEachKey::VarAccess(name, field) => match ctx.variables.get(name) {
                        None => Failure(format!("Variable with name {name} not found.")),
                        Some(value) => match value {
                            AnyValue | Failure(_) => value.clone(),
                            Named(values) => {
                                let filtered: Vec<(String, String)> = values
                                    .iter()
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
                    },
                };

                let values = match key_value {
                    Named(key_value) => key_value,
                    Failure(_) => return key_value,
                    _ => Vec::new(),
                };
                let previous_key_var = expr.key_var.as_ref().and_then(|key_var| ctx.variables.get(key_var).cloned());
                let previous_value_var =
                    expr.value_var.as_ref().and_then(|value_var| ctx.variables.get(value_var).cloned());
                for (k, val) in values {
                    if let Some(key_var) = &expr.key_var {
                        ctx.set_var(key_var, EvalResult::Value(k));
                    }
                    if let Some(value_var) = &expr.value_var {
                        ctx.set_var(value_var, EvalResult::Value(val));
                    }
                    expr.expression.eval(ctx, accessor);
                }
                if let Some(key_var) = &expr.key_var {
                    if let Some(previous) = previous_key_var {
                        ctx.set_var(key_var, previous);
                    } else {
                        ctx.variables.remove(key_var);
                    }
                }
                if let Some(value_var) = &expr.value_var {
                    if let Some(previous) = previous_value_var {
                        ctx.set_var(value_var, previous);
                    } else {
                        ctx.variables.remove(value_var);
                    }
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
