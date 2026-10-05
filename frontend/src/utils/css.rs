use web_sys::window;

/// Width below which the UI switches to its mobile layout (`$mobile-breakpoint` in `scss/_size.scss`).
pub const MOBILE_BREAKPOINT_PX: f64 = 780.0;

/// Resolves a CSS length to px. `getComputedStyle` returns custom properties as declared
/// (`4rem`), so `rem` is resolved against `root_font_px`. Only `px` and `rem` are supported.
pub fn parse_css_length_px(value: &str, root_font_px: f64) -> Option<f64> {
    let value = value.trim();
    let px = if let Some(rem) = value.strip_suffix("rem") {
        rem.trim().parse::<f64>().ok()? * root_font_px
    } else {
        value.strip_suffix("px")?.trim().parse::<f64>().ok()?
    };
    (px > 0.0).then_some(px)
}

/// Font size of the document root in px (16 when it cannot be read).
pub fn root_font_px() -> f64 {
    window()
        .and_then(|win| {
            let root = win.document()?.document_element()?;
            win.get_computed_style(&root).ok().flatten()
        })
        .and_then(|style| style.get_property_value("font-size").ok())
        .and_then(|value| parse_css_length_px(&value, 16.0))
        .unwrap_or(16.0)
}

/// Resolves a CSS length (`px` or `rem`) against the current root font size.
pub fn resolve_css_length_px(value: &str) -> Option<f64> { parse_css_length_px(value, root_font_px()) }

/// Value of a CSS custom property on the document root, resolved to px.
pub fn read_css_var_px(var_name: &str, fallback: f64) -> f64 {
    window()
        .and_then(|win| {
            let root = win.document()?.document_element()?;
            win.get_computed_style(&root).ok().flatten()
        })
        .and_then(|style| style.get_property_value(var_name).ok())
        .and_then(|value| resolve_css_length_px(&value))
        .unwrap_or(fallback)
}

/// Whether the window is narrower than the mobile breakpoint.
pub fn is_mobile_viewport() -> bool {
    window()
        .and_then(|win| win.inner_width().ok())
        .and_then(|width| width.as_f64())
        .is_some_and(|width| width < MOBILE_BREAKPOINT_PX)
}

#[cfg(test)]
mod tests {
    use super::parse_css_length_px;

    #[test]
    fn parse_css_length_px_resolves_rem_against_root_font() {
        assert_eq!(parse_css_length_px("4rem", 16.0), Some(64.0));
        assert_eq!(parse_css_length_px(" 3rem ", 16.0), Some(48.0));
        assert_eq!(parse_css_length_px("2.5rem", 15.0), Some(37.5));
    }

    #[test]
    fn parse_css_length_px_accepts_px() {
        assert_eq!(parse_css_length_px("60px", 16.0), Some(60.0));
    }

    #[test]
    fn parse_css_length_px_rejects_unknown_or_non_positive() {
        assert_eq!(parse_css_length_px("", 16.0), None);
        assert_eq!(parse_css_length_px("4em", 16.0), None);
        assert_eq!(parse_css_length_px("0px", 16.0), None);
        assert_eq!(parse_css_length_px("abc", 16.0), None);
    }
}
