use std::fmt::Display;
use crate::error::{TuliproxError, TuliproxErrorKind};
use crate::foundation::filter::{apply_templates_to_pattern, apply_templates_to_pattern_single};
use crate::model::{ItemField, PatternTemplate, TemplateValue};
use crate::{create_tuliprox_error, handle_tuliprox_error_result_list};
use regex::Regex;

fn compile_regex_vec(patterns: Option<&Vec<String>>) -> Result<Option<Vec<Regex>>, TuliproxError> {
    patterns.as_ref()
        .map(|seq| {
            seq.iter()
                .map(|s| Regex::new(s).map_err(|err| {
                    create_tuliprox_error!(TuliproxErrorKind::Info, "cant parse regex: {s} {err}")
                }))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose() // convert Option<Result<...>> to Result<Option<...>>
}

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub enum SortOrder {
    #[serde(rename = "asc")]
    Asc,
    #[serde(rename = "desc")]
    Desc,
}

impl Display for SortOrder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let str = match self {
            SortOrder::Asc => "asc".to_string(),
            SortOrder::Desc => "desc".to_string(),
        };
        write!(f, "{str}")
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSortGroupDto {
    pub order: SortOrder,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<Vec<String>>,
    #[serde(skip)]
    pub t_sequence: Option<Vec<Regex>>,
}

impl PartialEq for ConfigSortGroupDto {
    fn eq(&self, other: &Self) -> bool {
        self.order == other.order
            && self.sequence == other.sequence
    }
}

impl ConfigSortGroupDto {
    pub fn prepare(&mut self, templates: Option<&Vec<PatternTemplate>>) -> Result<(), TuliproxError> {
        let processed_sequence = match (&self.sequence, templates) {
            (Some(seqs), Some(_templs)) => {
                let mut result = Vec::new();
                for s in seqs {
                    match apply_templates_to_pattern(s, templates, true)? {
                        TemplateValue::Single(val) => result.push(val),
                        TemplateValue::Multi(vals) => result.extend(vals),
                    }
                }
                Some(result)
            }
            (Some(seqs), None) => Some(seqs.clone()),
            (None, _) => None,
        };

        self.t_sequence = compile_regex_vec(processed_sequence.as_ref())?;
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSortChannelDto {
    // channel field
    pub field: ItemField,
    // match against group title
    pub group_pattern: String,
    pub order: SortOrder,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<Vec<String>>,
    #[serde(skip)]
    pub t_sequence: Option<Vec<Regex>>,
}

impl PartialEq for ConfigSortChannelDto {
    fn eq(&self, other: &Self) -> bool {
        self.field == other.field
            && self.group_pattern == other.group_pattern
            && self.order == other.order
            && self.sequence == other.sequence
    }
}


impl ConfigSortChannelDto {
    pub fn prepare(&mut self, templates: Option<&Vec<PatternTemplate>>) -> Result<(), TuliproxError> {
        self.group_pattern = apply_templates_to_pattern_single(&self.group_pattern, templates)?;
        // Compile group_pattern
        Regex::new(&self.group_pattern).map_err(|err| {
            create_tuliprox_error!(TuliproxErrorKind::Info, "cant parse regex: {} {err}", &self.group_pattern)
        })?;

        // Transform sequence with templates if provided, otherwise use raw sequence
        let processed_sequence = match (&self.sequence, templates) {
            (Some(seqs), Some(tmpls)) => {
                let mut result = Vec::new();
                for s in seqs {
                    match apply_templates_to_pattern(s, Some(tmpls), true)? {
                        TemplateValue::Single(val) => result.push(val),
                        TemplateValue::Multi(vals) => result.extend(vals),
                    }
                }
                Some(result)
            }
            (Some(seqs), None) => Some(seqs.clone()),
            (None, _) => None,
        };

        // Compile regex patterns
        self.t_sequence = compile_regex_vec(processed_sequence.as_ref())?;
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigSortDto {
    #[serde(default)]
    pub match_as_ascii: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups: Option<ConfigSortGroupDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<Vec<ConfigSortChannelDto>>,
}


impl ConfigSortDto {
    pub fn prepare(&mut self, templates: Option<&Vec<PatternTemplate>>) -> Result<(), TuliproxError> {
        if let Some(group) = self.groups.as_mut() {
            group.prepare(templates)?;
        }
        if let Some(channels) = self.channels.as_mut() {
            handle_tuliprox_error_result_list!(TuliproxErrorKind::Info, channels.iter_mut().map(|csc| csc.prepare(templates)));
        }
        Ok(())
    }
}