use crate::utils::{get_local_storage_item, remove_local_storage_item, set_local_storage_item, set_timeout};
use shared::error::TuliproxError;
use std::{
    fmt::{Display, Formatter},
    str::FromStr,
};
use web_sys::window;

pub const TP_THEME_KEY: &str = "tp-theme";

const THEME_TRANSITION_CLASS: &str = "tp-theme-transition";
const THEME_TRANSITION_MS: i32 = 320;

const THEME_DARK: &str = "dark";
const THEME_REFINED_DARK: &str = "refined-dark";
const THEME_BRIGHT: &str = "bright";
const THEME_AURORA: &str = "aurora";
const THEME_MONOKAI: &str = "monokai";
const THEME_PAPER: &str = "paper";
const THEME_NATURE_PURE: &str = "nature-pure";
const THEME_DOPAMINE: &str = "dopamine";
const THEME_MERMAIDCORE: &str = "mermaidcore";
const THEME_COOL_ELEGANCE: &str = "cool-elegance";
const THEME_BANANA_YELLOW: &str = "banana-yellow";
const THEME_CLUBROOM_CONTRAST: &str = "clubroom-contrast";
const THEME_SUN_WASHED_SOFT: &str = "sun-washed-soft";
const THEME_VINTAGE_NEUTRAL: &str = "vintage-neutral";
const THEME_DRACULA: &str = "dracula";
const THEME_NORD: &str = "nord";
const THEME_GRAPEROOT_DARK: &str = "graperoot-dark";
const THEME_CVD_FRIENDLY_DARK: &str = "cvd-friendly-dark";
const THEME_CVD_FRIENDLY_BRIGHT: &str = "cvd-friendly-bright";
const THEME_VIBRANT_ELEGANCE: &str = "vibrant-elegance";

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum Theme {
    Dark,
    RefinedDark,
    GraperootDark,
    Nord,
    Dracula,
    Aurora,
    Monokai,
    Dopamine,
    Mermaidcore,
    ClubroomContrast,
    Bright,
    Paper,
    NaturePure,
    CoolElegance,
    BananaYellow,
    SunWashedSoft,
    VintageNeutral,
    CvdFriendlyDark,
    CvdFriendlyBright,
    VibrantElegance,
}

impl Display for Theme {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Theme::Dark => THEME_DARK,
                Theme::RefinedDark => THEME_REFINED_DARK,
                Theme::GraperootDark => THEME_GRAPEROOT_DARK,
                Theme::Nord => THEME_NORD,
                Theme::Dracula => THEME_DRACULA,
                Theme::Aurora => THEME_AURORA,
                Theme::Monokai => THEME_MONOKAI,
                Theme::Dopamine => THEME_DOPAMINE,
                Theme::Mermaidcore => THEME_MERMAIDCORE,
                Theme::ClubroomContrast => THEME_CLUBROOM_CONTRAST,
                Theme::Bright => THEME_BRIGHT,
                Theme::Paper => THEME_PAPER,
                Theme::NaturePure => THEME_NATURE_PURE,
                Theme::CoolElegance => THEME_COOL_ELEGANCE,
                Theme::BananaYellow => THEME_BANANA_YELLOW,
                Theme::SunWashedSoft => THEME_SUN_WASHED_SOFT,
                Theme::VintageNeutral => THEME_VINTAGE_NEUTRAL,
                Theme::CvdFriendlyDark => THEME_CVD_FRIENDLY_DARK,
                Theme::CvdFriendlyBright => THEME_CVD_FRIENDLY_BRIGHT,
                Theme::VibrantElegance => THEME_VIBRANT_ELEGANCE,
            }
        )
    }
}

impl FromStr for Theme {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        match s.to_lowercase().as_str() {
            THEME_DARK => Ok(Theme::Dark),
            THEME_REFINED_DARK => Ok(Theme::RefinedDark),
            THEME_GRAPEROOT_DARK => Ok(Theme::GraperootDark),
            THEME_NORD => Ok(Theme::Nord),
            THEME_DRACULA => Ok(Theme::Dracula),
            THEME_AURORA => Ok(Theme::Aurora),
            THEME_MONOKAI => Ok(Theme::Monokai),
            THEME_DOPAMINE => Ok(Theme::Dopamine),
            THEME_MERMAIDCORE => Ok(Theme::Mermaidcore),
            THEME_CLUBROOM_CONTRAST => Ok(Theme::ClubroomContrast),
            THEME_BRIGHT => Ok(Theme::Bright),
            THEME_PAPER => Ok(Theme::Paper),
            THEME_NATURE_PURE => Ok(Theme::NaturePure),
            THEME_COOL_ELEGANCE => Ok(Theme::CoolElegance),
            THEME_BANANA_YELLOW => Ok(Theme::BananaYellow),
            THEME_SUN_WASHED_SOFT => Ok(Theme::SunWashedSoft),
            THEME_VINTAGE_NEUTRAL => Ok(Theme::VintageNeutral),
            THEME_CVD_FRIENDLY_DARK => Ok(Theme::CvdFriendlyDark),
            THEME_CVD_FRIENDLY_BRIGHT => Ok(Theme::CvdFriendlyBright),
            THEME_VIBRANT_ELEGANCE => Ok(Theme::VibrantElegance),
            _ => Err(TuliproxError::Config(format!("Unknown theme: {s}"))),
        }
    }
}

