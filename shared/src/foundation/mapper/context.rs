use super::{
    AssignmentTarget, BuiltInFunction, EvalResult, ExprId, Expression, ForEachExpr, ForEachKey, MapCase, MapCaseKey,
    MapKey, MapperContext, MatchCase, MatchCaseKey, RegexSource, ValueAccessor,
};
use crate::{
    error::TuliproxError,
    foundation::mapper::EvalResult::Undefined,
    model::{PatternTemplate, TemplateValue},
};
use std::collections::{HashMap, HashSet};

impl<'a> MapperContext<'a> {
    pub(super) fn new(expressions: &'a [Expression], templates: Option<&'a [PatternTemplate]>) -> Self {
        Self { expressions, variables: HashMap::new(), templates: templates.filter(|templates| !templates.is_empty()) }
    }

    pub(super) fn get_template(&self, name: &str) -> Option<&str> {
        self.templates?.iter().rev().find(|template| template.name == name).and_then(|template| match &template.value {
            TemplateValue::Single(v) => Some(v.as_str()),
            TemplateValue::Multi(_) => None,
        })
    }

    pub(super) fn set_var(&mut self, name: &str, value: EvalResult) {
        if let Some(current) = self.variables.get_mut(name) {
            *current = value;
        } else {
            self.variables.insert(name.to_string(), value);
        }
    }

    pub(super) fn has_var(&self, name: &str) -> bool { self.variables.contains_key(name) }

    pub(super) fn get_var(&self, name: &str) -> &EvalResult { self.variables.get(name).unwrap_or(&Undefined) }

    pub(super) fn eval_expr_by_id(&mut self, id: usize, accessor: &mut ValueAccessor) -> EvalResult {
        let Some(expr) = self.expressions.get(id) else { return Undefined };
        expr.eval(self, accessor)
    }

