mod assistant;
mod list;
mod create;
mod page;
mod target_table;
mod target;
mod playlist_view;
mod processing;
mod mappings;
mod filter_view;
mod input_table;

mod input;

use std::rc::Rc;
use yew_i18n::YewI18n;
use crate::app::components::{convert_bool_to_chip_style, Tag};
pub use self::assistant::*;
pub use self::list::*;
pub use self::create::*;
pub use crate::app::context::*;
pub use self::page::*;
pub use self::target_table::*;
pub use self::target::*;
pub use self::processing::*;
pub use self::mappings::*;
pub use self::filter_view::*;
pub use self::input_table::*;
pub use self::input::*;
pub use self::playlist_view::*;

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