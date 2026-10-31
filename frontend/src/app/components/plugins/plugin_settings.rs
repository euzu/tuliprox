use crate::{
    app::components::{
        plugins::schema_form::{prefill_defaults, schema_supported, SchemaForm},
        AppIcon, ContentDialog, LoadingIndicator,
    },
    hooks::use_service_context,
    i18n::use_translation,
    model::{DialogAction, DialogActions, DialogResult},
    services::PluginSaveError,
};
use serde_json::Value;
use shared::model::{PluginFieldErrorDto, PluginSecretDto, PluginSettingsDto, PluginSettingsSaveDto};
use yew::{component, html, platform::spawn_local, use_effect_with, use_state, Callback, Html, Properties};

#[derive(Properties, Clone, PartialEq)]
pub struct PluginSettingsProps {
    pub plugin_id: String,
    /// Shown in the dialog title.
    pub plugin_name: String,
    pub on_close: Callback<()>,
}

/// What the dialog shows once the settings are loaded.
#[derive(Clone, PartialEq)]
struct LoadedSettings {
    schema: Option<Value>,
    secrets: Vec<PluginSecretDto>,
    /// The saved settings, as the server returned them (secrets masked).
    saved: Value,
}

fn pretty_json(value: &Value) -> String { serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()) }

