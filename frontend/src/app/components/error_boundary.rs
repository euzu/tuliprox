use crate::{
    app::components::{AppIcon, TextButton},
    i18n::use_translation,
};
use yew::prelude::*;

#[derive(Clone, PartialEq)]
pub struct ErrorBoundaryHandle {
    report: Callback<String>,
    reset: Callback<()>,
}

impl ErrorBoundaryHandle {
    pub fn report(&self, message: impl Into<String>) { self.report.emit(message.into()); }

    pub fn reset(&self) { self.reset.emit(()); }
}

#[derive(Properties, Clone, PartialEq)]
pub struct ErrorBoundaryProps {
    #[prop_or_default]
    pub name: AttrValue,
    #[prop_or_default]
    pub class: Classes,
    #[prop_or_default]
    pub children: Children,
}

#[component]
pub fn ErrorBoundary(props: &ErrorBoundaryProps) -> Html {
    let translate = use_translation();
    let error = use_state(|| None::<String>);
    let generation = use_state(|| 0usize);

    let report = {
        let error = error.clone();
        Callback::from(move |message: String| {
            log::error!("ErrorBoundary captured: {message}");
            error.set(Some(message));
        })
    };

    let reset = {
        let error = error.clone();
        let generation = generation.clone();
        Callback::from(move |()| {
            error.set(None);
            generation.set(*generation + 1);
        })
    };

    let handle = ErrorBoundaryHandle { report, reset: reset.clone() };

    if let Some(message) = error.as_ref() {
        let on_retry = {
            let reset = reset.clone();
            Callback::from(move |_: String| reset.emit(()))
        };
        let title = if props.name.is_empty() {
            translate.t("LABEL.SOMETHING_WENT_WRONG")
        } else {
            format!("{} — {}", props.name, translate.t("LABEL.SOMETHING_WENT_WRONG"))
        };

        return html! {
            <div class={classes!("tp__error-boundary", props.class.clone())} role="alert">
                <div class="tp__error-boundary__icon">
                    <AppIcon name="Error" />
                </div>
                <div class="tp__error-boundary__title">{ title }</div>
                <div class="tp__error-boundary__message">{ message.clone() }</div>
                <TextButton
                    name="retry"
                    icon="Refresh"
                    class="tp__error-boundary__retry"
                    title={translate.t("LABEL.RETRY")}
                    onclick={on_retry}
                />
            </div>
        };
    }

    html! {
        <ContextProvider<ErrorBoundaryHandle> context={handle} key={*generation}>
            { for props.children.iter() }
        </ContextProvider<ErrorBoundaryHandle>>
    }
}

#[hook]
pub fn use_error_boundary() -> Option<ErrorBoundaryHandle> { use_context::<ErrorBoundaryHandle>() }
