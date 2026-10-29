use super::{ItemField, PlaylistItemType};
use id_arena::{Arena, Id};
use smallvec::SmallVec;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct CompiledRegex {
    pub restr: String,
    pub re: Arc<regex::Regex>,
}

impl PartialEq for CompiledRegex {
    fn eq(&self, other: &Self) -> bool { self.restr == other.restr }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UnaryOperator {
    Not,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StringOperator {
    Eq,
    NotEq,
    Contains,
    StartsWith,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PresenceOperator {
    IsEmpty,
    IsNotEmpty,
}

impl std::fmt::Display for PresenceOperator {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match self {
            Self::IsEmpty => "IS EMPTY",
            Self::IsNotEmpty => "IS NOT EMPTY",
        })
    }
}

impl std::fmt::Display for StringOperator {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match *self {
            Self::Eq => "=",
            Self::NotEq => "!=",
            Self::Contains => "CONTAINS",
            Self::StartsWith => "STARTSWITH",
        })
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NumericOperator {
    Eq,
    NotEq,
    Greater,
    GreaterOrEqual,
    Less,
    LessOrEqual,
}

impl std::fmt::Display for NumericOperator {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match *self {
            Self::Eq => "=",
            Self::NotEq => "!=",
            Self::Greater => ">",
            Self::GreaterOrEqual => ">=",
            Self::Less => "<",
            Self::LessOrEqual => "<=",
        })
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BinaryOperator {
    And,
    Or,
}

impl BinaryOperator {
    pub(super) const OP_OR: &'static str = "OR";
    pub(super) const OP_AND: &'static str = "AND";
}

impl std::fmt::Display for BinaryOperator {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match *self {
                Self::Or => Self::OP_OR,
                Self::And => Self::OP_AND,
            }
        )
    }
}

/// Identifier of a node in a [`Filter`]'s arena.
pub type NodeId = Id<Node>;

/// Identifier of a leaf in a [`Filter`]'s arena.
pub type LeafId = Id<Leaf>;

/// The shape of a filter expression.
///
/// Children are [`NodeId`]s into the owning [`Filter`]'s arena instead of boxed children. A
/// filter is a chain of one node per term, so a boxed tree would be one heap allocation per
/// term and a `n`-deep recursive `Drop`; an arena keeps the whole tree in one buffer that is
/// released in a single step, and every walk over it is a loop over indices.
///
/// A builder only allocates a parent after its children, so a child's index is always lower
/// than its parent's. Traversal code relies on that only for copying a tree between arenas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Node {
    Leaf(LeafId),
    Group(NodeId),
    Not(NodeId),
    And(NodeId, NodeId),
    Or(NodeId, NodeId),
}

/// A comparison without children.
#[derive(Debug, Clone, PartialEq)]
pub enum Leaf {
    FieldComparison(ItemField, CompiledRegex),
    TypeComparison(ItemField, PlaylistItemType),
    StringComparison(ItemField, StringOperator, String),
    PresenceComparison(ItemField, PresenceOperator),
    NumericComparison(ItemField, NumericOperator, u32),
    SetComparison(ItemField, Vec<String>),
}

/// A parsed filter expression.
#[derive(Debug, Clone)]
pub struct Filter {
    nodes: Arena<Node>,
    leaves: Arena<Leaf>,
    root: NodeId,
}

impl Filter {
    pub(super) const LIVE: &'static str = "live";
    pub(super) const VOD: &'static str = "vod";
    pub(super) const MOVIE: &'static str = "movie";
    pub(super) const SERIES: &'static str = "series";
    pub(super) const UNSUPPORTED: &'static str = "unsupported";

    /// The root node. A parsed filter always has one; "no filter" is modelled by an absent
    /// `Option<Filter>` at the call sites, not by an empty tree.
    #[must_use]
    pub fn root(&self) -> NodeId { self.root }

    /// Looks up a node by id.
    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&Node> { self.nodes.get(id) }

    /// Looks up a leaf by id.
    #[must_use]
    pub fn leaf(&self, id: LeafId) -> Option<&Leaf> { self.leaves.get(id) }

    /// Number of nodes, useful for callers that pre-size a traversal stack.
    #[must_use]
    pub fn node_count(&self) -> usize { self.nodes.len() }
}

