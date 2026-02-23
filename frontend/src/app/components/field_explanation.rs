use crate::{
    app::components::AppIcon,
    model::{DialogAction, DialogActions, DialogResult},
    services::DialogService,
    utils::t_safe,
};
use yew::{platform::spawn_local, prelude::*};
use yew_i18n::{use_translation, YewI18n};

fn normalize_field_id(raw: &str) -> String {
    let normalized = raw
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch.to_ascii_uppercase() } else { '_' })
        .collect::<String>();

    normalized.split('_').filter(|part| !part.is_empty()).collect::<Vec<_>>().join("_")
}

fn field_tokens(field_id: &str) -> Vec<&str> { field_id.split('_').filter(|part| !part.is_empty()).collect::<Vec<_>>() }

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

fn explanation_key_candidates(field_id: &str) -> Vec<String> {
    let mut keys = Vec::new();
    push_unique(&mut keys, format!("EXPLANATION.{field_id}"));

    let tokens = field_tokens(field_id);
    if tokens.len() > 1 {
        for split in (1..tokens.len()).rev() {
            let prefix = tokens[..split].join("_");
            let suffix = tokens[split..].join("_");
            push_unique(&mut keys, format!("EXPLANATION.{prefix}.{suffix}"));
        }
        for start in 1..tokens.len() {
            let suffix = tokens[start..].join("_");
            push_unique(&mut keys, format!("EXPLANATION.{suffix}"));
        }
    }

    push_unique(&mut keys, "EXPLANATION.DEFAULT".to_string());
    keys
}

fn label_key_candidates(field_id: &str) -> Vec<String> {
    let mut keys = Vec::new();
    push_unique(&mut keys, format!("LABEL.{field_id}"));

    let tokens = field_tokens(field_id);
    if tokens.len() > 1 {
        for start in 1..tokens.len() {
            let suffix = tokens[start..].join("_");
            push_unique(&mut keys, format!("LABEL.{suffix}"));
        }
    }
    keys
}

pub fn show_field_explanation(field_id: &str, field_label: &str, dialog: &DialogService, translate: &YewI18n) {
    // Caller is expected to pass a normalized key-compatible field_id.
    let explanation = explanation_key_candidates(field_id)
        .into_iter()
        .find_map(|key| t_safe(translate, &key))
        .unwrap_or_else(|| "No explanation available for this field.".to_string());

    let title = if field_label.trim().is_empty() {
        label_key_candidates(field_id)
            .into_iter()
            .find_map(|key| t_safe(translate, &key))
            .unwrap_or_else(|| field_id.replace('_', " "))
    } else {
        field_label.to_string()
    };

    let actions = DialogActions {
        left: None,
        right: vec![DialogAction::new_focused(
            "close",
            "LABEL.CLOSE",
            DialogResult::Cancel,
            Some("Close".to_string()),
            None,
        )],
    };

    let dialog = dialog.clone();
    spawn_local(async move {
        let _ = dialog
            .content(
                html! {
                    <div class="tp__field-explanation-dialog">
                        <h2>{title}</h2>
                        <p>{explanation}</p>
                    </div>
                },
                Some(actions),
                true,
            )
            .await;
    });
}

#[derive(Properties, Clone, PartialEq)]
pub struct FieldLabelProps {
    pub label: String,
    pub field_id: String,
    #[prop_or_default]
    pub for_id: Option<String>,
}

#[function_component]
pub fn FieldLabel(props: &FieldLabelProps) -> Html {
    let dialog = use_context::<DialogService>().expect("Dialog service not found");
    let translate = use_translation();
    let normalized_field_id = normalize_field_id(&props.field_id);

    let handle_help_click = {
        let dialog = dialog.clone();
        let translate = translate.clone();
        let field_id = normalized_field_id.clone();
        let field_label = props.label.clone();
        Callback::from(move |event: MouseEvent| {
            event.prevent_default();
            event.stop_propagation();
            show_field_explanation(&field_id, &field_label, &dialog, &translate);
        })
    };
    let handle_help_mousedown = Callback::from(move |event: MouseEvent| {
        event.prevent_default();
        event.stop_propagation();
    });
    let rendered_label = if let Some(for_id) = props.for_id.as_ref().filter(|id| !id.trim().is_empty()) {
        html! { <label for={for_id.clone()}>{props.label.clone()}</label> }
    } else {
        html! { <label>{props.label.clone()}</label> }
    };

    html! {
        <div class="tp__field-label">
            {rendered_label}
            <button
                class="tp__icon-button tp__field-label__help"
                type="button"
                title={translate.t("LABEL.HELP")}
                onmousedown={handle_help_mousedown}
                onclick={handle_help_click}
            >
                <AppIcon name="QuestionMark"/>
            </button>
        </div>
    }
}
