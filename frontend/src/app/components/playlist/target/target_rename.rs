use crate::app::components::Card;
use shared::model::ConfigTargetDto;
use std::rc::Rc;
use yew::prelude::*;
use yew_i18n::use_translation;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct TargetRenameProps {
    pub target: Rc<ConfigTargetDto>,
}

#[function_component]
pub fn TargetRename(props: &TargetRenameProps) -> Html {
    let translator = use_translation();

    let renames = match props.target.rename.as_ref() {
        Some(s) => s,
        None => return html! {},
    };

    html! {
        <div class="tp__target-rename">
         <h2>{translator.t("LABEL.RENAME_SETTINGS")}</h2>

        <Card class="tp__target-rename__card">
            {
                for renames.iter().map(|rename| html! {
                    <>
                        <div class="tp__target-rename__section tp__target-rename__row tp__target-rename__new-field">
                            <span class="tp__target-rename__label">{ translator.t("LABEL.FIELD") }</span>
                            <span>{ rename.field.to_string() }</span>
                        </div>
                        <div class="tp__target-rename__section tp__target-rename__row">
                            <span class="tp__target-rename__label">{ translator.t("LABEL.PATTERN") }</span>
                            <span>{ rename.pattern.to_string() }</span>
                        </div>
                        <div class="tp__target-rename__section tp__target-rename__row">
                            <span class="tp__target-rename__label">{ translator.t("LABEL.NEW_NAME") }</span>
                            <span>{ rename.new_name.to_string() }</span>
                        </div>
                    </>
                })
            }
        </Card>

        </div>
    }
}
