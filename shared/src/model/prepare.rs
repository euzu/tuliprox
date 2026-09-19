//! One shape for the config tree's two-phase construction.
//!
//! Config types are deserialized first and *resolved* second: pattern templates
//! get expanded, regexes compiled, filters parsed, derived fields computed. That
//! second phase was spread across roughly 98 inherent `prepare` and `validate`
//! methods, and no two agreed on a signature. Some took nothing; some took
//! `Option<&[PatternTemplate]>`; some took a storage dir, a device number, a
//! port, or an `include_computed: bool`. Return types varied between `()`,
//! `Result<(), TuliproxError>`, `Result<(), &'static str>` and `bool`.
//!
//! Because the shape was invisible, the recursive walk down the tree had to be
//! written by hand at every level -- `for x in xs.iter_mut() { x.prepare(..)? }`,
//! an error-aggregating macro over `xs.iter_mut().map(..)`, and a `match`
//! per enum -- and a newly added config struct that forgot to call its
//! children's `prepare` failed silently at runtime rather than at compile time.
//!
//! [`Prepare`] names the shape without erasing anything. The context each node
//! needs from its parent is an associated type, so a node that needs pattern
//! templates and a node that needs a port are both `Prepare` implementors
//! without a lowest-common-denominator argument list, and dispatch stays static.
//!
//! The blanket impls for `Vec`, `Option` and slices are the payoff: the walk
//! over a collection of preparable children is written once here instead of
//! once per container per config type.

use crate::error::TuliproxError;

/// The resolve-after-deserialize phase of a config node.
pub trait Prepare {
    /// Everything this node needs from its parent.
    ///
    /// `Copy` so the blanket impls can hand the same context to every child in a
    /// loop without cloning or reborrowing. Use `()` for a node that needs
    /// nothing, and a tuple for a node that needs several things.
    type Ctx<'a>: Copy;

    fn prepare(&mut self, ctx: Self::Ctx<'_>) -> Result<(), TuliproxError>;
}

impl<T: Prepare> Prepare for Vec<T> {
    type Ctx<'a> = T::Ctx<'a>;

    fn prepare(&mut self, ctx: Self::Ctx<'_>) -> Result<(), TuliproxError> {
        for item in self.iter_mut() {
            item.prepare(ctx)?;
        }
        Ok(())
    }
}

impl<T: Prepare> Prepare for [T] {
    type Ctx<'a> = T::Ctx<'a>;

    fn prepare(&mut self, ctx: Self::Ctx<'_>) -> Result<(), TuliproxError> {
        for item in self.iter_mut() {
            item.prepare(ctx)?;
        }
        Ok(())
    }
}

/// A missing optional child is prepared trivially, so callers stop writing
/// `if let Some(child) = &mut self.child { child.prepare(ctx)?; }`.
impl<T: Prepare> Prepare for Option<T> {
    type Ctx<'a> = T::Ctx<'a>;

    fn prepare(&mut self, ctx: Self::Ctx<'_>) -> Result<(), TuliproxError> {
        match self {
            Some(item) => item.prepare(ctx),
            None => Ok(()),
        }
    }
}

impl<T: Prepare> Prepare for Box<T> {
    type Ctx<'a> = T::Ctx<'a>;

    fn prepare(&mut self, ctx: Self::Ctx<'_>) -> Result<(), TuliproxError> { (**self).prepare(ctx) }
}

/// Prepares every child and reports **all** failures, not just the first.
///
/// [`Prepare`]'s collection impls short-circuit on the first error, which is
/// right for a nested walk but wrong for config validation: a user with three
/// bad sort rules should be told about three, not made to fix them one round-trip
/// at a time. That is what the old error-aggregating macro did by hand, and
/// this is that behaviour as part of the trait — same aggregation, same
/// `TuliproxError::Errors` with newline-joined messages.
pub trait PrepareAll: Prepare {
    fn prepare_all(&mut self, ctx: Self::Ctx<'_>) -> Result<(), TuliproxError>;
}

/// Prepares every item, collecting each failure's rendered message.
fn prepare_each<'a, T>(items: impl Iterator<Item = &'a mut T>, ctx: T::Ctx<'_>) -> Result<(), TuliproxError>
where
    T: Prepare + 'a,
{
    let errors: Vec<String> = items.filter_map(|item| item.prepare(ctx).err().map(|err| err.to_string())).collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(TuliproxError::Errors(errors.join("\n")))
    }
}

impl<T: Prepare> PrepareAll for Vec<T> {
    fn prepare_all(&mut self, ctx: Self::Ctx<'_>) -> Result<(), TuliproxError> { prepare_each(self.iter_mut(), ctx) }
}

impl<T: Prepare> PrepareAll for [T] {
    fn prepare_all(&mut self, ctx: Self::Ctx<'_>) -> Result<(), TuliproxError> { prepare_each(self.iter_mut(), ctx) }
}

impl<T: PrepareAll> PrepareAll for Option<T> {
    fn prepare_all(&mut self, ctx: Self::Ctx<'_>) -> Result<(), TuliproxError> {
        match self {
            Some(item) => item.prepare_all(ctx),
            None => Ok(()),
        }
    }
}

impl<T: PrepareAll> PrepareAll for Box<T> {
    fn prepare_all(&mut self, ctx: Self::Ctx<'_>) -> Result<(), TuliproxError> { (**self).prepare_all(ctx) }
}

#[cfg(test)]
mod tests {
    use super::{Prepare, PrepareAll};
    use crate::error::{ErrorKind, TuliproxError};

