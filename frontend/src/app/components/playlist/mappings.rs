use crate::{
    app::components::{
        Accordion, AccordionPanel, ConfigContext, FilterView, MapperCounterView, MapperScriptView, NoContent,
        ToggleSwitch,
    },
    i18n::use_translation,
};
use shared::model::{MapperDto, MappingCounter, MappingDto};
use yew::prelude::*;

#[derive(Properties, PartialEq, Clone)]
pub struct PlaylistMappingsProps {
    pub mappings: Option<Vec<String>>,
}

#[component]
pub fn PlaylistMappings(props: &PlaylistMappingsProps) -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("Config context not found");
    let mappings = {
        let ids = props.mappings.clone();
        use_memo((config_ctx.clone(), ids), |(context, mapping_ids)| match mapping_ids {
            Some(ids) => context.config.as_ref().and_then(|c| c.mappings.as_ref()).map(|mappings_dto| {
                mappings_dto
                    .mappings
                    .mapping
                    .iter()
                    .filter(|m| ids.contains(&m.id))
                    .cloned()
                    .collect::<Vec<MappingDto>>()
            }),
            None => None,
        })
    };

    let render_mapper = |mapper: &MapperDto| {
        html! {
            <div class="tp__playlist-mappings__mapping-mapper-content">
                <FilterView filter={mapper.t_filter.clone()} />
                <MapperScriptView script={mapper.t_script.clone()} pretty={true}/>
            </div>
        }
    };

    let render_counter = |counter: &MappingCounter| {
        html! {
            <div class="tp__playlist-mappings__mapper-counter">
                <FilterView filter={counter.filter.clone()} />
                <MapperCounterView counter={counter.clone()} pretty={true}/>
            </div>
        }
    };

    let render_mapping = |mapping: &MappingDto| {
        html! {
            <div class="tp__playlist-mappings__mapping">
                <div class="tp__playlist-mappings__mapping-section">
                    <label>{translate.t("LABEL.ID")}</label>
                    {mapping.id.clone()}
                </div>
                <div class="tp__playlist-mappings__mapping-section">
                    <label>{translate.t("LABEL.MATCH_AS_ASCII")}</label>
                    <ToggleSwitch value={mapping.match_as_ascii} readonly={true} compact={true} />
                </div>
                <Accordion default_panel={None::<String>}>
                <div class="tp__playlist-mappings__list">
                    for (idx, mapper) in mapping.mapper.iter().flatten().enumerate() {
                        <AccordionPanel id={format!("script-{}", idx+1)} title={format!("{}-{}", translate.t("LABEL.SCRIPT"), idx+1)} >
                            { render_mapper(mapper) }
                        </AccordionPanel>
                    }
                </div>
                 <div class="tp__playlist-mappings__list">
                    for (idx, counter) in mapping.t_counter.iter().flatten().enumerate() {
                        <AccordionPanel id={format!("counter-{}", idx+1)} title={format!("{}-{}", translate.t("LABEL.COUNTER"), idx+1)} >
                            { render_counter(counter) }
                        </AccordionPanel>
                    }
                </div>
                </Accordion>
            </div>
        }
    };

    html! {
      <div class="tp__playlist-mappings">
        {
             match (*mappings).as_ref() {
                Some(vec) if !vec.is_empty() => html! {
                    for mapping in vec.iter() {
                        { render_mapping(mapping) }
                    }
                },
                _ => html! { <NoContent/> },
            }
        }
      </div>
    }
}
