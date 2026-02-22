mod epg_config_view;
mod epg_source_selector;
mod epg_view;
mod filter_view;
mod input;
mod input_table;
mod list;
mod mapper_counter_view;
mod mapper_script_view;
mod mappings;
mod playlist_explorer;
mod playlist_explorer_page;
mod playlist_explorer_view;
mod playlist_settings_view;
mod playlist_source_selector;
mod playlist_update_view;
mod processing;
mod target;
mod target_table;

pub use self::{
    epg_config_view::*, epg_source_selector::*, epg_view::*, filter_view::*, input::*, input_table::*, list::*,
    mapper_counter_view::*, mapper_script_view::*, mappings::*, playlist_explorer_page::*, playlist_explorer_view::*,
    playlist_settings_view::*, playlist_source_selector::*, playlist_update_view::*, processing::*, target::*,
    target_table::*,
};
use crate::app::components::{convert_bool_to_chip_style, Tag};
pub use crate::app::context::*;
use std::rc::Rc;
use yew_i18n::YewI18n;

pub fn make_tags(data: &[(bool, &str)], translate: &YewI18n) -> Vec<Rc<Tag>> {
    data.iter().map(|(o, t)| Rc::new(Tag { class: convert_bool_to_chip_style(*o), label: translate.t(t) })).collect()
}
