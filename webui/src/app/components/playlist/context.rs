use yew::UseStateHandle;
use crate::app::components::PlaylistPage;

#[derive(Clone, PartialEq)]
pub struct PlaylistContext {
    pub active_page: UseStateHandle<PlaylistPage>,
}