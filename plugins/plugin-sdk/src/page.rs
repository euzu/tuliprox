//! The paging convention for tools and rpc methods that return a list.
//!
//! A list-returning export takes an optional `limit` (the page size) and an
//! optional `cursor` (the position of the first item), and answers
//! `{"items": [...], "next_cursor": "…"}` with a JSON `null` as `next_cursor`
//! on the last page:
//!
//! ```json
//! {"items": [{"id": "a"}, {"id": "b"}], "next_cursor": "2"}
//! ```
//!
//! [`Page`] is the answer type; [`Page::slice`] cuts a `Vec` into the page a
//! cursor names, so a plugin does not have to implement the arithmetic itself.
//! The cursor is opaque to the caller: you may change the encoding, but a
//! client must only ever pass back what it received.
//!
//! ```ignore
//! use tuliprox_plugin_sdk::page::Page;
//!
//! fn search(args: &serde_json::Value, all: Vec<Item>) -> Result<String, String> {
//!     let cursor = args.get("cursor").and_then(serde_json::Value::as_str);
//!     let limit = args.get("limit").and_then(serde_json::Value::as_u64).unwrap_or(50) as usize;
//!     let page = Page::slice(all, cursor, limit)?;
//!     serde_json::to_string(&page).map_err(|err| err.to_string())
//! }
//! ```

use serde::{Deserialize, Serialize};

/// One page of a list, as a list-returning export answers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page<T> {
    /// The items of this page, in the order the export produces them.
    pub items: Vec<T>,
    /// Where the next page starts, or `None` on the last page.
    #[serde(default)]
    pub next_cursor: Option<String>,
}

impl<T> Page<T> {
    /// `items` as the only page.
    #[must_use]
    pub fn last(items: Vec<T>) -> Self { Self { items, next_cursor: None } }

    /// Cuts `items` into the page `cursor` names, at most `limit` items long.
    ///
    /// `cursor` is `None` (the first page) or a value a previous `next_cursor`
    /// gave. `next_cursor` of the answer is `Some` exactly when items remain.
    ///
    /// # Errors
    /// A `limit` of zero, and a cursor that is not a position this helper
    /// produced (not a number, or past the end of the list). An empty string is
    /// read as the first page, because some clients send empty parameters.
    pub fn slice(items: Vec<T>, cursor: Option<&str>, limit: usize) -> Result<Self, String> {
        if limit == 0 {
            return Err("limit must be at least 1".to_string());
        }
        let total = items.len();
        let start = match cursor.map(str::trim) {
            None | Some("") => 0,
            Some(cursor) => cursor
                .parse::<usize>()
                .map_err(|_| format!("cursor must be a `next_cursor` of a previous answer, not `{}`", short(cursor)))?,
        };
        if start > total {
            return Err(format!(
                "cursor {} is past the end of the list ({total} items); request the first page again",
                short(&start.to_string())
            ));
        }
        let page: Vec<T> = items.into_iter().skip(start).take(limit).collect();
        let next = start.saturating_add(page.len());
        Ok(Self { items: page, next_cursor: (next < total).then(|| next.to_string()) })
    }
}

/// A cursor or limit as it came in, cut down and stripped of control characters
/// so an error message never carries a hostile string into a log.
fn short(text: &str) -> String { text.chars().filter(|c| !c.is_control()).take(32).collect() }

#[cfg(test)]
mod tests {
    use super::*;

    fn one_through_five() -> Vec<u32> { (1..=5).collect() }

    #[test]
    fn the_first_page_starts_at_the_first_item() {
        let page = Page::slice(one_through_five(), None, 2).unwrap();
        assert_eq!(page, Page { items: vec![1, 2], next_cursor: Some("2".to_string()) });
        let empty_cursor = Page::slice(one_through_five(), Some(""), 2).unwrap();
        assert_eq!(empty_cursor, page, "an empty cursor reads as the first page");
    }

    #[test]
    fn the_last_page_has_no_next_cursor() {
        let page = Page::slice(one_through_five(), Some("4"), 2).unwrap();
        assert_eq!(page, Page { items: vec![5], next_cursor: None });
        // Exactly the rest in one page is still the last page.
        let page = Page::slice(one_through_five(), Some("2"), 3).unwrap();
        assert_eq!(page, Page { items: vec![3, 4, 5], next_cursor: None });
    }

    #[test]
    fn walking_the_cursors_yields_every_item_once() {
        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = Page::slice(one_through_five(), cursor.as_deref(), 2).unwrap();
            seen.extend(page.items);
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(seen, one_through_five());
    }

    #[test]
    fn an_empty_list_is_one_empty_last_page() {
        assert_eq!(Page::<u32>::slice(Vec::new(), None, 10).unwrap(), Page::last(Vec::new()));
    }

    #[test]
    fn a_cursor_past_the_end_or_not_a_number_is_an_error() {
        assert!(Page::slice(one_through_five(), Some("6"), 2).unwrap_err().contains("past the end"));
        assert!(Page::slice(one_through_five(), Some("abc"), 2).unwrap_err().contains("not `abc`"));
        assert!(Page::slice(one_through_five(), Some("-1"), 2).is_err());
        // A cursor of exactly the length is an empty last page, not an error.
        assert_eq!(Page::slice(one_through_five(), Some("5"), 2).unwrap(), Page::last(Vec::new()));
    }

    #[test]
    fn a_zero_limit_is_an_error_and_a_hostile_cursor_is_cut() {
        assert!(Page::slice(one_through_five(), None, 0).unwrap_err().contains("at least 1"));
        let hostile = format!("{}\u{1b}[31m", "x".repeat(100));
        let message = Page::slice(one_through_five(), Some(&hostile), 2).unwrap_err();
        assert!(message.len() < 100, "{message}");
        assert!(!message.contains('\u{1b}'), "{message}");
    }

    #[test]
    fn the_page_serializes_as_items_and_next_cursor() {
        let page = Page::slice(vec!["a", "b"], None, 1).unwrap();
        assert_eq!(serde_json::to_value(&page).unwrap(), serde_json::json!({ "items": ["a"], "next_cursor": "1" }));
        let page = Page::last(vec!["a"]);
        assert_eq!(serde_json::to_value(&page).unwrap(), serde_json::json!({ "items": ["a"], "next_cursor": null }));
        // A missing `next_cursor` reads as the last page.
        let decoded: Page<String> = serde_json::from_str(r#"{"items":["a"]}"#).unwrap();
        assert_eq!(decoded, Page::last(vec!["a".to_string()]));
    }
}
