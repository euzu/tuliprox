use crate::{
    app::components::IconButton,
    hooks::use_service_context,
    i18n::use_translation,
    services::{Toast, ToastCloseMode, ToastType},
};
use yew::prelude::*;
use yew_hooks::{use_clipboard, use_mount};

#[component]
pub fn ToastrView() -> Html {
    let service_ctx = use_service_context();
    let translate = use_translation();
    let clipboard = use_clipboard();
    let toasts = use_state(Vec::<Toast>::new);

    {
        // Subscribe to toast updates when component mounts
        let service_ctx = service_ctx.clone();
        let toasts = toasts.clone();
        use_mount(move || {
            service_ctx.toastr.subscribe(move |new_toasts| {
                toasts.set(new_toasts);
            })
        });
    }

    let render_message = |msg: &str| {
        html! {
         <>
             { for msg.split('\n').map(|line| html! { <span>{ line }</span> }) }
         </>
        }
    };

    if toasts.is_empty() {
        html! {}
    } else {
        html! {
            <div class="tp__toastr__container">
                {
                    // Render each toast and show an "X" icon button when close mode is Manual
                    for toasts.iter().cloned().map({
                        let service_ctx = service_ctx.clone();
                        let translate = translate.clone();
                        let clipboard = clipboard.clone();
                        move |toast| {
                            // Decide visual style per toast type
                            let type_class = match toast.toast_type {
                                ToastType::Success => "success",
                                ToastType::Info => "info",
                                ToastType::Warning => "warning",
                                ToastType::Error => "error",
                            };

                            // Create close button only for Manual close mode
                            let close_btn = if matches!(toast.close_mode, ToastCloseMode::Manual) {
                                let on_close = {
                                    let service_ctx = service_ctx.clone();
                                    let id = toast.id;
                                    // IconButton emits (name, MouseEvent); we only need to know it was clicked
                                    Callback::from(move |(_name, _e)| {
                                        service_ctx.toastr.dismiss(id);
                                    })
                                };

                                html! {
                                    <IconButton
                                        name={"toastr-close"}
                                        icon={"Close"}
                                        onclick={on_close}
                                    />
                                }
                            } else {
                                html! {}
                            };
                            
                            let copy_btn = if matches!(toast.toast_type, ToastType::Error) {
                                let on_copy = {
                                    let clipboard = clipboard.clone();
                                    let message = toast.message.clone();
                                    Callback::from(move |(_name, _e)| {
                                        if *clipboard.is_supported {
                                            clipboard.write_text(message.clone());
                                        }
                                    })
                                };
                                html! {
                                    <IconButton
                                        name={"toastr-copy"}
                                        icon={"Clipboard"}
                                        hint={translate.t("LABEL.COPY_DETAILS")}
                                        onclick={on_copy}
                                    />
                                }
                            } else {
                                html! {}
                            };

                            // Pause the auto-dismiss countdown while the pointer hovers the
                            // toast, and resume it on leave (the progress bar mirrors this in CSS).
                            let on_mouse_enter = {
                                let service_ctx = service_ctx.clone();
                                let id = toast.id;
                                Callback::from(move |_e: MouseEvent| {
                                    service_ctx.toastr.pause(id);
                                })
                            };
                            let on_mouse_leave = {
                                let service_ctx = service_ctx.clone();
                                let id = toast.id;
                                Callback::from(move |_e: MouseEvent| {
                                    service_ctx.toastr.resume(id);
                                })
                            };

                            // Auto-dismiss toasts get a progress bar whose duration matches the timer.
                            let progress = match toast.duration_ms() {
                                Some(duration_ms) => html! {
                                    <div
                                        class="tp__toast__progress"
                                        style={format!("animation-duration: {duration_ms}ms")}
                                    />
                                },
                                None => html! {},
                            };

                            html! {
                                <div
                                    key={toast.id}
                                    class={classes!("tp__toast", type_class)}
                                    onmouseenter={on_mouse_enter}
                                    onmouseleave={on_mouse_leave}
                                >
                                    <span class="tp__toast__message">
                                        { render_message(&toast.message) }
                                    </span>
                                    { copy_btn }
                                    { close_btn }
                                    { progress }
                                </div>
                            }
                        }
                    })
                }
            </div>
        }
    }
}
