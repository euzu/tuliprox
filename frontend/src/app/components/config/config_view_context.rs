use crate::app::components::config::config_page::ConfigForm;
use yew::{Callback, UseStateHandle};

#[derive(Clone)]
pub struct ConfigViewContext {
    pub edit_mode: UseStateHandle<bool>,
    pub show_restart_notice: bool,
    pub on_form_change: Callback<ConfigForm>,
}

impl PartialEq for ConfigViewContext {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}
