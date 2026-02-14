use crate::app::components::config::config_page::ConfigForm;
use yew::{Callback, UseStateHandle};

#[derive(Clone, PartialEq)]
pub struct ConfigViewContext {
    pub edit_mode: UseStateHandle<bool>,
    pub on_form_change: Callback<ConfigForm>,
}
