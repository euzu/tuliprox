//! `PluginSlot`: the contributions of one extension slot.
//!
//! Besides the slot component this holds what every slot shares: the app-wide
//! [`PluginUiContext`] that loads `GET /api/v1/plugins/ui` once, the helpers
//! that turn contributions into sidebar entries and config tabs, the explorer
//! action menu items with their dialog, and the full-page menu frame.

use super::{
    plugin_frame::PluginFrame,
    plugin_i18n::{contribution_title, load_plugin_strings, title_or_key, PluginStrings},
};
use crate::{
    app::components::{menu_item::MenuItem, Card, ContentDialog},
    hooks::{use_icon_context, use_service_context, Icons},
    i18n::{use_active_locale, use_translation, YewI18n},
    model::{DialogAction, DialogActions, DialogResult, EventMessage},
    services::{plugin_slot_id, PluginUiService, ToastType},
};
use serde_json::Value;
use shared::model::{permission::Permission, PluginContributionDto, PluginSlotDto, PluginStateDto, PluginUiDto};
use std::{cell::Cell, collections::HashMap, rc::Rc};
use yew::{
    classes, component, hook, html, platform::spawn_local, use_callback, use_context, use_effect_with, use_memo,
    use_reducer, use_state, Callback, Children, Html, Properties, Reducible,
};

/// The icon of a contribution that names none the app has.
const FALLBACK_ICON: &str = "Nodes";
/// Hash prefix of a plugin page: `#plugin/{plugin_id}/{element}`.
pub const PLUGIN_ROUTE_PREFIX: &str = "plugin/";
/// Id prefix of a plugin tab of the config page: `plugin:{plugin_id}`.
pub const CONFIG_TAB_PREFIX: &str = "plugin:";

// ---------------------------------------------------------------------------
// What a slot shows (pure)
// ---------------------------------------------------------------------------

/// A plugin is shown live only when it is `enabled`; every other state renders greyed out.
pub fn plugin_available(plugin: &PluginUiDto) -> bool { plugin.state == PluginStateDto::Enabled }

fn state_label_key(state: PluginStateDto) -> &'static str {
    match state {
        PluginStateDto::Enabled => "LABEL.ENABLED",
        PluginStateDto::Disabled => "LABEL.DISABLED",
        PluginStateDto::Failed => "LABEL.PLUGIN_STATE_FAILED",
        PluginStateDto::Invalid => "LABEL.PLUGIN_STATE_INVALID",
        PluginStateDto::PendingApproval => "LABEL.PLUGIN_STATE_PENDING_APPROVAL",
        PluginStateDto::Approved => "LABEL.PLUGIN_STATE_APPROVED",
        PluginStateDto::Loaded => "LABEL.PLUGIN_STATE_LOADED",
        PluginStateDto::Discovered => "LABEL.PLUGIN_STATE_DISCOVERED",
    }
}

/// Why a plugin does not render: its failure reason, else its state.
pub fn unavailable_reason(translate: &YewI18n, plugin: &PluginUiDto) -> String {
    plugin
        .failure
        .as_deref()
        .filter(|failure| !failure.trim().is_empty())
        .map_or_else(|| translate.t(state_label_key(plugin.state)), ToOwned::to_owned)
}

/// The `#plugin/{id}/{element}` route of a menu contribution.
pub fn plugin_route(plugin_id: &str, element: &str) -> String { format!("{PLUGIN_ROUTE_PREFIX}{plugin_id}/{element}") }

/// Whether a location hash addresses a plugin page.
pub fn is_plugin_route(hash: &str) -> bool { hash.starts_with(PLUGIN_ROUTE_PREFIX) }

/// The id of the plugin a config tab id (`plugin:{id}`, or `plugin:{id}:{n}` for
/// its `n`-th `config-tab` contribution) belongs to.
pub fn plugin_id_of_tab(tab_id: &str) -> Option<&str> {
    tab_id.strip_prefix(CONFIG_TAB_PREFIX).map(|rest| rest.split(':').next().unwrap_or(rest))
}

/// One contribution of a plugin.
#[derive(Clone, PartialEq)]
pub struct PluginContribution {
    pub plugin: Rc<PluginUiDto>,
    pub index: usize,
}

