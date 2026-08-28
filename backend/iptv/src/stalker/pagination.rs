//! Page arithmetic shared by every Stalker catalog fetch.
//!
//! A Stalker portal advertises three independent, individually-optional hints next to a
//! page of rows — `total_items`, `max_page_items` (the page *size*) and `max_page` (the
//! page *count*) — and a page is the last one when any of them says so, or when it came
//! back short. That rule used to be recomputed in four places: the accumulating loop in
//! [`super::catalog`], the two `parse_*_catalog_page` helpers, and the `apply_page_limit`
//! guard. This module is the single copy.

use crate::stalker::error::StalkerResult;
use serde_json::Value;

/// The pagination hints a portal advertises alongside a page of rows. Every field is
/// optional because portals disagree about which ones they emit — the terminal test
/// simply ignores whichever are absent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PageMeta {
    /// Total row count across all pages.
    pub total_items: Option<u32>,
    /// Rows per page. Named for the portal's field; it is a page *size*, not a count.
    pub max_page_items: Option<u32>,
    /// Total page count. Strictly distinct from `max_page_items`.
    pub max_page: Option<u32>,
}

/// One page of catalog rows plus the cursor needed to ask for the next one.
#[derive(Debug)]
pub struct CatalogPage<T> {
    pub items: Vec<T>,
    /// `None` once [`PageMeta::is_terminal`] says the catalog is exhausted.
    pub next_page: Option<u32>,
    pub total: Option<u32>,
}

impl PageMeta {
    /// Extract the hints from a catalog response body, unwrapping the `js` envelope.
    #[must_use]
    pub fn from_value(value: &Value) -> Self {
        let Some(object) = catalog_js(value).as_object() else {
            return Self::default();
        };
        let field = |key: &str| object.get(key).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok());
        Self { total_items: field("total_items"), max_page_items: field("max_page_items"), max_page: field("max_page") }
    }

    /// True when `current_page` is the last page worth asking for.
    ///
    /// `page_len` is the number of rows this page yielded and `fetched_so_far` the running
    /// total including them. Callers that hold only one page pass the best estimate they
    /// have (`current_page * max_page_items`); callers that accumulate pass the real count.
    #[must_use]
    pub fn is_terminal(&self, page_len: usize, current_page: u32, fetched_so_far: usize) -> bool {
        // An empty page is always the end: portals stop emitting rows rather than
        // reporting the last page number reliably.
        if page_len == 0 {
            return true;
        }
        if self.total_items.is_some_and(|total| fetched_so_far >= as_usize(total)) {
            return true;
        }
        // A short page — fewer rows than the advertised page size — is the last page.
        if self.max_page_items.is_some_and(|size| page_len < as_usize(size)) {
            return true;
        }
        self.max_page.is_some_and(|last| current_page >= last)
    }

    /// The row count a caller holding only `current_page` can claim to have fetched.
    /// Zero when the portal did not advertise a page size, which makes the `total_items`
    /// arm of [`Self::is_terminal`] inert rather than guessing.
    #[must_use]
    pub fn fetched_estimate(&self, current_page: u32) -> usize {
        self.max_page_items.map_or(0, |size| as_usize(current_page.saturating_mul(size)))
    }

    /// Build the page cursor for a response, applying [`Self::is_terminal`].
    #[must_use]
    pub fn into_page<T>(self, items: Vec<T>, current_page: u32) -> CatalogPage<T> {
        let terminal = self.is_terminal(items.len(), current_page, self.fetched_estimate(current_page));
        let next_page = (!terminal).then(|| current_page.checked_add(1)).flatten();
        CatalogPage { items, next_page, total: self.total_items }
    }
}

fn as_usize(value: u32) -> usize { usize::try_from(value).unwrap_or(usize::MAX) }

/// Unwrap the `js` envelope Stalker portals wrap every response in, falling back to the
/// document itself for the portals that do not.
pub fn catalog_js(value: &Value) -> &Value { value.as_object().and_then(|map| map.get("js")).unwrap_or(value) }

/// Where a catalog fetch delivers its rows.
///
/// The Stalker client tries several endpoint candidates in turn, and a failure part-way
/// through pagination means abandoning that candidate and starting over on the next one.
/// That retry is only sound if nothing has left the client yet, which is exactly what
/// separates the two sinks: [`CollectSink`] holds everything until the catalog is complete
/// and can therefore restart freely, while a streaming sink that has already handed rows
/// to its caller cannot, and says so through [`CatalogSink::can_restart`].
pub trait CatalogSink<T> {
    /// Take one page of rows. Returning `Err` aborts the whole fetch.
    fn accept(&mut self, batch: Vec<T>) -> impl std::future::Future<Output = StalkerResult<()>> + Send;

    /// Whether the driver may abandon a half-fetched candidate and retry the next one.
    fn can_restart(&self) -> bool;

    /// Discard whatever the abandoned candidate produced. Only called when
    /// [`Self::can_restart`] returned `true`.
    fn restart(&mut self);
}

