use crate::app::components::custom_dialog::CustomDialog;
use crate::app::components::TextButton;
use crate::model::{DialogAction, DialogActions, DialogResult};
use yew::prelude::*;
use yew_i18n::use_translation;

#[derive(Properties, PartialEq)]
pub struct ContentDialogProps {
    pub content: Html,
    pub actions: DialogActions,
    pub on_confirm: Callback<DialogResult>,
    #[prop_or(true)]
    pub close_on_backdrop_click: bool,
}

#[function_component]
pub fn ContentDialog(props: &ContentDialogProps) -> Html {
    let translate = use_translation();
    let is_open = use_state(|| true);

    let on_result = {
        let on_confirm = props.on_confirm.clone();
        let is_open = is_open.clone();
        move |result: DialogResult| {
            is_open.set(false);
            on_confirm.emit(result);
        }
    };

    let render_actions = |actions: Option<&Vec<DialogAction>>| {
        actions.map_or_else(|| html! {}, |actions| html! {
        <>
            { for actions.iter().map(|action| {
                let on_result = on_result.clone();
                let result = action.result.clone();
                html! {
                    <TextButton
                        autofocus={action.focus}
                        class={action.style.as_ref().map_or_else(String::new, ToString::to_string)}
                        name={action.name.clone()}
                        icon={action.icon.as_ref().map_or_else(String::new, |i| i.clone())}
                        onclick={Callback::from(move |_| on_result(result.clone()))}
                        title={translate.t(&action.label)}
                    />
                }
            }) }
        </>
    })
    };

    // Find a cancel action to use for backdrop clicks
    let on_close = {
        let on_result = on_result.clone();
        let cancel_action = props
            .actions
            .right
            .iter()
            .find(|action| matches!(action.result, DialogResult::Cancel))
            .or_else(|| props.actions.right.first());

        if let Some(action) = cancel_action {
            let result = action.result.clone();
            Some(Callback::from(move |_| {
                on_result(result.clone());
            }))
        } else {
            None
        }
    };

    html! {
        <CustomDialog
            open={*is_open}
            class="tp__content-dialog"
            modal=true
            close_on_backdrop_click={props.close_on_backdrop_click}
            on_close={on_close}
        >
            { props.content.clone() }
            <div class="tp__dialog__toolbar">
                <div class="tp__dialog__toolbar-left">
                    {render_actions(props.actions.left.as_ref())}
                </div>
                <div class="tp__dialog__toolbar-right">
                    {render_actions(Some(&props.actions.right))}
                </div>
            </div>
        </CustomDialog>
    }
}
