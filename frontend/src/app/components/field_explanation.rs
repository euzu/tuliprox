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

fn fallback_field_id(field_id: &str) -> Option<String> {
    field_id.rsplit('_').next().map(normalize_field_id).filter(|fallback| fallback != field_id)
}

pub fn show_field_explanation(field_id: &str, field_label: &str, dialog: &DialogService, translate: &YewI18n) {
    let normalized = normalize_field_id(field_id);
    let primary_key = format!("EXPLANATION.{normalized}");
    let fallback_key = fallback_field_id(&normalized).map(|id| format!("EXPLANATION.{id}"));

    let explanation = t_safe(translate, &primary_key)
        .or_else(|| fallback_key.as_ref().and_then(|key| t_safe(translate, key)))
        .or_else(|| t_safe(translate, "EXPLANATION.DEFAULT"))
        .unwrap_or_else(|| "No explanation available for this field.".to_string());

    let title = if field_label.trim().is_empty() {
        t_safe(translate, &format!("LABEL.{normalized}"))
            .or_else(|| fallback_field_id(&normalized).and_then(|id| t_safe(translate, &format!("LABEL.{id}"))))
            .unwrap_or_else(|| normalized.replace('_', " "))
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
}

#[function_component]
pub fn FieldLabel(props: &FieldLabelProps) -> Html {
    let dialog = use_context::<DialogService>().expect("Dialog service not found");
    let translate = use_translation();
    let field_id = normalize_field_id(&props.field_id);

    let handle_help_click = {
        let dialog = dialog.clone();
        let translate = translate.clone();
        let field_id = field_id.clone();
        let field_label = props.label.clone();
        Callback::from(move |event: MouseEvent| {
            event.prevent_default();
            event.stop_propagation();
            show_field_explanation(&field_id, &field_label, &dialog, &translate);
        })
    };

    html! {
        <div class="tp__field-label">
            <label>{props.label.clone()}</label>
            <button
                class="tp__icon-button tp__field-label__help"
                type="button"
                title={translate.t("LABEL.HELP")}
                onclick={handle_help_click}
            >
                <AppIcon name="QuestionMark"/>
            </button>
        </div>
    }
}
