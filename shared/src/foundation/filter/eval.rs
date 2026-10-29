use super::{
    CompiledRegex, Filter, ItemField, Leaf, Node, NodeId, NumericOperator, PlaylistItemType, PresenceOperator,
    StringOperator,
};
use crate::foundation::value_provider::ValueProvider;
use log::{log_enabled, trace, Level};
use smallvec::SmallVec;
use std::borrow::Cow;

fn get_caption<'a>(provider: &'a ValueProvider<'_>, rewc: &CompiledRegex) -> (bool, Cow<'a, str>) {
    if let Some(value) = provider.get_filter_value(ItemField::Title) {
        if rewc.re.is_match(&value) {
            return (true, value);
        }
    }

    if let Some(value) = provider.get_filter_value(ItemField::Name) {
        if rewc.re.is_match(&value) {
            return (true, value);
        }
    }
    (false, Cow::Borrowed(""))
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    haystack.as_bytes().windows(needle.len()).any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn starts_with_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    haystack.len() >= needle.len() && haystack.as_bytes()[..needle.len()].eq_ignore_ascii_case(needle.as_bytes())
}

fn string_operator_matches(actual: &str, op: StringOperator, value: &str) -> bool {
    match op {
        StringOperator::Eq => actual.eq_ignore_ascii_case(value),
        StringOperator::NotEq => !actual.eq_ignore_ascii_case(value),
        StringOperator::Contains => contains_ignore_ascii_case(actual, value),
        StringOperator::StartsWith => starts_with_ignore_ascii_case(actual, value),
    }
}

/// Caption matches against the title and falls back to the name, mirroring `get_caption`.
fn string_caption_matches(provider: &ValueProvider<'_>, op: StringOperator, value: &str) -> bool {
    let title = provider.get_filter_value(ItemField::Title).unwrap_or(Cow::Borrowed(""));
    let name = provider.get_filter_value(ItemField::Name).unwrap_or(Cow::Borrowed(""));
    match op {
        // NotEq is the negation of Eq-on-either, not "either differs"
        StringOperator::NotEq => !(title.eq_ignore_ascii_case(value) || name.eq_ignore_ascii_case(value)),
        StringOperator::Eq | StringOperator::Contains | StringOperator::StartsWith => {
            string_operator_matches(&title, op, value) || string_operator_matches(&name, op, value)
        }
    }
}

impl Leaf {
    /// Evaluates a comparison; the leaves carry no children.
    fn matches(&self, provider: &ValueProvider) -> bool {
        match self {
            Self::FieldComparison(field, rewc) => {
                let (is_match, value) = if field == &ItemField::Caption {
                    get_caption(provider, rewc)
                } else if let Some(value) = provider.get_filter_value(*field) {
                    (rewc.re.is_match(&value), value)
                } else {
                    (false, Cow::Borrowed(""))
                };
                if log_enabled!(Level::Trace) {
                    if is_match {
                        trace!("Match found: {rewc:?} {} => {field}='{value}'", rewc.restr);
                    } else {
                        trace!("Match failed: {self}: {rewc:?} {} => {field}='{value}'", rewc.restr);
                    }
                }
                is_match
            }
            Self::TypeComparison(field, item_type) => {
                let actual = provider.pli.header.item_type;
                let is_match = match item_type {
                    PlaylistItemType::Live => actual.is_live(),
                    PlaylistItemType::Video => actual.is_video(),
                    PlaylistItemType::Series => {
                        actual.is_series()
                            || matches!(actual, PlaylistItemType::SeriesInfo | PlaylistItemType::LocalSeriesInfo)
                    }
                    _ => actual == *item_type,
                };
                if log_enabled!(Level::Trace) {
                    if is_match {
                        trace!("Match found: {field:?} {}", actual.as_str());
                    } else {
                        trace!("Match failed: {self}: {field:?} {}", actual.as_str());
                    }
                }
                is_match
            }
            Self::StringComparison(field, op, value) => {
                // Missing values (e.g. absent epgid) are matched as empty strings.
                let is_match = if *field == ItemField::Caption {
                    string_caption_matches(provider, *op, value)
                } else {
                    let actual = provider.get_filter_value(*field).unwrap_or(Cow::Borrowed(""));
                    string_operator_matches(&actual, *op, value)
                };
                if log_enabled!(Level::Trace) {
                    let actual = provider.get_filter_value(*field).unwrap_or(Cow::Borrowed(""));
                    if is_match {
                        trace!("Match found: {field} {op} \"{value}\" => '{actual}'");
                    } else {
                        trace!("Match failed: {field} {op} \"{value}\" => '{actual}'");
                    }
                }
                is_match
            }
            Self::PresenceComparison(field, op) => {
                let is_empty = provider.get_filter_value(*field).is_none_or(|value| value.is_empty());
                match op {
                    PresenceOperator::IsEmpty => is_empty,
                    PresenceOperator::IsNotEmpty => !is_empty,
                }
            }
            Self::NumericComparison(field, op, value) => {
                // A numeric operator only accepts the numeric fields; any other field is a
                // non-match for this comparison.
                let actual = match field {
                    ItemField::Chno => provider.pli.header.chno,
                    ItemField::Quality => u32::from(provider.quality_rank()),
                    _ => return false,
                };
                match op {
                    NumericOperator::Eq => actual == *value,
                    NumericOperator::NotEq => actual != *value,
                    NumericOperator::Greater => actual > *value,
                    NumericOperator::GreaterOrEqual => actual >= *value,
                    NumericOperator::Less => actual < *value,
                    NumericOperator::LessOrEqual => actual <= *value,
                }
            }
            Self::SetComparison(field, values) => {
                if *field == ItemField::Caption {
                    let title = provider.get_filter_value(ItemField::Title).unwrap_or(Cow::Borrowed(""));
                    let name = provider.get_filter_value(ItemField::Name).unwrap_or(Cow::Borrowed(""));
                    values.iter().any(|value| title.eq_ignore_ascii_case(value) || name.eq_ignore_ascii_case(value))
                } else {
                    let actual = provider.get_filter_value(*field).unwrap_or(Cow::Borrowed(""));
                    values.iter().any(|value| actual.eq_ignore_ascii_case(value))
                }
            }
        }
    }
}

