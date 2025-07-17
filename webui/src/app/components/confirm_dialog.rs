use yew::prelude::*;
use web_sys::HtmlDialogElement;
use yew_i18n::use_translation;
use crate::app::components::TextButton;
use crate::model::DialogResult;

#[derive(Properties, PartialEq)]
pub struct ConfirmDialogProps {
    pub title: String,
    pub ok_caption: String,
    pub cancel_caption: String,
    pub on_confirm: Callback<DialogResult>,
}

#[function_component]
pub fn ConfirmDialog(props: &ConfirmDialogProps) -> Html {
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

    let on_ok = {
        let on_result = on_result.clone();
        Callback::from(move |_| on_result(DialogResult::Ok))
    };

    let on_cancel = Callback::from(move |_| on_result(DialogResult::Cancel));

    html! {
        <dialog ref={dialog_ref} class="tp__dialog tp__confirm-dialog">
            <h2>{ &props.title }</h2>
            <div class="tp__dialog__toolbar">
                <TextButton autofocus=true style="secondary" name="cancel" icon="Cancel" onclick={on_cancel} title={translate.t(&props.cancel_caption)} />
                <TextButton style="primary" name="ok" icon="Ok" onclick={on_ok} title={translate.t(&props.ok_caption)} />
            </div>
        </dialog>
    }
}
