use crate::app::components::{CollapsePanel, HdHomeRunOutput, M3uOutput, StrmOutput, XtreamOutput};
use shared::model::{ConfigTargetDto, TargetOutputDto};
use std::rc::Rc;
use yew::prelude::*;
use yew_i18n::use_translation;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct TargetOutputProps {
    pub target: Rc<ConfigTargetDto>,
}

#[function_component]
pub fn TargetOutput(props: &TargetOutputProps) -> Html {
    let translate = use_translation();

    html! {
        <div class="tp__target-output">
            {
                props.target.output.iter().map(|output: &TargetOutputDto| {
                    match output {
                    TargetOutputDto::Xtream(xc) => html! {
                        <CollapsePanel class={format!("tp__target-output__xtream{}", if xc.has_any_option() { " tp__target-output__has_options" } else {""}) }
                            expanded={false} title={translate.t("LABEL.XTREAM")}>
                            <XtreamOutput output={xc.clone()} />
                        </CollapsePanel>
                    },
                    TargetOutputDto::M3u(m3u) => html! {
                        <CollapsePanel class={format!("tp__target-output__m3u{}", if m3u.has_any_option() { " tp__target-output__has_options" } else {""}) }
                            expanded={false} title={translate.t("LABEL.M3U")}>
                            <M3uOutput output={m3u.clone()}/>
                        </CollapsePanel>
                    },
                    TargetOutputDto::Strm(strm) => html! {
                        <CollapsePanel class="tp__target-output__strm" expanded={false} title={translate.t("LABEL.STRM")}>
                            <StrmOutput output={strm.clone()}/>
                        </CollapsePanel>
                    },
                    TargetOutputDto::HdHomeRun(hdhr) => html! {
                        <CollapsePanel class="tp__target-output__hdhomerun" expanded={false} title={translate.t("LABEL.HDHOMERUN")}>
                                <HdHomeRunOutput output={hdhr.clone()}/>
                        </CollapsePanel>
                    },
                    }
                }).collect::<Html>()
            }
        </div>
    }
}