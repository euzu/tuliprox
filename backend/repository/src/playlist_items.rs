//! Concrete, nameable iterators over a [`crate::PlaylistSource`].
//!
//! `PlaylistSourceOps` used to return `Box<dyn Iterator<..> + Send + '_>` from
//! `into_items`, `items` and `items_mut`. The trait is private and was never
//! used as a trait object — `PlaylistSource` dispatches through the
//! `PlaylistSourceKind` enum — so the box bought nothing and cost one
//! allocation per call plus one indirect, uninlinable call **per playlist
//! item**. On a refresh of a few hundred thousand channels that is the hottest
//! traversal in the repository.
//!
//! Composed adapter chains (`flat_map(..).map(..)`) cannot be named, because
//! closure types cannot be written down, so the erasure could not simply be
//! deleted. This module replaces the chains with hand-rolled state machines
//! whose types *are* nameable, which lets the enum hold them directly:
//!
//! ```text
//! before:  PlaylistSource::into_items() -> Box<dyn Iterator<Item = PlaylistItem> + Send + '_>
//! after:   PlaylistSource::into_items() -> ClusterFiltered<SourceItems<'_>>
//! ```
//!
//! Nothing here allocates and nothing here dispatches dynamically: `next()` is
//! a discriminant branch the compiler can inline through and predict.

use crate::bplustree::BPlusTreeDiskIterator;
use log::error;
use serde::Deserialize;
use shared::model::{
    stalker_item::StalkerPlaylistItem, M3uPlaylistItem, PlaylistGroup, PlaylistItem, UUIDType, XtreamCluster,
    XtreamPlaylistItem,
};
use std::{borrow::Cow, collections::HashSet, sync::Arc};

// ---------------------------------------------------------------------------
// B+Tree building blocks
// ---------------------------------------------------------------------------

/// One on-disk B+Tree store, yielding values and skipping unreadable entries.
///
/// Replaces `q.iter().filter_map(log_and_skip_btree_error).map(..)`; the log
/// message is kept verbatim so operator-facing output does not change.
pub struct BTreeValues<'a, K, V> {
    inner: Option<BPlusTreeDiskIterator<'a, K, V>>,
}

impl<'a, K, V> BTreeValues<'a, K, V> {
    #[inline]
    pub fn new(inner: Option<BPlusTreeDiskIterator<'a, K, V>>) -> Self { Self { inner } }

    #[inline]
    pub fn empty() -> Self { Self { inner: None } }
}

impl<K, V> Iterator for BTreeValues<'_, K, V>
where
    K: Ord + for<'de> Deserialize<'de>,
    V: for<'de> Deserialize<'de>,
{
    type Item = V;

    #[inline]
    fn next(&mut self) -> Option<V> {
        let inner = self.inner.as_mut()?;
        loop {
            match inner.next()? {
                Ok((_, value)) => return Some(value),
                Err(error) => {
                    error!("Skipping unreadable B+Tree playlist entry; iteration continues when possible: {error}");
                }
            }
        }
    }
}

/// `N` B+Tree stores of the same shape, traversed back to back.
///
/// Replaces the `live.chain(vod).chain(series)` shape: `N == 1` for the
/// single-file sources, `3` for Xtream, `4` for Stalker.
pub struct BTreeStores<'a, K, V, const N: usize> {
    stores: [BTreeValues<'a, K, V>; N],
    at: usize,
}

impl<'a, K, V, const N: usize> BTreeStores<'a, K, V, N> {
    #[inline]
    pub fn new(stores: [BTreeValues<'a, K, V>; N]) -> Self { Self { stores, at: 0 } }
}

impl<K, V, const N: usize> Iterator for BTreeStores<'_, K, V, N>
where
    K: Ord + for<'de> Deserialize<'de>,
    V: for<'de> Deserialize<'de>,
{
    type Item = V;

    #[inline]
    fn next(&mut self) -> Option<V> {
        while self.at < N {
            if let Some(value) = self.stores[self.at].next() {
                return Some(value);
            }
            self.at += 1;
        }
        None
    }
}

// ---------------------------------------------------------------------------
// In-memory building blocks
// ---------------------------------------------------------------------------

/// Moves every channel out of every group, emptying the groups as it goes.
///
/// This is the in-memory `into_items`, which is destructive by design.
pub struct MemoryDrain<'a> {
    groups: std::slice::IterMut<'a, PlaylistGroup>,
    current: Option<std::vec::Drain<'a, PlaylistItem>>,
}

impl<'a> MemoryDrain<'a> {
    #[inline]
    pub fn new(groups: &'a mut [PlaylistGroup]) -> Self { Self { groups: groups.iter_mut(), current: None } }
}

impl Iterator for MemoryDrain<'_> {
    type Item = PlaylistItem;

