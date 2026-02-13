use crate::app::components::PlaylistAssistantContext;
use yew::prelude::*;

#[function_component]
pub fn NameStep() -> Html {
    let _playlist_ctx =
        use_context::<PlaylistAssistantContext>().expect("PlaylistAssistant context not found");

    html! {
        <div class={"tp__playlist-assistant__step-name"}>
        </div>
    }
}
