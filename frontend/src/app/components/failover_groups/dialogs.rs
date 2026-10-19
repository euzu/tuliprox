//! Small dialogs of the failover groups page: a name prompt and a list picker.

use crate::{
    app::components::{CustomDialog, TextButton},
    edit_field_text, generate_form_reducer,
    i18n::use_translation,
    use_default_form_reducer,
    utils::in_body,
};
use yew::prelude::*;

/// Form data of the name prompt; the type name is the prefix of the field id used for labels and explanations.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NewFailoverGroup {
    pub name: String,
}

generate_form_reducer!(
    state: NewFailoverGroupFormState { form: NewFailoverGroup },
    action_name: NewFailoverGroupFormAction,
    fields {
        Name => name: String,
    }
);

#[derive(Properties, PartialEq)]
pub struct NameDialogProps {
    pub title: String,
    pub on_submit: Callback<String>,
    pub on_cancel: Callback<()>,
}

/// Asks for the name of a new manual group.
#[component]
pub fn NameDialog(props: &NameDialogProps) -> Html {
    let translate = use_translation();
    let form_state: UseReducerHandle<NewFailoverGroupFormState> =
        use_default_form_reducer!(NewFailoverGroupFormState { form: NewFailoverGroup::default() });
    let name = form_state.form.name.trim().to_string();
    let can_submit = !name.is_empty();

    let on_submit = {
        let on_submit = props.on_submit.clone();
        let name = name.clone();
        Callback::from(move |_: String| {
            if !name.is_empty() {
                on_submit.emit(name.clone());
            }
        })
    };
    let on_cancel = {
        let on_cancel = props.on_cancel.clone();
        Callback::from(move |_: String| on_cancel.emit(()))
    };
    let on_close = {
        let on_cancel = props.on_cancel.clone();
        Callback::from(move |()| on_cancel.emit(()))
    };

    in_body(html! {
        <CustomDialog open={true} modal={true} class={Some("tp__failover-groups__dialog".to_string())}
            close_on_backdrop_click={false} on_close={Some(on_close)} aria_label={Some(props.title.clone())}>
            <h2>{ &props.title }</h2>
            <div class="tp__failover-groups__dialog-body">
                { edit_field_text!(form_state, translate.t("LABEL.NAME"), name, NewFailoverGroupFormAction::Name) }
            </div>
            <div class="tp__dialog__toolbar">
                <TextButton class="secondary" name="cancel" icon="Cancel" title={translate.t("LABEL.CANCEL")}
                    onclick={on_cancel} />
                <TextButton class="primary" name="submit" icon="Ok" title={translate.t("LABEL.SUBMIT")}
                    disabled={!can_submit} onclick={on_submit} />
            </div>
        </CustomDialog>
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct PickerItem {
    pub id: String,
    pub label: String,
}

#[derive(Properties, PartialEq)]
pub struct PickerDialogProps {
    pub title: String,
    pub items: Vec<PickerItem>,
    pub on_pick: Callback<String>,
    pub on_cancel: Callback<()>,
}

/// Lets the user pick one entry of a list; the list scrolls inside the dialog.
#[component]
pub fn PickerDialog(props: &PickerDialogProps) -> Html {
    let translate = use_translation();
    let on_cancel = {
        let on_cancel = props.on_cancel.clone();
        Callback::from(move |_: String| on_cancel.emit(()))
    };
    let on_close = {
        let on_cancel = props.on_cancel.clone();
        Callback::from(move |()| on_cancel.emit(()))
    };

    in_body(html! {
        <CustomDialog open={true} modal={true} class={Some("tp__failover-groups__dialog".to_string())}
            close_on_backdrop_click={false} on_close={Some(on_close)} aria_label={Some(props.title.clone())}>
            <h2>{ &props.title }</h2>
            <ul class="tp__failover-groups__dialog-list">
                { for props.items.iter().map(|item| {
                    let onclick = {
                        let on_pick = props.on_pick.clone();
                        let id = item.id.clone();
                        Callback::from(move |_: MouseEvent| on_pick.emit(id.clone()))
                    };
                    html! {
                        <li key={item.id.clone()}>
                            <button type="button" class="tp__failover-groups__dialog-item" onclick={onclick}>
                                { &item.label }
                            </button>
                        </li>
                    }
                }) }
                if props.items.is_empty() {
                    <li class="tp__failover-groups__list-empty">{ translate.t("LABEL.NO_CONTENT") }</li>
                }
            </ul>
            <div class="tp__dialog__toolbar">
                <TextButton class="secondary" name="cancel" icon="Cancel" title={translate.t("LABEL.CANCEL")}
                    onclick={on_cancel} />
            </div>
        </CustomDialog>
    })
}
