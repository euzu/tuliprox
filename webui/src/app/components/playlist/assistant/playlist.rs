use std::fmt;
use std::str::FromStr;
use yew::prelude::*;
use crate::app::components::{NameStep, Panel, PlaylistAssistantContext, TypeStep};

enum PlaylistAssistantStep {
    Name,
    Type,
    Scheduling,
    Processing
}

impl fmt::Display for PlaylistAssistantStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            PlaylistAssistantStep::Name => "name",
            PlaylistAssistantStep::Type => "type",
            PlaylistAssistantStep::Scheduling => "scheduling",
            PlaylistAssistantStep::Processing => "processing",
        };
        write!(f, "{s}")
    }
}

impl FromStr for PlaylistAssistantStep {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
             "name" => Ok(PlaylistAssistantStep::Name),
             "type" => Ok(PlaylistAssistantStep::Type),
             "scheduling" => Ok(PlaylistAssistantStep::Scheduling),
             "processing" => Ok(PlaylistAssistantStep::Processing),
            _ => Err(())
        }
    }
}


#[function_component]
pub fn PlaylistAssistant() -> Html {
    let active_step = use_state(|| PlaylistAssistantStep::Name);

    let custom_class = use_state(String::new);
    let context = PlaylistAssistantContext {
        custom_class: custom_class.clone(),
    };

    html! {
        <ContextProvider<PlaylistAssistantContext> context={context}>
            <div class="tp__playlist-assistant">
                <Panel value={PlaylistAssistantStep::Name.to_string()} active={active_step.to_string()}>
                    <NameStep/>
                </Panel>
                <Panel value={PlaylistAssistantStep::Type.to_string()} active={active_step.to_string()}>
                    <TypeStep/>
                </Panel>
            </div>
        </ContextProvider<PlaylistAssistantContext >>
    }
}