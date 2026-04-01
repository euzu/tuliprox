use crate::{
    app::components::{select::Select, Card, DropDownOption, DropDownSelection, TextButton},
    config_field_bool, config_field_child, config_field_custom, config_field_optional, edit_field_bool,
    edit_field_list_option, edit_field_number_u16, edit_field_text_option, generate_form_reducer,
    i18n::use_translation,
};
use shared::model::{EpgNamePrefix, EpgSmartMatchConfigDto};
use yew::{component, html, use_memo, use_reducer, Callback, Html, Properties, UseReducerHandle};

const LABEL_ENABLED: &str = "LABEL.ENABLED";
const LABEL_FUZZY_MATCHING: &str = "LABEL.FUZZY_MATCHING";
const LABEL_MATCH_THRESHOLD: &str = "LABEL.MATCH_THRESHOLD";
const LABEL_BEST_MATCH_THRESHOLD: &str = "LABEL.BEST_MATCH_THRESHOLD";
const LABEL_NORMALIZE_REGEX: &str = "LABEL.NORMALIZE_REGEX";
const LABEL_STRIP: &str = "LABEL.STRIP";
const LABEL_NAME_PREFIX: &str = "LABEL.NAME_PREFIX";
const LABEL_NAME_PREFIX_VALUE: &str = "LABEL.NAME_PREFIX_VALUE";
const LABEL_NAME_PREFIX_SEPARATOR: &str = "LABEL.NAME_PREFIX_SEPARATOR";
const LABEL_ADD_STRIP_ENTRY: &str = "LABEL.ADD_STRIP_ENTRY";

const NAME_PREFIX_MODE_IGNORE: &str = "ignore";
const NAME_PREFIX_MODE_SUFFIX: &str = "suffix";
const NAME_PREFIX_MODE_PREFIX: &str = "prefix";

#[derive(Debug, Clone, PartialEq)]
pub struct EpgSmartMatchFormData {
    pub enabled: bool,
    pub normalize_regex: Option<String>,
    pub strip: Option<Vec<String>>,
    pub name_prefix_mode: String,
    pub name_prefix_value: Option<String>,
    pub name_prefix_separator: Option<String>,
    pub fuzzy_matching: bool,
    pub match_threshold: u16,
    pub best_match_threshold: u16,
}

impl Default for EpgSmartMatchFormData {
    fn default() -> Self { Self::from(EpgSmartMatchConfigDto::default()) }
}

impl From<EpgSmartMatchConfigDto> for EpgSmartMatchFormData {
    fn from(value: EpgSmartMatchConfigDto) -> Self {
        let (name_prefix_mode, name_prefix_value) = match value.name_prefix {
            EpgNamePrefix::Ignore => (NAME_PREFIX_MODE_IGNORE.to_string(), None),
            EpgNamePrefix::Suffix(v) => (NAME_PREFIX_MODE_SUFFIX.to_string(), Some(v)),
            EpgNamePrefix::Prefix(v) => (NAME_PREFIX_MODE_PREFIX.to_string(), Some(v)),
        };
        let separator = value
            .name_prefix_separator
            .as_ref()
            .map(|chars| chars.iter().map(char::to_string).collect::<Vec<_>>().join(","));
        Self {
            enabled: value.enabled,
            normalize_regex: value.normalize_regex,
            strip: value.strip,
            name_prefix_mode,
            name_prefix_value,
            name_prefix_separator: separator,
            fuzzy_matching: value.fuzzy_matching,
            match_threshold: value.match_threshold,
            best_match_threshold: value.best_match_threshold,
        }
    }
}

impl From<&EpgSmartMatchFormData> for EpgSmartMatchConfigDto {
    fn from(value: &EpgSmartMatchFormData) -> Self {
        let prefix_value = value.name_prefix_value.clone().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let name_prefix = match value.name_prefix_mode.as_str() {
            NAME_PREFIX_MODE_SUFFIX => EpgNamePrefix::Suffix(prefix_value.unwrap_or_default()),
            NAME_PREFIX_MODE_PREFIX => EpgNamePrefix::Prefix(prefix_value.unwrap_or_default()),
            _ => EpgNamePrefix::Ignore,
        };
        let name_prefix_separator = value.name_prefix_separator.as_ref().and_then(|separator| {
            let chars = separator.split(',').filter_map(|part| part.trim().chars().next()).collect::<Vec<char>>();
            if chars.is_empty() {
                None
            } else {
                Some(chars)
            }
        });
        EpgSmartMatchConfigDto {
            enabled: value.enabled,
            normalize_regex: value.normalize_regex.clone().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()),
            strip: value.strip.clone(),
            name_prefix,
            name_prefix_separator,
            fuzzy_matching: value.fuzzy_matching,
            match_threshold: value.match_threshold,
            best_match_threshold: value.best_match_threshold,
        }
    }
}

generate_form_reducer!(
    state: EpgSmartMatchFormState { form: EpgSmartMatchFormData },
    action_name: EpgSmartMatchFormAction,
    fields {
        Enabled => enabled: bool,
        NormalizeRegex => normalize_regex: Option<String>,
        Strip => strip: Option<Vec<String>>,
        NamePrefixMode => name_prefix_mode: String,
        NamePrefixValue => name_prefix_value: Option<String>,
        NamePrefixSeparator => name_prefix_separator: Option<String>,
        FuzzyMatching => fuzzy_matching: bool,
        MatchThreshold => match_threshold: u16,
        BestMatchThreshold => best_match_threshold: u16,
    }
);

#[derive(Properties, PartialEq, Clone)]
pub struct EpgSmartMatchFormProps {
    pub on_submit: Callback<EpgSmartMatchConfigDto>,
    pub on_cancel: Callback<()>,
    #[prop_or_default]
    pub initial: Option<EpgSmartMatchConfigDto>,
    #[prop_or(false)]
    pub readonly: bool,
}

