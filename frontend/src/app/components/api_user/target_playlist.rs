use crate::app::components::bouquet_editor::BouquetEditor;
pub use crate::app::components::bouquet_editor::BouquetSelection;
use shared::model::{PlaylistClusterBouquetDto, PlaylistClusterCategoriesDto};
use std::{cell::RefCell, rc::Rc};
use yew::prelude::*;

#[derive(Properties, PartialEq)]
pub struct UserTargetPlaylistProps {
    pub categories: Option<PlaylistClusterCategoriesDto>,
    pub bouquet: Option<PlaylistClusterBouquetDto>,
    pub on_change: Callback<Rc<RefCell<BouquetSelection>>>,
}

#[component]
pub fn UserTargetPlaylist(props: &UserTargetPlaylistProps) -> Html {
    html! {
        <BouquetEditor
            categories={props.categories.clone()}
            bouquet={props.bouquet.clone()}
            on_change={props.on_change.clone()}
        />
    }
}
