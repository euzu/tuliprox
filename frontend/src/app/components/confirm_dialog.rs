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

    html! {
        <CustomDialog
            open={*is_open}
            class="tp__confirm-dialog"
            modal=true
            aria_label={Some(props.title.replace('\n', " "))}
            close_on_backdrop_click={props.close_on_backdrop_click}
            on_close={Some(on_close)}
        >
            {confirm_dialog_message(&props.title)}
            <div class="tp__dialog__toolbar">
                <TextButton autofocus=true class="secondary" name="cancel" icon="Cancel" onclick={on_cancel} title={translate.t(&props.cancel_caption)} />
                <TextButton class="primary" name="ok" icon="Ok" onclick={on_ok} title={translate.t(&props.ok_caption)} />
            </div>
        </CustomDialog>
    }
}

fn confirm_dialog_message(message: &str) -> Html {
    html! { <p class="tp__confirm-dialog__message">{message.to_owned()}</p> }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirm_dialog_renders_short_multiline_and_long_messages_as_body_text() {
        for message in [
            "Delete this item?".to_owned(),
            "First line\nSecond line\n".to_owned(),
            "A long confirmation message. ".repeat(30),
            "<script>must remain plain text</script>".to_owned(),
        ] {
            let rendered = confirm_dialog_message(&message);
            let Html::VTag(paragraph) = &rendered else {
                panic!("confirmation message must be a paragraph");
            };
            assert_eq!(paragraph.tag(), "p");
            assert_eq!(rendered, html! { <p class="tp__confirm-dialog__message">{message}</p> });
        }
    }

    #[test]
    fn confirm_dialog_uses_standard_desktop_padding_and_preserves_mobile_spacing() {
        let styles = include_str!("../../../scss/app/components/_custom_dialog.scss");
        let native_dialog = include_str!("../../../scss/app/components/_dialog.scss");
        let confirmation = styles.split("&.tp__confirm-dialog {").nth(1).expect("confirmation styles");
        let confirmation = confirmation.split("&.tp__content-dialog {").next().expect("confirmation styles");

        let (desktop, mobile_and_body) =
            confirmation.split_once("@media (max-width: size.$mobile-breakpoint)").expect("existing mobile breakpoint");
        assert!(desktop.contains("--dialog-padding: var(--padding-default);"));
        assert!(!desktop.contains("--padding-larger"));
        let mobile = mobile_and_body.split('}').next().expect("mobile spacing");
        assert!(mobile.contains("--dialog-padding: var(--padding-larger);"));

        for shared_spacing in ["padding: var(--dialog-padding);", "gap: var(--gap-large);"] {
            assert!(native_dialog.contains(shared_spacing));
            assert!(confirmation.contains(shared_spacing));
        }
        assert!(confirmation.contains("max-width: min(size.$confirm-dialog-max-width, 90vw);"));
        assert!(confirmation.contains("box-sizing: border-box;"));
        assert!(confirmation.contains("font-size: var(--font-size-norm);"));
        assert!(confirmation.contains("white-space: pre-wrap;"));
        assert!(confirmation.contains("overflow-y: auto;"));
        assert!(confirmation.contains("flex-shrink: 0;"));
    }

    #[test]
    fn confirm_dialog_toolbar_spacing_matches_each_shared_dialog_padding() {
        let styles = include_str!("../../../scss/app/components/_custom_dialog.scss");
        let native_dialog = include_str!("../../../scss/app/components/_dialog.scss");

        assert!(styles.contains(".tp__custom-dialog {\n  --dialog-padding: var(--padding-default);"));
        assert!(styles.contains("&.tp__confirm-dialog {\n    --dialog-padding: var(--padding-default);"));
        assert_eq!(styles.matches("padding: var(--dialog-padding);").count(), 2);
        assert!(native_dialog.contains("--dialog-padding: var(--padding-larger);"));
        assert!(native_dialog.contains("padding: var(--dialog-padding);"));
        assert!(styles.contains("padding-top: var(--dialog-padding, var(--padding-small));"));
    }
}
