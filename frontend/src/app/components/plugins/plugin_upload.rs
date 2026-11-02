//! Upload dialog and capability-grant review.
//!
//! Two steps in one dialog. Choosing a `.tlpx` and pressing Upload stages it on
//! the server and shows what it asks for. Approving echoes the reviewed hash and
//! capability names back, so the server can refuse a package that changed in
//! between. Nothing is installed before that.
use crate::{
    app::components::{AppIcon, Chip, ContentDialog},
    hooks::use_service_context,
    i18n::{use_translation, YewI18n},
    model::{DialogAction, DialogActions, DialogResult},
    services::PluginActionError,
};
use shared::model::{PluginApproveDto, PluginRpcKindDto, PluginSlotDto, PluginUploadReviewDto};
use web_sys::{File, HtmlInputElement};
use yew::{classes, component, html, platform::spawn_local, use_state, Callback, Event, Html, Properties, TargetCast};

#[derive(Properties, Clone, PartialEq)]
pub struct PluginUploadProps {
    pub on_close: Callback<()>,
    /// Called after a plugin was approved and installed, so the list can refresh.
    pub on_installed: Callback<()>,
    /// The review of an update already waiting in staging: the dialog opens on it
    /// instead of on the file picker.
    #[prop_or_default]
    pub initial_review: Option<PluginUploadReviewDto>,
}

/// The `network` capability needs a warning of its own.
const NETWORK: &str = "network";

/// What approval sends back: the reviewed hash and capability names, and the
/// MCP switch only when the plugin asked for it and the admin ticked it.
fn approve_body(review: &PluginUploadReviewDto, expose_mcp: bool) -> PluginApproveDto {
    PluginApproveDto {
        wasm_sha256: review.wasm_sha256.clone(),
        capabilities: review.capabilities.iter().map(|capability| capability.name.clone()).collect(),
        mcp: expose_mcp && review.mcp_requested,
    }
}

/// The wire name of a slot (`menu`, `config-tab`, ...).
fn slot_name(slot: PluginSlotDto) -> String {
    serde_json::to_value(slot).ok().and_then(|name| name.as_str().map(ToString::to_string)).unwrap_or_default()
}

/// Whether an update review marks capability `name` as new: the installed
/// approval does not cover it.
fn is_added_capability(review: &PluginUploadReviewDto, name: &str) -> bool {
    review.capability_diff.as_ref().is_some_and(|diff| diff.added.iter().any(|added| added == name))
}

fn render_capabilities(translate: &YewI18n, review: &PluginUploadReviewDto) -> Html {
    html! {
        <section class="tp__plugin-upload__section">
            <h3>{ translate.t("LABEL.PLUGIN_CAPABILITIES") }</h3>
            if review.capabilities.is_empty() {
                <p class="tp__plugins__muted">{ translate.t("LABEL.PLUGIN_NO_CAPABILITIES") }</p>
            } else {
                <ul class="tp__plugin-upload__capabilities">
                    { for review.capabilities.iter().map(|capability| {
                        let added = is_added_capability(review, &capability.name);
                        html! {
                            <li key={capability.name.clone()}
                                class={classes!(added.then_some("tp__plugin-upload__capability--added"))}>
                                if added {
                                    <span class="tp__plugin-upload__badge">
                                        { translate.t("LABEL.PLUGIN_CAPABILITY_ADDED") }
                                    </span>{ " " }
                                }
                                <strong>{ capability.name.clone() }</strong>{ ": " }{ capability.description.clone() }
                            </li>
                        }
                    }) }
                </ul>
            }
        </section>
    }
}

/// What an update changes in the approval besides the added capabilities:
/// the ones no longer asked for, and that MCP exposure starts off again.
fn render_capability_diff(translate: &YewI18n, review: &PluginUploadReviewDto) -> Html {
    let Some(diff) = review.capability_diff.as_ref() else {
        return html! {};
    };
    html! {
        <>
            if !diff.removed.is_empty() {
                <p class="tp__plugin-upload__removed">
                    { format!("{}: {}", translate.t("LABEL.PLUGIN_CAPABILITIES_REMOVED"), diff.removed.join(", ")) }
                </p>
            }
            if diff.mcp_was_approved {
                <p class="tp__plugins__muted">{ translate.t("MESSAGES.PLUGIN_UPDATE_MCP_RESET") }</p>
            }
        </>
    }
}

