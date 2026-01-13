use crate::error::{TuliproxError, TuliproxErrorKind};
use crate::foundation::filter::{apply_templates_to_pattern, get_filter, Filter};
use crate::model::{ItemField, PatternTemplate, TemplateValue};
use crate::{handle_tuliprox_error_result_list, info_err, info_err_res};
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt::{Display, Formatter};
use std::str::FromStr;
use std::sync::Arc;

fn compile_regex_vec(patterns: Option<&Vec<String>>) -> Result<Option<Vec<Arc<Regex>>>, TuliproxError> {
    patterns.as_ref()
        .map(|seq| {
            seq.iter()
                .map(|s| crate::model::REGEX_CACHE.get_or_compile(s).map_err(|err| {
                    info_err!("can't parse regex: {s} {err}")
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
    #[serde(rename = "none")]
    None,
}

impl SortOrder {
    pub fn as_str(&self) -> &'static str {
        match *self {
            Self::Asc => "asc",
            Self::Desc => "desc",
            Self::None => "none",
        }
    }
}

impl Display for SortOrder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}


#[derive(Serialize, Debug, Copy, Clone, Eq, PartialEq)]
pub enum SortTarget {
    #[serde(rename = "group")]
    Group,
    #[serde(rename = "channel")]
    Channel,
}

impl SortTarget {
    const GROUP: &'static str = "group";
    const CHANNEL: &'static str = "channel";

    pub fn as_str(&self) -> &'static str {
        match *self {
            Self::Group => Self::GROUP,
            Self::Channel => Self::CHANNEL,
        }
    }
}

impl FromStr for SortTarget {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        if s.eq_ignore_ascii_case(Self::GROUP) {
            Ok(Self::Group)
        } else if s.eq_ignore_ascii_case(Self::CHANNEL) {
            Ok(Self::Channel)
        } else {
            info_err_res!("Unknown SortTarget: {}", s)
        }
    }
}

impl<'de> Deserialize<'de> for SortTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        SortTarget::from_str(&s).map_err(serde::de::Error::custom)
    }
}

impl Display for SortTarget {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSortRuleDto {
    pub target: SortTarget,
    // channel/Group field
    pub field: ItemField,
    pub order: SortOrder,
    #[serde(default)]
    pub sequence: Option<Vec<String>>,
    pub filter: String,
    #[serde(skip)]
    pub t_sequence: Option<Vec<Arc<Regex>>>,
    #[serde(skip)]
    pub t_filter: Option<Filter>,
}

impl PartialEq for ConfigSortRuleDto {
    fn eq(&self, other: &Self) -> bool {
        self.field == other.field
            && self.target == other.target
            && self.order == other.order
            && self.sequence == other.sequence
            && self.filter == other.filter
    }
}


impl ConfigSortRuleDto {
    pub fn prepare(&mut self, templates: Option<&Vec<PatternTemplate>>) -> Result<(), TuliproxError> {
        if self.target == SortTarget::Group {
            // What the user sets is not important, we allow this but force to use Group
            if !matches!(self.field, ItemField::Group | ItemField::Title | ItemField::Name | ItemField::Caption) {
                return info_err_res!("Group sorting can only be done on the Group field");
            }
            self.field = ItemField::Group; // hard coded because we only can't match a group until we can use PlaylistGroup with filter
        }

        self.t_filter = Some(get_filter(&self.filter, templates)?);

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
    #[serde(default)]
    pub rules: Vec<ConfigSortRuleDto>,
}

impl ConfigSortDto {
    pub fn prepare(&mut self, templates: Option<&Vec<PatternTemplate>>) -> Result<(), TuliproxError> {
        handle_tuliprox_error_result_list!(TuliproxErrorKind::Info, self.rules.iter_mut().map(|rule| rule.prepare(templates)));
        Ok(())
    }
}
