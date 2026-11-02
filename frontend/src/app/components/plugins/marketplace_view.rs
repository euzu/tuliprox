//! The Marketplace tab of the Plugin Manager: browse the catalogs of the enabled sources, pick a
//! release and install it through the existing review dialog. Nothing is approved or enabled
//! without the admin's explicit action.
use crate::{
    app::components::{
        input::Input,
        plugins::{marketplace_sources::MarketplaceSources, plugin_upload::PluginUpload},
        Card, Chip, DropDownOption, DropDownSelection, LoadingIndicator, NoContent, Select, TextButton,
    },
    hooks::use_service_context,
    i18n::{use_translation, YewI18n},
    services::PluginActionError,
};
use shared::model::{
    MarketplaceDto, MarketplaceInstallDto, MarketplacePluginDto, MarketplaceQueryDto, MarketplaceReleaseDto,
    PluginRepositoryInfoDto, PluginUploadReviewDto,
};
use std::rc::Rc;
use web_sys::KeyboardEvent;
use yew::{
    classes, component, html, platform::spawn_local, use_effect_with, use_state, Callback, Event, Html, Properties,
};

/// The release a card shows: the chosen one while it is still installable, else the newest
/// compatible one (releases arrive newest first, compatible before incompatible).
fn effective_release<'a>(
    releases: &'a [MarketplaceReleaseDto],
    chosen: Option<&str>,
) -> Option<&'a MarketplaceReleaseDto> {
    chosen
        .and_then(|version| releases.iter().find(|release| release.version == version && release.compatible))
        .or_else(|| releases.iter().find(|release| release.compatible))
}

/// `None` for the "all" entry of a filter, so it is not sent.
fn filter_value(selection: &DropDownSelection) -> Option<String> {
    match selection {
        DropDownSelection::Single(value) if !value.is_empty() => Some(value.clone()),
        _ => None,
    }
}

fn filter_options(all_label: &str, values: Vec<(String, String)>, selected: Option<&str>) -> Rc<Vec<DropDownOption>> {
    let mut options = vec![DropDownOption::new("", html! { {all_label.to_owned()} }, selected.is_none())];
    options.extend(values.into_iter().map(|(id, label)| {
        let is_selected = selected == Some(id.as_str());
        DropDownOption::new(&id, html! { {label} }, is_selected)
    }));
    Rc::new(options)
}

#[derive(Properties, Clone, PartialEq)]
struct PluginCardProps {
    plugin: MarketplacePluginDto,
    can_write: bool,
    busy: bool,
    /// Source id, plugin id and release version to install.
    on_install: Callback<(String, String, String)>,
}

fn render_release(
    translate: &YewI18n,
    plugin: &MarketplacePluginDto,
    release: &MarketplaceReleaseDto,
    selected: bool,
    on_pick: &Callback<String>,
) -> Html {
    let pick = {
        let on_pick = on_pick.clone();
        let version = release.version.clone();
        Callback::from(move |_: Event| on_pick.emit(version.clone()))
    };
    html! {
        <li key={release.version.clone()}
            class={classes!("tp__plugins__release", (!release.compatible).then_some("tp__plugins__release--incompatible"))}>
            <label>
                <input type="radio" name={format!("release-{}-{}", plugin.source, plugin.id)}
                    checked={selected} disabled={!release.compatible} onchange={pick} />
                { " " }<strong>{ release.version.clone() }</strong>
                if let Some(published) = release.published.as_ref() {
                    <span class="tp__plugins__muted">{ format!(" ({published})") }</span>
                }
            </label>
            if !release.compatible {
                <span class="tp__plugins__release-reason tp__input-error">
                    { release.incompatible_reason.clone().unwrap_or_else(|| translate.t("LABEL.PLUGIN_MARKETPLACE_INCOMPATIBLE")) }
                </span>
            }
        </li>
    }
}