/// The unrestricted-network warning, prominent and ahead of everything else.
fn render_network_warning(translate: &YewI18n, review: &PluginUploadReviewDto) -> Html {
    let Some(network) = review.capabilities.iter().find(|capability| capability.name == NETWORK) else {
        return html! {};
    };
    html! {
        <div class="tp__plugin-upload__warning tp__plugin-upload__warning--network" role="alert">
            <AppIcon name="Warning"/>
            <div>
                <strong>{ translate.t("LABEL.PLUGIN_NETWORK_WARNING_TITLE") }</strong>
                <p>{ network.description.clone() }</p>
            </div>
        </div>
    }
}

fn render_custom_ui(translate: &YewI18n, review: &PluginUploadReviewDto) -> Html {
    if !review.custom_ui {
        return html! {};
    }
    html! {
        <section class="tp__plugin-upload__section">
            <div class="tp__plugin-upload__warning tp__plugin-upload__warning--ui" role="alert">
                <AppIcon name="Warning"/>
                <div>
                    <strong>{ translate.t("LABEL.PLUGIN_CUSTOM_UI") }</strong>
                    <p>{ translate.t("MESSAGES.PLUGIN_CUSTOM_UI_WARNING") }</p>
                </div>
            </div>
            if !review.contributions.is_empty() {
                <ul>
                    { for review.contributions.iter().map(|contribution| html! {
                        <li key={contribution.element.clone()}>
                            { contribution.title.clone() }{ " " }
                            <code>{ slot_name(contribution.slot) }</code>
                        </li>
                    }) }
                </ul>
            }
        </section>
    }
}

/// The i18n key of an rpc export kind.
fn rpc_kind_label_key(kind: PluginRpcKindDto) -> &'static str {
    match kind {
        PluginRpcKindDto::Read => "LABEL.PLUGIN_RPC_READ",
        PluginRpcKindDto::Write => "LABEL.PLUGIN_RPC_WRITE",
    }
}

fn render_rpc_exports(translate: &YewI18n, review: &PluginUploadReviewDto) -> Html {
    if review.rpc_exports.is_empty() {
        return html! {};
    }
    html! {
        <section class="tp__plugin-upload__section">
            <h3>{ translate.t("LABEL.PLUGIN_RPC_EXPORTS") }</h3>
            <ul>
                { for review.rpc_exports.iter().map(|export| html! {
                    <li key={export.name.clone()}>
                        <code>{ export.name.clone() }</code>{ " " }
                        { translate.t(rpc_kind_label_key(export.kind)) }
                        if let Some(description) = export.description.as_ref() {
                            { ": " }{ description.clone() }
                        }
                    </li>
                }) }
            </ul>
        </section>
    }
}

fn render_review(
    translate: &YewI18n,
    review: &PluginUploadReviewDto,
    expose_mcp: bool,
    on_mcp: &Callback<Event>,
) -> Html {
    html! {
        <>
            <div class="tp__plugin-upload__title">
                <h3>{ review.name.clone() }</h3>
                <span class="tp__plugins__version">{ format!("{} {}", translate.t("LABEL.VERSION"), review.version) }</span>
                <code>{ review.id.clone() }</code>
            </div>
            if let Some(replaces) = review.replaces.as_ref() {
                <p class="tp__plugin-upload__replaces">
                    { format!("{} {replaces}", translate.t("LABEL.PLUGIN_REPLACES")) }
                </p>
                <p class="tp__plugins__muted">{ translate.t("MESSAGES.PLUGIN_UPDATE_KEEPS_PRIOR") }</p>
            }
            if let Some(description) = review.description.as_ref() {
                <p class="tp__plugins__description">{ description.clone() }</p>
            }
            <p class="tp__plugins__muted">
                if let Some(author) = review.author.as_ref() {
                    { format!("{}: {author}", translate.t("LABEL.PLUGIN_AUTHOR")) }{ " " }
                }
                if let Some(homepage) = review.homepage.as_ref() {
                    // shown as text on purpose: a manifest-supplied URL is never made a link here
                    { format!("{}: {homepage}", translate.t("LABEL.PLUGIN_HOMEPAGE")) }
                }
            </p>
            { render_network_warning(translate, review) }
            { render_capabilities(translate, review) }
            { render_capability_diff(translate, review) }
            { render_custom_ui(translate, review) }
            { render_rpc_exports(translate, review) }
            if !review.secrets.is_empty() {
                <section class="tp__plugin-upload__section">
                    <h3>{ translate.t("LABEL.PLUGIN_SECRETS") }</h3>
                    <p>{ translate.t("MESSAGES.PLUGIN_SECRETS_HINT") }</p>
                    <div class="tp__plugins__chips">
                        { for review.secrets.iter().map(|path| html! { <Chip key={path.clone()} label={path.clone()} /> }) }
                    </div>
                </section>
            }
            if !review.hooks.is_empty() {
                <section class="tp__plugin-upload__section">
                    <h3>{ translate.t("LABEL.PLUGIN_HOOKS") }</h3>
                    <div class="tp__plugins__chips">
                        { for review.hooks.iter().map(|hook| html! { <Chip key={hook.clone()} label={hook.clone()} /> }) }
                    </div>
                </section>
            }
            if !review.warnings.is_empty() {
                <ul class="tp__plugin-upload__notes">
                    { for review.warnings.iter().map(|warning| html! {
                        <li><AppIcon name="Warning"/>{ " " }{ warning.clone() }</li>
                    }) }
                </ul>
            }
            if review.mcp_requested {
                // separate from the approval, and off until the admin ticks it
                <label class="tp__plugin-upload__mcp">
                    <input type="checkbox" checked={expose_mcp} onchange={on_mcp.clone()} />
                    { " " }{ translate.t("LABEL.PLUGIN_EXPOSE_MCP") }
                </label>
            }
        </>
    }
}

