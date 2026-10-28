#![allow(clippy::empty_docs)]

pub use crate::model::{ItemField, PatternTemplate, PlaylistItemType, TemplateValue};

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