impl PluginContribution {
    pub fn contribution(&self) -> &PluginContributionDto { &self.plugin.contributions[self.index] }
}

/// A sidebar entry for a menu contribution.
#[derive(Clone, PartialEq, Debug)]
pub struct PluginMenuEntry {
    /// Doubles as the entry's `name` and as the location hash it navigates to.
    pub route: String,
    pub title: String,
    pub icon: String,
    pub available: bool,
}

/// A plugin tab of the config page.
#[derive(Clone, PartialEq)]
pub struct PluginConfigTab {
    pub plugin: Rc<PluginUiDto>,
    /// `plugin:{plugin_id}`, or `plugin:{plugin_id}:{n}` for a contribution.
    pub id: String,
    pub title: String,
    pub icon: String,
    /// `None`: the plugin's `ui.config_tab`. `Some`: the frame of a `config-tab`
    /// contribution.
    pub element: Option<String>,
}

/// The icon a contribution asks for if the app has it. A plugin-supplied name
/// is never used as anything but a lookup key: `AppIcon` would treat an
/// unknown name as an image URL.
pub fn resolve_icon(icons: &Icons, requested: Option<&str>) -> String {
    requested.filter(|name| icons.get_icon(name).is_some()).map_or_else(|| FALLBACK_ICON.to_owned(), ToOwned::to_owned)
}

// ---------------------------------------------------------------------------
// The app-wide context
// ---------------------------------------------------------------------------

/// What `GET /api/v1/plugins/ui` returned, with the plugins' strings for the
/// active locale.
#[derive(Clone)]
pub struct PluginUiContext {
    pub plugins: Rc<Vec<Rc<PluginUiDto>>>,
    strings: Rc<HashMap<String, PluginStrings>>,
    pub service: Rc<PluginUiService>,
    /// Loads the list again. To be called after a plugin was installed, enabled, disabled or removed.
    pub refresh: Callback<()>,
}

impl PartialEq for PluginUiContext {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.plugins, &other.plugins)
            && Rc::ptr_eq(&self.strings, &other.strings)
            && Rc::ptr_eq(&self.service, &other.service)
            && self.refresh == other.refresh
    }
}

impl PluginUiContext {
    /// The strings of a plugin for the active locale; empty when it has none.
    pub fn strings_for(&self, plugin_id: &str) -> PluginStrings {
        self.strings.get(plugin_id).cloned().unwrap_or_default()
    }

    pub fn plugin(&self, plugin_id: &str) -> Option<Rc<PluginUiDto>> {
        self.plugins.iter().find(|plugin| plugin.id == plugin_id).cloned()
    }

    /// Every contribution to `slot`, in plugin order.
    pub fn contributions(&self, slot: PluginSlotDto) -> Vec<PluginContribution> {
        self.plugins
            .iter()
            .flat_map(|plugin| {
                plugin
                    .contributions
                    .iter()
                    .enumerate()
                    .filter(move |(_, contribution)| contribution.slot == slot)
                    .map(|(index, _)| PluginContribution { plugin: Rc::clone(plugin), index })
            })
            .collect()
    }

    pub fn has_contributions(&self, slot: PluginSlotDto) -> bool {
        self.plugins.iter().any(|plugin| plugin.contributions.iter().any(|contribution| contribution.slot == slot))
    }

    pub fn title_of(&self, entry: &PluginContribution) -> String {
        contribution_title(&self.strings_for(&entry.plugin.id), entry.contribution())
    }

    /// The sidebar entries of the `menu` slot.
    pub fn menu_entries(&self, icons: &Icons) -> Vec<PluginMenuEntry> {
        self.contributions(PluginSlotDto::Menu)
            .iter()
            .map(|entry| PluginMenuEntry {
                route: plugin_route(&entry.plugin.id, &entry.contribution().element),
                title: self.title_of(entry),
                icon: resolve_icon(icons, entry.contribution().icon.as_deref()),
                available: plugin_available(&entry.plugin),
            })
            .collect()
    }

