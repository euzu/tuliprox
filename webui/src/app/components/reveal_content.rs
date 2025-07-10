use wasm_bindgen::JsCast;
use yew::platform::spawn_local;
use yew::prelude::*;
use crate::app::components::AppIcon;
use crate::model::{DialogActions};
use crate::services::DialogService;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct RevealContentProps {
    #[prop_or_default]
    pub icon: String,
    #[prop_or_default]
    pub preview: Html,
    pub children: Html,
    #[prop_or_default]
    pub actions: Option<DialogActions>
}

#[function_component]
pub fn RevealContent(props: &RevealContentProps) -> Html {
    let dialog = use_context::<DialogService>().expect("Dialog service not found");

    let handle_click = {
        let dialog = dialog.clone();
        let content = props.children.clone();
        let actions = props.actions.clone();
        Callback::from(move |e: MouseEvent| {
            e.prevent_default();
            let content = content.clone();
            let actions = actions.clone();
            let dlg = dialog.clone();
            spawn_local(async move {
                let _result = dlg.content(content, actions).await;
            });
        })
    };

    html! {
        <div class={"tp__reveal-content"} onclick={handle_click}>
           <span class="tp__reveal-content__preview">{props.preview.clone()}</span>
           <AppIcon name={if props.icon.is_empty() {"Expand".to_string()} else {props.icon.to_string()} } />
        </div>
    }
}