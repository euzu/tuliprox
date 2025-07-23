use yew::prelude::*;
use web_sys::HtmlDialogElement;
use yew_i18n::use_translation;
use crate::app::components::TextButton;
use crate::model::{DialogAction, DialogActions, DialogResult};

#[derive(Properties, PartialEq)]
pub struct ContentDialogProps {
    pub content: Html,
    pub actions: DialogActions,
    pub on_confirm: Callback<DialogResult>,
}

#[function_component]
pub fn ContentDialog(props: &ContentDialogProps) -> Html {
    let dialog_ref = use_node_ref();
    let translate = use_translation();

    {
        let dialog_ref = dialog_ref.clone();
        use_effect(move || {
            if let Some(dialog) = dialog_ref.cast::<HtmlDialogElement>() {
                let _ = dialog.show_modal();
            }
            || ()
        });
    }

    let on_result = {
        let dialog_ref = dialog_ref.clone();
        let on_confirm = props.on_confirm.clone();
        move |result: DialogResult| {
            if let Some(dialog) = dialog_ref.cast::<HtmlDialogElement>() {
                dialog.close();
            }
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
                        style={action.style.as_ref().map_or_else(String::new, ToString::to_string)}
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

    html! {
        <dialog ref={dialog_ref} class="tp__dialog tp__content-dialog">
            { props.content.clone() }
            <div class="tp__dialog__toolbar">
                <div class="tp__dialog__toolbar-left">
                    {render_actions(props.actions.left.as_ref())}
                </div>
                <div class="tp__dialog__toolbar-right">
                    {render_actions(Some(&props.actions.right))}
                </div>
            </div>
        </dialog>
    }
}
