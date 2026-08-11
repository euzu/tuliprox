use crate::{hooks::use_service_context, i18n::use_translation, services::DialogService};
use yew::{platform::spawn_local, prelude::*};
use yew_hooks::use_clipboard;

/// Returns a callback that copies text to the clipboard with a success toast,
/// falling back to a dialog with a selectable input when clipboard access is
/// unavailable (e.g. non-HTTPS contexts).
#[hook]
pub fn use_clipboard_copy() -> Callback<String> {
    let clipboard = use_clipboard();
    let dialog = use_context::<DialogService>().expect("Dialog service not found");
    let services = use_service_context();
    let translate = use_translation();

    Callback::from(move |text: String| {
        if *clipboard.is_supported {
            clipboard.write_text(text);
            services.toastr.success(translate.t("MESSAGES.COPIED_TO_CLIPBOARD"));
        } else {
            let dlg = dialog.clone();
            spawn_local(async move {
                let _ = dlg
                    .content(html! {<input value={text} readonly={true} class="tp__copy-input"/>}, None, false)
                    .await;
            });
        }
    })
}
