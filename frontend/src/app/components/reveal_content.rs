use crate::{app::components::AppIcon, model::DialogActions, services::DialogService};
use yew::{platform::spawn_local, prelude::*};

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct RevealContentProps {
    #[prop_or_default]
    pub icon: String,
    #[prop_or_default]
    pub preview: Option<Html>,
    pub children: Html,
    #[prop_or_default]
    pub actions: Option<DialogActions>,
}

#[component]
pub fn RevealContent(props: &RevealContentProps) -> Html {
    let dialog = use_context::<DialogService>().expect("Dialog service not found");

    let handle_click = {
        let dialog = dialog.clone();
        let content = props.children.clone();
        let actions = props.actions.clone();
        Callback::from(move |e: MouseEvent| {
            e.prevent_default();
            e.stop_propagation();
            let content = content.clone();
            let actions = actions.clone();
            let dlg = dialog.clone();
            spawn_local(async move {
                let _result = dlg.content(content, actions, true).await;
            });
        })
    };

    html! {
        <div class={"tp__reveal-content"} onclick={handle_click}>
        {
            match props.preview.as_ref() {
              None => html! {},
              Some(preview) => html! {
                <span class="tp__reveal-content__preview">{preview.clone()}</span>
              }
            }
        }

         <AppIcon name={
            if props.icon.is_empty() {
               if props.preview.is_some() {
                    "Expand".to_owned()
                } else {
                    "Ellipsis".to_owned()
                }
            } else {
                props.icon.clone()
            }
        } />
        </div>
    }
}
