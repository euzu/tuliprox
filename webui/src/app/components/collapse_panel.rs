use yew::prelude::*;
use crate::app::components::AppIcon;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct CollapsePanelProps {
    #[prop_or(true)]
    pub expanded: bool,
    pub title: String,
    pub children: Children,
}

#[function_component]
pub fn CollapsePanel(props: &CollapsePanelProps) -> Html {
    let expanded = use_state(|| props.expanded);

    let toggle = {
        let expanded = expanded.clone();
        Callback::from(move |_| expanded.set(!*expanded))
    };

    html! {
        <div class="collapse-panel">
            <div class="collapse-panel__header" onclick={toggle}>
                <span>{ props.title.clone() }</span>
                <AppIcon name={ if *expanded { "ChevronUp" } else {"ChevronDown"} }/>
            </div>
            if *expanded {
                <div class="collapse-panel__body">
                { for props.children.iter() }
                </div>
            }
        </div>
    }
}