use crate::{
    app::components::{Chip, HideContent, InputHeaders},
    html_if,
    i18n::use_translation,
};
use shared::model::{ClusterSource, InputType, StagedInputDto};
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct StagedInputViewProps {
    pub input: Option<StagedInputDto>,
}

#[component]
pub fn StagedInputView(props: &StagedInputViewProps) -> Html {
    let translate = use_translation();

    match props.input.as_ref() {
        Some(input) => {
            let label = match input.input_type {
                InputType::M3u => "LABEL.M3U",
                InputType::Xtream => "LABEL.XTREAM",
                InputType::M3uBatch => "LABEL.M3U_BATCH",
                InputType::XtreamBatch => "LABEL.XTREAM_BATCH",
                InputType::Library => "LABEL.LIBRARY",
            };
            html! {
                <div class="tp__staged-input-view">
                    <Chip label={translate.t(label)} class={input.input_type.to_string()} />
                    <div class="tp__staged-input-view__row">
                        <label>{translate.t("LABEL.URL")}</label>
                        { &input.url }
                    </div>
                    {
                        html_if!(input.username.is_some() || input.password.is_some(), {
                        <div class="tp__staged-input-view__row">
                            <label>{translate.t("LABEL.USERNAME")}</label>
                            { input.username.as_ref().map_or_else(String::new, |username| username.clone()) }
                        </div>
                        })
                    }
                    <div class="tp__staged-input-view__row">
                        <label>{translate.t("LABEL.PASSWORD")}</label>
                        <HideContent content={input.password.as_ref().map_or_else(String::new, |password| password.clone())}></HideContent>
                    </div>
                    <InputHeaders headers={input.headers.clone()} />
                    {
                        {
                            let fmt = |cs: &Option<ClusterSource>| -> String {
                                cs.as_ref().map(ToString::to_string).unwrap_or_else(|| ClusterSource::default().to_string())
                            };
                            html! {
                                <>
                                <div class="tp__staged-input-view__row">
                                    <label>{translate.t("LABEL.LIVE_SOURCE")}</label>
                                    { fmt(&input.live_source) }
                                </div>
                                <div class="tp__staged-input-view__row">
                                    <label>{translate.t("LABEL.VOD_SOURCE")}</label>
                                    { fmt(&input.vod_source) }
                                </div>
                                <div class="tp__staged-input-view__row">
                                    <label>{translate.t("LABEL.SERIES_SOURCE")}</label>
                                    { fmt(&input.series_source) }
                                </div>
                                </>
                            }
                        }
                    }
                </div>
            }
        }
        None => html! {},
    }
}
