#![allow(clippy::empty_docs)]

pub use crate::model::{ItemField, PatternTemplate, PlaylistItemType, TemplateValue};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod tests_edge_cases;

#[cfg(test)]
mod tests_plugin_calls;

mod eval;
mod expression;
mod parser;
mod templates;
pub use expression::{
    BinaryOperator, CompiledRegex, Filter, FilterBuilder, Leaf, LeafId, NoPluginCalls, Node, NodeId, NumericOperator,
    PluginCallBatch, PluginCallDecisions, PluginCallRef, PresenceOperator, StringOperator,
};
pub use parser::{
    get_filter, get_filter_detailed, get_target_processing_filter, get_target_processing_filter_detailed,
    FilterParsePosition,
};
pub use templates::{apply_templates_to_pattern, apply_templates_to_pattern_single, prepare_templates};
