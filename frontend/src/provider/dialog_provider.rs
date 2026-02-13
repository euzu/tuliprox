use crate::app::{ConfirmDialog, ContentDialog};
use crate::model::DialogResult;
use crate::services::{DialogRequest, DialogService};
use yew::{
    function_component, html, use_effect_with, use_state, Callback, Children, ContextProvider,
    Html, Properties,
};

#[derive(Properties, PartialEq)]
pub struct ConfirmProviderProps {
    pub children: Children,
}

#[function_component]
pub fn DialogProvider(props: &ConfirmProviderProps) -> Html {
    let service = use_state(DialogService::new);
    let dialog_request = use_state(|| None::<DialogRequest>);

    {
        let service = service.clone();
        let request = dialog_request.clone();
        use_effect_with((), move |_| {
            service.register(Callback::from(move |req: DialogRequest| {
                request.set(Some(req));
            }));
            || ()
        });
    }

    let on_confirm = {
        let request = dialog_request.clone();
        Callback::from(move |result: DialogResult| {
            if let Some(req) = &*request {
                match req {
                    DialogRequest::Confirm(confirm) => {
                        if let Some(cb) = confirm.resolve.borrow_mut().take() {
                            cb(result);
                        }
                    }
                    DialogRequest::Content(content) => {
                        if let Some(cb) = content.resolve.borrow_mut().take() {
                            cb(result);
                        }
                    }
                }
            }
            request.set(None);
        })
    };

    html! {
        <ContextProvider<DialogService> context={(*service).clone()}>
            { for props.children.iter() }
            {
                if let Some(request) = &*dialog_request {
                     match request {
                        DialogRequest::Confirm(confirm) => {
                            html! {
                              <ConfirmDialog
                                    title={confirm.title.clone()}
                                    ok_caption={confirm.ok_caption.clone()}
                                    cancel_caption={confirm.cancel_caption.clone()}
                                    on_confirm={on_confirm.clone()}
                                />
                            }
                        }
                        DialogRequest::Content(content) => {
                            html! {
                              <ContentDialog
                                    content={content.content.clone()}
                                    actions={content.actions.clone()}
                                    close_on_backdrop_click={content.close_on_backdrop_click}
                                    on_confirm={on_confirm.clone()}
                                />
                            }
                        }
                    }
                } else {
                    html! {}
                }
            }
        </ContextProvider<DialogService>>
    }
}