impl Default for Filter {
    /// A filter that accepts every item.
    fn default() -> Self {
        let mut builder = FilterBuilder::new();
        let root = match crate::model::REGEX_CACHE.get_or_compile(".*") {
            Ok(re) => {
                let leaf = builder
                    .leaf(Leaf::FieldComparison(ItemField::Group, CompiledRegex { restr: ".*".to_string(), re }));
                builder.group(leaf)
            }
            // `.*` is a valid pattern, so this means the regex engine or its cache is
            // unusable. Fall back to a match-all expression that needs no regex at all
            // instead of aborting the process.
            Err(err) => {
                log::error!("cannot compile the default filter regex '.*': {err}");
                let empty = builder.leaf(Leaf::PresenceComparison(ItemField::Group, PresenceOperator::IsEmpty));
                let present = builder.leaf(Leaf::PresenceComparison(ItemField::Group, PresenceOperator::IsNotEmpty));
                builder.or(empty, present)
            }
        };
        builder.finish(root)
    }
}

/// Structural equality over the arena contents in allocation order.
///
/// [`Id`]s are scoped to the arena that issued them, so two filters that describe the same
/// expression never hold equal ids. Nodes are compared by index and leaves by value instead.
impl PartialEq for Filter {
    fn eq(&self, other: &Self) -> bool {
        self.root.index() == other.root.index()
            && self.nodes.len() == other.nodes.len()
            && self.leaves.len() == other.leaves.len()
            && self.nodes.iter().zip(other.nodes.iter()).all(|((_, left), (_, right))| nodes_match(left, right))
            && self.leaves.iter().zip(other.leaves.iter()).all(|((_, left), (_, right))| left == right)
    }
}

/// Compares two nodes by the position of their children rather than by their arena ids.
fn nodes_match(left: &Node, right: &Node) -> bool {
    match (left, right) {
        (Node::Leaf(a), Node::Leaf(b)) => a.index() == b.index(),
        (Node::Group(a), Node::Group(b)) | (Node::Not(a), Node::Not(b)) => a.index() == b.index(),
        (Node::And(a1, a2), Node::And(b1, b2)) | (Node::Or(a1, a2), Node::Or(b1, b2)) => {
            a1.index() == b1.index() && a2.index() == b2.index()
        }
        _ => false,
    }
}

/// Builds a [`Filter`] bottom-up.
///
/// Parents are allocated after their children, so every edge points to a lower index.
#[derive(Debug, Default)]
pub struct FilterBuilder {
    nodes: Arena<Node>,
    leaves: Arena<Leaf>,
}

impl FilterBuilder {
    #[must_use]
    pub fn new() -> Self { Self::default() }

    pub fn leaf(&mut self, leaf: Leaf) -> NodeId {
        let leaf_id = self.leaves.alloc(leaf);
        self.nodes.alloc(Node::Leaf(leaf_id))
    }

    pub fn group(&mut self, inner: NodeId) -> NodeId { self.nodes.alloc(Node::Group(inner)) }

    pub fn not(&mut self, inner: NodeId) -> NodeId { self.nodes.alloc(Node::Not(inner)) }

    pub fn and(&mut self, left: NodeId, right: NodeId) -> NodeId { self.nodes.alloc(Node::And(left, right)) }

    pub fn or(&mut self, left: NodeId, right: NodeId) -> NodeId { self.nodes.alloc(Node::Or(left, right)) }

    /// Copies another filter's tree into this builder and returns the new root.
    ///
    /// Both arenas are filled in allocation order, and a node only refers to nodes allocated
    /// before it, so a single pass maps every child id.
    ///
    /// Returns `None` when `other` refers to a node that is not in its own arena. That cannot
    /// happen for a filter this crate built, but the constructor takes an arbitrary id, and a
    /// filter that carries a foreign root must not turn a copy into a panic. Nodes allocated
    /// before the failure stay in this builder unused; callers drop the builder on error.
    pub fn append(&mut self, other: &Filter) -> Option<NodeId> {
        let mut leaf_ids = Vec::with_capacity(other.leaves.len());
        for (_, leaf) in &other.leaves {
            leaf_ids.push(self.leaves.alloc(leaf.clone()));
        }
        let mut node_ids = Vec::with_capacity(other.nodes.len());
        for (_, node) in &other.nodes {
            let mapped = match *node {
                Node::Leaf(id) => Node::Leaf(*leaf_ids.get(id.index())?),
                Node::Group(id) => Node::Group(*node_ids.get(id.index())?),
                Node::Not(id) => Node::Not(*node_ids.get(id.index())?),
                Node::And(left, right) => Node::And(*node_ids.get(left.index())?, *node_ids.get(right.index())?),
                Node::Or(left, right) => Node::Or(*node_ids.get(left.index())?, *node_ids.get(right.index())?),
            };
            node_ids.push(self.nodes.alloc(mapped));
        }
        // The root must resolve in `other` as well, so a filter with a foreign root is refused
        // here instead of being copied into a wrong position.
        other.node(other.root)?;
        node_ids.get(other.root.index()).copied()
    }