    /// The plugin tabs of the config page: a plugin's `ui.config_tab` first,
    /// then one tab per `config-tab` contribution, in manifest order.
    pub fn config_tabs(&self, icons: &Icons) -> Vec<PluginConfigTab> {
        let mut tabs = Vec::new();
        for plugin in self.plugins.iter() {
            let strings = self.strings_for(&plugin.id);
            if let Some(tab) = plugin.config_tab.as_ref() {
                let title =
                    title_or_key(&strings, tab.title.as_deref().unwrap_or(&plugin.name), tab.title_key.as_deref());
                tabs.push(PluginConfigTab {
                    plugin: Rc::clone(plugin),
                    id: format!("{CONFIG_TAB_PREFIX}{}", plugin.id),
                    title,
                    icon: resolve_icon(icons, tab.icon.as_deref()),
                    element: None,
                });
            }
            for (n, entry) in plugin.contributions.iter().filter(|c| c.slot == PluginSlotDto::ConfigTab).enumerate() {
                tabs.push(PluginConfigTab {
                    plugin: Rc::clone(plugin),
                    id: format!("{CONFIG_TAB_PREFIX}{}:{n}", plugin.id),
                    title: contribution_title(&strings, entry),
                    icon: resolve_icon(icons, entry.icon.as_deref()),
                    element: Some(entry.element.clone()),
                });
            }
        }
        tabs
    }
}

#[hook]
pub fn use_plugin_ui() -> Option<PluginUiContext> { use_context::<PluginUiContext>() }

/// Counts refresh requests; a new value reloads the list.
#[derive(Clone, Copy, PartialEq, Eq)]
struct RefreshCount(u32);

impl Reducible for RefreshCount {
    type Action = ();

    fn reduce(self: Rc<Self>, (): ()) -> Rc<Self> { Rc::new(Self(self.0.wrapping_add(1))) }
}

#[derive(Properties, PartialEq)]
pub struct PluginUiProviderProps {
    pub children: Children,
}

/// Loads the plugin UI list once, for users who may read the system, and shares
/// it with every slot. It also loads the plugins' strings for the active locale.
#[component]
pub fn PluginUiProvider(props: &PluginUiProviderProps) -> Html {
    let services = use_service_context();
    let locale = use_active_locale();
    let service = use_memo((), |()| Rc::new(PluginUiService::new()));
    let can_read = services.auth.has_permission(Permission::SystemRead);
    let plugins = use_state(|| Rc::new(Vec::<Rc<PluginUiDto>>::new()));
    let strings = use_state(|| Rc::new(HashMap::<String, PluginStrings>::new()));
    let refreshes = use_reducer(|| RefreshCount(0));

    let refresh = {
        let refreshes = refreshes.clone();
        use_callback((), move |(), ()| refreshes.dispatch(()))
    };

    {
        let service = Rc::clone(&service);
        let plugins = plugins.clone();
        use_effect_with((can_read, refreshes.0), move |(can_read, _)| {
            let cancelled = Rc::new(Cell::new(false));
            if *can_read {
                let cancelled = Rc::clone(&cancelled);
                spawn_local(async move {
                    match service.list().await {
                        Ok(list) if !cancelled.get() => {
                            let unchanged = plugins.len() == list.len()
                                && plugins.iter().zip(list.iter()).all(|(known, fresh)| **known == *fresh);
                            if !unchanged {
                                plugins.set(Rc::new(list.into_iter().map(Rc::new).collect()));
                            }
                        }
                        Ok(_) => {}
                        Err(err) => log::warn!("plugin ui list not loaded: {err}"),
                    }
                });
            }
            move || cancelled.set(true)
        });
    }

    // A configuration change can enable or disable a plugin.
    {
        let events = services.event.clone();
        let refreshes = refreshes.clone();
        use_effect_with((), move |()| {
            let subscription = events.subscribe(move |message| {
                if matches!(message, EventMessage::ConfigChange(_)) {
                    refreshes.dispatch(());
                }
            });
            move || events.unsubscribe(subscription)
        });
    }

    {
        let service = Rc::clone(&service);
        let strings = strings.clone();
        let list = Rc::clone(&plugins);
        use_effect_with((Rc::as_ptr(&plugins) as usize, locale.code.clone()), move |(_, lang)| {
            let cancelled = Rc::new(Cell::new(false));
            let lang = lang.clone();
            {
                let cancelled = Rc::clone(&cancelled);
                spawn_local(async move {
                    let mut loaded = HashMap::new();
                    for plugin in list.iter().filter(|plugin| !plugin.locales.is_empty()) {
                        loaded.insert(plugin.id.clone(), load_plugin_strings(&service, plugin, &lang).await);
                    }
                    if !cancelled.get() {
                        strings.set(Rc::new(loaded));
                    }
                });
            }
            move || cancelled.set(true)
        });
    }

    let context = PluginUiContext {
        plugins: (*plugins).clone(),
        strings: (*strings).clone(),
        service: Rc::clone(&service),
        refresh,
    };
    html! {
        <yew::ContextProvider<PluginUiContext> context={context}>
            { for props.children.iter() }
        </yew::ContextProvider<PluginUiContext>>
    }
}

