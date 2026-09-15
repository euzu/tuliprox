use crate::{app::components::Chip, i18n::use_translation};
use shared::model::InputType;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct InputTypeViewProps {
    pub input_type: InputType,
}

#[must_use]
pub const fn input_type_label_key(input_type: InputType) -> &'static str {
    match input_type {
        InputType::M3u => "LABEL.M3U",
        InputType::Xtream => "LABEL.XTREAM",
        InputType::M3uBatch => "LABEL.M3U_BATCH",
        InputType::XtreamBatch => "LABEL.XTREAM_BATCH",
        InputType::Stalker => "LABEL.STALKER",
        InputType::StalkerBatch => "LABEL.STALKER_BATCH",
        InputType::Library => "LABEL.LIBRARY",
        InputType::Emby => "LABEL.EMBY",
        InputType::Jellyfin => "LABEL.JELLYFIN",
        InputType::Plex => "LABEL.PLEX",
        InputType::Staged => "LABEL.STAGED",
    }
}

#[component]
pub fn InputTypeView(props: &InputTypeViewProps) -> Html {
    let translate = use_translation();

    html! {
        <Chip label={translate.t(input_type_label_key(props.input_type))} class={props.input_type.to_string()} />
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_input_type_label_exists_in_each_supported_locale() {
        let input_types = [
            InputType::M3u,
            InputType::Xtream,
            InputType::M3uBatch,
            InputType::XtreamBatch,
            InputType::Stalker,
            InputType::StalkerBatch,
            InputType::Library,
            InputType::Emby,
            InputType::Jellyfin,
            InputType::Plex,
            InputType::Staged,
        ];
        for locale in [
            include_str!("../../../../../public/assets/i18n/en.json"),
            include_str!("../../../../../public/assets/i18n/ar.json"),
            include_str!("../../../../../public/assets/i18n/ru.json"),
        ] {
            let translations: serde_json::Value = serde_json::from_str(locale).unwrap();
            for input_type in input_types {
                let pointer = format!("/{}", input_type_label_key(input_type).replace('.', "/"));
                assert!(translations.pointer(&pointer).and_then(serde_json::Value::as_str).is_some(), "{pointer}");
            }
        }
    }
}