/// One plugin of one source, with its release picker and Install.
#[component]
fn MarketplacePluginCard(props: &PluginCardProps) -> Html {
    let translate = use_translation();
    let chosen = use_state(|| None::<String>);
    let plugin = &props.plugin;
    let selected = effective_release(&plugin.releases, chosen.as_deref());
    let on_pick = {
        let chosen = chosen.clone();
        Callback::from(move |version: String| chosen.set(Some(version)))
    };
    let install = {
        let on_install = props.on_install.clone();
        let source = plugin.source.clone();
        let id = plugin.id.clone();
        let version = selected.map(|release| release.version.clone());
        Callback::from(move |_: String| {
            if let Some(version) = version.clone() {
                on_install.emit((source.clone(), id.clone(), version));
            }
        })
    };
    html! {
        <Card class="tp__plugins__card tp__plugins__market-card">
            <div class="tp__plugins__header">
                <h3>{ plugin.name.clone() }</h3>
                <code>{ plugin.id.clone() }</code>
                if let Some(category) = plugin.category.as_ref() {
                    <span class="tp__plugin-badge">{ category.clone() }</span>
                }
                if let Some(installed) = plugin.installed.as_ref() {
                    <span class="tp__task-status tp__task-status--done">
                        { translate.t("LABEL.PLUGIN_MARKETPLACE_INSTALLED") }
                        if let Some(version) = installed.version.as_ref() {
                            { format!(" {version}") }
                        }
                    </span>
                    if let Some(origin) = installed.origin.as_ref() {
                        <span class="tp__plugins__origin">
                            { format!("{} {} @ {}", translate.t("LABEL.PLUGIN_MARKETPLACE_INSTALLED_FROM"), origin.source, origin.version) }
                        </span>
                    }
                }
            </div>
            <p class="tp__plugins__muted">
                { format!("{}: {}", translate.t("LABEL.PLUGIN_MARKETPLACE_SOURCE"), plugin.source_name) }
                if let Some(author) = plugin.author.as_ref() {
                    { format!(" | {}: {author}", translate.t("LABEL.PLUGIN_AUTHOR")) }
                }
                if let Some(license) = plugin.license.as_ref() {
                    { format!(" | {}: {license}", translate.t("LABEL.PLUGIN_MARKETPLACE_LICENSE")) }
                }
                if let Some(homepage) = plugin.homepage.as_ref() {
                    // shown as text on purpose: a catalog-supplied URL is never made a link here
                    { format!(" | {}: {homepage}", translate.t("LABEL.PLUGIN_HOMEPAGE")) }
                }
            </p>
            if let Some(description) = plugin.description.as_ref() {
                <p class="tp__plugins__description">{ description.clone() }</p>
            }
            if !plugin.also_in.is_empty() {
                <div class="tp__plugins__also-in" role="note">
                    { format!("{} {}", translate.t("MESSAGES.PLUGIN_MARKETPLACE_ALSO_IN"), plugin.also_in.join(", ")) }
                </div>
            }
            if plugin.releases.is_empty() {
                <p class="tp__plugins__muted">{ translate.t("MESSAGES.PLUGIN_MARKETPLACE_NO_RELEASES") }</p>
            } else {
                <h4>{ translate.t("LABEL.PLUGIN_MARKETPLACE_RELEASES") }</h4>
                <ul class="tp__plugins__releases">
                    { for plugin.releases.iter().map(|release| {
                        let is_selected = selected.is_some_and(|picked| picked.version == release.version);
                        render_release(&translate, plugin, release, is_selected, &on_pick)
                    }) }
                </ul>
            }
            if let Some(release) = selected {
                <div class="tp__plugins__release-details">
                    if let Some(changelog) = release.changelog.as_ref() {
                        <h4>{ translate.t("LABEL.PLUGIN_MARKETPLACE_CHANGELOG") }</h4>
                        <p class="tp__plugins__changelog">{ changelog.clone() }</p>
                    }
                    <div class="tp__plugins__chips">
                        <span>{ translate.t("LABEL.PLUGIN_MARKETPLACE_CATALOG_CAPABILITIES") }</span>
                        if release.capabilities.is_empty() {
                            <span class="tp__plugins__muted">{ translate.t("LABEL.PLUGIN_NO_CAPABILITIES") }</span>
                        }
                        { for release.capabilities.iter().map(|name| html! { <Chip key={name.clone()} label={name.clone()} /> }) }
                    </div>
                    <p class="tp__plugins__muted">{ translate.t("MESSAGES.PLUGIN_MARKETPLACE_CAPABILITIES_HINT") }</p>
                </div>
            }
            if props.can_write {
                <div class="tp__plugins__market-actions">
                    <TextButton
                        name={format!("install-{}-{}", plugin.source, plugin.id)}
                        icon="Download"
                        class="primary"
                        title={translate.t("LABEL.PLUGIN_MARKETPLACE_INSTALL")}
                        aria_label={format!("{} {} ({})", translate.t("LABEL.PLUGIN_MARKETPLACE_INSTALL"), plugin.name, plugin.source_name)}
                        disabled={props.busy || selected.is_none()}
                        onclick={install}
                    />
                </div>
            }
        </Card>
    }
}

#[derive(Properties, Clone, PartialEq)]
pub struct MarketplaceViewProps {
    /// Only users with `SystemWrite` get the mutation controls.
    pub can_write: bool,
    /// A plugin was installed or enabled: the installed list must reload.
    pub on_changed: Callback<()>,
}

