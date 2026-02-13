use crate::app::components::{AccordionState, AppIcon};
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct AccordionPanelProps {
    #[prop_or_default]
    pub id: String,
    #[prop_or_default]
    pub title: String,
    #[prop_or_default]
    pub title_content: Option<Html>,
    pub children: Children,
    #[prop_or_default]
    pub class: String,
}

#[function_component]
pub fn AccordionPanel(props: &AccordionPanelProps) -> Html {
    let context =
        use_context::<AccordionState>().expect("AccordionPanel must be used inside Accordion");
    let expanded = (**context.active_panel).as_ref() == Some(&props.id);

    let toggle = {
        let id = props.id.clone();
        let context = context.clone();

        Callback::from(move |_| {
            if (**context.active_panel).as_ref() == Some(&id) {
                context.active_panel.set(None);
            } else {
                context.active_panel.set(Some(id.clone()));
            }
        })
    };

    html! {
        <div class={classes!("tp__collapse-panel", if expanded {""} else {"tp__collapsed"}, props.class.to_string())}>
            <div class="tp__collapse-panel__header" onclick={toggle}>
                <span class="tp__collapse-panel__header-title">
                    { props.title_content.clone().unwrap_or_else(|| html! { &props.title }) }
                </span>
                <AppIcon name={ if expanded { "ChevronUp" } else {"ChevronDown"} }/>
            </div>
            <div class="tp__collapse-panel__body">
            { for props.children.iter() }
            </div>
        </div>
    }
}
