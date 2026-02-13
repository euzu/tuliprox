use crate::app::components::{Card, Chip, NoContent};
use crate::{config_field, config_field_bool, config_field_child, config_field_optional};
use shared::model::{EpgConfigDto, EpgSmartMatchConfigDto, EpgSourceDto};
use yew::prelude::*;
use yew_i18n::use_translation;

#[derive(Properties, PartialEq, Clone)]
pub struct EpgConfigViewProps {
    #[prop_or_default]
    pub epg: Option<EpgConfigDto>,
}

#[function_component]
pub fn EpgConfigView(props: &EpgConfigViewProps) -> Html {
    let translate = use_translation();

    let render_sources = |sources: Option<&Vec<EpgSourceDto>>| {
        let render_empty_sources = || {
            html! {
                <Card class="tp__config-view__card">
                    <h1>{translate.t("LABEL.SOURCES")}</h1>
                    <NoContent />
                </Card>
            }
        };

        if let Some(sources) = sources {
            if sources.is_empty() {
                render_empty_sources()
            } else {
                html! {
                  for sources.iter().map(|entry| html! {
                    <Card class="tp__config-view__card">
                        <h1>{translate.t("LABEL.SOURCE")}</h1>
                        { config_field!(entry, translate.t("LABEL.URL"), url) }
                        { config_field!(entry, translate.t("LABEL.PRIORITY"), priority) }
                        { config_field_bool!(entry, translate.t("LABEL.LOGO_OVERRIDE"), logo_override) }
                    </Card>
                })}
            }
        } else {
            render_empty_sources()
        }
    };

    let render_smart_match = |epg_smart_match: Option<&EpgSmartMatchConfigDto>| {
        let render_empty_smart_match = || {
            html! {
                <Card class="tp__config-view__card">
                    <h1>{translate.t("LABEL.EPG_SMART_MATCH")}</h1>
                    <NoContent />
                </Card>
            }
        };

        if let Some(entry) = epg_smart_match {
            html! {
                <Card class="tp__config-view__card">
                  <h1>{translate.t("LABEL.EPG_SMART_MATCH")}</h1>
                  { config_field_bool!(entry, translate.t("LABEL.ENABLED"), enabled) }
                  { config_field_bool!(entry, translate.t("LABEL.FUZZY_MATCHING"), fuzzy_matching) }
                  { config_field!(entry, translate.t("LABEL.MATCH_THRESHOLD"), match_threshold) }
                  { config_field!(entry, translate.t("LABEL.BEST_MATCH_THRESHOLD"), best_match_threshold) }
                  { config_field_optional!(entry, translate.t("LABEL.NORMALIZE_REGEX"), normalize_regex) }
                  { config_field!(entry, translate.t("LABEL.NAME_PREFIX"), name_prefix) }
                  { config_field_child!(translate.t("LABEL.NAME_PREFIX_SEPARATOR"), {
                        html! {
                            <div class="tp__config-view__tags">
                                {
                                    if let Some(name_prefixe_seperators) = entry.name_prefix_separator.as_ref() {
                                        if name_prefixe_seperators.is_empty() {
                                            html! {}
                                        } else {
                                            html! {
                                               for name_prefixe_seperators.iter().map(|value| html! {
                                                  <Chip label={value.to_string()} />
                                               })
                                            }
                                        }
                                    } else {
                                        html! {}
                                    }
                                }
                            </div>
                        }
                    })}
                  { config_field_child!(translate.t("LABEL.STRIP"), {
                        html! {
                            <div class="tp__config-view__tags">
                                {
                                    if let Some(strip) = entry.strip.as_ref() {
                                        if strip.is_empty() {
                                            html! {}
                                        } else {
                                            html! {
                                               for strip.iter().map(|value| html! {
                                                  <Chip label={value.clone()} />
                                               })
                                            }
                                        }
                                    } else {
                                        html! {}
                                    }
                                }
                            </div>
                        }
                    })}
                </Card>
            }
        } else {
            render_empty_smart_match()
        }
    };

    let render_empty = || {
        html! {
            <div class="tp__epg-config-view__body tp__config-view-page__body">
                 {render_smart_match(None)}
                 {render_sources(None)}
            </div>
        }
    };

    html! {
        <div class="tp__epg-config-view tp__config-view-page">
        {
            if let Some(epg) = &props.epg {
               html! {
                <div class="tp__epg-config-view__body tp__config-view-page__body">
                  {render_smart_match(epg.smart_match.as_ref())}
                  {render_sources(epg.sources.as_ref())}
                </div>
               }
            } else {
                { render_empty() }
            }
        }
        </div>
    }
}
