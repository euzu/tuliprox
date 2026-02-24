use crate::{
    app::components::{CustomDialog, TextButton},
    i18n::use_translation,
    model::DialogResult,
};
use yew::prelude::*;

#[derive(Properties, PartialEq)]
pub struct ConfirmDialogProps {
    pub title: String,
    pub ok_caption: String,
    pub cancel_caption: String,
    pub on_confirm: Callback<DialogResult>,
    #[prop_or(true)]
    pub close_on_backdrop_click: bool,
}

#[component]
pub fn ConfirmDialog(props: &ConfirmDialogProps) -> Html {
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

    let on_ok = {
        let on_result = on_result.clone();
        Callback::from(move |_: String| on_result(DialogResult::Ok))
    };

    let on_cancel = {
        let on_result = on_result.clone();
        Callback::from(move |_: String| on_result(DialogResult::Cancel))
    };

    let on_close = {
        let on_result = on_result.clone();
        Callback::from(move |()| on_result(DialogResult::Cancel))
    };

    let title_html: Html = props
        .title
        .split('\n')
        .map(|line| {
            html! { <>
                { line }
                <br/>
            </> }
        })
        .collect::<Html>();

    html! {
        <CustomDialog
            open={*is_open}
            class="tp__confirm-dialog"
            modal=true
            close_on_backdrop_click={props.close_on_backdrop_click}
            on_close={Some(on_close)}
        >
            <h2>{ title_html }</h2>
            <div class="tp__dialog__toolbar">
                <TextButton autofocus=true class="secondary" name="cancel" icon="Cancel" onclick={on_cancel} title={translate.t(&props.cancel_caption)} />
                <TextButton class="primary" name="ok" icon="Ok" onclick={on_ok} title={translate.t(&props.ok_caption)} />
            </div>
        </CustomDialog>
    }
}