    #[inline]
    fn next(&mut self) -> Option<PlaylistItem> {
        loop {
            if let Some(item) = self.current.as_mut().and_then(Iterator::next) {
                return Some(item);
            }
            self.current = Some(self.groups.next()?.channels.drain(..));
        }
    }
}

/// Yields a mutable reference to every channel in every group.
pub struct MemoryItemsMut<'a> {
    groups: std::slice::IterMut<'a, PlaylistGroup>,
    current: Option<std::slice::IterMut<'a, PlaylistItem>>,
}

impl<'a> MemoryItemsMut<'a> {
    #[inline]
    pub fn new(groups: &'a mut [PlaylistGroup]) -> Self { Self { groups: groups.iter_mut(), current: None } }
}

impl<'a> Iterator for MemoryItemsMut<'a> {
    type Item = &'a mut PlaylistItem;

    #[inline]
    fn next(&mut self) -> Option<&'a mut PlaylistItem> {
        loop {
            if let Some(item) = self.current.as_mut().and_then(Iterator::next) {
                return Some(item);
            }
            self.current = Some(self.groups.next()?.channels.iter_mut());
        }
    }
}

/// Yields a shared reference to every channel in every group.
pub struct MemoryItems<'a> {
    groups: std::slice::Iter<'a, PlaylistGroup>,
    current: Option<std::slice::Iter<'a, PlaylistItem>>,
}

impl<'a> MemoryItems<'a> {
    #[inline]
    pub fn new(groups: &'a [PlaylistGroup]) -> Self { Self { groups: groups.iter(), current: None } }
}

impl<'a> Iterator for MemoryItems<'a> {
    type Item = &'a PlaylistItem;

    #[inline]
    fn next(&mut self) -> Option<&'a PlaylistItem> {
        loop {
            if let Some(item) = self.current.as_mut().and_then(Iterator::next) {
                return Some(item);
            }
            self.current = Some(self.groups.next()?.channels.iter());
        }
    }
}

// ---------------------------------------------------------------------------
// Owned-item iteration over any source
// ---------------------------------------------------------------------------

