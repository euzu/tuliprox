use crate::app::components::{
    Card, DiscordActionCard, DocumentationActionCard, GithubActionCard, IpinfoActionCard,
    UserActionCard, VersionActionCard,
};
use crate::app::context::StatusContext;
use yew::prelude::*;
use yew_i18n::use_translation;

#[function_component]
pub fn DashboardView() -> Html {
    let translate = use_translation();
    let status_ctx = use_context::<StatusContext>().expect("Status context not found");

    html! {
      <div class="tp__dashboard">
        <div class="tp__dashboard__header">
         <h1>{ translate.t("LABEL.DASHBOARD")}</h1>
        </div>
        <div class="tp__dashboard__body">
            <div class="tp__dashboard__body-actions">
              <Card>
                 <VersionActionCard version={status_ctx.status.as_ref().map_or_else(String::new,  |s| s.version.clone())}
                     build_time={status_ctx.status.as_ref().map_or_else(String::new,  |s| s.build_time.as_ref().map_or_else(String::new, |v| v.clone()))}/>
              </Card>
              <Card><UserActionCard /></Card>
              <Card><DocumentationActionCard /></Card>
              <Card><DiscordActionCard /></Card>
              <Card><GithubActionCard /></Card>
              <Card><IpinfoActionCard /></Card>
            </div>
        </div>
      </div>
    }
}
