use super::{build_pagination_items, pagination_range, PaginationItem};

#[test]
fn pagination_items_show_all_pages_for_small_page_counts() {
    assert_eq!(
        build_pagination_items(1, 5),
        vec![
            PaginationItem::Page(1),
            PaginationItem::Page(2),
            PaginationItem::Page(3),
            PaginationItem::Page(4),
            PaginationItem::Page(5),
        ]
    );
}

#[test]
fn pagination_items_collapse_middle_near_start() {
    assert_eq!(
        build_pagination_items(1, 9),
        vec![
            PaginationItem::Page(1),
            PaginationItem::Page(2),
            PaginationItem::Page(3),
            PaginationItem::Ellipsis,
            PaginationItem::Page(8),
            PaginationItem::Page(9),
        ]
    );
}

#[test]
fn pagination_items_show_window_around_current_page() {
    assert_eq!(
        build_pagination_items(6, 11),
        vec![
            PaginationItem::Page(1),
            PaginationItem::Ellipsis,
            PaginationItem::Page(4),
            PaginationItem::Page(5),
            PaginationItem::Page(6),
            PaginationItem::Page(7),
            PaginationItem::Page(8),
            PaginationItem::Ellipsis,
            PaginationItem::Page(11),
        ]
    );
}

#[test]
fn pagination_ranges_cover_empty_partial_and_invalid_pages() {
    assert_eq!(pagination_range(1, 25, 0), (0, 0));
    assert_eq!(pagination_range(1, 25, 60), (1, 25));
    assert_eq!(pagination_range(3, 25, 60), (51, 60));
    assert_eq!(pagination_range(0, 25, 60), (1, 25));
    assert_eq!(pagination_range(1, 0, 60), (1, 1));
    assert_eq!(pagination_range(4, 25, 60), (60, 60));
}

#[test]
fn pagination_ranges_widen_before_multiplication() {
    let page = u32::MAX;
    let size = u16::MAX;
    let end = u64::from(page) * u64::from(size);
    assert_eq!(pagination_range(page, size, u64::MAX), (end - u64::from(size) + 1, end));
    assert_eq!(build_pagination_items(page, page).last(), Some(&PaginationItem::Page(page)));
}
