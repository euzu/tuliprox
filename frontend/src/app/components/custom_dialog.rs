use web_sys::MouseEvent;
use yew::prelude::*;

#[derive(Properties, PartialEq)]
pub struct CustomDialogProps {
    pub children: Children,
    pub class: Option<String>,
    #[prop_or(true)]
    pub open: bool,
    #[prop_or(true)]
    pub modal: bool,
    #[prop_or(false)]
    pub close_on_backdrop_click: bool,
    pub on_close: Option<Callback<()>>,
}

#[function_component]
pub fn CustomDialog(props: &CustomDialogProps) -> Html {
    let is_open = use_state(|| props.open);

    // Update state when props change
    {
        let is_open = is_open.clone();
        use_effect_with(props.open, move |&open| {
            is_open.set(open);
            || ()
        });
    }

    // Handle backdrop click
    let on_backdrop_click = {
        let on_close = props.on_close.clone();
        let close_on_backdrop = props.close_on_backdrop_click;

        Callback::from(move |_e: MouseEvent| {
            if close_on_backdrop {
                if let Some(on_close) = &on_close {
                    on_close.emit(());
                }
            }
        })
    };

    // Only render if open
    if !*is_open {
        return html! {};
    }

    html! {
        <div class={classes!("tp__custom-dialog-backdrop", if props.modal {"tp__custom-dialog-modal"} else {""})} onclick={on_backdrop_click}>
            <div class={classes!("tp__custom-dialog", props.class.as_ref().map_or_else(||"".to_owned(), |s|s.clone()))} onclick={Callback::from(|e: MouseEvent| e.stop_propagation())}>
                { for props.children.iter() }
            </div>
        </div>
    }
}
