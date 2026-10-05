pub(super) const TIME_BLOCK_MINS: i64 = 30;
pub(super) const DEFAULT_PIXELS_PER_MIN: f64 = 7.0; // 210px / 30min
pub(super) const MIN_PIXELS_PER_MIN: f64 = 2.0;
pub(super) const MAX_PIXELS_PER_MIN: f64 = 28.0;
/// Zoom per wheel pixel: one mouse notch (about 100 px) zooms by about 12 %.
const WHEEL_ZOOM_PER_PX: f64 = 0.0012;
/// Upper bound of the zoom change from a single wheel event.
const WHEEL_ZOOM_MAX_STEP: f64 = 1.25;
const WHEEL_LINE_PX: f64 = 16.0;
pub(super) const ZOOM_EQUALITY_TOLERANCE: f64 = 0.01;
pub(super) const FALLBACK_ROW_HEIGHT_PX: f64 = 64.0;
pub(super) const FALLBACK_HEADER_HEIGHT_PX: f64 = 40.0;

#[derive(Clone, Copy)]
pub(super) struct TimelineZoom(f64);

impl TimelineZoom {
    pub(super) fn new(pixels_per_min: f64) -> Self {
        Self(pixels_per_min.clamp(MIN_PIXELS_PER_MIN, MAX_PIXELS_PER_MIN))
    }

    pub(super) fn value(self) -> f64 { self.0 }
}

impl PartialEq for TimelineZoom {
    fn eq(&self, other: &Self) -> bool { (self.0 - other.0).abs() < ZOOM_EQUALITY_TOLERANCE }
}

/// Zoom factor for one wheel event, or `None` when the event is a horizontal scroll (or
/// empty) and should scroll natively. `delta_mode`: 0 = pixels, 1 = lines, 2 = pages.
pub(super) fn wheel_zoom_factor(delta_x: f64, delta_y: f64, delta_mode: u32, page_height: f64) -> Option<f64> {
    if delta_y == 0.0 || delta_x.abs() > delta_y.abs() {
        return None;
    }
    let delta_px = match delta_mode {
        1 => delta_y * WHEEL_LINE_PX,
        2 => delta_y * page_height,
        _ => delta_y,
    };
    Some((-delta_px * WHEEL_ZOOM_PER_PX).exp().clamp(1.0 / WHEEL_ZOOM_MAX_STEP, WHEEL_ZOOM_MAX_STEP))
}

/// Zoom anchor: the minute (relative to the window start) that stays under the pointer at
/// `viewport_x` (pointer x relative to the scroll container). It is valid for every zoom
/// level, so it survives several zoom steps before the DOM has caught up.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ZoomAnchor {
    pub viewport_x: f64,
    pub minute: f64,
}

/// Minute at `viewport_x` for a scroll position. Programme content starts after the sticky
/// channel column of width `channels_width`.
pub(super) fn minute_at(scroll_left: f64, viewport_x: f64, channels_width: f64, pixels_per_min: f64) -> f64 {
    (scroll_left + viewport_x - channels_width).max(0.0) / pixels_per_min.max(f64::EPSILON)
}

/// Scroll position that puts the anchor minute under its viewport x at the given zoom.
pub(super) fn anchor_scroll_left(anchor: ZoomAnchor, channels_width: f64, pixels_per_min: f64) -> f64 {
    (anchor.minute * pixels_per_min + channels_width - anchor.viewport_x).max(0.0)
}

/// Moves an existing anchor to a new pointer position, measured at the zoom it was set for.
pub(super) fn shift_anchor(anchor: ZoomAnchor, viewport_x: f64, pixels_per_min: f64) -> ZoomAnchor {
    ZoomAnchor {
        viewport_x,
        minute: anchor.minute + (viewport_x - anchor.viewport_x) / pixels_per_min.max(f64::EPSILON),
    }
}

pub(super) fn compute_panned_scroll(start_scroll: i32, start_pointer: i32, current_pointer: i32) -> i32 {
    start_scroll - (current_pointer - start_pointer)
}

pub(super) fn min_to_px(min: i64, pixels_per_min: f64) -> i64 { (min as f64 * pixels_per_min).round() as i64 }

pub(super) const ROW_OVERSCAN: usize = 3;
/// Horizontal overscan per side, as a fraction of the visible programme width.
pub(super) const HORIZONTAL_OVERSCAN_FACTOR: f64 = 0.5;
/// Time window bounds are rounded to this many minutes, so small horizontal scrolls keep
/// the same window and do not re-render the rows.
pub(super) const MINUTE_QUANTUM: i64 = 60;

/// Rows and minutes (relative to the window start) that are rendered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Viewport {
    pub row_start: usize,
    pub row_end: usize,
    pub from_min: i64,
    pub to_min: i64,
}

