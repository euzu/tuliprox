use crate::defaults::{default_page, default_page_size};
use serde::{Deserialize, Serialize};

/// Search mode for paged queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    Text,
    Regex,
}

/// Request for a single page of a paged collection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageRequestDto {
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_page_size")]
    pub page_size: u16,
}

/// Valid page sizes allowed by the backend.
pub const VALID_PAGE_SIZES: &[u16] = &[25, 50, 100, 200];

/// Hard maximum page size enforced by the backend.
pub const MAX_PAGE_SIZE: u16 = 200;

impl PageRequestDto {
    /// Normalizes page to be at least 1.
    pub fn normalize_page(&mut self) {
        if self.page < 1 {
            self.page = 1;
        }
    }

    /// Clamps page_size to the allowed range [25, 200] and snaps to nearest valid value.
    pub fn normalize_page_size(&mut self) {
        let mut size = self.page_size;
        if size < VALID_PAGE_SIZES[0] {
            size = VALID_PAGE_SIZES[0];
        } else if size > MAX_PAGE_SIZE {
            size = MAX_PAGE_SIZE;
        }
        // Snap to nearest valid value
        self.page_size = VALID_PAGE_SIZES.iter().min_by_key(|&&s| s.abs_diff(size)).copied().unwrap_or(50);
    }

    /// Creates a normalized request with defaults applied.
    pub fn normalized(page: u32, page_size: u16) -> Self {
        let mut req = Self { page, page_size };
        req.normalize_page();
        req.normalize_page_size();
        req
    }
}

/// Response for a paged query containing a slice of items.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PagedResponseDto<T: PartialEq> {
    pub items: Vec<T>,
    pub page: u32,
    pub page_size: u16,
    pub total_items: u64,
    pub total_pages: u32,
    pub has_prev: bool,
    pub has_next: bool,
}

impl<T: PartialEq> PagedResponseDto<T> {
    /// Constructs a paged response from a full items vec and pagination parameters.
    /// `all_items` is the complete filtered+aggregated result set (already sorted).
    pub fn new(items: Vec<T>, page: u32, page_size: u16, total_items: u64) -> Self {
        let total_pages = if total_items == 0 { 0 } else { ((total_items as f64) / (page_size as f64)).ceil() as u32 };
        let has_prev = page > 1;
        let has_next = total_items > 0 && page < total_pages;
        Self { items, page, page_size, total_items, total_pages, has_prev, has_next }
    }

    /// Returns the 1-based start index of the current page (1-indexed for display).
    pub fn range_start(&self) -> u64 {
        if self.total_items == 0 {
            0
        } else {
            ((self.page - 1) * self.page_size as u32) as u64 + 1
        }
    }

    /// Returns the 1-based end index of the current page.
    pub fn range_end(&self) -> u64 {
        let end = (self.page as u64) * self.page_size as u64;
        end.min(self.total_items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_request_normalizes_page_to_at_least_1() {
        let mut req = PageRequestDto { page: 0, page_size: 50 };
        req.normalize_page();
        assert_eq!(req.page, 1);
    }

    #[test]
    fn page_request_normalizes_page_size_clamps_and_snaps() {
        let mut req = PageRequestDto { page: 1, page_size: 10 };
        req.normalize_page_size();
        assert_eq!(req.page_size, 25); // snaps to nearest valid: 25

        let mut req2 = PageRequestDto { page: 1, page_size: 200 };
        req2.normalize_page_size();
        assert_eq!(req2.page_size, 200); // exact valid

        let mut req3 = PageRequestDto { page: 1, page_size: 150 };
        req3.normalize_page_size();
        assert_eq!(req3.page_size, 100); // nearest of 25,50,100,200
    }

    #[test]
    fn paged_response_empty_result() {
        let resp = PagedResponseDto::<i32>::new(Vec::new(), 1, 50, 0);
        assert_eq!(resp.total_pages, 0);
        assert!(!resp.has_prev);
        assert!(!resp.has_next);
        assert_eq!(resp.range_start(), 0);
        assert_eq!(resp.range_end(), 0);
    }

    #[test]
    fn paged_response_partial_page() {
        let items = vec![1, 2, 3];
        let resp = PagedResponseDto::new(items, 1, 50, 3);
        assert_eq!(resp.total_pages, 1);
        assert!(!resp.has_prev);
        assert!(!resp.has_next);
        assert_eq!(resp.range_start(), 1);
        assert_eq!(resp.range_end(), 3);
    }

    #[test]
    fn paged_response_exact_pages() {
        let items = vec![1; 50];
        let resp = PagedResponseDto::new(items, 2, 50, 100);
        assert_eq!(resp.total_pages, 2);
        assert!(resp.has_prev);
        assert!(!resp.has_next); // on last page
        assert_eq!(resp.range_start(), 51);
        assert_eq!(resp.range_end(), 100);
    }

    #[test]
    fn paged_response_has_next_when_more_pages() {
        let resp = PagedResponseDto::new(vec![1; 50], 1, 50, 150);
        assert!(!resp.has_prev); // page 1 with total > 50 has no prev
        assert!(resp.has_next);
        assert_eq!(resp.total_pages, 3);
    }

    #[test]
    fn paged_response_page_1_has_no_prev() {
        let resp = PagedResponseDto::new(vec![1; 50], 1, 50, 100);
        assert!(!resp.has_prev);
    }
}