/// What one load brings back.
struct Loaded {
    marketplace: MarketplaceDto,
    sources: Vec<PluginRepositoryInfoDto>,
}

async fn load(
    services: Rc<crate::services::PluginService>,
    query: MarketplaceQueryDto,
) -> Result<Loaded, PluginActionError> {
    // the marketplace first: the server syncs a source whose cache is still empty
    let marketplace = services.marketplace(&query).await?;
    let sources = services.list_repositories().await.map_err(|err| PluginActionError {
        status: 0,
        error: err.to_string(),
        problems: Vec::new(),
    })?;
    Ok(Loaded { marketplace, sources })
}

/// The Marketplace tab: the sources, a search with category and source filters, and the plugin cards.
#[component]
pub fn MarketplaceView(props: &MarketplaceViewProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();

    let marketplace = use_state(|| None::<Rc<MarketplaceDto>>);
    let sources = use_state(|| Rc::new(Vec::<PluginRepositoryInfoDto>::new()));
    let loading = use_state(|| true);
    let error = use_state(|| None::<String>);
    let reload = use_state(|| 0_u32);
    let search_draft = use_state(String::new);
    let search = use_state(|| None::<String>);
    let category = use_state(|| None::<String>);
    let source = use_state(|| None::<String>);
    let installing = use_state(|| false);
    let review = use_state(|| None::<PluginUploadReviewDto>);
    // The plugin just installed (id, name): the admin may enable it from the notice.
    let installed = use_state(|| None::<(String, String)>);

    {
        let marketplace = marketplace.clone();
        let sources = sources.clone();
        let loading = loading.clone();
        let error = error.clone();
        let service = services.plugins.clone();
        use_effect_with(
            (*reload, (*search).clone(), (*category).clone(), (*source).clone()),
            move |(_, q, category, source)| {
                let query = MarketplaceQueryDto { q: q.clone(), category: category.clone(), source: source.clone() };
                loading.set(true);
                spawn_local(async move {
                    match load(service, query).await {
                        Ok(loaded) => {
                            marketplace.set(Some(Rc::new(loaded.marketplace)));
                            sources.set(Rc::new(loaded.sources));
                            error.set(None);
                        }
                        Err(err) => error.set(Some(err.message())),
                    }
                    loading.set(false);
                });
                || ()
            },
        );
    }

    let on_refresh = {
        let reload = reload.clone();
        Callback::from(move |_: String| reload.set(*reload + 1))
    };
    let on_sources_changed = {
        let reload = reload.clone();
        let on_changed = props.on_changed.clone();
        Callback::from(move |()| {
            reload.set(*reload + 1);
            // a removed source changes the installed badges' origins only on reload of the list
            on_changed.emit(());
        })
    };
    let on_draft = {
        let search_draft = search_draft.clone();
        Callback::from(move |value: String| search_draft.set(value))
    };
    let apply_search = {
        let search = search.clone();
        let search_draft = search_draft.clone();
        Callback::from(move |(): ()| {
            let text = search_draft.trim().to_owned();
            search.set((!text.is_empty()).then_some(text));
        })
    };
    let on_search_key = {
        let apply_search = apply_search.clone();
        Callback::from(move |event: KeyboardEvent| {
            if event.key() == "Enter" {
                apply_search.emit(());
            }
        })
    };
    let on_search_click = {
        let apply_search = apply_search.clone();
        Callback::from(move |_: String| apply_search.emit(()))
    };
    let on_category = {
        let category = category.clone();
        Callback::from(move |(_, selection): (String, DropDownSelection)| category.set(filter_value(&selection)))
    };
    let on_source = {
        let source = source.clone();
        Callback::from(move |(_, selection): (String, DropDownSelection)| source.set(filter_value(&selection)))
    };

    let on_install = {
        let services = services.clone();
        let installing = installing.clone();
        let review = review.clone();
        Callback::from(move |(source, id, version): (String, String, String)| {
            if *installing {
                return;
            }
            installing.set(true);
            let services = services.clone();
            let installing = installing.clone();
            let review = review.clone();
            spawn_local(async move {
                match services.plugins.marketplace_install(&MarketplaceInstallDto { source, id, version }).await {
                    Ok(dto) => review.set(Some(dto)),
                    Err(err) => services.toastr.error(err.message()),
                }
                installing.set(false);
            });
        })
    };
    let on_close_review = {
        let review = review.clone();
        Callback::from(move |()| review.set(None))
    };
    let on_enable = {
        let services = services.clone();
        let translate = translate.clone();
        let installed = installed.clone();
        let reload = reload.clone();
        let on_changed = props.on_changed.clone();
        Callback::from(move |_: String| {
            let Some((id, _)) = (*installed).clone() else {
                return;
            };
            installed.set(None);
            let services = services.clone();
            let translate = translate.clone();
            let reload = reload.clone();
            let on_changed = on_changed.clone();
            let next = *reload + 1;
            spawn_local(async move {
                match services.plugins.set_enabled(&id, true).await {
                    Ok(summary) => {
                        services.toastr.success(format!(
                            "{}: {}",
                            translate.t("MESSAGES.PLUGIN_ENABLED"),
                            summary.name
                        ));
                    }
                    Err(err) => services.toastr.error(err.message()),
                }
                reload.set(next);
                on_changed.emit(());
            });
        })
    };
    let on_dismiss_enable = {
        let installed = installed.clone();
        Callback::from(move |_: String| installed.set(None))
    };

    let category_options = filter_options(
        &translate.t("LABEL.PLUGIN_MARKETPLACE_ALL_CATEGORIES"),
        marketplace
            .as_ref()
            .map(|data| data.categories.iter().map(|name| (name.clone(), name.clone())).collect())
            .unwrap_or_default(),
        category.as_deref(),
    );
    let source_options = filter_options(
        &translate.t("LABEL.PLUGIN_MARKETPLACE_ALL_SOURCES"),
        sources.iter().filter(|info| info.enabled).map(|info| (info.id.clone(), info.name.clone())).collect(),
        source.as_deref(),
    );

    let results = if let Some(message) = error.as_ref() {
        html! {
            <div class="tp__plugins__error tp__input-error" role="alert">
                { format!("{} {message}", translate.t("MESSAGES.PLUGIN_MARKETPLACE_LOAD_FAILED")) }
            </div>
        }
    } else if let Some(data) = marketplace.as_ref() {
        if data.plugins.is_empty() {
            html! { <NoContent text={translate.t("MESSAGES.PLUGIN_MARKETPLACE_EMPTY")} /> }
        } else {
            html! {
                <>{ for data.plugins.iter().map(|plugin| html! {
                    <MarketplacePluginCard
                        key={format!("{}/{}", plugin.source, plugin.id)}
                        plugin={plugin.clone()}
                        can_write={props.can_write}
                        busy={*installing}
                        on_install={on_install.clone()}
                    />
                }) }</>
            }
        }
    } else {
        html! {}
    };

    let on_review_installed = {
        let installed = installed.clone();
        let reload = reload.clone();
        let on_changed = props.on_changed.clone();
        let target = (*review).as_ref().map(|dto| (dto.id.clone(), dto.name.clone()));
        Callback::from(move |()| {
            installed.set(target.clone());
            reload.set(*reload + 1);
            on_changed.emit(());
        })
    };

    html! {
        <div class="tp__plugins__marketplace">
            <MarketplaceSources sources={(*sources).clone()} can_write={props.can_write}
                on_changed={on_sources_changed} />
            <div class="tp__plugins__market-filters">
                <Input name="marketplace_search" value={(*search_draft).clone()} on_change={on_draft}
                    onkeydown={Some(on_search_key)} icon={Some("Search".to_owned())}
                    placeholder={Some(translate.t("LABEL.SEARCH"))} />
                <Select name="marketplace_category" options={category_options} on_select={on_category} />
                <Select name="marketplace_source" options={source_options} on_select={on_source} />
                <TextButton name="search" icon="Search" title={translate.t("LABEL.SEARCH")} onclick={on_search_click} />
                <TextButton name="market-refresh" icon="Refresh" title={translate.t("LABEL.REFRESH")}
                    disabled={*loading} onclick={on_refresh} />
            </div>
            <LoadingIndicator loading={*loading || *installing} />
            if let Some((_, name)) = (*installed).clone().filter(|_| props.can_write) {
                <div class="tp__plugins__update" role="status">
                    <strong>{ format!("{}: {name}", translate.t("MESSAGES.PLUGIN_INSTALLED")) }</strong>
                    { " " }{ translate.t("MESSAGES.PLUGIN_MARKETPLACE_ENABLE_OFFER") }
                    <TextButton name="enable-installed" icon="Ok" title={translate.t("LABEL.ENABLED")}
                        onclick={on_enable} />
                    <TextButton name="dismiss-installed" icon="Close" title={translate.t("LABEL.CLOSE")}
                        onclick={on_dismiss_enable} />
                </div>
            }
            <div class="tp__plugins__market-results">{ results }</div>
            if let Some(dto) = (*review).clone().filter(|_| props.can_write) {
                <PluginUpload key={format!("market-{}-{}", dto.id, dto.version)} on_close={on_close_review}
                    on_installed={on_review_installed} initial_review={Some(dto.clone())} />
            }
        </div>
    }
}
