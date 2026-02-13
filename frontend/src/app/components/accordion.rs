use std::rc::Rc;
use yew::prelude::*;

#[derive(Debug, Clone, PartialEq)]
pub struct AccordionState {
    pub active_panel: Rc<UseStateHandle<Option<String>>>,
}

#[derive(Properties, PartialEq)]
pub struct AccordionProps {
    #[prop_or_default]
    pub children: Children,
    #[prop_or_default]
    pub default_panel: Option<String>,
}

#[function_component]
pub fn Accordion(props: &AccordionProps) -> Html {
    let active_panel = use_state(|| props.default_panel.clone());
    let state = AccordionState {
        active_panel: Rc::new(active_panel),
    };

    html! {
        <ContextProvider<AccordionState> context={state}>
            <div class="tp__accordion">
                { for props.children.iter() }
            </div>
        </ContextProvider<AccordionState>>
    }
}
