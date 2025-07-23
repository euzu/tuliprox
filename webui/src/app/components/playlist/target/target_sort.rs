use crate::app::components::{convert_bool_to_chip_style, Card, Chip};
use shared::model::{ConfigTargetDto};
use std::rc::Rc;
use yew::prelude::*;
use yew_i18n::use_translation;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct TargetSortProps {
    pub target: Rc<ConfigTargetDto>,
}

#[function_component]
pub fn TargetSort(props: &TargetSortProps) -> Html {
    let translator = use_translation();

    let sort = match props.target.sort.as_ref() {
        Some(s) => s,
        None => return html! {},
    };

    let groups_html = match sort.groups.as_ref() {
        Some(groups) => {
            match groups.sequence.as_ref() {
                Some(seq) => html! {
                    <Card class="tp__target-sort__card">
                    <h2>{translator.t("LABEL.GROUPS")}</h2>
                    <div class="tp__target-sort__section tp__target-sort__row">
                        <span class="tp__target-sort__label">{translator.t("LABEL.ORDER")}</span>
                        <span>{ groups.order.to_string() }</span>
                    </div>
                    <div class="tp__target-sort__section  tp__target-sort__row">
                        <span class="tp__target-sort__label">{translator.t("LABEL.SEQUENCE")}</span>
                        <span class="tp__target-sort__sequence">
                            <ul>
                                { for seq.iter().map(|p| html! { <li>{p}</li> }) }
                            </ul>
                        </span>
                    </div>
                    </Card>
                },
                None => html! {},
            }
        },
        None => html! {},
    };

    let channels_html = match sort.channels.as_ref() {
        Some(channels) => html! {
        <Card class="tp__target-sort__card">
            <h2>{ translator.t("LABEL.CHANNELS") }</h2>
            {
                for channels.iter().map(|channel| html! {
                    <>
                        <div class="tp__target-sort__section tp__target-sort__row tp__target-sort__new-field">
                            <span class="tp__target-sort__label">{ translator.t("LABEL.FIELD") }</span>
                            <span>{ channel.field.to_string() }</span>
                        </div>
                        <div class="tp__target-sort__section tp__target-sort__row">
                            <span class="tp__target-sort__label">{ translator.t("LABEL.GROUP_PATTERN") }</span>
                            <span>{ channel.group_pattern.to_string() }</span>
                        </div>
                        <div class="tp__target-sort__section tp__target-sort__row">
                            <span class="tp__target-sort__label">{ translator.t("LABEL.ORDER") }</span>
                            <span>{ channel.order.to_string() }</span>
                        </div>
                        {
                            match channel.sequence.as_ref() {
                                Some(seq) => html! {
                                    <div class="tp__target-sort__section tp__target-sort__row">
                                        <span class="tp__target-sort__label">{ translator.t("LABEL.SEQUENCE") }</span>
                                        <span class="tp__target-sort__sequence">
                                            <ul>
                                                { for seq.iter().map(|p| html! { <li>{ p }</li> }) }
                                            </ul>
                                        </span>
                                    </div>
                                },
                                None => html! {},
                            }
                        }
                    </>
                })
            }
        </Card>
    },
        None => html! {},
    };


    html! {
        <div class="tp__target-sort">
            <h2>{translator.t("LABEL.SORT_SETTINGS")}</h2>
            <div class="tp__target-sort__section  tp__target-sort__row">
                <Chip class={ convert_bool_to_chip_style(sort.match_as_ascii) }
                      label={translator.t("LABEL.MATCH_AS_ASCII")} />
            </div>
            { groups_html }
            { channels_html }
        </div>
    }
}
