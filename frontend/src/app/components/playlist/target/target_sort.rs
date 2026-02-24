use crate::{
    app::components::{convert_bool_to_chip_style, Card, Chip, FilterView},
    i18n::use_translation,
};
use shared::model::ConfigTargetDto;
use std::rc::Rc;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct TargetSortProps {
    pub target: Rc<ConfigTargetDto>,
}

#[component]
pub fn TargetSort(props: &TargetSortProps) -> Html {
    let translator = use_translation();

    let sort = match props.target.sort.as_ref() {
        Some(s) => s,
        None => return html! {},
    };
    let rules_html = if sort.rules.is_empty() {
        html! {}
    } else {
        html! {
        <Card class="tp__target-sort__card">
            <h2>{ translator.t("LABEL.CHANNELS") }</h2>
            for rule in &sort.rules {
                <>
                    <div class="tp__target-sort__section tp__target-sort__row tp__target-sort__new-field">
                        <span class="tp__target-sort__label">{ translator.t("LABEL.TARGET") }</span>
                        <span>{ rule.target.as_str() }</span>
                    </div>
                    <div class="tp__target-sort__section tp__target-sort__row">
                        <span class="tp__target-sort__label">{ translator.t("LABEL.FIELD") }</span>
                        <span>{ rule.field.as_str() }</span>
                    </div>
                    <div class="tp__target-sort__section tp__target-sort__row">
                        <span class="tp__target-sort__label">{ translator.t("LABEL.ORDER") }</span>
                        <span>{ rule.order.to_string() }</span>
                    </div>
                    <div class="tp__target-sort__section tp__target-sort__row">
                        <span class="tp__target-sort__label">{ translator.t("LABEL.FILTER") }</span>
                        <FilterView inline={true} filter={rule.t_filter.clone()} />
                    </div>
                    {
                        match rule.sequence.as_ref() {
                            Some(seq) => html! {
                                <div class="tp__target-sort__section tp__target-sort__row">
                                    <span class="tp__target-sort__label">{ translator.t("LABEL.SEQUENCE") }</span>
                                    <span class="tp__target-sort__sequence">
                                        <ul>
                                            for p in seq {
                                                <li>{ p }</li>
                                            }
                                        </ul>
                                    </span>
                                </div>
                            },
                            None => html! {},
                        }
                    }
                </>
            }
            </Card>
        }
    };

    html! {
        <div class="tp__target-sort">
            <h2>{translator.t("LABEL.SORT_SETTINGS")}</h2>
            <div class="tp__target-sort__section  tp__target-sort__row">
                <Chip class={ convert_bool_to_chip_style(sort.match_as_ascii) }
                      label={translator.t("LABEL.MATCH_AS_ASCII")} />
            </div>
            { rules_html }
        </div>
    }
}
