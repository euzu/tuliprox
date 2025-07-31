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
}

#[function_component]
pub fn CollapsePanel(props: &CollapsePanelProps) -> Html {
    let expanded = use_state(|| props.expanded);
    // let panel_ref = use_node_ref();

    let toggle = {
        let expanded = expanded.clone();
        Callback::from(move |_| expanded.set(!*expanded))
    };

    // use_effect_with((expanded.clone(), panel_ref.clone()),move |(expanded, panel_ref)| {
    //     if let Some(element) = panel_ref.cast::<HtmlElement>() {
    //         if **expanded {
    //             let scroll_height = element.scroll_height();
    //             element.style().set_property("height", &format!("{scroll_height}px")).unwrap();
    //         } else {
    //             element.style().set_property("height", "0px").unwrap();
    //         }
    //     }
    //     || ()
    // });

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