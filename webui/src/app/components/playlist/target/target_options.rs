use crate::app::components::{convert_bool_to_chip_style, CollapsePanel, Tag, TagList};
use shared::model::{ClusterFlags, ConfigTargetDto};
use std::rc::Rc;
use yew::prelude::*;
use yew_i18n::{use_translation, YewI18n};

fn make_tags(data: &[(bool, &str)], translate: &YewI18n) -> Vec<Rc<Tag>> {
    data.iter()
        .map(|(o, t)| {
            Rc::new(Tag {
                class: convert_bool_to_chip_style(*o),
                label: translate.t(t),
            })
        })
        .collect()
}

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct TargetOptionsProps {
    pub target: Rc<ConfigTargetDto>,
}

#[function_component]
pub fn TargetOptions(props: &TargetOptionsProps) -> Html {
    let translate = use_translation();
    let tags = use_memo(props.target.clone(), |target| {
        let redirect_default= vec![
            (false, "LABEL.LIVE"),
            (false, "LABEL.VOD"),
            (false, "LABEL.SERIES"),

        ];
        let (flags, options, redirect) = match target.options.as_ref() {
            None => (
                vec![false, false, false, false, false, false],
                vec![
                (false, "LABEL.IGNORE_LOGO"),
                (false, "LABEL.SHARE_LIVE_STREAMS"),
                (false, "LABEL.REMOVE_DUPLICATES"),
                ],
                redirect_default.clone(),
            ),
            Some(options) => {
                let force_redirect =                     match options.force_redirect {
                    None => redirect_default.clone(),
                    Some(force_redirect) => vec![
                        (force_redirect.contains(ClusterFlags::Live), "LABEL.LIVE"),
                        (force_redirect.contains(ClusterFlags::Vod), "LABEL.VOD"),
                        (force_redirect.contains(ClusterFlags::Series), "LABEL.SERIES"),
                    ],
                };
                (
                    vec![options.ignore_logo, options.share_live_streams, options.remove_duplicates, force_redirect[0].0, force_redirect[1].0, force_redirect[2].0],
                    vec![
                        (options.ignore_logo, "LABEL.IGNORE_LOGO"),
                        (options.share_live_streams, "LABEL.SHARE_LIVE_STREAMS"),
                        (options.remove_duplicates, "LABEL.REMOVE_DUPLICATES"),
                    ],
                    force_redirect,
                )
            }
        };

        (
         flags.iter().any(|&v| v),
         make_tags(&options, &translate),
         make_tags(&redirect, &translate),
        )
    });

    let has_options = tags.0;
    let opts: Vec<Rc<Tag>> = (&tags.1).clone();
    let redirect: Vec<Rc<Tag>> = (&tags.2).clone();

    html! {
        <CollapsePanel expanded={false} class={format!("tp__target-options__panel{}", if has_options { " tp__target-options__has_options"} else {""})}
                       title={translate.t("LABEL.SETTINGS")}>
            <div class="tp__target-options">
                <div class="tp__target-options__section">
                  <TagList tags={opts} />
                </div>
                <div class="tp__target-options__section">
                  <span class="tp__target-options__label">{translate.t("LABEL.FORCE_REDIRECT")}</span>
                  <TagList tags={redirect} />
                </div>
            </div>
        </CollapsePanel>
    }
}