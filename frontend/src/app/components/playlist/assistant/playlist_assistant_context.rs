use yew::UseStateHandle;

#[derive(Clone, PartialEq)]
#[allow(dead_code)]
pub struct PlaylistAssistantContext {
    pub custom_class: UseStateHandle<String>,
}