#[component]
pub fn EpgSmartMatchForm(props: &EpgSmartMatchFormProps) -> Html {
    let translate = use_translation();

    let initial = props.initial.clone().unwrap_or_default();

    let form_state: UseReducerHandle<EpgSmartMatchFormState> =
        use_reducer(|| EpgSmartMatchFormState { form: EpgSmartMatchFormData::from(initial), modified: false });

    let name_prefix_options = use_memo(form_state.form.name_prefix_mode.clone(), |selected_mode| {
        vec![
            DropDownOption::new(NAME_PREFIX_MODE_IGNORE, html! { "Ignore" }, selected_mode == NAME_PREFIX_MODE_IGNORE),
            DropDownOption::new(NAME_PREFIX_MODE_SUFFIX, html! { "Suffix" }, selected_mode == NAME_PREFIX_MODE_SUFFIX),
            DropDownOption::new(NAME_PREFIX_MODE_PREFIX, html! { "Prefix" }, selected_mode == NAME_PREFIX_MODE_PREFIX),
        ]
    });

    let handle_submit = {
        let form_state = form_state.clone();
        let on_submit = props.on_submit.clone();
        Callback::from(move |_| {
            on_submit.emit(EpgSmartMatchConfigDto::from(&form_state.form));
        })
    };

    let handle_cancel = {
        let on_cancel = props.on_cancel.clone();
        Callback::from(move |_| on_cancel.emit(()))
    };

    html! {
        <Card class="tp__config-view__card tp__item-form">
        if props.readonly {
            { config_field_bool!(form_state.form, translate.t(LABEL_ENABLED), enabled) }
            { config_field_bool!(form_state.form, translate.t(LABEL_FUZZY_MATCHING), fuzzy_matching) }
            { config_field_custom!(translate.t(LABEL_MATCH_THRESHOLD), form_state.form.match_threshold.to_string()) }
            { config_field_custom!(translate.t(LABEL_BEST_MATCH_THRESHOLD), form_state.form.best_match_threshold.to_string()) }
            { config_field_optional!(form_state.form, translate.t(LABEL_NORMALIZE_REGEX), normalize_regex) }
            { config_field_custom!(
                translate.t(LABEL_STRIP),
                form_state.form.strip.as_ref().map(|values| values.join(", ")).unwrap_or_default()
            ) }
            { config_field_custom!(
                translate.t(LABEL_NAME_PREFIX),
                EpgSmartMatchConfigDto::from(&form_state.form).name_prefix.to_string()
            ) }
            { config_field_optional!(form_state.form, translate.t(LABEL_NAME_PREFIX_SEPARATOR), name_prefix_separator) }
        } else {
            { edit_field_bool!(form_state, translate.t(LABEL_ENABLED), enabled, EpgSmartMatchFormAction::Enabled) }
            { edit_field_bool!(form_state, translate.t(LABEL_FUZZY_MATCHING), fuzzy_matching, EpgSmartMatchFormAction::FuzzyMatching) }
            { edit_field_number_u16!(form_state, translate.t(LABEL_MATCH_THRESHOLD), match_threshold, EpgSmartMatchFormAction::MatchThreshold) }
            { edit_field_number_u16!(form_state, translate.t(LABEL_BEST_MATCH_THRESHOLD), best_match_threshold, EpgSmartMatchFormAction::BestMatchThreshold) }
            { edit_field_text_option!(form_state, translate.t(LABEL_NORMALIZE_REGEX), normalize_regex, EpgSmartMatchFormAction::NormalizeRegex) }
            { edit_field_list_option!(form_state, translate.t(LABEL_STRIP), strip, EpgSmartMatchFormAction::Strip, translate.t(LABEL_ADD_STRIP_ENTRY)) }

            { config_field_child!(translate.t(LABEL_NAME_PREFIX), "EPG_SMART_MATCH_FORM.NAME_PREFIX_MODE", {
                let form_state_mode = form_state.clone();
                html! {
                    <Select
                        name={"name_prefix_mode"}
                        multi_select={false}
                        on_select={Callback::from(move |(_, selection): (String, DropDownSelection)| {
                            let mode = match selection {
                                DropDownSelection::Single(mode) => mode,
                                DropDownSelection::Multi(modes) => {
                                    modes.first().cloned().unwrap_or_else(|| NAME_PREFIX_MODE_IGNORE.to_string())
                                }
                                DropDownSelection::Empty => NAME_PREFIX_MODE_IGNORE.to_string(),
                            };
                            form_state_mode.dispatch(EpgSmartMatchFormAction::NamePrefixMode(mode));
                        })}
                        options={name_prefix_options.clone()}
                    />
                }
            }) }
            if form_state.form.name_prefix_mode != NAME_PREFIX_MODE_IGNORE {
                { edit_field_text_option!(form_state, translate.t(LABEL_NAME_PREFIX_VALUE), name_prefix_value, EpgSmartMatchFormAction::NamePrefixValue) }
            }
            { edit_field_text_option!(form_state, translate.t(LABEL_NAME_PREFIX_SEPARATOR), name_prefix_separator, EpgSmartMatchFormAction::NamePrefixSeparator) }
        }

        <div class="tp__form-page__toolbar">
            <TextButton
                class="secondary"
                name="cancel_smart_match"
                icon="Cancel"
                title={translate.t("LABEL.CANCEL")}
                onclick={handle_cancel}
            />
            if !props.readonly {
                <TextButton
                    class="primary"
                    name="submit_smart_match"
                    icon="Accept"
                    title={translate.t("LABEL.SUBMIT")}
                    onclick={handle_submit}
                />
            }
        </div>
        </Card>
    }
}
