use crate::{
    app::components::{Card, CollapsePanel, PlaylistProgressStatusCard, StatusCard, StatusContext, StreamsView},
    i18n::use_translation,
};
use shared::utils::human_readable_byte_size;
use yew::prelude::*;

#[component]
pub fn StatsView() -> Html {
    let translate = use_translation();
    let status_ctx = use_context::<StatusContext>().expect("Status context not found");

    let (mem, cpu) = status_ctx.system_info.as_ref().map_or_else(
        || ("n/a".to_string(), "n/a".to_string()),
        |system| {
            (
                format!(
                    "{} / {}",
                    human_readable_byte_size(system.memory_usage),
                    human_readable_byte_size(system.memory_total)
                ),
                format!("{:.2}%", system.cpu_usage),
            )
        },
    );

    let cache = status_ctx.status.as_ref().map_or_else(
        || "n/a".to_string(),
        |status| status.cache.as_ref().map_or_else(|| "n/a".to_string(), |c| c.clone()),
    );

    html! {
      <div class="tp__stats">
        <CollapsePanel expanded={true} title_content={Some(html! {
            <div class="tp__stats__header">
             <h1>{ translate.t("LABEL.STATS")}</h1>
            </div>
            })}>
            <div class="tp__stats__body">
              <div class="tp__stats__body-group">
                <Card><StatusCard title={translate.t("LABEL.MEMORY")} data={mem} /></Card>
                <Card><StatusCard title={translate.t("LABEL.CPU")} data={cpu} /></Card>
                <Card><StatusCard title={translate.t("LABEL.CACHE")} data={cache} /></Card>
              </div>
              <div class="tp__stats__body-group">
                <Card><PlaylistProgressStatusCard /></Card>
              </div>
            </div>
        </CollapsePanel>
        <CollapsePanel expanded={true} title_content={Some(html! {
            <div class="tp__stats__header">
             <h1>{ translate.t("LABEL.STREAMS")}</h1>
            </div>
            })}>
            <div class="tp__stats__body-group">
                <StreamsView />
            </div>
        </CollapsePanel>
      </div>
    }
}
