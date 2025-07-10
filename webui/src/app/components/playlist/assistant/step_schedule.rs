use yew::prelude::*;
use crate::app::components::PlaylistAssistantContext;

#[function_component]
pub fn ScheduleStep() -> Html {
    let _playlist_ctx = use_context::<PlaylistAssistantContext>();

    html! {
        <div class={"tp__playlist-assistant__step-schedule"}>
        </div>
    }
}