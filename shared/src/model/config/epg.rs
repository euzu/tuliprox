use crate::{info_err};
use crate::error::{TuliproxError, TuliproxErrorKind};
use crate::model::EpgSmartMatchConfigDto;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpgSourceDto {
    pub url: String,
    #[serde(default)]
    pub priority: i16,
    #[serde(default)]
    pub logo_override: bool,
}

impl EpgSourceDto {
    pub fn prepare(&mut self) {
        self.url = self.url.trim().to_string();
    }

    pub fn is_valid(&self) -> bool {
        !self.url.is_empty()
    }
}



#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpgConfigDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<EpgSourceDto>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smart_match: Option<EpgSmartMatchConfigDto>,
    #[serde(skip)]
    pub t_sources: Vec<EpgSourceDto>,
}

impl EpgConfigDto {
    pub fn prepare<F>(&mut self, create_auto_url: F, include_computed: bool) -> Result<(), TuliproxError>
    where
        F: Fn() -> Result<String, String>,
    {
        if include_computed {
            self.t_sources = Vec::new();
            if let Some(epg_sources) = self.sources.as_mut() {
                for epg_source in epg_sources {
                    epg_source.prepare();
                    if epg_source.is_valid() {
                        if include_computed && epg_source.url.eq_ignore_ascii_case("auto") {
                            let auto_url = create_auto_url();
                            match auto_url {
                                Ok(provider_url) => {
                                    self.t_sources.push(EpgSourceDto {
                                        url: provider_url,
                                        priority: epg_source.priority,
                                        logo_override: epg_source.logo_override,
                                    });
                                }
                                Err(err) => return Err(info_err!(err))
                            }
                        } else {
                            self.t_sources.push(epg_source.clone());
                        }
                    }
                }
            }

            if let Some(smart_match) = self.smart_match.as_mut() {
                smart_match.prepare()?;
            }
        }
        Ok(())
    }
}