use crate::{
    app::components::{chip::convert_bool_to_chip_style, make_tags, Tag, TagList},
    i18n::use_translation,
};
use shared::{
    defaults::{default_probe_delay_secs, default_probe_live_interval, default_resolve_delay_secs},
    model::ConfigInputDto,
};
use std::rc::Rc;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct InputOptionsProps {
    pub input: Rc<ConfigInputDto>,
}

#[component]
pub fn InputOptions(props: &InputOptionsProps) -> Html {
    let translate = use_translation();
    let tags = use_memo(props.input.clone(), |input| {
        let (has_options, options1, options2, resolve_tags, probe_tags) = match input.options.as_ref() {
            None => (
                false,
                vec![(false, "LABEL.LIVE"), (false, "LABEL.VOD"), (false, "LABEL.SERIES")],
                vec![
                    (false, "LABEL.LIVE_STREAM_USE_PREFIX"),
                    (false, "LABEL.LIVE_STREAM_WITHOUT_EXTENSION"),
                    (false, "LABEL.DISABLE_HLS_STREAMING"),
                    (false, "LABEL.RESOLVE_TMDB"),
                ],
                vec![
                    Rc::new(Tag {
                        class: convert_bool_to_chip_style(false),
                        label: format!("{} / {}s", translate.t("LABEL.SERIES"), default_resolve_delay_secs()),
                    }),
                    Rc::new(Tag {
                        class: convert_bool_to_chip_style(false),
                        label: format!("{} / {}s", translate.t("LABEL.VOD"), default_resolve_delay_secs()),
                    }),
                    Rc::new(Tag {
                        class: convert_bool_to_chip_style(true),
                        label: translate.t("LABEL.RESOLVE_BACKGROUND").clone(),
                    }),
                ],
                vec![
                    Rc::new(Tag {
                        class: convert_bool_to_chip_style(false),
                        label: format!("{} / {}h", translate.t("LABEL.LIVE"), default_probe_live_interval()),
                    }),
                    Rc::new(Tag { class: convert_bool_to_chip_style(false), label: translate.t("LABEL.VOD").clone() }),
                    Rc::new(Tag {
                        class: convert_bool_to_chip_style(false),
                        label: translate.t("LABEL.SERIES").clone(),
                    }),
                    Rc::new(Tag {
                        class: convert_bool_to_chip_style(false),
                        label: format!("{} / {}s", translate.t("LABEL.PROBE_DELAY_SEC"), default_probe_delay_secs()),
                    }),
                ],
            ),
            Some(options) => {
                let has_options = options.skip_live
                    || options.skip_vod
                    || options.skip_series
                    || !options.xtream_live_stream_use_prefix
                    || options.xtream_live_stream_without_extension
                    || options.disable_hls_streaming
                    || options.resolve_tmdb
                    || !options.resolve_background
                    || options.resolve_series
                    || options.resolve_vod
                    || options.probe_series
                    || options.probe_vod
                    || options.probe_live
                    || options.resolve_delay != default_resolve_delay_secs()
                    || options.probe_delay != default_probe_delay_secs()
                    || options.probe_live_interval_hours != default_probe_live_interval();

                (
                    has_options,
                    vec![
                        (options.skip_live, "LABEL.LIVE"),
                        (options.skip_vod, "LABEL.VOD"),
                        (options.skip_series, "LABEL.SERIES"),
                    ],
                    vec![
                        (options.xtream_live_stream_use_prefix, "LABEL.LIVE_STREAM_USE_PREFIX"),
                        (options.xtream_live_stream_without_extension, "LABEL.LIVE_STREAM_WITHOUT_EXTENSION"),
                        (options.disable_hls_streaming, "LABEL.DISABLE_HLS_STREAMING"),
                        (options.resolve_tmdb, "LABEL.RESOLVE_TMDB"),
                    ],
                    vec![
                        Rc::new(Tag {
                            class: convert_bool_to_chip_style(options.resolve_series),
                            label: format!("{} / {}s", translate.t("LABEL.SERIES"), options.resolve_delay),
                        }),
                        Rc::new(Tag {
                            class: convert_bool_to_chip_style(options.resolve_vod),
                            label: format!("{} / {}s", translate.t("LABEL.VOD"), options.resolve_delay),
                        }),
                        Rc::new(Tag {
                            class: convert_bool_to_chip_style(options.resolve_background),
                            label: translate.t("LABEL.RESOLVE_BACKGROUND").clone(),
                        }),
                    ],
                    vec![
                        Rc::new(Tag {
                            class: convert_bool_to_chip_style(options.probe_live),
                            label: format!("{} / {}h", translate.t("LABEL.LIVE"), options.probe_live_interval_hours),
                        }),
                        Rc::new(Tag {
                            class: convert_bool_to_chip_style(options.probe_vod),
                            label: translate.t("LABEL.VOD").clone(),
                        }),
                        Rc::new(Tag {
                            class: convert_bool_to_chip_style(options.probe_series),
                            label: translate.t("LABEL.SERIES").clone(),
                        }),
                        Rc::new(Tag {
                            class: convert_bool_to_chip_style(options.probe_delay != default_probe_delay_secs()),
                            label: format!("{} / {}s", translate.t("LABEL.PROBE_DELAY_SEC"), options.probe_delay),
                        }),
                    ],
                )
            }
        };
        (has_options, make_tags(&options1, &translate), make_tags(&options2, &translate), resolve_tags, probe_tags)
    });

    let opts1: Vec<Rc<Tag>> = tags.1.clone();
    let opts2: Vec<Rc<Tag>> = tags.2.clone();
    let resolve: Vec<Rc<Tag>> = tags.3.clone();
    let probe: Vec<Rc<Tag>> = tags.4.clone();

    html! {
            <div class="tp__target-options">
                <div class="tp__target-options__section">
                  <TagList tags={opts2} />
                </div>
                <div class="tp__target-options__section">
                  <span class="tp__target-common__label">{translate.t("LABEL.RESOLVE")}</span>
                  <TagList tags={resolve} />
                </div>
                <div class="tp__target-options__section">
                  <span class="tp__target-common__label">{translate.t("LABEL.PROBE")}</span>
                  <TagList tags={probe} />
                </div>
                <div class="tp__target-options__section">
                 <span class="tp__target-common__label">{translate.t("LABEL.SKIP")}</span>
                  <TagList tags={opts1} />
                </div>
            </div>
    }
}
