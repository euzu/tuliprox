#![allow(clippy::empty_docs)]

use crate::model::{FieldGetAccessor, FieldSetAccessor, PatternTemplate, PlaylistItem};
use log::{error, trace};
use std::{collections::HashMap, sync::Arc};

///
/// The `ValueAccessor` does not respect `match_as_ascii` when returning values.
/// This is intentional because assignments must not change the value content through normalization.
/// Normalization should only be applied during comparisons or when processing values with regular expressions.
///
pub struct ValueAccessor<'a> {
    pub pli: &'a mut PlaylistItem,
    pub virtual_items: Vec<(String, PlaylistItem)>,
    pub match_as_ascii: bool,
    pub changed_fields: Vec<String>,
}

impl ValueAccessor<'_> {
    pub fn get(&self, field: &str) -> Option<Arc<str>> { self.pli.header.get_field(field) }

    pub fn set(&mut self, field: &str, value: &str) {
        if self.pli.header.set_field(field, value) {
            if !self.changed_fields.iter().any(|changed| changed.eq_ignore_ascii_case(field)) {
                self.changed_fields.push(field.to_string());
            }
            trace!("Property {field} set to {value}");
        } else {
            error!("Can't set unknown field {field} set to {value}");
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MapperScript {
    pub expressions: Vec<Expression>,
    pub statements: Vec<Statement>,
}

pub struct MapperContext<'a> {
    expressions: &'a [Expression],
    variables: HashMap<String, EvalResult>,
    templates: Option<&'a [PatternTemplate]>,
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

#[cfg(test)]
mod tests;

mod ast;
mod builtins;
mod context;
mod diagnostics;
mod eval;
mod parser;
pub use ast::{
    AssignmentTarget, ExprId, Expression, ForEachExpr, ForEachKey, MapCase, MapCaseKey, MapKey, MatchCase,
    MatchCaseKey, RegexSource, Statement,
};
pub use builtins::BuiltInFunction;
pub use diagnostics::{MappingDiagnostic, MappingOutcome};
#[cfg(test)]
use eval::eval_regex;
pub use eval::EvalResult;
pub use parser::Rule;