/// The install dialog: pick a `.tlpx`, review what it asks for, approve.
#[component]
pub fn PluginUpload(props: &PluginUploadProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();

    let file = use_state(|| None::<File>);
    let review = use_state(|| props.initial_review.clone());
    let busy = use_state(|| false);
    let error = use_state(|| None::<PluginActionError>);
    let expose_mcp = use_state(|| false);

    let on_file = {
        let file = file.clone();
        let error = error.clone();
        Callback::from(move |event: Event| {
            let input: HtmlInputElement = event.target_unchecked_into();
            file.set(input.files().and_then(|files| files.get(0)));
            error.set(None);
        })
    };
    let on_mcp = {
        let expose_mcp = expose_mcp.clone();
        Callback::from(move |event: Event| {
            let input: HtmlInputElement = event.target_unchecked_into();
            expose_mcp.set(input.checked());
        })
    };

    let upload = {
        let services = services.clone();
        let file = file.clone();
        let review = review.clone();
        let busy = busy.clone();
        let error = error.clone();
        Callback::from(move |()| {
            let Some(selected) = (*file).clone() else {
                return;
            };
            if *busy {
                return;
            }
            busy.set(true);
            let services = services.clone();
            let review = review.clone();
            let busy = busy.clone();
            let error = error.clone();
            spawn_local(async move {
                match services.plugins.upload(&selected).await {
                    Ok(dto) => {
                        review.set(Some(dto));
                        error.set(None);
                    }
                    Err(err) => error.set(Some(err)),
                }
                busy.set(false);
            });
        })
    };

    let approve = {
        let services = services.clone();
        let translate = translate.clone();
        let review = review.clone();
        let busy = busy.clone();
        let expose_mcp = expose_mcp.clone();
        let on_installed = props.on_installed.clone();
        let on_close = props.on_close.clone();
        Callback::from(move |()| {
            let Some(reviewed) = (*review).clone() else {
                return;
            };
            if *busy {
                return;
            }
            busy.set(true);
            let body = approve_body(&reviewed, *expose_mcp);
            let services = services.clone();
            let translate = translate.clone();
            let busy = busy.clone();
            let on_installed = on_installed.clone();
            let on_close = on_close.clone();
            spawn_local(async move {
                match services.plugins.approve(&reviewed.id, &body).await {
                    Ok(summary) => {
                        services.toastr.success(format!(
                            "{}: {}",
                            translate.t("MESSAGES.PLUGIN_INSTALLED"),
                            summary.name
                        ));
                        on_installed.emit(());
                        on_close.emit(());
                    }
                    Err(err) => services.toastr.error(err.message()),
                }
                busy.set(false);
            });
        })
    };

    let on_result = {
        let on_close = props.on_close.clone();
        let upload = upload.clone();
        let approve = approve.clone();
        let reviewing = review.is_some();
        Callback::from(move |result: DialogResult| match result {
            DialogResult::Cancel => on_close.emit(()),
            DialogResult::Ok if reviewing => approve.emit(()),
            DialogResult::Ok => upload.emit(()),
        })
    };

    let primary = if review.is_some() {
        DialogAction::new(
            "approve",
            "LABEL.PLUGIN_APPROVE_INSTALL",
            DialogResult::Ok,
            Some("Save".to_owned()),
            Some("primary".to_owned()),
        )
        .with_disabled(*busy)
    } else {
        DialogAction::new(
            "upload",
            "LABEL.PLUGIN_UPLOAD",
            DialogResult::Ok,
            Some("Add".to_owned()),
            Some("primary".to_owned()),
        )
        .with_disabled(*busy || file.is_none())
    };
    let actions = DialogActions {
        left: Some(vec![DialogAction::new(
            "cancel",
            "LABEL.CANCEL",
            DialogResult::Cancel,
            Some("Close".to_owned()),
            Some("secondary".to_owned()),
        )]),
        right: vec![primary],
    };

    let body = if let Some(reviewed) = review.as_ref() {
        render_review(&translate, reviewed, *expose_mcp, &on_mcp)
    } else {
        html! {
            <>
                <p>{ translate.t("MESSAGES.PLUGIN_UPLOAD_HINT") }</p>
                // no Content-Type is set anywhere: the browser derives the multipart boundary itself
                <input type="file" accept=".tlpx" aria-label={translate.t("LABEL.PLUGIN_FILE")} onchange={on_file} />
                if let Some(err) = error.as_ref() {
                    <div class="tp__plugin-upload__error tp__input-error" role="alert">
                        <strong>{ translate.t("MESSAGES.PLUGIN_UPLOAD_FAILED") }</strong>{ " " }{ err.error.clone() }
                        if !err.problems.is_empty() {
                            <ul>{ for err.problems.iter().map(|problem| html! { <li>{ problem.clone() }</li> }) }</ul>
                        }
                    </div>
                }
            </>
        }
    };

    let content = html! {
        <div class="tp__plugin-upload">
            <h2>{ translate.t("LABEL.PLUGIN_INSTALL") }</h2>
            <div class="tp__plugin-upload__body">{ body }</div>
        </div>
    };

    html! {
        <ContentDialog
            content={content}
            actions={actions}
            close_on_backdrop_click={false}
            close_on_confirm={false}
            on_confirm={on_result}
        />
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{PluginCapabilityDiffDto, PluginCapabilityReviewDto};

    fn review(capabilities: &[&str], mcp_requested: bool) -> PluginUploadReviewDto {
        PluginUploadReviewDto {
            id: "demo".to_string(),
            name: "Demo".to_string(),
            version: "1.0.0".to_string(),
            author: None,
            homepage: None,
            description: None,
            abi: "1.0.0".to_string(),
            wasm_sha256: Some("ab".repeat(32)),
            capabilities: capabilities
                .iter()
                .map(|name| PluginCapabilityReviewDto { name: (*name).to_string(), description: String::new() })
                .collect(),
            hooks: Vec::new(),
            events: Vec::new(),
            tasks: Vec::new(),
            secrets: Vec::new(),
            custom_ui: false,
            contributions: Vec::new(),
            rpc_exports: Vec::new(),
            mcp_requested,
            replaces: None,
            capability_diff: None,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn approval_echoes_the_reviewed_hash_and_capability_names() {
        let body = approve_body(&review(&["network", "storage"], false), true);
        assert_eq!(body.wasm_sha256, Some("ab".repeat(32)));
        assert_eq!(body.capabilities, ["network", "storage"]);
    }

    #[test]
    fn mcp_needs_both_the_request_and_the_tick() {
        assert!(!approve_body(&review(&[], true), false).mcp, "unticked stays off");
        assert!(approve_body(&review(&[], true), true).mcp);
        assert!(!approve_body(&review(&[], false), true).mcp, "a plugin that did not ask cannot be exposed");
    }

    #[test]
    fn a_ui_only_package_approves_without_a_hash() {
        let mut ui_only = review(&[], false);
        ui_only.wasm_sha256 = None;
        assert_eq!(approve_body(&ui_only, false).wasm_sha256, None);
    }

    #[test]
    fn only_capabilities_in_the_diff_are_marked_added() {
        let mut update = review(&["network", "storage"], false);
        assert!(!is_added_capability(&update, "network"), "a fresh install marks nothing");
        update.capability_diff = Some(PluginCapabilityDiffDto {
            added: vec!["network".to_string()],
            removed: Vec::new(),
            unchanged: vec!["storage".to_string()],
            mcp_was_approved: false,
        });
        assert!(is_added_capability(&update, "network"));
        assert!(!is_added_capability(&update, "storage"));
    }

    #[test]
    fn an_update_still_echoes_every_capability_not_just_the_added_ones() {
        let mut update = review(&["network", "storage"], false);
        update.capability_diff = Some(PluginCapabilityDiffDto {
            added: vec!["network".to_string()],
            removed: Vec::new(),
            unchanged: vec!["storage".to_string()],
            mcp_was_approved: true,
        });
        assert_eq!(approve_body(&update, false).capabilities, ["network", "storage"]);
        assert!(!approve_body(&update, false).mcp, "the earlier MCP approval is not carried over");
    }

    #[test]
    fn slots_are_shown_by_their_wire_names() {
        assert_eq!(slot_name(PluginSlotDto::ConfigTab), "config-tab");
        assert_eq!(slot_name(PluginSlotDto::Menu), "menu");
    }
}