// ---------------------------------------------------------------------------
// Slot rendering
// ---------------------------------------------------------------------------

/// The plugin name as a badge.
#[derive(Properties, PartialEq)]
pub struct PluginBadgeProps {
    pub name: String,
}

#[component]
pub fn PluginBadge(props: &PluginBadgeProps) -> Html {
    html! { <span class="tp__plugin-badge">{ props.name.clone() }</span> }
}

#[derive(Properties, PartialEq)]
pub struct PluginUnavailableProps {
    pub plugin: Rc<PluginUiDto>,
    pub title: String,
}

/// A disabled or failed plugin: greyed out, with its reason and without a frame.
#[component]
pub fn PluginUnavailable(props: &PluginUnavailableProps) -> Html {
    let translate = use_translation();
    html! {
        <div class="tp__plugin-slot__item tp__plugin--disabled" aria-disabled="true">
            <div class="tp__plugin-slot__header">
                <PluginBadge name={props.plugin.name.clone()} />
                <span class="tp__plugin-slot__title">{ props.title.clone() }</span>
            </div>
            <span class="tp__plugin__reason">{ unavailable_reason(&translate, &props.plugin) }</span>
        </div>
    }
}

#[derive(Properties, PartialEq)]
pub struct PluginSlotProps {
    pub slot: PluginSlotDto,
    /// The slot subject, in the context shape the slot passes to plugins: `{ channels }`,
    /// `{ channel }`, `{ channel, programme }`. `None` is `null`.
    #[prop_or_default]
    pub context: Option<Value>,
    /// Wrap each contribution in a card.
    #[prop_or_default]
    pub card: bool,
}

/// Renders every contribution to a frame slot: the config section, source
/// editor, dashboard card, channel detail panel, EPG overlay and setup step.
/// The menu, the config tab and the explorer action are not frames in place
/// and have their own entry points below.
#[component]
pub fn PluginSlot(props: &PluginSlotProps) -> Html {
    let ui = use_plugin_ui();
    let Some(ui) = ui else { return html! {} };
    if matches!(props.slot, PluginSlotDto::Menu | PluginSlotDto::ConfigTab | PluginSlotDto::PlaylistExplorerAction) {
        return html! {};
    }
    let entries = ui.contributions(props.slot);
    if entries.is_empty() {
        return html! {};
    }
    html! {
        // A card slot lets its cards take part in the parent's grid.
        <div class={classes!("tp__plugin-slot", format!("tp__plugin-slot--{}", plugin_slot_id(props.slot)))}
            style={props.card.then_some("display:contents")}>
            { for entries.iter().map(|entry| {
                let title = ui.title_of(entry);
                let key = format!("{}:{}:{}", entry.plugin.id, entry.index, entry.contribution().element);
                if !plugin_available(&entry.plugin) {
                    return html! { <PluginUnavailable key={key} plugin={Rc::clone(&entry.plugin)} title={title} /> };
                }
                let item = html! {
                    <div class="tp__plugin-slot__item">
                        <div class="tp__plugin-slot__header">
                            <PluginBadge name={entry.plugin.name.clone()} />
                            <span class="tp__plugin-slot__title">{ title }</span>
                        </div>
                        <PluginFrame
                            plugin={Rc::clone(&entry.plugin)}
                            element={entry.contribution().element.clone()}
                            slot={props.slot}
                            context={props.context.clone()}
                        />
                    </div>
                };
                if props.card {
                    html! { <Card key={key}>{ item }</Card> }
                } else {
                    html! { <div key={key}>{ item }</div> }
                }
            }) }
        </div>
    }
}

