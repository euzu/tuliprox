use super::StagedTargetFilterDto;
use crate::{
    error::TuliproxError,
    foundation::{get_filter, Filter},
    model::PatternTemplate,
};

#[derive(Debug, Clone, Default)]
pub struct ConfigTargetFilterDto {
    pub processing: Option<String>,
    pub persist: Option<String>,
    pub t_processing: Option<Filter>,
    pub t_persist: Option<Filter>,
}

impl PartialEq for ConfigTargetFilterDto {
    fn eq(&self, other: &Self) -> bool { self.processing == other.processing && self.persist == other.persist }
}

impl From<String> for ConfigTargetFilterDto {
    fn from(processing: String) -> Self { Self { processing: Some(processing), ..Self::default() } }
}

impl From<&str> for ConfigTargetFilterDto {
    fn from(processing: &str) -> Self { processing.to_string().into() }
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum TargetFilterRepr {
    Processing(String),
    Staged(StagedTargetFilterDto),
}

impl<'de> serde::Deserialize<'de> for ConfigTargetFilterDto {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match <TargetFilterRepr as serde::Deserialize>::deserialize(deserializer)? {
            TargetFilterRepr::Processing(processing) => Ok(processing.into()),
            TargetFilterRepr::Staged(staged) => {
                if staged.processing.is_none() && staged.persist.is_none() {
                    return Err(serde::de::Error::custom("staged target filter requires at least one stage"));
                }
                Ok(Self { processing: staged.processing, persist: staged.persist, ..Self::default() })
            }
        }
    }
}

impl ConfigTargetFilterDto {
    pub const fn is_empty(&self) -> bool { self.processing.is_none() && self.persist.is_none() }

    pub(super) fn prepare(&mut self, templates: Option<&[PatternTemplate]>) -> Result<(), TuliproxError> {
        fn compile_filter(
            value: Option<&str>,
            templates: Option<&[PatternTemplate]>,
        ) -> Result<Option<Filter>, TuliproxError> {
            value
                .map(str::trim)
                .filter(|filter| !filter.is_empty())
                .map(|filter| get_filter(filter, templates))
                .transpose()
        }

        self.t_processing = compile_filter(self.processing.as_deref(), templates)?;
        self.t_persist = compile_filter(self.persist.as_deref(), templates)?;
        Ok(())
    }
}

impl serde::Serialize for ConfigTargetFilterDto {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        if self.persist.is_none() {
            return serializer.serialize_str(self.processing.as_deref().unwrap_or_default());
        }
        let field_count = usize::from(self.processing.is_some()) + usize::from(self.persist.is_some());
        let mut state = serializer.serialize_struct("ConfigTargetFilterDto", field_count)?;
        if let Some(processing) = self.processing.as_ref() {
            state.serialize_field("processing", processing)?;
        }
        if let Some(persist) = self.persist.as_ref() {
            state.serialize_field("persist", persist)?;
        }
        state.end()
    }
}
