use crate::hooks::use_service_context;
use crate::model::{BusyStatus, EventMessage};
use yew::{
    classes, function_component, html, use_effect_with, use_mut_ref, use_state, Html, Properties,
};

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct LoadingIndicatorProps {
    pub loading: bool,
    #[prop_or_default]
    pub class: String,
}

#[function_component]
pub fn LoadingIndicator(props: &LoadingIndicatorProps) -> Html {
    if !props.loading {
        html! {<div class={classes!("tp__loading-bar-placeholder", props.class.clone())}></div> }
    } else {
        html! {
         <div class={classes!("tp__loading-bar-container", props.class.clone())}>
            <div class="tp__loading-bar"></div>
          </div>
        }
    }
}

#[function_component]
pub fn BusyIndicator() -> Html {
    let counter = use_mut_ref(|| 0u32);
    let loading = use_state(|| false);
    let service_ctx = use_service_context();
    {
        let services = service_ctx.clone();
        let loading = loading.clone();
        let counter = counter.clone();
        use_effect_with((), move |_| {
            let sub_id = services.event.subscribe(move |msg| {
                if let EventMessage::Busy(status) = msg {
                    match status {
                        BusyStatus::Show => {
                            *counter.borrow_mut() += 1;
                            loading.set(true);
                        }
                        BusyStatus::Hide => {
                            let mut cnt = counter.borrow_mut();
                            if *cnt > 0 {
                                *cnt -= 1;
                            }
                            if *cnt == 0 {
                                loading.set(false);
                            }
                        }
                    }
                }
            });
            move || services.event.unsubscribe(sub_id)
        });
    }

    html! {
        <div class="tp__busy-indicator">
            <LoadingIndicator loading={*loading} />
        </div>
    }
}
