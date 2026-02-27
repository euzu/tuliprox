use crate::{error::TuliproxError, info_err_res, model::EpgSmartMatchConfigDto, utils::is_false};

const AUTO_URL: &str = "auto";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EpgSourceDto {
    pub url: String,
    #[serde(default)]
    pub priority: i16,
    #[serde(default, skip_serializing_if = "is_false")]
    pub logo_override: bool,
}

impl EpgSourceDto {
    pub fn prepare(&mut self) { self.url = self.url.trim().to_string(); }

    pub fn is_valid(&self) -> bool { !self.url.is_empty() }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Default)]
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
    /// Prepares the EPG configuration by resolving all source URLs into `t_sources`.
    ///
    /// - `create_auto_url` — closure that derives an XMLTV URL from the parent input
    ///   (called when `url` is `auto`).
    /// - `include_computed` — when `false` the resolution is skipped (used for serialisation
    ///   round-trips that do not need fully-resolved URLs).
    pub fn prepare<F>(&mut self, create_auto_url: F, include_computed: bool) -> Result<(), TuliproxError>
    where
        F: Fn() -> Result<String, String>,
    {
        if include_computed {
            self.t_sources = Vec::new();
            if let Some(epg_sources) = self.sources.as_mut() {
                for epg_source in epg_sources.iter_mut() {
                    epg_source.prepare();
                    if !epg_source.is_valid() {
                        continue;
                    }

                    if epg_source.url.eq_ignore_ascii_case(AUTO_URL) {
                        match create_auto_url() {
                            Ok(provider_url) => {
                                self.t_sources.push(EpgSourceDto {
                                    url: provider_url,
                                    priority: epg_source.priority,
                                    logo_override: epg_source.logo_override,
                                });
                            }
                            Err(err) => return info_err_res!("{err}"),
                        }
                    } else {
                        self.t_sources.push(epg_source.clone());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_url_passthrough() {
        let mut cfg = EpgConfigDto {
            sources: Some(vec![EpgSourceDto {
                url: "http://example.com/xmltv.php".to_owned(),
                priority: 0,
                logo_override: false,
            }]),
            ..Default::default()
        };
        cfg.prepare(|| Err("no auto".to_owned()), true).expect("prepare failed");
        assert_eq!(cfg.t_sources.len(), 1);
        assert_eq!(cfg.t_sources[0].url, "http://example.com/xmltv.php");
    }

    #[test]
    fn test_provider_scheme_kept_unresolved() {
        let mut cfg = EpgConfigDto {
            sources: Some(vec![EpgSourceDto {
                url: "provider://myprovider/xmltv.php?username=u&password=p".to_owned(),
                priority: 1,
                logo_override: true,
            }]),
            ..Default::default()
        };
        cfg.prepare(|| Err("no auto".to_owned()), true).expect("prepare failed");
        assert_eq!(cfg.t_sources.len(), 1);
        assert_eq!(cfg.t_sources[0].url, "provider://myprovider/xmltv.php?username=u&password=p");
        assert_eq!(cfg.t_sources[0].priority, 1);
        assert!(cfg.t_sources[0].logo_override);
    }

    #[test]
    fn test_auto_url_used() {
        let mut cfg = EpgConfigDto {
            sources: Some(vec![EpgSourceDto { url: AUTO_URL.to_owned(), priority: 0, logo_override: false }]),
            ..Default::default()
        };
        cfg.prepare(|| Ok("http://auto.example.com/xmltv.php?username=u&password=p".to_owned()), true)
            .expect("prepare failed");
        assert_eq!(cfg.t_sources.len(), 1);
        assert!(cfg.t_sources[0].url.starts_with("http://auto.example.com/"));
    }

    #[test]
    fn test_include_computed_false_skips_resolution() {
        let mut cfg = EpgConfigDto {
            sources: Some(vec![EpgSourceDto {
                url: "provider://myprovider/xmltv.php".to_owned(),
                priority: 0,
                logo_override: false,
            }]),
            ..Default::default()
        };
        cfg.prepare(|| Err("no auto".to_owned()), false).expect("prepare with include_computed=false should succeed");
        assert!(cfg.t_sources.is_empty());
    }
}
