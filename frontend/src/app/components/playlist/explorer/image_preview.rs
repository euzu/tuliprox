use crate::i18n::use_translation;
use yew::prelude::*;

/// Resolves the URL for an image preview.
/// - Provider values (which may already be sealed `/api/v1/playlist/resource` URLs) load through that URL.
/// - User-typed URLs are loaded directly in the browser and are never proxied or sent to server.
pub fn resolve_preview_image_url(url: &str, is_provider_value: bool) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }
    let _ = is_provider_value;
    Some(trimmed.to_string())
}

#[derive(Properties, PartialEq, Clone)]
pub struct ImagePreviewProps {
    pub url: String,
    #[prop_or(false)]
    pub is_provider_value: bool,
    #[prop_or_default]
    pub class: Option<String>,
    #[prop_or_default]
    pub alt: Option<String>,
}

#[component]
pub fn ImagePreview(props: &ImagePreviewProps) -> Html {
    let translate = use_translation();
    let has_error = use_state(|| false);

    {
        let has_error = has_error.clone();
        use_effect_with(props.url.clone(), move |_| {
            has_error.set(false);
            || ()
        });
    }

    let preview_url = resolve_preview_image_url(&props.url, props.is_provider_value);
    let Some(src) = preview_url else {
        return html! {
            <div class={classes!("tp__image-preview", "tp__image-preview--empty", props.class.clone())}>
                <span class="tp__image-preview__empty-icon">{ "🖼" }</span>
            </div>
        };
    };

    if *has_error {
        return html! {
            <div class={classes!("tp__image-preview", "tp__image-preview--error", props.class.clone())}>
                <span class="tp__image-preview__error-text">
                    { translate.t("LABEL.EXPLORER_IMG_COULD_NOT_LOAD") }
                </span>
            </div>
        };
    }

    let on_error = {
        let has_error = has_error.clone();
        Callback::from(move |_| has_error.set(true))
    };

    let alt = props.alt.clone().unwrap_or_else(|| translate.t("LABEL.LOGO"));

    html! {
        <div class={classes!("tp__image-preview", props.class.clone())}>
            <img
                src={src}
                alt={alt}
                loading="lazy"
                onerror={on_error}
            />
        </div>
    }
}

#[cfg(test)]
#[path = "image_preview.test.rs"]
mod tests;
