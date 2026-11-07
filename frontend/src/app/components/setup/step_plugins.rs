use super::setup_helpers::{move_to_next_step, move_to_previous_step};
use crate::{
    app::components::{Card, PluginSlot, SetupContext, SetupStep, TextButton},
    i18n::use_translation,
};
use shared::model::PluginSlotDto;
use yew::prelude::*;

/// The wizard step plugins contribute to (`setup-wizard-step`): every
/// contribution renders here, in one step. The wizard skips the step when no
/// plugin has one.
#[component]
pub fn PluginsStep() -> Html {
    let setup_ctx = use_context::<SetupContext>().expect("Setup context not found");
    let translate = use_translation();
    let step = SetupStep::Plugins;

    let handle_previous = {
        let setup_ctx = setup_ctx.clone();
        Callback::from(move |_| move_to_previous_step(&setup_ctx, step))
    };
    let handle_next = {
        let setup_ctx = setup_ctx.clone();
        Callback::from(move |_| move_to_next_step(&setup_ctx, step))
    };
    let next_title = setup_ctx.next_step(step).map_or_else(
        || translate.t("SETUP.LABEL.NEXT"),
        |next| format!("{}: {}", translate.t("SETUP.LABEL.NEXT"), translate.t(next.title_key())),
    );

    html! {
        <div class="tp__setup__step tp__setup__step-plugins">
            <Card>
                <div class="tp__config-view__header">
                    <h1>{translate.t(step.title_key())}</h1>
                </div>
                <div class="tp__config-view__body">
                    <div class="tp__webui-config-view__info tp__config-view-page__info">
                        <span class="info">
                            {format!(
                                "{} {}/{}: {}",
                                translate.t("SETUP.LABEL.STEP"),
                                setup_ctx.step_position(step),
                                setup_ctx.step_total(),
                                translate.t(step.description_key())
                            )}
                        </span>
                    </div>
                    <PluginSlot slot={PluginSlotDto::SetupWizardStep} />
                </div>
                <div class="tp__config-view__toolbar tp__form-page__toolbar">
                    <TextButton
                        class="secondary"
                        name="setup_plugins_previous"
                        icon="ArrowLeft"
                        title={translate.t("SETUP.LABEL.BACK")}
                        onclick={handle_previous}
                    />
                    <TextButton
                        class="primary"
                        name="setup_plugins_next"
                        icon="ArrowRight"
                        title={next_title}
                        onclick={handle_next}
                    />
                </div>
            </Card>
        </div>
    }
}