    pub(super) fn validate_expr(
        &mut self,
        expr_id: ExprId,
        identifiers: &mut HashSet<String>,
    ) -> Result<(), TuliproxError> {
        let Some(expr) = self.expressions.get(expr_id.0) else {
            return Err(TuliproxError::Mapper(format!("No matching expression found at index {}", expr_id.0)));
        };
        match expr {
            Expression::Identifier(ident) | Expression::VarAccess(ident, _) => {
                if !identifiers.contains(ident.as_str()) {
                    return Err(TuliproxError::Mapper(format!("Identifier unknown {ident}, {expr:?}")));
                }
            }
            Expression::NullValue
            | Expression::FieldAccess(_)
            | Expression::StringLiteral(_)
            | Expression::NumberLiteral(_) => {}
            Expression::RegexExpr { field, pattern: _pattern, re_pattern: _re_pattern } => match field {
                RegexSource::Identifier(ident) => {
                    if !identifiers.contains(ident.as_str()) {
                        return Err(TuliproxError::Mapper(format!("Regex identifier unknown {ident}, {expr:?}")));
                    }
                }
                RegexSource::Field(_) => {}
            },
            Expression::Assignment { target, expr } => {
                match target {
                    AssignmentTarget::Identifier(ident) => {
                        identifiers.insert(ident.clone());
                    }
                    AssignmentTarget::Field(_) => {}
                }
                self.validate_expr(*expr, identifiers)?;
            }
            Expression::FunctionCall { name, args } => {
                if args.is_empty() {
                    return Err(TuliproxError::Mapper(format!("Function needs at least one argument {name:?}")));
                }
                match name {
                    BuiltInFunction::ToNumber
                    | BuiltInFunction::Template
                    | BuiltInFunction::First
                    | BuiltInFunction::AddFavourite
                        if args.len() > 1 =>
                    {
                        return Err(TuliproxError::Mapper(format!(
                            "Function accepts only one argument {:?}, {} given",
                            name,
                            args.len()
                        )));
                    }
                    BuiltInFunction::Split if args.len() != 2 => {
                        return Err(TuliproxError::Mapper(format!(
                            "Function accepts two arguments {:?}, {} given",
                            name,
                            args.len()
                        )));
                    }
                    BuiltInFunction::Replace if args.len() != 3 => {
                        return Err(TuliproxError::Mapper(format!(
                            "Function accepts three arguments {:?}, {} given",
                            name,
                            args.len()
                        )));
                    }
                    BuiltInFunction::Pad if !(args.len() == 3 || args.len() == 4) => {
                        return Err(TuliproxError::Mapper(format!(
                            "Function accepts three or four arguments {:?}, {} given",
                            name,
                            args.len()
                        )));
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

    pub(super) fn validate_match_block(
        &mut self,
        identifiers: &mut HashSet<String>,
        cases: &Vec<MatchCase>,
    ) -> Result<(), TuliproxError> {
        let mut case_keys = HashSet::new();
        for match_case in cases {
            let mut any_match_count = 0;
            let mut identifier_key = String::with_capacity(56);
            for identifier in &match_case.keys {
                match identifier {
                    MatchCaseKey::Identifier(ident) => {
                        if !identifiers.contains(ident.as_str()) {
                            return Err(TuliproxError::Mapper(format!("Match case identifier unknown {ident}")));
                        }
                        identifier_key.push_str(ident.as_str());
                        identifier_key.push_str(", ");
                    }
                    MatchCaseKey::AnyMatch => {
                        any_match_count += 1;
                        if any_match_count > 1 {
                            return Err(TuliproxError::Mapper("Match case can only have one '_'".to_string()));
                        }
                        identifier_key.push_str("_, ");
                    }
                }
            }
            if case_keys.contains(&identifier_key) {
                return Err(TuliproxError::Mapper(format!("Duplicate case {identifier_key}")));
            }
            case_keys.insert(identifier_key);
            self.validate_expr(match_case.expression, identifiers)?;
        }
        Ok(())
    }

    pub(super) fn validate_map_block(
        &mut self,
        identifiers: &mut HashSet<String>,
        key: &MapKey,
        cases: &Vec<MapCase>,
    ) -> Result<(), TuliproxError> {
        match key {
            MapKey::Identifier(ident) | MapKey::VarAccess(ident, _) => {
                if !identifiers.contains(ident.as_str()) {
                    return Err(TuliproxError::Mapper(format!("Map key identifier unknown {ident}")));
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
                            return Err(TuliproxError::Mapper(format!("Duplicate case {value}")));
                        }
                        case_keys.insert(value.as_str());
                    }
                    MapCaseKey::RangeEq(_) | MapCaseKey::RangeTo(_) | MapCaseKey::RangeFrom(_) => {}
                    MapCaseKey::RangeFull(from, to) => {
                        if *from > *to {
                            return Err(TuliproxError::Mapper(format!("Invalid range {from}..{to}")));
                        }
                    }
                    MapCaseKey::AnyMatch => {
                        any_match_count += 1;
                        if any_match_count > 1 {
                            return Err(TuliproxError::Mapper("Map case can only have one '_'".to_string()));
                        }
                    }
                }
            }
            self.validate_expr(map_case.expression, identifiers)?;
        }
        Ok(())
    }

    pub(super) fn validate_for_each_block(
        &mut self,
        identifiers: &mut HashSet<String>,
        key: &ForEachKey,
        expr: &ForEachExpr,
    ) -> Result<(), TuliproxError> {
        match key {
            ForEachKey::Identifier(ident) | ForEachKey::VarAccess(ident, _) => {
                if !identifiers.contains(ident.as_str()) {
                    return Err(TuliproxError::Mapper(format!("For each key identifier unknown {ident}")));
                }
            }
        }
        let mut local_identifiers = identifiers.clone();

        if let Some(key_var) = &expr.key_var {
            if local_identifiers.contains(key_var) {
                return Err(TuliproxError::Mapper(format!(
                    "For each key variable shadows existing identifier {key_var}"
                )));
            }
            local_identifiers.insert(key_var.clone());
        }

        if let Some(value_var) = &expr.value_var {
            if local_identifiers.contains(value_var) {
                return Err(TuliproxError::Mapper(format!(
                    "For each value variable shadows existing identifier {value_var}"
                )));
            }
            local_identifiers.insert(value_var.clone());
        }

        self.validate_expr(expr.expression, &mut local_identifiers)?;

        Ok(())
    }
}
