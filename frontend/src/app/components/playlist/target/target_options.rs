use crate::{
    app::components::{make_tags, Tag, TagList},
    i18n::use_translation,
};
use shared::model::{ClusterFlags, ConfigTargetDto};
use std::rc::Rc;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct TargetOptionsProps {
    pub target: Rc<ConfigTargetDto>,
}

#[component]
pub fn TargetOptions(props: &TargetOptionsProps) -> Html {
    let translate = use_translation();
    let tags = use_memo((props.target.clone(), translate.clone()), |(target, translate)| {
        let redirect_default = vec![(false, "LABEL.LIVE"), (false, "LABEL.VOD"), (false, "LABEL.SERIES")];
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
                let force_redirect = match options.force_redirect {
                    None => redirect_default.clone(),
                    Some(force_redirect) => vec![
                        (force_redirect.contains(ClusterFlags::Live), "LABEL.LIVE"),
                        (force_redirect.contains(ClusterFlags::Vod), "LABEL.VOD"),
                        (force_redirect.contains(ClusterFlags::Series), "LABEL.SERIES"),
                    ],
                };
                (
                    vec![
                        options.ignore_logo,
                        options.share_live_streams,
                        options.remove_duplicates,
                        force_redirect[0].0,
                        force_redirect[1].0,
                        force_redirect[2].0,
                    ],
                    vec![
                        (options.ignore_logo, "LABEL.IGNORE_LOGO"),
                        (options.share_live_streams, "LABEL.SHARE_LIVE_STREAMS"),
                        (options.remove_duplicates, "LABEL.REMOVE_DUPLICATES"),
                    ],
                    force_redirect,
                )
            }
        };

        (flags.iter().any(|&v| v), make_tags(&options, translate), make_tags(&redirect, translate))
    });

    let opts: Vec<Rc<Tag>> = (tags.1).clone();
    let redirect: Vec<Rc<Tag>> = (tags.2).clone();

    html! {
            <div class="tp__target-options">
                <div class="tp__target-options__section">
                  <TagList tags={opts} />
                </div>
                <div class="tp__target-options__section">
                  <span class="tp__target-options__label">{translate.t("LABEL.FORCE_REDIRECT")}</span>
                  <TagList tags={redirect} />
                </div>
            </div>
    }
}