    /// Number of nodes allocated so far; callers use it to bound the size of a filter.
    #[must_use]
    pub fn node_count(&self) -> usize { self.nodes.len() }

    /// Finishes the filter at `root`.
    ///
    /// `root` must come from this builder. A foreign id is preserved as given, and readers
    /// treat an unresolvable root as an empty filter; [`FilterBuilder::append`] refuses it.
    #[must_use]
    pub fn finish(self, root: NodeId) -> Filter { Filter { nodes: self.nodes, leaves: self.leaves, root } }
}

impl std::fmt::Display for Leaf {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::FieldComparison(field, rewc) => write!(f, "{} ~ \"{}\"", field, rewc.restr),
            Self::TypeComparison(field, item_type) => {
                write!(
                    f,
                    "{} = {}",
                    field,
                    match item_type {
                        PlaylistItemType::Live => Filter::LIVE,
                        PlaylistItemType::Video | PlaylistItemType::LocalVideo => Filter::MOVIE,
                        PlaylistItemType::Series
                        | PlaylistItemType::SeriesInfo
                        | PlaylistItemType::LocalSeries
                        | PlaylistItemType::LocalSeriesInfo => Filter::SERIES,
                        _ => Filter::UNSUPPORTED,
                    }
                )
            }
            Self::StringComparison(field, op, value) => {
                write!(f, "{field} {op} \"{}\"", value.replace('"', "\\\""))
            }
            Self::PresenceComparison(field, op) => write!(f, "{field} {op}"),
            Self::NumericComparison(field, op, value) => write!(f, "{field} {op} {value}"),
            Self::SetComparison(field, values) => {
                write!(f, "{field} IN [")?;
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "\"{}\"", value.replace('"', "\\\""))?;
                }
                f.write_str("]")
            }
        }
    }
}

impl Filter {
    /// Whether a child needs its own parentheses to keep the grouping on reparse.
    ///
    /// The grammar has no operator precedence; it groups a chain from the right. A binary
    /// operand on the left of a binary node would therefore be regrouped, and `NOT` binds to a
    /// single operand, so a binary or negated child of `NOT` would escape the negation. A child
    /// on the right of a binary node, a negated left operand and every `Group` keep their
    /// grouping without extra parentheses.
    fn needs_operand_parens(&self, id: NodeId, under_not: bool) -> bool {
        match self.nodes.get(id) {
            Some(Node::And(..) | Node::Or(..)) => true,
            Some(Node::Not(_)) => under_not,
            _ => false,
        }
    }
}

impl std::fmt::Display for Filter {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        enum Step {
            Node(NodeId),
            Text(&'static str),
        }

        // Inline storage carries the common case without touching the heap; the stack grows
        // with nesting depth, not with chain length.
        let mut steps: SmallVec<[Step; 16]> = SmallVec::new();
        steps.push(Step::Node(self.root));
        while let Some(step) = steps.pop() {
            let node = match step {
                Step::Text(text) => {
                    f.write_str(text)?;
                    continue;
                }
                Step::Node(id) => match self.nodes.get(id) {
                    Some(node) => node,
                    None => continue,
                },
            };
            match *node {
                Node::Leaf(id) => {
                    if let Some(leaf) = self.leaves.get(id) {
                        write!(f, "{leaf}")?;
                    }
                }
                Node::Group(inner) => {
                    f.write_str("(")?;
                    steps.push(Step::Text(")"));
                    steps.push(Step::Node(inner));
                }
                Node::Not(inner) => {
                    // `NOT` binds to one operand, so a binary or negated child needs parens.
                    let parens = self.needs_operand_parens(inner, true);
                    if parens {
                        steps.push(Step::Text(")"));
                    }
                    steps.push(Step::Node(inner));
                    if parens {
                        steps.push(Step::Text("("));
                    }
                    steps.push(Step::Text("NOT "));
                }
                Node::And(left, right) | Node::Or(left, right) => {
                    let op_text = match *node {
                        Node::And(..) => " AND ",
                        _ => " OR ",
                    };
                    // A binary left operand would be regrouped by the right-associative grammar.
                    let parens = self.needs_operand_parens(left, false);
                    steps.push(Step::Node(right));
                    steps.push(Step::Text(op_text));
                    if parens {
                        steps.push(Step::Text(")"));
                    }
                    steps.push(Step::Node(left));
                    if parens {
                        steps.push(Step::Text("("));
                    }
                }
            }
        }
        Ok(())
    }
}
