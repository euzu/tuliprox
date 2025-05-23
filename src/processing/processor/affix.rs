use crate::model::{ConfigInput, InputAffix, valid_property, MAPPER_FIELDS};
use crate::model::{FetchedPlaylist, FieldGetAccessor, FieldSetAccessor, PlaylistItem};
use crate::utils::{debug_if_enabled};

type AffixProcessor<'a> = Box<dyn Fn(&mut PlaylistItem) + 'a>;

fn create_affix_processor(affix: &InputAffix, is_prefix: bool) -> AffixProcessor {
    Box::new(move |channel: &mut PlaylistItem| {
        let header = &mut channel.header;
        let value = header.get_field(affix.field.as_str()).map_or_else(|| String::from(&affix.value), |field_value| if is_prefix {
            format!("{}{field_value}", &affix.value, )
        } else {
            format!("{field_value}{}", , &affix.value)
        });
        debug_if_enabled!("Applying input {}:  {}={}",  if is_prefix {"prefix"} else {"suffix"},  &affix.field, &value);
        header.set_field(&affix.field, value.as_str());
    })
}

fn validate_and_create_affix_processor(affix: Option<&InputAffix>, is_prefix: bool) -> Option<AffixProcessor> {
    if let Some(affix_def) = affix {
        if (valid_property!(&affix_def.field.as_str(), MAPPER_FIELDS) && !affix_def.value.is_empty()) {
            return Some(create_affix_processor(affix_def, is_prefix));
        }
    }
    None
}

fn get_affix_processor(input: &ConfigInput) -> Option<AffixProcessor> {
    if input.suffix.is_some() || input.prefix.is_some() {
        let processors: Vec<AffixProcessor> = vec![
            validate_and_create_affix_processor(input.prefix.as_ref(), true),
            validate_and_create_affix_processor(input.suffix.as_ref(), false)
        ].into_iter().flatten().collect();

        if !processors.is_empty() {
            let apply_affix: AffixProcessor = Box::new(move |channel: &mut PlaylistItem| {
                for x in &processors {
                    x(channel);
                }
            });
            return Some(apply_affix);
        }
    }
    None
}

pub fn apply_affixes(fetched_playlists: &mut [FetchedPlaylist]) {
    for fetched_playlist in fetched_playlists.iter_mut() {
        let FetchedPlaylist { input, playlistgroups: playlist, epg: _ } = fetched_playlist;
        if let Some(affix_processor) = get_affix_processor(input) {
            for group in playlist.iter_mut() {
                group.channels.iter_mut().for_each(|channel| {
                    affix_processor(channel);
                });
            }
        }
    }
}
