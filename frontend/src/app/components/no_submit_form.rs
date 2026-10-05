use yew::prelude::*;

#[derive(Properties, Clone, PartialEq)]
pub struct NoSubmitFormProps {
    pub children: Children,
}

/// Groups credential fields in a `<form>` so browsers stop warning about password fields
/// outside a form. It never submits: actions stay on their buttons and Enter handlers,
/// and `display: contents` keeps the surrounding layout unchanged.
#[component]
pub fn NoSubmitForm(props: &NoSubmitFormProps) -> Html {
    let onsubmit = Callback::from(|event: SubmitEvent| event.prevent_default());
    html! {
        <form class="tp__no-submit-form" {onsubmit}>
            { for props.children.iter() }
        </form>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod browser_tests {
    use super::{NoSubmitForm, NoSubmitFormProps};
    use gloo_timers::future::TimeoutFuture;
    use wasm_bindgen::JsValue;
    use wasm_bindgen_test::wasm_bindgen_test;
    use web_sys::{Event, EventInit};
    use yew::{html, Renderer};

    #[wasm_bindgen_test]
    async fn submit_is_always_prevented() -> Result<(), JsValue> {
        let document = gloo_utils::document();
        let body = document.body().ok_or_else(|| JsValue::from_str("test document has no body"))?;
        let root = document.create_element("div")?;
        body.append_child(&root)?;
        let children = html! { <input type="password" name="password" /> };
        let handle = Renderer::<NoSubmitForm>::with_root_and_props(
            root.clone(),
            NoSubmitFormProps { children: yew::Children::new(vec![children]) },
        )
        .render();
        TimeoutFuture::new(0).await;

        let form = root.query_selector("form.tp__no-submit-form")?.ok_or_else(|| JsValue::from_str("form missing"))?;
        assert!(form.query_selector("input[type=\"password\"]")?.is_some(), "password field must sit inside the form");
        let init = EventInit::new();
        init.set_bubbles(true);
        init.set_cancelable(true);
        let submit = Event::new_with_event_init_dict("submit", &init)?;
        form.dispatch_event(&submit)?;
        assert!(submit.default_prevented());

        handle.destroy();
        TimeoutFuture::new(0).await;
        root.remove();
        Ok(())
    }
}
