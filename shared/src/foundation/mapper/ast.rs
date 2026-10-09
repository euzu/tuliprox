use super::{BuiltInFunction, EvalResult, MapperContext, ValueAccessor};
use crate::foundation::mapper::EvalResult::Failure;
use log::debug;
use regex::Regex;
use std::{ops::Deref, sync::Arc};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExprId(pub usize);

impl Deref for ExprId {
    type Target = usize;

    fn deref(&self) -> &Self::Target { &self.0 }
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
        use Expression::{
            Assignment, Block, FieldAccess, ForEachBlock, FunctionCall, Identifier, MapBlock, MatchBlock, NullValue,
            NumberLiteral, RegexExpr, StringLiteral, VarAccess,
        };
        match (self, other) {
            (Identifier(a), Identifier(b)) => a == b,
            (StringLiteral(a), StringLiteral(b)) => a == b,
            (NumberLiteral(a), NumberLiteral(b)) => a == b,
            (FieldAccess(a), FieldAccess(b)) => a == b,
            (VarAccess(a1, b1), VarAccess(a2, b2)) => a1 == a2 && b1 == b2,
            (RegexExpr { field: f1, pattern: p1, .. }, RegexExpr { field: f2, pattern: p2, .. }) => {
                f1 == f2 && p1 == p2
            }
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

impl ExprId {
    pub fn eval(self, ctx: &mut MapperContext, accessor: &mut ValueAccessor) -> EvalResult {
        let id = self.0;
        ctx.eval_expr_by_id(id, accessor)
    }
}

impl Statement {
    pub fn eval(&self, ctx: &mut MapperContext, setter: &mut ValueAccessor) -> Option<String> {
        match self {
            Statement::Expression(expr_id) => {
                let result = expr_id.eval(ctx, setter);
                if let Failure(err) = &result {
                    debug!("{err}");
                    Some(err.clone())
                } else {
                    None
                }
            }
            Statement::Comment(_) => None,
        }
    }
}