// ---------------------------------------------------------------------------
// Explorer actions
// ---------------------------------------------------------------------------

/// The explorer action a user picked.
#[derive(Clone, PartialEq, Debug)]
pub struct PluginActionRequest {
    pub plugin: Rc<PluginUiDto>,
    pub element: String,
    pub title: String,
    /// The slot the contribution belongs to; the dialog frames it as that slot.
    pub slot: PluginSlotDto,
}

#[derive(Properties, PartialEq)]
pub struct PluginActionMenuProps {
    pub onselect: Callback<PluginActionRequest>,
    /// `playlist-explorer-action`, or `channel-detail-panel` for live and VOD
    /// channels, which have no detail view of their own.
    #[prop_or(PluginSlotDto::PlaylistExplorerAction)]
    pub slot: PluginSlotDto,
}

/// The contributions of `props.slot` as entries of the explorer's popup menu.
/// A plugin that is not enabled shows greyed out and says why.
#[component]
pub fn PluginActionMenuItems(props: &PluginActionMenuProps) -> Html {
    let ui = use_plugin_ui();
    let icons = use_icon_context();
    let translate = use_translation();
    let services = use_service_context();
    let Some(ui) = ui else { return html! {} };
    let slot = props.slot;
    let entries = ui.contributions(slot);
    html! {
        <>
            { for entries.iter().map(|entry| {
                let title = ui.title_of(entry);
                let available = plugin_available(&entry.plugin);
                let on_click = {
                    let request = PluginActionRequest {
                        plugin: Rc::clone(&entry.plugin),
                        element: entry.contribution().element.clone(),
                        title: title.clone(),
                        slot,
                    };
                    let onselect = props.onselect.clone();
                    let toastr = Rc::clone(&services.toastr);
                    let reason = unavailable_reason(&translate, &entry.plugin);
                    Callback::from(move |(_name, _event): (String, web_sys::MouseEvent)| {
                        if available {
                            onselect.emit(request.clone());
                        } else {
                            toastr.plugin_toast(&request.plugin.id, &request.plugin.name, ToastType::Warning, &reason);
                        }
                    })
                };
                html! {
                    <MenuItem
                        key={format!("{}:{}", entry.plugin.id, entry.index)}
                        icon={resolve_icon(&icons, entry.contribution().icon.as_deref())}
                        name={format!("plugin:{}:{}", entry.plugin.id, entry.index)}
                        label={format!("{}: {title}", entry.plugin.name)}
                        class={if available { "" } else { "tp__plugin--disabled" }}
                        onclick={on_click}
                    />
                }
            }) }
        </>
    }
}

#[derive(Properties, PartialEq)]
pub struct PluginActionDialogProps {
    pub request: PluginActionRequest,
    pub context: Option<Value>,
    pub on_close: Callback<()>,
}

/// The dialog an explorer menu entry opens: the plugin frame, as the slot of
/// the entry, with the selected channel(s).
#[component]
pub fn PluginActionDialog(props: &PluginActionDialogProps) -> Html {
    let request = &props.request;
    let content = html! {
        <div class="tp__plugin-action">
            <div class="tp__plugin-slot__header">
                <PluginBadge name={request.plugin.name.clone()} />
                <h2>{ request.title.clone() }</h2>
            </div>
            <PluginFrame
                plugin={Rc::clone(&request.plugin)}
                element={request.element.clone()}
                slot={request.slot}
                context={props.context.clone()}
            />
        </div>
    };
    let actions = DialogActions {
        left: None,
        right: vec![DialogAction::new_focused(
            "close",
            "LABEL.CLOSE",
            DialogResult::Cancel,
            Some("Close".to_owned()),
            None,
        )],
    };
    let on_confirm = {
        let on_close = props.on_close.clone();
        Callback::from(move |_: DialogResult| on_close.emit(()))
    };
    html! { <ContentDialog content={content} actions={actions} on_confirm={on_confirm} close_on_backdrop_click={true} /> }
}

