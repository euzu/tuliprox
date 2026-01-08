use shared::error::{TuliproxError, info_err_res};
use std::fmt::{Display, Formatter};
use std::str::FromStr;
use web_sys::window;
use crate::utils::{get_local_storage_item, remove_local_storage_item, set_local_storage_item};

pub const TP_THEME_KEY: &str = "tp-theme";

const THEME_DARK: &str = "dark";
const THEME_BRIGHT: &str = "bright";



#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Bright,
}

impl Display for Theme {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(f, "{}",
            match self {
                Theme::Dark => THEME_DARK,
                Theme::Bright => THEME_BRIGHT,
            }
        )
    }
}

impl FromStr for Theme {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        match s.to_lowercase().as_str() {
            THEME_DARK => Ok(Theme::Dark),
            THEME_BRIGHT => Ok(Theme::Bright),
            _ => info_err_res!("Unknown theme: {s}"),
        }
    }
}

impl Theme {

    pub fn get_current_theme() -> Theme {
       let theme = get_local_storage_item(TP_THEME_KEY).map_or(Theme::Dark, |t| Theme::from_str(&t).unwrap_or(Theme::Dark));
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
            Theme::Bright => set_local_storage_item(TP_THEME_KEY, &self.to_string()),
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