    /// A node that needs a borrowed context, like the pattern-template families.
    #[derive(Debug, PartialEq, Eq)]
    struct Node {
        name: String,
        resolved: Option<String>,
    }

    impl Prepare for Node {
        type Ctx<'a> = Option<&'a [&'a str]>;

        fn prepare(&mut self, ctx: Self::Ctx<'_>) -> Result<(), TuliproxError> {
            if self.name == "boom" {
                return Err(TuliproxError::Config("node refused to prepare"));
            }
            let prefix = ctx.and_then(|templates| templates.first().copied()).unwrap_or("bare");
            self.resolved = Some(format!("{prefix}:{}", self.name));
            Ok(())
        }
    }

    fn node(name: &str) -> Node { Node { name: name.to_string(), resolved: None } }

    #[test]
    fn vec_impl_prepares_every_child_with_the_same_context() {
        let mut nodes = vec![node("a"), node("b"), node("c")];
        let templates = ["tpl"];
        nodes.prepare(Some(&templates)).expect("all children prepare");
        assert_eq!(
            nodes.iter().map(|n| n.resolved.as_deref().unwrap_or("")).collect::<Vec<_>>(),
            ["tpl:a", "tpl:b", "tpl:c"]
        );
    }

    #[test]
    fn vec_impl_stops_at_the_first_failing_child_and_propagates_the_error() {
        let mut nodes = vec![node("a"), node("boom"), node("c")];
        let err = nodes.prepare(None).expect_err("the middle child fails");
        assert_eq!(err.kind(), ErrorKind::Config);
        // First child ran, third did not: `?` short-circuits, as the hand-written
        // `for` loops did.
        assert_eq!(nodes[0].resolved.as_deref(), Some("bare:a"));
        assert_eq!(nodes[2].resolved, None);
    }

    #[test]
    fn option_impl_makes_an_absent_child_a_no_op() {
        let mut absent: Option<Node> = None;
        absent.prepare(None).expect("absent child is trivially prepared");
        assert_eq!(absent, None);

        let mut present = Some(node("x"));
        present.prepare(None).expect("present child prepares");
        assert_eq!(present.and_then(|n| n.resolved).as_deref(), Some("bare:x"));
    }

    #[test]
    fn impls_nest_so_a_vec_of_optional_children_needs_no_hand_written_walk() {
        let mut nested: Vec<Option<Node>> = vec![Some(node("a")), None, Some(node("b"))];
        nested.prepare(None).expect("nested children prepare");
        let resolved: Vec<_> = nested.iter().map(|slot| slot.as_ref().and_then(|n| n.resolved.as_deref())).collect();
        assert_eq!(resolved, [Some("bare:a"), None, Some("bare:b")]);
    }

    #[test]
    fn prepare_all_reports_every_failing_child_not_just_the_first() {
        let mut nodes = vec![node("boom"), node("ok"), node("boom")];
        let err = nodes.prepare_all(None).expect_err("two children fail");
        assert_eq!(err.kind(), ErrorKind::Errors);
        // Both failures are present, newline-joined, exactly as
        // the old aggregating macro produced them.
        assert_eq!(err.message().lines().count(), 2);
        assert!(err.message().lines().all(|line| line.contains("node refused to prepare")));
        // And unlike `prepare`, every child still ran.
        assert_eq!(nodes[1].resolved.as_deref(), Some("bare:ok"));
    }

    #[test]
    fn prepare_all_is_ok_when_every_child_succeeds() {
        let mut nodes = vec![node("a"), node("b")];
        nodes.prepare_all(None).expect("all children prepare");
        assert_eq!(nodes[1].resolved.as_deref(), Some("bare:b"));
    }

    #[test]
    fn prepare_all_nests_through_option() {
        let mut wrapped: Option<Vec<Node>> = Some(vec![node("boom"), node("boom")]);
        let err = wrapped.prepare_all(None).expect_err("both children fail");
        assert_eq!(err.message().lines().count(), 2);

        let mut absent: Option<Vec<Node>> = None;
        absent.prepare_all(None).expect("absent collection is a no-op");
    }

    #[test]
    fn box_impl_forwards_to_the_inner_node() {
        let mut boxed = Box::new(node("b"));
        boxed.prepare(None).expect("boxed child prepares");
        assert_eq!(boxed.resolved.as_deref(), Some("bare:b"));
    }
}
