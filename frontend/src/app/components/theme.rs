use crate::utils::{get_local_storage_item, remove_local_storage_item, set_local_storage_item};
use shared::error::{info_err_res, TuliproxError};
use std::{
    fmt::{Display, Formatter},
    str::FromStr,
};
use web_sys::window;

pub const TP_THEME_KEY: &str = "tp-theme";

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
            _ => info_err_res!("Unknown theme: {s}"),
        }
    }
}

impl Theme {
    pub const ALL: [Self; 17] = [
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
        Theme::Bright,
        Theme::Paper,
        Theme::NaturePure,
        Theme::CoolElegance,
        Theme::BananaYellow,
        Theme::SunWashedSoft,
        Theme::VintageNeutral,
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
            | Theme::SunWashedSoft
            | Theme::VintageNeutral
            | Theme::Bright => set_local_storage_item(TP_THEME_KEY, &self.to_string()),
        }
    }

    fn set_body_theme(&self) {
        if let Some(window) = window() {
            if let Some(document) = window.document() {
                if let Some(body) = document.body() {
                    let _ = body.set_attribute("data-theme", &self.to_string());
                }
            }
        }
    }
}