/// The settings dialog of one plugin: loads `GET …/settings`, edits them with a
/// [`SchemaForm`] over the shipped schema and saves with `PUT …/settings`.
#[component]
pub fn PluginSettings(props: &PluginSettingsProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();

    let loaded = use_state(|| None::<LoadedSettings>);
    let loading = use_state(|| true);
    let load_error = use_state(|| None::<String>);
    let draft = use_state(|| Value::Object(serde_json::Map::new()));
    let field_errors = use_state(Vec::<PluginFieldErrorDto>::new);
    let top_error = use_state(|| None::<String>);
    let warnings = use_state(Vec::<String>::new);
    let saving = use_state(|| false);
    // Bumped after every load and save so the form is rebuilt from the returned settings.
    let generation = use_state(|| 0_u32);

    {
        let services = services.clone();
        let loaded = loaded.clone();
        let loading = loading.clone();
        let load_error = load_error.clone();
        let draft = draft.clone();
        let generation = generation.clone();
        use_effect_with(props.plugin_id.clone(), move |plugin_id| {
            let plugin_id = plugin_id.clone();
            loading.set(true);
            spawn_local(async move {
                match services.plugins.get_settings(&plugin_id).await {
                    Ok(dto) => {
                        let initial = dto
                            .schema
                            .as_ref()
                            .map_or_else(|| dto.settings.clone(), |schema| prefill_defaults(schema, &dto.settings));
                        draft.set(initial);
                        loaded.set(Some(LoadedSettings {
                            schema: dto.schema,
                            secrets: dto.secrets,
                            saved: dto.settings,
                        }));
                        load_error.set(None);
                        generation.set(*generation + 1);
                    }
                    Err(err) => load_error.set(Some(err.to_string())),
                }
                loading.set(false);
            });
            || ()
        });
    }

    let on_change = {
        let draft = draft.clone();
        Callback::from(move |value: Value| draft.set(value))
    };

    let can_save = loaded.as_ref().and_then(|settings| settings.schema.as_ref()).is_some_and(schema_supported);

    let save = {
        let services = services.clone();
        let plugin_id = props.plugin_id.clone();
        let translate = translate.clone();
        let loaded = loaded.clone();
        let draft = draft.clone();
        let field_errors = field_errors.clone();
        let top_error = top_error.clone();
        let warnings = warnings.clone();
        let saving = saving.clone();
        let generation = generation.clone();
        Callback::from(move |()| {
            if *saving {
                return;
            }
            saving.set(true);
            let services = services.clone();
            let plugin_id = plugin_id.clone();
            let translate = translate.clone();
            let loaded = loaded.clone();
            let draft = draft.clone();
            let field_errors = field_errors.clone();
            let top_error = top_error.clone();
            let warnings = warnings.clone();
            let saving = saving.clone();
            let generation = generation.clone();
            let body = PluginSettingsSaveDto { settings: (*draft).clone() };
            spawn_local(async move {
                match services.plugins.save_settings(&plugin_id, &body).await {
                    Ok(PluginSettingsDto { settings, schema, secrets, warnings: notes }) => {
                        services.toastr.success(translate.t("MESSAGES.PLUGIN_SETTINGS_SAVED"));
                        let schema = schema.or_else(|| loaded.as_ref().and_then(|current| current.schema.clone()));
                        let next = schema
                            .as_ref()
                            .map_or_else(|| settings.clone(), |schema| prefill_defaults(schema, &settings));
                        draft.set(next);
                        loaded.set(Some(LoadedSettings { schema, secrets, saved: settings }));
                        field_errors.set(Vec::new());
                        top_error.set(None);
                        warnings.set(notes);
                        generation.set(*generation + 1);
                    }
                    Err(PluginSaveError::Invalid(dto)) => {
                        top_error.set(Some(dto.error));
                        field_errors.set(dto.fields);
                        warnings.set(Vec::new());
                    }
                    Err(PluginSaveError::Other(message)) => services.toastr.error(message),
                }
                saving.set(false);
            });
        })
    };

    let on_result = {
        let on_close = props.on_close.clone();
        let save = save.clone();
        Callback::from(move |result: DialogResult| match result {
            DialogResult::Ok => save.emit(()),
            DialogResult::Cancel => on_close.emit(()),
        })
    };

    let actions = DialogActions {
        left: Some(vec![DialogAction::new(
            "close",
            "LABEL.CLOSE",
            DialogResult::Cancel,
            Some("Close".to_owned()),
            Some("secondary".to_owned()),
        )]),
        right: vec![DialogAction::new(
            "save",
            "LABEL.SAVE",
            DialogResult::Ok,
            Some("Save".to_owned()),
            Some("primary".to_owned()),
        )
        .with_disabled(*saving || *loading || !can_save)],
    };

    let body = if let Some(message) = load_error.as_ref() {
        html! {
            <div class="tp__plugin-settings__error tp__input-error" role="alert">
                { format!("{} {message}", translate.t("MESSAGES.PLUGIN_SETTINGS_LOAD_FAILED")) }
            </div>
        }
    } else if let Some(settings) = loaded.as_ref() {
        let form = match settings.schema.as_ref() {
            Some(schema) => html! {
                <SchemaForm
                    key={generation.to_string()}
                    schema={schema.clone()}
                    value={(*draft).clone()}
                    secrets={settings.secrets.clone()}
                    errors={(*field_errors).clone()}
                    on_change={on_change.clone()}
                />
            },
            None => html! {
                <p class="tp__plugin-settings__note">{ translate.t("MESSAGES.PLUGIN_SETTINGS_NO_SCHEMA") }</p>
            },
        };
        // Without a schema, or when the form cannot render it, the settings are shown as they are stored.
        let read_only = if can_save {
            html! {}
        } else {
            html! { <pre class="tp__plugin-settings__json"><code>{ pretty_json(&settings.saved) }</code></pre> }
        };
        html! {
            <>
                {
                    if let Some(error) = top_error.as_ref() {
                        html! { <div class="tp__plugin-settings__error tp__input-error" role="alert">{ error.clone() }</div> }
                    } else {
                        html! {}
                    }
                }
                { form }
                { read_only }
                if !warnings.is_empty() {
                    <ul class="tp__plugin-settings__warnings">
                        { for warnings.iter().map(|warning| html! {
                            <li><AppIcon name="Warning"/>{ " " }{ warning.clone() }</li>
                        }) }
                    </ul>
                }
            </>
        }
    } else {
        html! {}
    };

    let content = html! {
        <div class="tp__plugin-settings">
            <h2>{ format!("{} - {}", translate.t("LABEL.SETTINGS"), props.plugin_name) }</h2>
            <LoadingIndicator loading={*loading || *saving} />
            <div class="tp__plugin-settings__body">{ body }</div>
        </div>
    };

    html! {
        <ContentDialog
            content={content}
            actions={actions}
            close_on_backdrop_click={false}
            close_on_confirm={false}
            on_confirm={on_result}
        />
    }
}
