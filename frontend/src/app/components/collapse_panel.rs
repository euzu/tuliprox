use yew::prelude::*;
use crate::app::components::AppIcon;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct CollapsePanelProps {
    #[prop_or(true)]
    pub expanded: bool,
    #[prop_or_default]
    pub title: String,
    #[prop_or_default]
    pub title_content: Option<Html>,
    pub children: Children,
    #[prop_or_default]
    pub class: String,
    #[prop_or_default]
    pub on_state_change: Callback<bool>,
}

#[function_component]
pub fn CollapsePanel(props: &CollapsePanelProps) -> Html {
    let expanded = use_state(|| props.expanded);
    // let panel_ref = use_node_ref();

    let toggle = {
        let expanded = expanded.clone();
        let on_state_change = props.on_state_change.clone();
        Callback::from(move |_| {
            let new_state = !*expanded;
            expanded.set(new_state);
            on_state_change.emit(new_state);
        })
    };

    html! {
        <div class={classes!("tp__collapse-panel", if *expanded {""} else {"tp__collapsed"}, props.class.to_string())}>
            <div class="tp__collapse-panel__header" onclick={toggle}>
                <span class="tp__collapse-panel__header-title">
                    { props.title_content.clone().unwrap_or_else(|| html! { &props.title }) }
                </span>
                <AppIcon name={ if *expanded { "ChevronUp" } else {"ChevronDown"} }/>
            </div>
            <div /*ref={panel_ref} */class="tp__collapse-panel__body">
            { for props.children.iter() }
            </div>
        </div>
    }
}