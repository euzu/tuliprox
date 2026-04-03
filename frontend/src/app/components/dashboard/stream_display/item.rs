use super::{
    helpers::{build_technical_chips, is_background_transfer_stream, render_cluster},
    meter::{MeterDisplayKind, StreamMeterBadge},
};
use crate::{
    app::components::{AppIcon, Chip, RevealContent, ToggleSwitch},
    hooks::use_service_context,
    i18n::use_translation,
    utils::{format_duration, t_safe},
};
use shared::{
    model::StreamInfo,
    utils::{current_time_secs, strip_port},
};
use std::rc::Rc;
use web_sys::MouseEvent;
use yew::{prelude::*, virtual_dom::AttrValue};

fn display_country_code(country_code: Option<&str>) -> Option<String> {
    let normalized = country_code?.trim().to_ascii_uppercase();
    let is_iso_country = normalized.len() == 2 && normalized.as_bytes().iter().all(|byte| byte.is_ascii_alphabetic());
    if is_iso_country {
        Some(normalized)
    } else {
        None
    }
}

#[derive(Properties, PartialEq, Clone)]
pub struct StreamDisplayItemProps {
    pub stream: Rc<StreamInfo>,
    pub metrics_enabled: bool,
    pub on_popup_click: Callback<(Rc<StreamInfo>, MouseEvent)>,
}

#[component]
pub fn StreamDisplayItem(props: &StreamDisplayItemProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let stream = props.stream.clone();
    let is_background_transfer = is_background_transfer_stream(&stream);
    let chips = build_technical_chips(stream.channel.item_type, stream.channel.technical.as_ref());
    let country_code = display_country_code(stream.country_code.as_deref());
    let country = country_code.as_ref().map_or_else(String::new, |code| {
        t_safe(&translate, &format!("COUNTRY.{code}")).unwrap_or_else(|| code.clone())
    });
    let flag_svg = country_code.as_ref().and_then(|code| services.flags.get_flag(code));
    let client_ip = strip_port(&stream.client_ip).to_string();

    let handle_popup_click = {
        let stream = stream.clone();
        let on_popup_click = props.on_popup_click.clone();
        Callback::from(move |event: MouseEvent| on_popup_click.emit((stream.clone(), event)))
    };

    html! {
        <article class="tp__stream-display__item">
            if !is_background_transfer {
                <button class="tp__stream-display__menu" onclick={handle_popup_click}>
                    <AppIcon name="Handle" />
                </button>
            }
        <div class="tp__stream-display__item-content">
            <div class="tp__stream-display__item-head">
                <div class="tp__stream-display__identity">
                    <div class="tp__stream-display__title-row">
                        <div class="tp__stream-display__title-block">
                            <div class="tp__stream-display__title">{stream.channel.title.to_string()}</div>
                            <div class="tp__stream-display__subtitle">
                                <span class="tp__stream-display__username"> {stream.username.clone()}</span>
                                {" • "}
                                <span class="tp__stream-display__channel_type">{render_cluster(&stream.channel)}</span>
                                {" • "}
                                <span class="tp__stream-display__provider">{stream.provider.clone()}</span>
                            </div>
                        </div>
                    </div>
                </div>
           </div>
           <div class="tp__stream-display__row">
            <div class="tp__stream-display__stats">
                    <div class="tp__stream-display__stat tp__stream-display__stat--category">
                        <span class="tp__stream-display__stat-label">{translate.t("LABEL.GROUP")}</span>
                        <span class="tp__stream-display__stat-value">{stream.channel.group.to_string()}</span>
                    </div>
                    <div class="tp__stream-display__stat tp__stream-display__stat--client">
                        <span class="tp__stream-display__stat-label">{translate.t("LABEL.CLIENT_IP")}</span>
                        <span class="tp__stream-display__stat-value tp__stream-display__stat-value--ip">{client_ip.clone()}</span>
                    </div>
                    if !country.is_empty() || country_code.is_some() {
                        <div class="tp__stream-display__stat tp__stream-display__stat--country">
                            <span class="tp__stream-display__stat-label">{translate.t("LABEL.COUNTRY")}</span>
                            <span class="tp__stream-display__stat-value">
                                <span class="tp__stream-display__country tp__stream-display__country--stat">
                                    if let Some(svg) = flag_svg.as_ref() {
                                        <span class="tp__stream-display__flag" aria-hidden="true">
                                            // SAFETY: flags.dat is built offline by flags_builder from a trusted flag directory.
                                            // If this source ever becomes user-controlled, replace this with sanitized SVG rendering.
                                            {Html::from_html_unchecked(AttrValue::from(svg.clone()))}
                                        </span>
                                    }
                                    if !country.is_empty() {
                                        <span class="tp__stream-display__country-name">{country.clone()}</span>
                                    } else if let Some(code) = country_code.as_ref() {
                                        <span class="tp__stream-display__country-code">{code.clone()}</span>
                                    }
                                </span>
                            </span>
                        </div>
                    }
                    <div class="tp__stream-display__stat">
                        <span class="tp__stream-display__stat-label">{translate.t("LABEL.SHARED")}</span>
                        <span class="tp__stream-display__stat-value"><ToggleSwitch value={stream.channel.shared} readonly={true} compact={true}/></span>
                    </div>
                    <div class="tp__stream-display__stat">
                        <span class="tp__stream-display__stat-label">{translate.t("LABEL.DURATION")}</span>
                        <span class="tp__stream-display__stat-value tp__stream-display__duration" data-ts={stream.ts.to_string()}>
                            {format_duration(current_time_secs().saturating_sub(stream.ts))}
                        </span>
                    </div>
                    if props.metrics_enabled && stream.meter_uid != 0 {
                        <div class="tp__stream-display__stat">
                            <span class="tp__stream-display__stat-label">{translate.t("LABEL.BANDWIDTH")}</span>
                            <span class="tp__stream-display__stat-value">
                                <StreamMeterBadge uid={stream.uid} meter_uid={stream.meter_uid} kind={MeterDisplayKind::Bandwidth} />
                            </span>
                        </div>
                        <div class="tp__stream-display__stat">
                            <span class="tp__stream-display__stat-label">{translate.t("LABEL.TRANSFERRED")}</span>
                            <span class="tp__stream-display__stat-value">
                                <StreamMeterBadge uid={stream.uid} meter_uid={stream.meter_uid} kind={MeterDisplayKind::Transferred} />
                            </span>
                        </div>
                    }
                    <div class="tp__stream-display__stat tp__stream-display__detail">
                        <span class="tp__stream-display__stat-label">{translate.t("LABEL.PLAYER")}</span>
                        <span class="tp__stream-display__stat-value">
                            <RevealContent preview={Some(html! { &stream.user_agent })}>{&stream.user_agent}</RevealContent>
                        </span>
                    </div>
                </div>
           </div>
           <div class="tp__stream-display__row">
                if !chips.is_empty() {
                    <div class="tp__stream-display__chips">
                        { for chips.into_iter().map(|(label, chip_class)| html! {
                            <Chip label={label} class={Some(format!("tp__stream-display__chip {chip_class}"))} />
                        })}
                    </div>
                }
            </div>
        </div>
        </article>
    }
}

#[cfg(test)]
mod tests {
    use super::display_country_code;

    #[test]
    fn display_country_code_filters_special_network_labels() {
        assert_eq!(display_country_code(Some("de")), Some("DE".to_string()));
        assert_eq!(display_country_code(Some("LOOPBACK")), None);
        assert_eq!(display_country_code(Some("LAN")), None);
        assert_eq!(display_country_code(Some("  ")), None);
    }
}