impl Default for Viewport {
    fn default() -> Self { Self { row_start: 0, row_end: 20, from_min: 0, to_min: 240 } }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Geometry {
    pub row_height: f64,
    pub header_height: f64,
    pub channels_width: f64,
    pub pixels_per_min: f64,
    pub total_rows: usize,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ScrollState {
    pub scroll_top: f64,
    pub scroll_left: f64,
    pub client_height: f64,
    pub client_width: f64,
}

pub(super) fn compute_viewport(s: &ScrollState, g: &Geometry) -> Viewport {
    let row_height = g.row_height.max(1.0);
    let content_top = (s.scroll_top - g.header_height).max(0.0);
    let first = (content_top / row_height).floor() as usize;
    let last = ((content_top + s.client_height) / row_height).ceil() as usize;
    let row_start = first.saturating_sub(ROW_OVERSCAN).min(g.total_rows);
    let row_end = (last + ROW_OVERSCAN).min(g.total_rows).max(row_start);

    let ppm = g.pixels_per_min.max(f64::EPSILON);
    let visible_w = (s.client_width - g.channels_width).max(0.0);
    let overscan_w = visible_w * HORIZONTAL_OVERSCAN_FACTOR;
    let from = ((s.scroll_left - overscan_w) / ppm).floor() as i64;
    let to = ((s.scroll_left + visible_w + overscan_w) / ppm).ceil() as i64;
    let from_min = from.div_euclid(MINUTE_QUANTUM) * MINUTE_QUANTUM;
    let to_min = (to + MINUTE_QUANTUM - 1).div_euclid(MINUTE_QUANTUM) * MINUTE_QUANTUM;
    Viewport { row_start, row_end, from_min: from_min.max(0), to_min }
}

#[cfg(test)]
mod tests {
    use super::{
        anchor_scroll_left, compute_panned_scroll, compute_viewport, min_to_px, minute_at, shift_anchor,
        wheel_zoom_factor, Geometry, ScrollState, TimelineZoom, Viewport, ZoomAnchor, MINUTE_QUANTUM,
        MIN_PIXELS_PER_MIN, ROW_OVERSCAN,
    };

    #[test]
    fn timeline_zoom_new_clamps_bounds() {
        assert_eq!(TimelineZoom::new(0.5).value(), 2.0);
        assert_eq!(TimelineZoom::new(999.0).value(), 28.0);
        assert_eq!(TimelineZoom::new(7.0).value(), 7.0);
    }

    #[test]
    fn anchor_keeps_minute_under_pointer_at_any_zoom() {
        // far into the EPG: day 3 under the pointer
        let (scroll, viewport_x, channels) = (30_000.0, 700.0, 300.0);
        let anchor = ZoomAnchor { viewport_x, minute: minute_at(scroll, viewport_x, channels, 7.0) };
        for zoom in [7.7, 6.3, 2.0, 28.0] {
            let next_scroll = anchor_scroll_left(anchor, channels, zoom);
            assert!((minute_at(next_scroll, viewport_x, channels, zoom) - anchor.minute).abs() < 1e-9, "zoom {zoom}");
        }
    }

    #[test]
    fn anchor_stays_valid_when_dom_lags_behind() {
        // Logged failure: zoom 14.129 -> 12.322 rendered, its scroll not yet applied, next step to
        // 9.373 computed from the stale DOM. With an anchor the stale DOM is never read.
        let (viewport_x, channels) = (990.0, 300.0);
        let anchor = ZoomAnchor { viewport_x, minute: minute_at(43_545.0, viewport_x, channels, 14.129) };
        for zoom in [12.322, 9.373, 7.129] {
            let scroll = anchor_scroll_left(anchor, channels, zoom);
            assert!((minute_at(scroll, viewport_x, channels, zoom) - anchor.minute).abs() < 1e-9);
        }
    }

    #[test]
    fn shifted_anchor_follows_pointer_without_moving_content() {
        let channels = 300.0;
        let zoom = 7.0;
        let scroll = 12_000.0;
        let anchor = ZoomAnchor { viewport_x: 500.0, minute: minute_at(scroll, 500.0, channels, zoom) };
        let moved = shift_anchor(anchor, 800.0, zoom);
        assert!((moved.minute - minute_at(scroll, 800.0, channels, zoom)).abs() < 1e-9);
        assert!((anchor_scroll_left(moved, channels, zoom) - scroll).abs() < 1e-9);
    }

    #[test]
    fn anchor_scroll_clamps_at_start() {
        let anchor = ZoomAnchor { viewport_x: 400.0, minute: 10.0 };
        assert_eq!(anchor_scroll_left(anchor, 300.0, 2.0), 0.0);
    }

    #[test]
    fn wheel_zoom_factor_direction_and_magnitude() {
        let zoom_in = wheel_zoom_factor(0.0, -100.0, 0, 800.0).expect("vertical wheel zooms");
        let zoom_out = wheel_zoom_factor(0.0, 100.0, 0, 800.0).expect("vertical wheel zooms");
        assert!(zoom_in > 1.1 && zoom_in < 1.15);
        assert!((zoom_in * zoom_out - 1.0).abs() < 1e-9);
        // small trackpad deltas give small steps
        let small = wheel_zoom_factor(0.0, -4.0, 0, 800.0).expect("vertical wheel zooms");
        assert!(small > 1.0 && small < 1.01);
    }

    #[test]
    fn wheel_zoom_factor_ignores_horizontal_and_empty_scrolls() {
        assert_eq!(wheel_zoom_factor(30.0, 0.0, 0, 800.0), None);
        assert_eq!(wheel_zoom_factor(40.0, 10.0, 0, 800.0), None);
        assert_eq!(wheel_zoom_factor(0.0, 0.0, 0, 800.0), None);
    }

    #[test]
    fn wheel_zoom_factor_normalizes_line_and_page_mode_and_caps_steps() {
        let lines = wheel_zoom_factor(0.0, -3.0, 1, 800.0).expect("line mode zooms");
        let pixels = wheel_zoom_factor(0.0, -48.0, 0, 800.0).expect("pixel mode zooms");
        assert!((lines - pixels).abs() < 1e-12);
        assert_eq!(wheel_zoom_factor(0.0, -1.0, 2, 800.0), Some(1.25));
    }

    #[test]
    fn compute_panned_scroll_moves_in_drag_direction() {
        assert_eq!(compute_panned_scroll(300, 100, 140), 260);
        assert_eq!(compute_panned_scroll(300, 100, 60), 340);
    }

    #[test]
    fn min_to_px_rounds() {
        assert_eq!(min_to_px(30, 7.0), 210);
    }

    fn scroll() -> ScrollState {
        ScrollState { scroll_top: 0.0, scroll_left: 0.0, client_height: 640.0, client_width: 1300.0 }
    }

    fn geo() -> Geometry {
        Geometry { row_height: 64.0, header_height: 40.0, channels_width: 300.0, pixels_per_min: 7.0, total_rows: 1000 }
    }

    #[test]
    fn viewport_rows_at_top() {
        let v = compute_viewport(&scroll(), &geo());
        assert_eq!((v.row_start, v.row_end), (0, 10 + ROW_OVERSCAN)); // ceil(640/64)=10
    }

    #[test]
    fn viewport_rows_account_for_header_and_clamp() {
        // max scroll = 40 header + 1000 * 64 - 640 = 63_440; content_top = 63_400 -> first row 990
        let v = compute_viewport(&ScrollState { scroll_top: 63_440.0, ..scroll() }, &geo());
        assert_eq!((v.row_start, v.row_end), (990 - ROW_OVERSCAN, 1000));
    }

    #[test]
    fn viewport_uses_given_row_height() {
        // after a resize to the mobile breakpoint (48px rows) the same scroll shows later rows
        let at = ScrollState { scroll_top: 6_440.0, ..scroll() };
        let desktop = compute_viewport(&at, &geo());
        let mobile = compute_viewport(&at, &Geometry { row_height: 48.0, ..geo() });
        assert!(mobile.row_start > desktop.row_start);
    }

    #[test]
    fn viewport_minutes_cover_visible_plus_half_screen_each_side_and_are_quantized() {
        // visible programme width = 1300-300 = 1000px = ~143 min at 7 px/min; half = ~71 min
        let v = compute_viewport(&ScrollState { scroll_left: 7.0 * 600.0, ..scroll() }, &geo());
        assert_eq!(v.from_min % MINUTE_QUANTUM, 0);
        assert_eq!(v.to_min % MINUTE_QUANTUM, 0);
        assert!(v.from_min <= 600 - 71);
        assert!(v.to_min >= 600 + 143 + 71);
        assert_eq!((v.from_min, v.to_min), (480, 840));
    }

    #[test]
    fn small_horizontal_scroll_keeps_same_viewport() {
        let a = compute_viewport(&ScrollState { scroll_left: 7.0 * 600.0, ..scroll() }, &geo());
        let b = compute_viewport(&ScrollState { scroll_left: 7.0 * 610.0, ..scroll() }, &geo());
        assert_eq!(a, b);
    }

    /// Upper bound of rendered programme boxes if every programme lasts `programme_min`.
    fn max_programmes(v: &Viewport, programme_min: i64) -> usize {
        (v.row_end - v.row_start) * ((v.to_min - v.from_min) / programme_min + 1) as usize
    }

    #[test]
    fn dom_budget_at_min_zoom() {
        let min_zoom = Geometry { pixels_per_min: MIN_PIXELS_PER_MIN, ..geo() };
        let mid = ScrollState { scroll_top: 6_440.0, scroll_left: MIN_PIXELS_PER_MIN * 3000.0, ..scroll() };
        // 1300 x 640: 16 rows x 37 programmes = 592
        assert!(max_programmes(&compute_viewport(&mid, &min_zoom), 30) <= 1000);
        // 1920 x 1080: 23 rows x 57 programmes = 1311
        let full_hd = ScrollState { client_height: 1080.0, client_width: 1920.0, ..mid };
        assert!(max_programmes(&compute_viewport(&full_hd, &min_zoom), 30) <= 1500);
    }
}