/// Accumulates the whole catalog in memory. This is the historical behaviour, and the one
/// that keeps the retry-from-scratch guarantee: a truncated catalog is never returned as
/// `Ok`, because nothing is visible to the caller until the fetch has completed.
#[derive(Debug, Default)]
pub struct CollectSink<T> {
    rows: Vec<T>,
}

impl<T> CollectSink<T> {
    #[must_use]
    pub fn new() -> Self {
        Self { rows: Vec::new() }
    }

    #[must_use]
    pub fn into_rows(self) -> Vec<T> {
        self.rows
    }
}

impl<T: Send> CatalogSink<T> for CollectSink<T> {
    fn accept(&mut self, mut batch: Vec<T>) -> impl std::future::Future<Output = StalkerResult<()>> + Send {
        self.rows.append(&mut batch);
        async { Ok(()) }
    }

    fn can_restart(&self) -> bool {
        true
    }

    fn restart(&mut self) {
        self.rows.clear();
    }
}

/// Forwards each page to a caller-supplied callback as it arrives, so a large catalog
/// never has to be resident all at once.
///
/// The trade is stated in [`CatalogSink`]: once a page has been handed over, the fetch can
/// no longer fall back to another endpoint candidate. An `Err` from a streaming fetch
/// therefore means the batches already delivered are an incomplete prefix and must be
/// discarded by the caller.
pub struct BatchSink<F> {
    on_batch: F,
    emitted_any: bool,
}

impl<F> BatchSink<F> {
    pub fn new(on_batch: F) -> Self {
        Self { on_batch, emitted_any: false }
    }

    /// Whether any page reached the callback. Useful to a caller deciding whether an empty
    /// catalog was genuinely empty.
    #[must_use]
    pub fn emitted_any(&self) -> bool {
        self.emitted_any
    }
}

impl<T, F, Fut> CatalogSink<T> for BatchSink<F>
where
    T: Send,
    F: FnMut(Vec<T>) -> Fut + Send,
    Fut: std::future::Future<Output = StalkerResult<()>> + Send,
{
    fn accept(&mut self, batch: Vec<T>) -> impl std::future::Future<Output = StalkerResult<()>> + Send {
        self.emitted_any = true;
        (self.on_batch)(batch)
    }

    fn can_restart(&self) -> bool {
        !self.emitted_any
    }

    fn restart(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::{catalog_js, PageMeta};

    #[test]
    fn empty_page_terminates_regardless_of_hints() {
        let meta = PageMeta { total_items: Some(1000), max_page_items: Some(50), max_page: Some(20) };
        assert!(meta.is_terminal(0, 1, 0));
    }

    #[test]
    fn short_page_terminates() {
        let meta = PageMeta { total_items: None, max_page_items: Some(50), max_page: None };
        assert!(meta.is_terminal(17, 3, 117));
        assert!(!meta.is_terminal(50, 3, 150));
    }

    #[test]
    fn accumulated_total_terminates() {
        let meta = PageMeta { total_items: Some(100), max_page_items: Some(50), max_page: None };
        assert!(meta.is_terminal(50, 2, 100));
        assert!(!meta.is_terminal(50, 1, 50));
    }

    #[test]
    fn max_page_is_a_page_count_not_a_page_size() {
        let meta = PageMeta { total_items: None, max_page_items: Some(50), max_page: Some(3) };
        assert!(meta.is_terminal(50, 3, 150));
        assert!(!meta.is_terminal(50, 2, 100));
    }

    #[test]
    fn missing_page_size_leaves_the_total_arm_inert() {
        // Without `max_page_items` a single-page caller cannot estimate progress, so the
        // `total_items` arm must not fire and truncate the catalog at page one.
        let meta = PageMeta { total_items: Some(100), max_page_items: None, max_page: None };
        assert_eq!(meta.fetched_estimate(1), 0);
        assert!(!meta.is_terminal(30, 1, meta.fetched_estimate(1)));
    }

    #[test]
    fn into_page_advances_until_terminal() {
        let meta = PageMeta { total_items: Some(100), max_page_items: Some(50), max_page: None };
        assert_eq!(meta.into_page(vec![0_u8; 50], 1).next_page, Some(2));
        assert_eq!(meta.into_page(vec![0_u8; 50], 2).next_page, None);
    }

    #[test]
    fn meta_is_read_from_the_js_envelope_or_the_root() {
        let wrapped = serde_json::json!({"js": {"total_items": 7, "max_page_items": 3, "max_page": 3}});
        assert_eq!(
            PageMeta::from_value(&wrapped),
            PageMeta { total_items: Some(7), max_page_items: Some(3), max_page: Some(3) }
        );
        let bare = serde_json::json!({"total_items": 7});
        assert_eq!(PageMeta::from_value(&bare).total_items, Some(7));
        assert!(catalog_js(&bare).is_object());
    }
}