impl Filter {
    /// Depth that is evaluated recursively before the rest of the tree is walked with an
    /// explicit stack.
    ///
    /// Recursing over node indices costs about 5.5 ns per term, the explicit stack about
    /// 12.6 ns more, so realistic filters stay on the fast path. Measured stack use for the
    /// full budget: 135 KiB in a debug build, 32 KiB in a release build. A thread spawned with
    /// `std` gets 2 MiB regardless of the libc (verified on static musl: 2 099 568 bytes), and
    /// the only thread in this project with a smaller stack is the watchdog, which never
    /// evaluates a filter. The remainder of a deeper tree is walked iteratively.
    const RECURSIVE_EVAL_DEPTH: usize = 256;

    pub fn filter(&self, provider: &ValueProvider) -> bool {
        self.eval_node(self.root(), provider, Self::RECURSIVE_EVAL_DEPTH)
    }

    fn eval_node(&self, id: NodeId, provider: &ValueProvider, budget: usize) -> bool {
        if budget == 0 {
            return self.eval_iterative(id, provider);
        }
        match self.node(id) {
            Some(Node::Leaf(leaf_id)) => self.leaf(*leaf_id).is_some_and(|leaf| leaf.matches(provider)),
            Some(Node::Group(inner)) => self.eval_node(*inner, provider, budget - 1),
            Some(Node::Not(inner)) => !self.eval_node(*inner, provider, budget - 1),
            Some(Node::And(left, right)) => {
                self.eval_node(*left, provider, budget - 1) && self.eval_node(*right, provider, budget - 1)
            }
            Some(Node::Or(left, right)) => {
                self.eval_node(*left, provider, budget - 1) || self.eval_node(*right, provider, budget - 1)
            }
            None => false,
        }
    }

    /// Walks the subtree with an explicit stack, so its depth is not limited by the native
    /// stack. `and`/`or` still short-circuit: the right operand is only scheduled once the
    /// left result leaves the outcome open.
    fn eval_iterative(&self, root: NodeId, provider: &ValueProvider) -> bool {
        enum Step {
            Eval(NodeId),
            Not,
            And(NodeId),
            Or(NodeId),
        }

        // Inline storage keeps this allocation-free; both stacks grow with the nesting depth,
        // not with the length of the chain.
        let mut steps: SmallVec<[Step; 16]> = SmallVec::new();
        steps.push(Step::Eval(root));
        let mut results: SmallVec<[bool; 16]> = SmallVec::new();

        // The result of a completed subtree stays on `results` until its operator consumes it.
        while let Some(step) = steps.pop() {
            let id = match step {
                Step::Not => {
                    if let Some(value) = results.pop() {
                        results.push(!value);
                    }
                    continue;
                }
                Step::And(right) => {
                    if results.pop() == Some(true) {
                        steps.push(Step::Eval(right));
                    } else {
                        results.push(false);
                    }
                    continue;
                }
                Step::Or(right) => {
                    if results.pop() == Some(false) {
                        steps.push(Step::Eval(right));
                    } else {
                        results.push(true);
                    }
                    continue;
                }
                Step::Eval(id) => id,
            };

            match self.node(id) {
                Some(Node::Leaf(leaf_id)) => {
                    results.push(self.leaf(*leaf_id).is_some_and(|leaf| leaf.matches(provider)));
                }
                Some(Node::Group(inner)) => steps.push(Step::Eval(*inner)),
                Some(Node::Not(inner)) => {
                    steps.push(Step::Not);
                    steps.push(Step::Eval(*inner));
                }
                Some(Node::And(left, right)) => {
                    steps.push(Step::And(*right));
                    steps.push(Step::Eval(*left));
                }
                Some(Node::Or(left, right)) => {
                    steps.push(Step::Or(*right));
                    steps.push(Step::Eval(*left));
                }
                None => {}
            }
        }

        results.pop().unwrap_or(false)
    }
}
