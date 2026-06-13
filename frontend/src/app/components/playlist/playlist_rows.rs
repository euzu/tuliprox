use crate::app::components::InputRow;
use shared::model::{ConfigInputDto, ConfigTargetDto, SourcesConfigDto};
use std::{collections::HashMap, rc::Rc, sync::Arc};

pub type PlaylistRows = Vec<(Vec<Rc<InputRow>>, Vec<Rc<ConfigTargetDto>>)>;
pub type ProviderButton = (Arc<str>, u16);

pub fn map_sources_to_playlist_rows(sources: &SourcesConfigDto) -> Rc<PlaylistRows> {
    let mut mapped_sources = vec![];
    let mut inputs_map: HashMap<Arc<str>, Vec<&ConfigInputDto>> = HashMap::with_capacity(sources.inputs.len());
    for input in &sources.inputs {
        inputs_map.entry(input.name.clone()).or_default().push(input);
    }

    for source in &sources.sources {
        let mut inputs = vec![];
        let mut per_source_name_occurrence: HashMap<Arc<str>, usize> = HashMap::new();

        for input_name in &source.inputs {
            let input_cfg = if let Some(candidates) = inputs_map.get(input_name) {
                let name_occurrence = per_source_name_occurrence.entry(input_name.clone()).or_insert(0usize);
                let resolved = candidates.get(*name_occurrence).or_else(|| candidates.last()).copied();
                *name_occurrence += 1;
                resolved
            } else {
                None
            };

            if let Some(input_cfg) = input_cfg {
                let input = Rc::new(input_cfg.clone());
                inputs.push(Rc::new(InputRow::Input(Rc::clone(&input))));
                if let Some(aliases) = input_cfg.aliases.as_ref() {
                    for alias in aliases {
                        inputs.push(Rc::new(InputRow::Alias(Rc::new(alias.clone()), Rc::clone(&input))));
                    }
                }
            } else {
                log::error!("Input '{}' not found in global inputs", input_name);
            }
        }

        let targets = source.targets.iter().map(|target| Rc::new(target.clone())).collect::<Vec<_>>();
        mapped_sources.push((inputs, targets));
    }

    Rc::new(mapped_sources)
}

pub fn collect_provider_buttons(rows: &PlaylistRows) -> Vec<ProviderButton> {
    let mut seen = std::collections::HashSet::new();
    let mut buttons = Vec::new();

    for (inputs, _targets) in rows {
        for row in inputs {
            let InputRow::Input(input) = &**row else {
                continue;
            };
            if input.input_type.is_batch() {
                continue;
            }
            if seen.insert(input.id) {
                buttons.push((input.name.clone(), input.id));
            }
        }
    }

    buttons.sort_by_cached_key(|(name, _id)| name.to_ascii_lowercase());
    buttons
}

#[cfg(test)]
mod tests {
    use super::{collect_provider_buttons, map_sources_to_playlist_rows};
    use crate::app::components::InputRow;
    use shared::{
        model::{ConfigInputAliasDto, ConfigInputDto, ConfigSourceDto, InputType, SourcesConfigDto},
        utils::Internable,
    };

    #[test]
    fn map_sources_to_playlist_rows_resolves_duplicate_input_names_by_occurrence() {
        let sources = SourcesConfigDto {
            inputs: vec![
                ConfigInputDto {
                    id: 10,
                    name: "dup".intern(),
                    input_type: InputType::Xtream,
                    url: "http://one".to_string(),
                    ..Default::default()
                },
                ConfigInputDto {
                    id: 20,
                    name: "dup".intern(),
                    input_type: InputType::Xtream,
                    url: "http://two".to_string(),
                    ..Default::default()
                },
            ],
            sources: vec![ConfigSourceDto { inputs: vec!["dup".intern(), "dup".intern()], targets: vec![] }],
            ..Default::default()
        };

        let rows = map_sources_to_playlist_rows(&sources);
        let ids = rows[0]
            .0
            .iter()
            .filter_map(|row| match &**row {
                InputRow::Input(input) => Some(input.id),
                InputRow::Alias(_, _) => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(ids, vec![10, 20]);
    }

    #[test]
    fn collect_provider_buttons_deduplicates_root_inputs_and_excludes_aliases_and_batches() {
        let sources = SourcesConfigDto {
            inputs: vec![
                ConfigInputDto {
                    id: 1,
                    name: "b1g".intern(),
                    input_type: InputType::Xtream,
                    url: "http://one".to_string(),
                    aliases: Some(vec![ConfigInputAliasDto {
                        id: 11,
                        name: "b1g2".intern(),
                        url: "http://one/2".to_string(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
                ConfigInputDto {
                    id: 2,
                    name: "batch".intern(),
                    input_type: InputType::XtreamBatch,
                    url: "batch://accounts.csv".to_string(),
                    ..Default::default()
                },
            ],
            sources: vec![
                ConfigSourceDto { inputs: vec!["b1g".intern(), "batch".intern()], targets: vec![] },
                ConfigSourceDto { inputs: vec!["b1g".intern()], targets: vec![] },
            ],
            ..Default::default()
        };

        let rows = map_sources_to_playlist_rows(&sources);
        let buttons = collect_provider_buttons(&rows);

        assert_eq!(buttons, vec![("b1g".intern(), 1)]);
    }
}
