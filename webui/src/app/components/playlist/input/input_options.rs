use crate::app::components::{make_tags, CollapsePanel, Tag, TagList};
use shared::model::ConfigInputDto;
use std::rc::Rc;
use yew::prelude::*;
use yew_i18n::{use_translation};


#[derive(Properties, Clone, PartialEq, Debug)]
pub struct InputOptionsProps {
    pub input: Rc<ConfigInputDto>,
}

#[function_component]
pub fn InputOptions(props: &InputOptionsProps) -> Html {
    let translate = use_translation();
    let tags = use_memo(props.input.clone(), |input| {
        let (has_options, options1, options2) = match input.options.as_ref() {
            None => (false, vec![
                (false, "LABEL.LIVE"),
                (false, "LABEL.VOD"),
                (false, "LABEL.SERIES")],
                     vec![
                (false, "LABEL.LIVE_STREAM_USE_PREFIX"),
                (false, "LABEL.LIVE_STREAM_WITHOUT_EXTENSION"),
            ]),
            Some(options) => {
                let has_options = options.xtream_skip_live
                    || options.xtream_skip_vod
                    || options.xtream_skip_series
                    || options.xtream_live_stream_use_prefix
                    || options.xtream_live_stream_without_extension;

                (has_options, vec![
                    (options.xtream_skip_live, "LABEL.LIVE"),
                    (options.xtream_skip_vod, "LABEL.VOD"),
                    (options.xtream_skip_series, "LABEL.SERIES"),],
                   vec![
                    (options.xtream_live_stream_use_prefix, "LABEL.LIVE_STREAM_USE_PREFIX"),
                    (options.xtream_live_stream_without_extension, "LABEL.LIVE_STREAM_WITHOUT_EXTENSION"),
                ])
            }
        };
        (has_options, make_tags(&options1, &translate), make_tags(&options2, &translate))
    });

    let has_options = tags.0;
    let opts1: Vec<Rc<Tag>> = tags.1.clone();
    let opts2: Vec<Rc<Tag>> = tags.2.clone();

    html! {
        <CollapsePanel expanded={false} class={format!("tp__target-options__panel{}", if has_options { " tp__target-options__has_options"} else {""})}
                       title={translate.t("LABEL.SETTINGS")}>
            <div class="tp__target-options">
                <div class="tp__target-options__section">
                  <TagList tags={opts2} />
                </div>
                <div class="tp__target-options__section">
                 <span class="tp__target-common__label">{translate.t("LABEL.SKIP")}</span>
                  <TagList tags={opts1} />
                </div>
            </div>
        </CollapsePanel>
    }
}