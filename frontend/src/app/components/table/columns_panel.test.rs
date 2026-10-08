use super::*;
#[test]
fn panel_starts_at_anchor_and_moves_up_at_bottom_edge() {
    let viewport = PanelBounds { top: 0.0, left: 0.0, width: 1100.0, height: 800.0 };
    let anchor = PanelBounds { top: 180.0, left: 500.0, width: 28.0, height: 96.0 };
    assert_eq!(panel_position(anchor, (320.0, 200.0), viewport, false), (180.0, 172.0));
    assert_eq!(panel_position(PanelBounds { top: 760.0, ..anchor }, (320.0, 200.0), viewport, false), (592.0, 172.0));
    assert_eq!(panel_position(anchor, (320.0, 784.0), viewport, false), (8.0, 172.0));
}

#[test]
fn panel_uses_opposite_side_when_preferred_side_has_no_room() {
    let viewport = PanelBounds { top: 0.0, left: 0.0, width: 1100.0, height: 800.0 };
    let anchor = PanelBounds { top: 180.0, left: 500.0, width: 28.0, height: 96.0 };
    assert_eq!(panel_position(anchor, (320.0, 200.0), viewport, true), (180.0, 536.0));
    assert_eq!(panel_position(PanelBounds { left: 20.0, ..anchor }, (320.0, 200.0), viewport, false), (180.0, 56.0));
    assert_eq!(panel_position(PanelBounds { left: 1000.0, ..anchor }, (320.0, 200.0), viewport, true), (180.0, 672.0));
}

#[test]
fn panel_stays_inside_offset_visual_viewport() {
    let viewport = PanelBounds { top: 40.0, left: 20.0, width: 360.0, height: 500.0 };
    let anchor = PanelBounds { top: 600.0, left: 500.0, width: 28.0, height: 96.0 };
    assert_eq!(panel_position(anchor, (344.0, 300.0), viewport, false), (232.0, 28.0));
}

#[test]
fn reordering_preserves_unknown_columns() {
    let layout =
        TableLayoutPreferencesDto { column_order: vec!["a".into(), "future".into(), "b".into()], ..Default::default() };
    assert_eq!(move_column(&layout, "b", "a").column_order, vec!["b", "a", "future"]);
    assert_eq!(layout.column_order, vec!["a", "future", "b"]);
}

#[test]
fn pointer_reordering_preserves_unknown_columns_and_skips_unchanged_positions() -> Result<(), &'static str> {
    let layout = TableLayoutPreferencesDto {
        column_order: vec!["a".into(), "future".into(), "b".into()],
        column_visibility: [("a".into(), false)].into(),
    };
    assert!(reordered_layout(&layout, "a", 0).is_none());
    assert!(reordered_layout(&layout, "missing", 0).is_none());
    assert!(reordered_layout(&layout, "a", 3).is_none());
    let moved = reordered_layout(&layout, "b", 0).ok_or("moving a known column must produce a layout")?;
    assert_eq!(moved.column_order, vec!["b", "a", "future"]);
    assert_eq!(moved.column_visibility, layout.column_visibility);
    assert_eq!(layout.column_order, vec!["a", "future", "b"]);
    let moved = reordered_layout(&layout, "a", 2).ok_or("moving a known column must produce a layout")?;
    assert_eq!(moved.column_order, vec!["future", "b", "a"]);
    Ok(())
}
