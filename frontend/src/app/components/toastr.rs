use crate::app::components::IconButton;
use crate::hooks::use_service_context;
use crate::services::{Toast, ToastCloseMode, ToastType};
use yew::prelude::*;
use yew_hooks::use_mount;

#[function_component]
pub fn ToastrView() -> Html {
    let service_ctx = use_service_context();
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

                            html! {
                                <div key={toast.id} class={classes!("tp__toast", type_class)}>
                                    <span class="tp__toast__message">
                                        { render_message(&toast.message) }
                                    </span>
                                    { close_btn }
                                </div>
                            }
                        }
                    })
                }
            </div>
        }
    }
}
