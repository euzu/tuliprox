use yew::prelude::*;
use crate::app::components::AppIcon;

#[derive(Properties, PartialEq, Clone)]
pub struct PlaylistMappingsProps {
    pub mappings: Option<Vec<String>>,
}

#[function_component]
pub fn PlaylistMappings(props: &PlaylistMappingsProps) -> Html {

    html! {
      <div class="tp__playlist-mappings">
        <ul>
        {
            match props.mappings.as_ref() {
                Some(vec) => vec.iter().map(|item| html! { <li>{ item } <AppIcon name="Link" /></li> }).collect::<Html>(),
                None => html! {},
            }
        }
        </ul>
      </div>
    }
}