// ---------------------------------------------------------------------------
// Menu page
// ---------------------------------------------------------------------------

#[derive(Properties, PartialEq)]
pub struct PluginMenuPageProps {
    /// The `plugin/{id}/{element}` route of the page.
    pub route: String,
}

/// A menu contribution as a full page: the frame fills the content area.
#[component]
pub fn PluginMenuPage(props: &PluginMenuPageProps) -> Html {
    let ui = use_plugin_ui();
    let translate = use_translation();
    let Some(ui) = ui else { return html! {} };
    let entry = ui
        .contributions(PluginSlotDto::Menu)
        .into_iter()
        .find(|entry| plugin_route(&entry.plugin.id, &entry.contribution().element) == props.route);
    let Some(entry) = entry else {
        // The list may still be loading, or the plugin was removed.
        return html! { <div class="tp__plugin-page"><span class="tp__plugin__reason">{ translate.t("LABEL.LOADING") }</span></div> };
    };
    let title = ui.title_of(&entry);
    html! {
        <div class="tp__plugin-page">
            <div class="tp__plugin-page__header">
                <h1>{ title.clone() }</h1>
                <PluginBadge name={entry.plugin.name.clone()} />
            </div>
            if plugin_available(&entry.plugin) {
                <div class="tp__plugin-page__body">
                    <PluginFrame
                        key={props.route.clone()}
                        plugin={Rc::clone(&entry.plugin)}
                        element={entry.contribution().element.clone()}
                        slot={PluginSlotDto::Menu}
                        fill={true}
                    />
                </div>
            } else {
                <span class="tp__plugin__reason">{ unavailable_reason(&translate, &entry.plugin) }</span>
            }
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::PluginLocaleMap;
    use shared::model::{PluginConfigTabDto, PluginRpcExportDto};

    fn contribution(slot: PluginSlotDto, element: &str) -> PluginContributionDto {
        PluginContributionDto {
            slot,
            title: format!("{element} title"),
            title_key: Some("title.key".to_string()),
            icon: None,
            element: element.to_string(),
        }
    }

    fn plugin(id: &str, state: PluginStateDto, contributions: Vec<PluginContributionDto>) -> Rc<PluginUiDto> {
        Rc::new(PluginUiDto {
            id: id.to_string(),
            name: id.to_uppercase(),
            version: "1.0.0".to_string(),
            state,
            failure: None,
            locales: vec!["en".to_string()],
            config_section: false,
            config_extensions: Vec::new(),
            config_tab: None,
            contributions,
            rpc_exports: Vec::<PluginRpcExportDto>::new(),
        })
    }

    fn context(plugins: Vec<Rc<PluginUiDto>>, strings: &[(&str, &[(&str, &str)])]) -> PluginUiContext {
        PluginUiContext {
            plugins: Rc::new(plugins),
            strings: Rc::new(
                strings
                    .iter()
                    .map(|(id, entries)| {
                        let map: PluginLocaleMap =
                            entries.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
                        (id.to_string(), Rc::new(map))
                    })
                    .collect(),
            ),
            service: Rc::new(PluginUiService::with_base("/".to_string())),
            refresh: Callback::noop(),
        }
    }

    #[test]
    fn contributions_are_listed_per_slot_in_plugin_order() {
        let ctx = context(
            vec![
                plugin(
                    "a",
                    PluginStateDto::Enabled,
                    vec![
                        contribution(PluginSlotDto::DashboardCard, "ui/a.html"),
                        contribution(PluginSlotDto::Menu, "ui/menu.html"),
                        contribution(PluginSlotDto::DashboardCard, "ui/a2.html"),
                    ],
                ),
                plugin("b", PluginStateDto::Disabled, vec![contribution(PluginSlotDto::DashboardCard, "ui/b.html")]),
            ],
            &[],
        );
        let cards = ctx.contributions(PluginSlotDto::DashboardCard);
        let found: Vec<_> = cards.iter().map(|c| (c.plugin.id.as_str(), c.contribution().element.as_str())).collect();
        assert_eq!(found, [("a", "ui/a.html"), ("a", "ui/a2.html"), ("b", "ui/b.html")]);
        assert!(ctx.has_contributions(PluginSlotDto::Menu));
        assert!(!ctx.has_contributions(PluginSlotDto::SetupWizardStep));
        assert!(ctx.contributions(PluginSlotDto::SetupWizardStep).is_empty());
    }

    #[test]
    fn titles_use_the_plugins_strings_when_they_have_the_key() {
        let ctx = context(
            vec![
                plugin("a", PluginStateDto::Enabled, vec![contribution(PluginSlotDto::DashboardCard, "ui/a.html")]),
                plugin("b", PluginStateDto::Enabled, vec![contribution(PluginSlotDto::DashboardCard, "ui/b.html")]),
            ],
            &[("a", &[("title.key", "Translated")])],
        );
        let cards = ctx.contributions(PluginSlotDto::DashboardCard);
        assert_eq!(ctx.title_of(&cards[0]), "Translated");
        assert_eq!(ctx.title_of(&cards[1]), "ui/b.html title");
    }

    #[test]
    fn only_enabled_plugins_are_available() {
        for (state, available) in [
            (PluginStateDto::Enabled, true),
            (PluginStateDto::Disabled, false),
            (PluginStateDto::Failed, false),
            (PluginStateDto::Loaded, false),
        ] {
            assert_eq!(plugin_available(&plugin("a", state, Vec::new())), available, "{state:?}");
        }
    }

    #[test]
    fn plugin_routes_round_trip_through_the_hash() {
        let route = plugin_route("sports", "ui/guide.html");
        assert_eq!(route, "plugin/sports/ui/guide.html");
        assert!(is_plugin_route(&route));
        assert!(!is_plugin_route("dashboard"));
        assert!(!is_plugin_route("plugins"));
        assert_eq!(plugin_id_of_tab("plugin:sports"), Some("sports"));
        assert_eq!(plugin_id_of_tab("plugin:sports:1"), Some("sports"));
        assert_eq!(plugin_id_of_tab("main"), None);
    }

    #[test]
    fn config_tabs_come_from_the_tab_or_a_config_tab_contribution() {
        let mut with_tab = (*plugin("a", PluginStateDto::Enabled, Vec::new())).clone();
        with_tab.config_tab = Some(PluginConfigTabDto {
            title: Some("A tab".to_string()),
            title_key: None,
            icon: None,
            schema: None,
            element: Some("ui/tab.html".to_string()),
        });
        let with_contribution =
            plugin("b", PluginStateDto::Enabled, vec![contribution(PluginSlotDto::ConfigTab, "ui/b-tab.html")]);
        let none = plugin("c", PluginStateDto::Enabled, vec![contribution(PluginSlotDto::DashboardCard, "ui/c.html")]);
        let ctx = context(vec![Rc::new(with_tab), with_contribution, none], &[]);
        let tabs = ctx.config_tabs(&Icons::new());
        let ids: Vec<_> = tabs.iter().map(|tab| (tab.id.as_str(), tab.title.as_str())).collect();
        assert_eq!(ids, [("plugin:a", "A tab"), ("plugin:b:0", "ui/b-tab.html title")]);
        assert!(tabs.iter().all(|tab| tab.icon == FALLBACK_ICON));
    }

    #[test]
    fn menu_entries_route_by_id_and_element() {
        let ctx = context(
            vec![plugin("p", PluginStateDto::Failed, vec![contribution(PluginSlotDto::Menu, "ui/menu.html")])],
            &[],
        );
        let entries = ctx.menu_entries(&Icons::new());
        assert_eq!(
            entries,
            [PluginMenuEntry {
                route: "plugin/p/ui/menu.html".to_string(),
                title: "ui/menu.html title".to_string(),
                icon: FALLBACK_ICON.to_string(),
                available: false,
            }]
        );
    }

    #[test]
    fn a_plugin_supplied_icon_name_is_only_a_lookup_key() {
        // The app has no icon called like this, so it falls back instead of becoming an image URL.
        assert_eq!(resolve_icon(&Icons::new(), Some("https://evil.test/x.png")), FALLBACK_ICON);
        assert_eq!(resolve_icon(&Icons::new(), None), FALLBACK_ICON);
    }
}
