use shared::model::XtreamCluster;
use std::{cell::RefCell, fmt, ops::Deref, rc::Rc};
use yew::prelude::*;

/// Identifies the active explorer source, catalog revision, target, and cluster so async
/// results from an earlier source, reload, target, cluster, or catalog edit cannot mutate
/// or overwrite a newer state.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CatalogGeneration {
    /// Incremented on every external playlist load, reload, or source switch.
    pub source_revision: u32,
    /// Incremented on every `context.playlist` replacement (external loads and local edits).
    pub catalog_revision: u32,
    pub target_name: Option<String>,
    pub cluster: XtreamCluster,
}

impl CatalogGeneration {
    /// Returns `true` when `current` still belongs to the same source reload, target, and cluster.
    #[must_use]
    pub fn matches_source(&self, current: &Self) -> bool {
        self.source_revision == current.source_revision
            && self.target_name == current.target_name
            && self.cluster == current.cluster
    }

    /// Returns `true` when `current` matches the exact catalog generation, including local edits.
    #[must_use]
    pub fn matches_catalog(&self, current: &Self) -> bool { self == current }
}

/// Group-order update payload emitted after saving or conflict-reloading a cluster's order.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GroupOrderUpdate {
    pub generation: CatalogGeneration,
    pub groups: Vec<String>,
    /// `Some(pending)` after a save; `None` when reloading the stored order after a conflict.
    pub pending: Option<bool>,
}

/// Identity handle for hook dependencies and component props.
///
/// `Rc<T>` implements `PartialEq` by comparing the pointed-to values. Playlist
/// categories hold hundreds of thousands of items, so using a raw `Rc` as a
/// `use_memo`/`use_effect_with` dependency or as a `#[derive(PartialEq)]`
/// component prop turns every render into several full catalog scans.
///
/// `Id` compares by pointer only: two handles are equal exactly when they wrap
/// the same allocation. Values that never change in place can therefore be
/// passed around for free.
pub struct Id<T: ?Sized>(Rc<T>);

impl<T> Id<T> {
    pub fn new(inner: Rc<T>) -> Self { Self(inner) }

    pub fn from_option(inner: Option<Rc<T>>) -> Option<Self> { inner.map(Self) }

    pub fn into_inner(self) -> Rc<T> { self.0 }
}

impl<T> Clone for Id<T> {
    fn clone(&self) -> Self { Self(Rc::clone(&self.0)) }
}

impl<T> PartialEq for Id<T> {
    fn eq(&self, other: &Self) -> bool { Rc::ptr_eq(&self.0, &other.0) }
}

impl<T> Deref for Id<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target { &self.0 }
}

impl<T: fmt::Debug> fmt::Debug for Id<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { fmt::Debug::fmt(&self.0, f) }
}

/// Counts how often the value behind `value` was replaced by a different allocation.
///
/// Memo dependencies can use the returned revision instead of the playlist value,
/// which keeps a render at the cost of a pointer comparison instead of a catalog scan.
#[hook]
pub fn use_identity_revision<T: 'static>(value: &Option<Rc<T>>) -> u32 {
    let seen = use_mut_ref(|| value.clone());
    let revision = use_mut_ref(|| 0u32);

    let changed = {
        let seen_ref = seen.borrow();
        match (seen_ref.as_ref(), value.as_ref()) {
            (Some(previous), Some(current)) => !Rc::ptr_eq(previous, current),
            (None, None) => false,
            _ => true,
        }
    };

    if changed {
        *seen.borrow_mut() = value.clone();
        let mut revision = revision.borrow_mut();
        *revision = revision.wrapping_add(1);
    }

    read_revision(&revision)
}

/// Copies the counter out of its `RefCell`; reading it in the hook body would let the
/// `Ref` temporary outlive the handle, which the borrow checker rejects in tail position.
fn read_revision(handle: &Rc<RefCell<u32>>) -> u32 { *handle.borrow() }

/// Returns the effective override identity of a playlist item.
///
/// Uses `explorer_meta.override_id` when present, falling back to `provider_id`.
#[must_use]
pub fn item_override_id(item: &shared::model::UiPlaylistItem) -> &str {
    item.explorer_meta.as_ref().and_then(|m| m.override_id.as_deref()).unwrap_or(item.provider_id.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{PlaylistItemType, UiItemExplorerMeta, UiPlaylistItem, XtreamCluster};
    use std::sync::Arc;

    #[test]
    fn item_override_id_prefers_override_id_over_provider_id() {
        let item_with_override = UiPlaylistItem {
            virtual_id: 1,
            provider_id: Arc::from("pid-123"),
            name: Arc::from("Channel"),
            title: Arc::from("Channel"),
            group: Arc::from("Group"),
            logo: Arc::from(""),
            url: Arc::from(""),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            category_id: 1,
            rating: 0.0,
            input_name: Arc::from("input"),
            input_stream_id: "".into(),
            epg_channel_id: None,
            explorer_meta: Some(UiItemExplorerMeta {
                override_id: Some(Arc::from("urlhash-v1:abc123blake3")),
                ..Default::default()
            }),
            ..Default::default()
        };

        assert_eq!(item_override_id(&item_with_override), "urlhash-v1:abc123blake3");

        let item_without_override = UiPlaylistItem {
            virtual_id: 2,
            provider_id: Arc::from("pid-456"),
            name: Arc::from("Channel"),
            title: Arc::from("Channel"),
            group: Arc::from("Group"),
            logo: Arc::from(""),
            url: Arc::from(""),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            category_id: 1,
            rating: 0.0,
            input_name: Arc::from("input"),
            input_stream_id: "".into(),
            epg_channel_id: None,
            explorer_meta: None,
            ..Default::default()
        };

        assert_eq!(item_override_id(&item_without_override), "pid-456");
    }

    #[test]
    fn equal_values_in_different_allocations_are_not_equal() {
        let left = Id::new(Rc::new(vec![1, 2, 3]));
        let right = Id::new(Rc::new(vec![1, 2, 3]));
        assert_ne!(left, right);
    }

    #[test]
    fn same_allocation_is_equal() {
        let value = Rc::new(vec![1, 2, 3]);
        assert_eq!(Id::new(value.clone()), Id::new(value));
    }

    #[test]
    fn none_handles_are_equal() {
        assert_eq!(Id::<u8>::from_option(None), Id::<u8>::from_option(None));
    }

    #[test]
    fn catalog_generation_distinguishes_local_edits_from_reloads_and_switches() {
        let base = CatalogGeneration {
            source_revision: 1,
            catalog_revision: 3,
            target_name: Some("main".to_string()),
            cluster: XtreamCluster::Live,
        };
        let after_local_edit = CatalogGeneration { catalog_revision: 4, ..base.clone() };
        let after_reload = CatalogGeneration { source_revision: 2, catalog_revision: 4, ..base.clone() };
        let other_target = CatalogGeneration { target_name: Some("backup".to_string()), ..base.clone() };
        let other_cluster = CatalogGeneration { cluster: XtreamCluster::Video, ..base.clone() };

        assert!(base.matches_catalog(&base));
        assert!(base.matches_source(&base));

        // Local edits preserve source identity while invalidating exact catalog snapshots.
        assert!(base.matches_source(&after_local_edit));
        assert!(!base.matches_catalog(&after_local_edit));

        // Reloads, target switches, and cluster switches invalidate both checks.
        assert!(!base.matches_source(&after_reload));
        assert!(!base.matches_catalog(&after_reload));
        assert!(!base.matches_source(&other_target));
        assert!(!base.matches_source(&other_cluster));
    }
}
