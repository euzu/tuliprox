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
    pub class: Option<String>,
}

#[function_component]
pub fn CollapsePanel(props: &CollapsePanelProps) -> Html {
    let expanded = use_state(|| props.expanded);

    let toggle = {
        let expanded = expanded.clone();
        Callback::from(move |_| expanded.set(!*expanded))
    };

    html! {
        <div class={classes!("tp__collapse-panel", if *expanded {""} else {"tp__collapsed"}, props.class.as_ref().map(ToString::to_string))}>
            <div class="tp__collapse-panel__header" onclick={toggle}>
                <span class="tp__collapse-panel__header-title">
                    { props.title_content.clone().unwrap_or_else(|| html! { &props.title }) }
                </span>
                <AppIcon name={ if *expanded { "ChevronUp" } else {"ChevronDown"} }/>
            </div>
            <div class="tp__collapse-panel__body">
            { for props.children.iter() }
            </div>
        </div>
    }
}