/// Every playlist item of one source, as an owned [`PlaylistItem`].
///
/// One variant per `PlaylistSourceKind`, collapsed where the payload and the
/// conversion coincide: the library and media-server stores are both
/// `XtreamPlaylistItem` keyed by [`UUIDType`], so they share `SingleXtream`.
pub enum SourceItems<'a> {
    Empty,
    Xtream(BTreeStores<'a, u32, XtreamPlaylistItem, 3>),
    Stalker { stores: BTreeStores<'a, u32, StalkerPlaylistItem, 4>, input_name: Arc<str> },
    M3u(BTreeStores<'a, Arc<str>, M3uPlaylistItem, 1>),
    SingleXtream(BTreeStores<'a, UUIDType, XtreamPlaylistItem, 1>),
    Memory(MemoryDrain<'a>),
}

impl Iterator for SourceItems<'_> {
    type Item = PlaylistItem;

    #[inline]
    fn next(&mut self) -> Option<PlaylistItem> {
        match self {
            Self::Empty => None,
            Self::Xtream(stores) => stores.next().map(|item| PlaylistItem::from(&item)),
            Self::SingleXtream(stores) => stores.next().map(|item| PlaylistItem::from(&item)),
            Self::M3u(stores) => stores.next().map(|item| PlaylistItem::from(&item)),
            Self::Stalker { stores, input_name } => {
                stores.next().map(|item| PlaylistItem::from_stalker(&item, input_name))
            }
            Self::Memory(drain) => drain.next(),
        }
    }
}

// ---------------------------------------------------------------------------
// Cow and mutable iteration over any source
// ---------------------------------------------------------------------------

/// Every playlist item of one source, as a [`Cow`].
///
/// Disk sources have to materialise each item, so they yield [`Cow::Owned`] and
/// reuse [`SourceItems`] verbatim. Only the in-memory source can hand out a
/// borrow, and it is the only one that yields [`Cow::Borrowed`].
///
/// `Owned` must never be constructed from [`SourceItems::Memory`]: the
/// in-memory owned iterator *drains* its groups, whereas `items()` is
/// non-destructive. `PlaylistSource::items` upholds this.
pub enum SourceCowItems<'a> {
    Owned(SourceItems<'a>),
    Borrowed(MemoryItems<'a>),
}

impl<'a> Iterator for SourceCowItems<'a> {
    type Item = Cow<'a, PlaylistItem>;

    #[inline]
    fn next(&mut self) -> Option<Cow<'a, PlaylistItem>> {
        match self {
            Self::Owned(items) => items.next().map(Cow::Owned),
            Self::Borrowed(items) => items.next().map(Cow::Borrowed),
        }
    }
}

/// Every playlist item of one source, mutably.
///
/// Disk-backed sources are read-only, so they are always `Empty`; the warning
/// that used to be emitted from `items_mut` is now emitted where the `Empty`
/// variant is built, which keeps the observable behaviour identical.
pub enum SourceItemsMut<'a> {
    Empty,
    Memory(MemoryItemsMut<'a>),
}

impl<'a> Iterator for SourceItemsMut<'a> {
    type Item = &'a mut PlaylistItem;

    #[inline]
    fn next(&mut self) -> Option<&'a mut PlaylistItem> {
        match self {
            Self::Empty => None,
            Self::Memory(items) => items.next(),
        }
    }
}

// ---------------------------------------------------------------------------
// Skip-set filtering
// ---------------------------------------------------------------------------

/// Reads an item's Xtream cluster regardless of how the iterator yields it.
///
/// Lets one filter serve the owned, `Cow` and mutable iterators instead of
/// three near-identical closures.
pub trait HasCluster {
    fn cluster(&self) -> XtreamCluster;
}

impl HasCluster for PlaylistItem {
    #[inline]
    fn cluster(&self) -> XtreamCluster { self.header.xtream_cluster }
}

impl HasCluster for &mut PlaylistItem {
    #[inline]
    fn cluster(&self) -> XtreamCluster { self.header.xtream_cluster }
}

impl HasCluster for Cow<'_, PlaylistItem> {
    #[inline]
    fn cluster(&self) -> XtreamCluster { self.as_ref().header.xtream_cluster }
}

/// Drops items whose cluster is in the skip set.
///
/// The skip set used to be applied by boxing the inner iterator a *second*
/// time inside a `.filter(..)`. Folding it in here means a filtered traversal
/// allocates nothing at all.
pub struct ClusterFiltered<I> {
    inner: I,
    skip: Option<Arc<HashSet<XtreamCluster>>>,
}

impl<I> ClusterFiltered<I> {
    #[inline]
    pub fn new(inner: I, skip: Option<Arc<HashSet<XtreamCluster>>>) -> Self { Self { inner, skip } }
}

impl<I> Iterator for ClusterFiltered<I>
where
    I: Iterator,
    I::Item: HasCluster,
{
    type Item = I::Item;

    #[inline]
    fn next(&mut self) -> Option<I::Item> {
        let Some(skip) = self.skip.as_ref() else {
            return self.inner.next();
        };
        loop {
            let item = self.inner.next()?;
            if !skip.contains(&item.cluster()) {
                return Some(item);
            }
        }
    }
}