impl Theme {
    pub const ALL: [Self; 20] = [
        Theme::Dark,
        Theme::RefinedDark,
        Theme::GraperootDark,
        Theme::Nord,
        Theme::Dracula,
        Theme::Aurora,
        Theme::Monokai,
        Theme::Dopamine,
        Theme::Mermaidcore,
        Theme::ClubroomContrast,
        Theme::CvdFriendlyDark,
        Theme::VibrantElegance,
        Theme::Bright,
        Theme::Paper,
        Theme::NaturePure,
        Theme::CoolElegance,
        Theme::BananaYellow,
        Theme::SunWashedSoft,
        Theme::VintageNeutral,
        Theme::CvdFriendlyBright,
    ];

    pub const fn all() -> &'static [Self] { &Self::ALL }

    pub const fn label(self) -> &'static str {
        match self {
            Theme::Dark => "Dark",
            Theme::RefinedDark => "Refined Dark",
            Theme::GraperootDark => "GrapeRoot Dark",
            Theme::Dracula => "Dracula",
            Theme::Nord => "Nord",
            Theme::Monokai => "Monokai",
            Theme::Dopamine => "Dopamine",
            Theme::Mermaidcore => "Mermaidcore",
            Theme::ClubroomContrast => "Clubroom Contrast",
            Theme::Bright => "Bright",
            Theme::Aurora => "Aurora",
            Theme::Paper => "Paper",
            Theme::NaturePure => "Nature Pure",
            Theme::CoolElegance => "Cool Elegance",
            Theme::BananaYellow => "Banana Yellow",
            Theme::SunWashedSoft => "Sun-Washed Soft",
            Theme::VintageNeutral => "Vintage Neutral",
            Theme::CvdFriendlyDark => "CVD Friendly Dark",
            Theme::CvdFriendlyBright => "CVD Friendly Bright",
            Theme::VibrantElegance => "Vibrant Elegance",
        }
    }

    pub const fn is_light(self) -> bool {
        matches!(
            self,
            Theme::Bright
                | Theme::Paper
                | Theme::NaturePure
                | Theme::CoolElegance
                | Theme::BananaYellow
                | Theme::SunWashedSoft
                | Theme::VintageNeutral
                | Theme::CvdFriendlyBright
        )
    }

    pub fn get_current_theme() -> Theme {
        let theme =
            get_local_storage_item(TP_THEME_KEY).map_or(Theme::Dark, |t| Theme::from_str(&t).unwrap_or(Theme::Dark));
        theme.switch_theme();
        theme
    }

    pub fn switch_theme(&self) {
        self.save_to_local_storage();
        self.set_body_theme();
    }

    /// Applies the theme visually without persisting it (used for live previews).
    pub fn preview(&self) { self.set_body_theme(); }

    fn save_to_local_storage(&self) {
        match self {
            Theme::Dark => remove_local_storage_item(TP_THEME_KEY),
            Theme::RefinedDark
            | Theme::GraperootDark
            | Theme::Nord
            | Theme::Dracula
            | Theme::Aurora
            | Theme::Monokai
            | Theme::Paper
            | Theme::NaturePure
            | Theme::Dopamine
            | Theme::Mermaidcore
            | Theme::CoolElegance
            | Theme::BananaYellow
            | Theme::ClubroomContrast
            | Theme::CvdFriendlyDark
            | Theme::CvdFriendlyBright
            | Theme::VibrantElegance
            | Theme::SunWashedSoft
            | Theme::VintageNeutral
            | Theme::Bright => set_local_storage_item(TP_THEME_KEY, &self.to_string()),
        }
    }

    fn set_body_theme(&self) {
        if let Some(window) = window() {
            if let Some(document) = window.document() {
                if let Some(body) = document.body() {
                    let theme = self.to_string();
                    let is_switch = body.get_attribute("data-theme").is_some_and(|current| current != theme);

                    if is_switch {
                        let _ = body.class_list().add_1(THEME_TRANSITION_CLASS);
                    }

                    let _ = body.set_attribute("data-theme", &theme);

                    if is_switch {
                        let body = body.clone();
                        set_timeout(
                            move || {
                                let _ = body.class_list().remove_1(THEME_TRANSITION_CLASS);
                            },
                            THEME_TRANSITION_MS,
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod scss_tests {
    use super::Theme;

    const DARK_THEME_STYLES: [(&str, &str); 12] = [
        ("aurora", include_str!("../../../scss/themes/_theme-aurora.scss")),
        ("clubroom-contrast", include_str!("../../../scss/themes/_theme-clubroom-contrast.scss")),
        ("cvd-friendly-dark", include_str!("../../../scss/themes/_theme-cvd-friendly-dark.scss")),
        ("dark", include_str!("../../../scss/themes/_theme-dark.scss")),
        ("dopamine", include_str!("../../../scss/themes/_theme-dopamine.scss")),
        ("dracula", include_str!("../../../scss/themes/_theme-dracula.scss")),
        ("graperoot-dark", include_str!("../../../scss/themes/_theme-graperoot-dark.scss")),
        ("mermaidcore", include_str!("../../../scss/themes/_theme-mermaidcore.scss")),
        ("monokai", include_str!("../../../scss/themes/_theme-monokai.scss")),
        ("nord", include_str!("../../../scss/themes/_theme-nord.scss")),
        ("refined-dark", include_str!("../../../scss/themes/_theme-refined-dark.scss")),
        ("vibrant-elegance", include_str!("../../../scss/themes/_theme-vibrant-elegance.scss")),
    ];

    fn declared_value<'a>(scss: &'a str, name: &str) -> &'a str {
        let prefix = format!("{name}:");
        scss.lines()
            .find_map(|line| line.trim().strip_prefix(&prefix))
            .map(str::trim)
            .map(|value| value.strip_suffix(';').unwrap_or(value))
            .unwrap_or_else(|| panic!("{name} is not declared"))
    }

    fn resolved_value<'a>(scss: &'a str, name: &str) -> &'a str {
        let value = declared_value(scss, name);
        match value.strip_prefix("var(").and_then(|inner| inner.strip_suffix(')')) {
            Some(reference) => declared_value(scss, reference),
            None => value,
        }
    }

    fn hex_rgb(value: &str) -> (f64, f64, f64) {
        let hex = value.strip_prefix('#').unwrap_or_else(|| panic!("{value} is not a hex color"));
        let channel = |index: usize| u8::from_str_radix(&hex[index..index + 2], 16).expect("hex channel") as f64;
        (channel(0), channel(2), channel(4))
    }

    fn rgba_alpha(value: &str) -> f64 {
        value
            .strip_prefix("rgba(")
            .and_then(|inner| inner.strip_suffix(')'))
            .and_then(|channels| channels.split(',').nth(3))
            .unwrap_or_else(|| panic!("{value} is not an rgba color"))
            .trim()
            .parse()
            .expect("alpha")
    }

    fn luminance((r, g, b): (f64, f64, f64)) -> f64 {
        fn linear(channel: f64) -> f64 {
            let channel = channel / 255.0;
            if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
    }

    fn contrast(foreground: (f64, f64, f64), background: (f64, f64, f64)) -> f64 {
        let foreground = luminance(foreground);
        let background = luminance(background);
        let (lighter, darker) =
            if foreground >= background { (foreground, background) } else { (background, foreground) };
        (lighter + 0.05) / (darker + 0.05)
    }

    #[test]
    fn explorer_font_sizes_are_defined_in_the_theme() {
        let theme = include_str!("../../../scss/_theme.scss");
        let explorer = include_str!("../../../scss/app/components/playlist/_playlist_explorer.scss");
        let mut defined = std::collections::HashSet::new();
        for line in theme.lines() {
            let line = line.trim();
            if let Some((name, _)) = line.split_once(':') {
                if name.starts_with("--font-size-") {
                    defined.insert(name);
                }
            }
        }
        for usage in explorer.split("var(").skip(1) {
            if let Some(name) = usage.split(')').next() {
                if name.starts_with("--font-size-") {
                    assert!(defined.contains(name), "undefined font-size variable {name}");
                }
            }
        }
    }

    #[test]
    fn dark_theme_rating_badges_stay_readable_on_white_artwork() {
        assert_eq!(
            DARK_THEME_STYLES.len(),
            Theme::ALL.iter().filter(|theme| !theme.is_light()).count(),
            "every dark theme must be checked"
        );
        for (name, scss) in DARK_THEME_STYLES {
            let surface = declared_value(scss, "--theme-palette-explorer-rating-surface");
            let alpha = rgba_alpha(surface);
            let component = 255.0 * (1.0 - alpha);
            let background = (component, component, component);
            let text = hex_rgb(resolved_value(scss, "--theme-palette-explorer-rating-text"));
            let ratio = contrast(text, background);
            assert!(ratio >= 4.5, "{name}: rating text contrast is {ratio:.3}, expected at least 4.5");
        }
    }

    #[test]
    fn sun_washed_soft_secondary_text_meets_contrast() {
        let scss = include_str!("../../../scss/themes/_theme-sun-washed-soft.scss");
        let text = hex_rgb(resolved_value(scss, "--theme-palette-explorer-text-secondary"));
        let surface = hex_rgb(resolved_value(scss, "--theme-palette-neutral-surface-920"));
        let ratio = contrast(text, surface);
        assert!(ratio >= 4.5, "secondary text contrast is {ratio:.3}, expected at least 4.5");
    }
}
