mod assistant;
mod create;
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
mod playlist_editor_page;
mod playlist_editor_view;
mod playlist_explorer;
mod playlist_explorer_page;
mod playlist_explorer_view;
mod playlist_source_selector;
mod playlist_update_view;
mod processing;
mod target;
mod target_table;

pub use self::assistant::*;
pub use self::create::*;
pub use self::epg_config_view::*;
pub use self::epg_source_selector::*;
pub use self::epg_view::*;
pub use self::filter_view::*;
pub use self::input::*;
pub use self::input_table::*;
pub use self::list::*;
pub use self::mapper_counter_view::*;
pub use self::mapper_script_view::*;
pub use self::mappings::*;
pub use self::playlist_editor_page::*;
pub use self::playlist_editor_view::*;
pub use self::playlist_explorer_page::*;
pub use self::playlist_explorer_view::*;
pub use self::playlist_source_selector::*;
pub use self::playlist_update_view::*;
pub use self::processing::*;
pub use self::target::*;
pub use self::target_table::*;
use crate::app::components::{convert_bool_to_chip_style, Tag};
pub use crate::app::context::*;
use std::rc::Rc;
use yew_i18n::YewI18n;

pub fn make_tags(data: &[(bool, &str)], translate: &YewI18n) -> Vec<Rc<Tag>> {
    data.iter()
        .map(|(o, t)| {
            Rc::new(Tag {
                class: convert_bool_to_chip_style(*o),
                label: translate.t(t),
            })
        })
        .collect()
}
