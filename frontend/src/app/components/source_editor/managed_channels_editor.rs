use crate::{
    app::components::{Card, DropDownOption, DropDownSelection, IconButton, Select},
    hooks::use_service_context,
    i18n::use_translation,
};
use shared::model::{ManagedChannelBinding, ManagedChannelStage, PluginRef, PluginStateDto, PluginSummaryDto};
use std::rc::Rc;
use yew::{component, html, platform::spawn_local, use_effect_with, use_state, Callback, Html, Properties};

pub const MANAGED_CHANNELS_HOOK: &str = "playlist.managed-channels";

/// Returns `true` if the plugin is enabled and declares the `playlist.managed-channels` hook.
pub fn is_managed_channel_plugin(plugin: &PluginSummaryDto) -> bool {
    plugin.state == PluginStateDto::Enabled
        && plugin.enabled
        && plugin.hooks.iter().any(|hook| hook == MANAGED_CHANNELS_HOOK)
}

/// Checks whether managed channels should be available for the form:
/// at least one enabled plugin provides the `playlist.managed-channels` hook OR
/// the form already has managed-channel bindings.
pub fn is_managed_channel_available(plugins: &[PluginSummaryDto], has_bindings: bool) -> bool {
    has_bindings || plugins.iter().any(is_managed_channel_plugin)
}

#[derive(Properties, Clone, PartialEq)]
pub struct ManagedChannelsEditorProps {
    pub bindings: Vec<ManagedChannelBinding>,
    #[prop_or(false)]
    pub target: bool,
    #[prop_or(true)]
    pub allow_write: bool,
    pub on_change: Callback<Vec<ManagedChannelBinding>>,
}

#[component]
pub fn ManagedChannelsEditor(props: &ManagedChannelsEditorProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let plugins = use_state(Vec::<PluginSummaryDto>::new);
    {
        let plugins = plugins.clone();
        let services = services.clone();
        use_effect_with(props.allow_write, move |allowed| {
            if *allowed {
                spawn_local(async move {
                    if let Ok(list) = services.plugins.list().await {
                        plugins.set(list);
                    }
                });
            }
        });
    }
    let available = Rc::new(
        plugins
            .iter()
            .filter(|plugin| {
                is_managed_channel_plugin(plugin)
                    && !props.bindings.iter().any(|binding| binding.plugin.id() == plugin.id)
            })
            .map(|plugin| DropDownOption::new(&plugin.id, html! {{&plugin.name}}, false))
            .collect::<Vec<_>>(),
    );
    let add = {
        let bindings = props.bindings.clone();
        let changed = props.on_change.clone();
        Callback::from(move |(_, selection): (String, DropDownSelection)| {
            if let DropDownSelection::Single(id) = selection {
                if let Ok(plugin) = PluginRef::new(&id) {
                    let mut next = bindings.clone();
                    next.push(ManagedChannelBinding { plugin, stage: ManagedChannelStage::AfterFilter });
                    changed.emit(next);
                }
            }
        })
    };
    html! {
        <Card class="tp__config-view__card">
            <h3>{translate.t("LABEL.MANAGED_CHANNELS")}</h3>
            <p>{translate.t(if props.target {"MANAGED_CHANNELS.TARGET_HINT"} else {"MANAGED_CHANNELS.INPUT_HINT"})}</p>
            {for props.bindings.iter().enumerate().map(|(index,binding)| {
                let bindings=props.bindings.clone();let changed=props.on_change.clone();
                let remove=Callback::from(move |_| {let mut next=bindings.clone();next.remove(index);changed.emit(next);});
                let mut options=Vec::new();
                for (stage,label) in [(ManagedChannelStage::BeforeFilter,"MANAGED_CHANNELS.BEFORE_FILTER"),(ManagedChannelStage::AfterFilter,"MANAGED_CHANNELS.AFTER_FILTER")] {
                    options.push(DropDownOption::new(stage.as_str(),html!{{translate.t(label)}},binding.stage==stage));
                }
                let bindings=props.bindings.clone();let changed=props.on_change.clone();
                let select=Callback::from(move |(_,selection):(String,DropDownSelection)| {
                    if let DropDownSelection::Single(stage)=selection {
                        let mut next=bindings.clone();next[index].stage=if stage=="before_filter" {ManagedChannelStage::BeforeFilter}else{ManagedChannelStage::AfterFilter};changed.emit(next);
                    }
                });
                html!{<div key={binding.plugin.id().to_string()} class="tp__managed-channels__row">
                    <span>{binding.plugin.id()}</span>
                    {if props.target && props.allow_write {html!{<Select name={format!("{} ({})",translate.t("MANAGED_CHANNELS.POINT"),binding.plugin.id())} options={Rc::new(options)} on_select={select}/>}}
                     else if props.target {html!{<span>{translate.t(if binding.stage==ManagedChannelStage::BeforeFilter {"MANAGED_CHANNELS.BEFORE_FILTER"}else{"MANAGED_CHANNELS.AFTER_FILTER"})}</span>}}
                     else {Html::default()}}
                    {if props.allow_write {html!{<IconButton name={format!("remove-managed-{index}")} icon="Delete" hint={translate.t("LABEL.DELETE")} onclick={remove}/>}}else{Html::default()}}
                </div>}
            })}
            {if props.allow_write && !available.is_empty() {html!{<Select name={translate.t("MANAGED_CHANNELS.ADD_PLUGIN")} options={Rc::new({let mut choices=vec![DropDownOption::new("",html!{{translate.t("MANAGED_CHANNELS.ADD_PLUGIN")}},true)];choices.extend(available.iter().cloned());choices})} on_select={add}/>}}else{Html::default()}}
        </Card>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_plugin(id: &str, enabled: bool, state: PluginStateDto, hooks: &[&str]) -> PluginSummaryDto {
        PluginSummaryDto {
            id: id.to_string(),
            name: id.to_string(),
            version: None,
            description: None,
            category: None,
            state,
            failure: None,
            enabled,
            priority: 0,
            hooks: hooks.iter().map(|hook| (*hook).to_string()).collect(),
            capabilities: Vec::new(),
            abi: String::new(),
            wasm_sha256: None,
            settings_review: None,
            update: None,
            rollback: None,
            origin: None,
            update_policy: Default::default(),
        }
    }

    #[test]
    fn test_managed_channel_availability() {
        // no plugins
        assert!(!is_managed_channel_available(&[], false));

        // plugin without the hook
        let no_hook = sample_plugin("plugin-a", true, PluginStateDto::Enabled, &["playlist.other"]);
        assert!(!is_managed_channel_available(&[no_hook], false));

        // disabled plugin with the hook
        let disabled_not_enabled = sample_plugin("plugin-b", false, PluginStateDto::Enabled, &[MANAGED_CHANNELS_HOOK]);
        assert!(!is_managed_channel_available(&[disabled_not_enabled], false));

        let disabled_state = sample_plugin("plugin-c", true, PluginStateDto::Disabled, &[MANAGED_CHANNELS_HOOK]);
        assert!(!is_managed_channel_available(&[disabled_state], false));

        // enabled plugin with the hook
        let enabled = sample_plugin("plugin-d", true, PluginStateDto::Enabled, &[MANAGED_CHANNELS_HOOK]);
        assert!(is_managed_channel_available(&[enabled], false));

        // no plugin but existing bindings
        assert!(is_managed_channel_available(&[], true));
    }
}
