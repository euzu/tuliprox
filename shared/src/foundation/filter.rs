#![allow(clippy::empty_docs)]

pub use crate::model::{ItemField, PatternTemplate, PlaylistItemType, TemplateValue};

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

#[cfg(test)]
mod tests;

mod eval;
mod expression;
mod parser;
mod templates;
pub use expression::{
    BinaryOperator, CompiledRegex, Filter, NumericOperator, PresenceOperator, StringOperator, UnaryOperator,
};
pub use parser::{get_filter, get_filter_detailed, FilterParsePosition};
pub use templates::{apply_templates_to_pattern, apply_templates_to_pattern_single, prepare_templates};
