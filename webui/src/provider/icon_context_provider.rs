use crate::hooks::{IconContext, IconDefinition};
use std::rc::Rc;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq)]
pub struct IconContextProps {
    pub children: Children,
    pub icons: Vec<Rc<IconDefinition>>,
}

#[function_component]
pub fn IconContextProvider(props: &IconContextProps) -> Html {
    let icon_ctx = use_state(||IconContext::new(&props.icons));

    html! {
        <ContextProvider<UseStateHandle<IconContext>> context={icon_ctx}>
            { for props.children.iter() }
        </ContextProvider<UseStateHandle<IconContext>>>
    }
}