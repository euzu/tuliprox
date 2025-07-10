use crate::app::components::{ActionCard, TextButton};
use crate::hooks::use_service_context;
use shared::model::IpCheckDto;
use std::future;
use yew::prelude::*;
use yew::suspense::use_future;
use yew_i18n::use_translation;
use crate::app::CardContext;

#[function_component]
pub fn IpinfoActionCard() -> Html {
    let services = use_service_context();
    let translate = use_translation();
    let config_exists = use_state(|| false);
    let ip_address = use_state(|| None::<IpCheckDto>);
    let card_ctx = use_context::<CardContext>();

    let fetch_ip_info = {
        let services_ctx = services.clone();
        let ip_address_state = ip_address.clone();
        let card_ctx_1 = card_ctx.clone();

        Callback::from(move |exists: bool| {
            if exists {
                let card_ctx_2 = card_ctx.clone();
                if let Some(card_context) = card_ctx_1.as_ref() {
                    card_context.custom_class.set("tp__pulse".to_string());
                }
                let services_ctx = services_ctx.clone();
                let ip_address_state = ip_address_state.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let result = services_ctx.config.get_ip_info().await;
                    ip_address_state.set(result);
                    gloo_timers::future::TimeoutFuture::new(500).await;
                    if let Some(card_context) = card_ctx_2.as_ref() {
                        card_context.custom_class.set(String::new());
                    }
                });
            }
        })
    };

    let handle_update = {
        let config_exists_state = config_exists.clone();
        let fetch_ip_info_cb = fetch_ip_info.clone();
        Callback::from(move |_| {
            fetch_ip_info_cb.emit(*config_exists_state);
        })
    };

    {
        // first register for config update
        let services_ctx = services.clone();
        let config_exists_state = config_exists.clone();
        let fetch_ip_info_cb = fetch_ip_info.clone();
        let _ = use_future(|| async move {
            services_ctx.config.config_subscribe(
                &mut |cfg| {
                    let exists = if let Some(app_cfg) = &cfg {
                        if app_cfg.config.ipcheck.is_some() {
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    config_exists_state.set(exists);
                    fetch_ip_info_cb.emit(exists);
                    future::ready(())
                }
            ).await
        });
    }

    {
        let services_ctx = services.clone();
        let config_exists_state = config_exists.clone();
        let _ = use_future(|| async move {
            let cfg = services_ctx.config.get_server_config().await;
            config_exists_state.set(if let Some(app_cfg) = &cfg {
                app_cfg.config.ipcheck.is_some()
            } else {
                false
            });
        });
    }

    html! {
        <ActionCard icon="Network" classname="tp__ipinfo" title={translate.t("LABEL.IP_INFO")}
        subtitle_html={if *config_exists {
            if let Some(ip_info) = &*ip_address {
                let ip4 = ip_info.ipv4.as_ref().map_or_else(|| "n/a".to_string(), |ip| ip.to_string());
                let ip6 = ip_info.ipv6.as_ref().map_or_else(|| "n/a".to_string(), |ip| ip.to_string());
                format!("{}: {ip4}<br> {}: {ip6}", translate.t("LABEL.IPv4"), translate.t("LABEL.IPv6"))
            } else {
                String::new()
            }
        } else { translate.t("LABEL.NOT_ACTIVATED")} }>
          <TextButton name="ipinfo" title={translate.t("LABEL.UPDATE")} icon="Refresh" onclick={handle_update} />
        </ActionCard>
    }
}