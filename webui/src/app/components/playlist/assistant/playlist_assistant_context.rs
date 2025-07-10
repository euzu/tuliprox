use yew::UseStateHandle;

#[derive(Clone, PartialEq)]
pub struct PlaylistAssistantContext {
    pub custom_class: UseStateHandle<String>,
}