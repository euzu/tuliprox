use crate::app::components::AppIcon;
use crate::app::components::Panel;
use std::fmt;
use std::str::FromStr;
use wasm_bindgen::JsCast;
use yew::prelude::*;
use yew_i18n::use_translation;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum PrefPage {
    Update,
    User,
    ApiServer,
    MainConfig,
    Status,
}

impl fmt::Display for PrefPage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            PrefPage::Update => "update",
            PrefPage::User => "user",
            PrefPage::ApiServer => "apiserver",
            PrefPage::MainConfig => "mainconfig",
            PrefPage::Status => "status",
        };
        write!(f, "{s}")
    }
}

impl FromStr for PrefPage {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "update" => Ok(PrefPage::Update),
            "user" => Ok(PrefPage::User),
            "apiserver" => Ok(PrefPage::ApiServer),
            "mainconfig" => Ok(PrefPage::MainConfig),
            "status" => Ok(PrefPage::Status),
            _ => Err(())
        }
    }
}

struct SidebarAction {
    page: PrefPage,
    icon: &'static str,
    label: &'static str,
}

const SIDEBAR_ACTIONS: &[SidebarAction] = &[
    SidebarAction { page: PrefPage::Update, icon: "Refresh", label: "LABEL.REFRESH" },
    SidebarAction { page: PrefPage::User, icon: "User", label: "LABEL.USERS" },
    SidebarAction { page: PrefPage::ApiServer, icon: "ApiServer", label: "LABEL.PROXY" },
    SidebarAction { page: PrefPage::MainConfig, icon: "Config", label: "LABEL.CONFIG" },
    SidebarAction { page: PrefPage::Status, icon: "Status", label: "LABEL.STATUS" },
];

#[function_component]
pub fn Preferences() -> Html {
    let active_page = use_state(|| PrefPage::Update);

    let translation = use_translation();

    let handle_sidebar_action = {
        let active_page = active_page.clone();
        Callback::from(move |e: MouseEvent| {
            if let Some(target) = e.target() {
                if let Ok(element) = target.dyn_into::<web_sys::Element>() {
                    if let Some(data_page) = element.get_attribute("data-page") {
                        if let Ok(page) = PrefPage::from_str(&data_page) {
                            active_page.set(page);
                        }
                    }
                }
            }
        })
    };

    html! {
        <div class="tp__preferences">
            <div class="tp__preferences__content">
                <div class="tp__preferences__sidebar">
                {
                    SIDEBAR_ACTIONS.iter().map(|action| {
                      html! {
                            <div key={format!("pref_{:?}", action.page).to_lowercase()}
                                data-page={action.page.to_string()}
                                class={format!("tp__preferences__sidebar-menu-action preferences__sidebar-menu-action_{}{}",
                                         action.page.to_string(),
                                         if action.page == *active_page { " selected" } else { "" })}
                                onclick={handle_sidebar_action.clone()}
                            >
                               <AppIcon name={action.icon}/>
                               <label>{translation.t(action.label)}</label>
                            </div>
                        }
                      }).collect::<Vec<_>>()
                    }
                </div>
                <div class="tp__preferences__panels">
                    <Panel value={PrefPage::Update.to_string()} active={active_page.to_string()}>
                        <div class="tp__card">{"Update"}</div>
                    </Panel>
                    <Panel value={PrefPage::User.to_string()} active={active_page.to_string()}>
                        <div class="tp__card">{"User"}</div>
                    </Panel>
                    <Panel value={PrefPage::ApiServer.to_string()} active={active_page.to_string()}>
                        <div class="tp__card">{"Api"}</div>
                    </Panel>
                    <Panel value={PrefPage::MainConfig.to_string()} active={active_page.to_string()}>
                        <div class="tp__card">{"MainConfig"}</div>
                    </Panel>
                    <Panel value={PrefPage::Status.to_string()} active={active_page.to_string()}>
                        <div class="tp__card">{"Status"}</div>
                    </Panel>
                </div>
            </div>
        </div>
    }
}