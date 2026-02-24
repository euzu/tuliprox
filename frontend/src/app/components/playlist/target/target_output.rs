use crate::{
    app::components::{HdHomeRunOutput, M3uOutput, RevealContent, StrmOutput, XtreamOutput},
    i18n::use_translation,
};
use shared::model::{ConfigTargetDto, TargetOutputDto};
use std::rc::Rc;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct TargetOutputProps {
    pub target: Rc<ConfigTargetDto>,
}

#[component]
pub fn TargetOutput(props: &TargetOutputProps) -> Html {
    let translate = use_translation();

    html! {
        <div class="tp__target-output">
            for output in props.target.output.iter() {
                { match output {
                    TargetOutputDto::Xtream(xc) => html! {
                        <RevealContent preview={ html!{
                            <span class={format!("tp__target-output__xtream{}", if xc.has_any_option() { " tp__target-output__has_options" } else {""})}>
                            {translate.t("LABEL.XTREAM")}
                            </span>
                        }}>
                            <XtreamOutput output={xc.clone()} />
                        </RevealContent>
                    },
                    TargetOutputDto::M3u(m3u) => html! {
                        <RevealContent preview={ html!{
                            <span class={format!("tp__target-output__m3u{}", if m3u.has_any_option() { " tp__target-output__has_options" } else {""})}>
                            {translate.t("LABEL.M3U")}
                            </span>
                        }}>
                            <M3uOutput output={m3u.clone()}/>
                        </RevealContent>
                    },
                    TargetOutputDto::Strm(strm) => html! {
                        <RevealContent preview={ html!{
                            <span class={"tp__target-output__strm"}>
                            {translate.t("LABEL.STRM")}
                            </span>
                        }}>
                            <StrmOutput output={strm.clone()}/>
                        </RevealContent>
                    },
                    TargetOutputDto::HdHomeRun(hdhr) => html! {
                        <RevealContent preview={ html!{
                            <span class={"tp__target-output__hdhomerun"}>
                            {translate.t("LABEL.HDHOMERUN")}
                            </span>
                        }}>
                                <HdHomeRunOutput output={hdhr.clone()}/>
                        </RevealContent>
                    },
                    }
                }
            }
        </div>
    }
}
