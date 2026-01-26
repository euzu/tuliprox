use crate::model::WebConfig;
use crate::services::{get_base_href, request_get, request_post, EventService};
use shared::model::{AppConfigDto, ConfigDto, ConfigInputDto, IpCheckDto, LibraryScanRequest,  SourcesConfigDto, TargetOutputDto};
use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use log::error;
use futures_signals::signal::Mutable;
use futures_signals::signal::SignalExt;
use shared::foundation::{get_filter, prepare_templates};
use shared::foundation::MapperScript;
use shared::utils::{concat_path, concat_path_leading_slash};
use crate::error::Error;

pub struct ConfigService {
    pub ui_config: Rc<WebConfig>,
    pub server_config: RefCell<Option<Rc<AppConfigDto>>>,
    config_channel: Mutable<Option<Rc<AppConfigDto>>>,
    is_fetching: AtomicBool,
    config_path: String,
    sources_path: String,
    ip_check_path: String,
    batch_input_content_path: String,
    geoip_path: String,
    library_path: String,
    event_service: Rc<EventService>
}

impl ConfigService {
    pub fn new(config: &WebConfig, event_service: Rc<EventService>) -> Self {
        let base_href = get_base_href();
        let config_path = concat_path_leading_slash(&base_href, "api/v1/config");
        Self {
            ui_config: Rc::new(config.clone()),
            server_config: RefCell::new(None),
            config_channel: Mutable::new(None),
            is_fetching: AtomicBool::new(false),
            config_path: config_path.clone(),
            sources_path: concat_path(&config_path, "sources"),
            ip_check_path: concat_path_leading_slash(&base_href, "api/v1/ipinfo"),
            batch_input_content_path: concat_path_leading_slash(&base_href, "api/v1/config/batchContent"),
            geoip_path: concat_path_leading_slash(&base_href, "api/v1/geoip/update"),
            library_path: concat_path_leading_slash(&base_href, "api/v1/library"),
            event_service
        }
    }

    pub async fn config_subscribe<F, U>(&self, callback: &mut F)
    where
        U: Future<Output=()>,
        F: FnMut(Option<Rc<AppConfigDto>>) -> U,
    {
        let fut = self.config_channel.signal_cloned().for_each(callback);
        fut.await
    }

    pub async fn get_server_config(&self) -> Option<Rc<AppConfigDto>> {
        self.fetch_server_config().await;
        self.server_config.borrow().clone()
    }

    async fn fetch_server_config(&self) {
        if self.is_fetching.swap(true, Ordering::AcqRel) {
            return;
        }
        let result = match request_get::<AppConfigDto>(&self.config_path, None, None).await {
            Ok(Some(mut app_config)) => {
                let templates = {
                    if let Some(templ) = app_config.sources.templates.as_mut() {
                        prepare_templates(templ).ok()
                    }  else {
                        None
                    }
                };

                for source in app_config.sources.sources.iter_mut() {
                    for target in source.targets.iter_mut() {
                        target.t_filter = get_filter(target.filter.as_str(), templates.as_ref()).ok();
                        if let Some(sort) = target.sort.as_mut() {
                            for rule in sort.rules.iter_mut() {
                                rule.t_filter = get_filter(&rule.filter, templates.as_ref()).map_err(|e| error!("Failed to parse sort rule filter: {}", e)).ok();
                            }
                        }
                        for output in target.output.iter_mut() {
                            match output {
                                TargetOutputDto::Xtream(o) =>
                                    o.t_filter = o.filter.as_ref().and_then(|flt| get_filter(flt, templates.as_ref()).map_err(|e| error!("Failed to parse Xtream output filter: {}", e)).ok()),
                                TargetOutputDto::M3u(o) =>
                                    o.t_filter = o.filter.as_ref().and_then(|flt| get_filter(flt, templates.as_ref()).map_err(|e| error!("Failed to parse M3U output filter: {}", e)).ok()),
                                TargetOutputDto::Strm(o) =>
                                    o.t_filter = o.filter.as_ref().and_then(|flt| get_filter(flt, templates.as_ref()).map_err(|e| error!("Failed to parse Strm output filter: {}", e)).ok()),
                                TargetOutputDto::HdHomeRun(_) => {}
                            }
                        }
                    }
                }

                if let Some(mappings) = app_config.mappings.as_mut() {
                    for mapping in mappings.mappings.mapping.iter_mut() {
                        let templates = mapping.templates.as_ref();
                        if let Some(mappers) = mapping.mapper.as_mut() {
                            for mapper in mappers.iter_mut() {
                                mapper.t_filter = get_filter(mapper.filter.as_str(), templates).ok();
                                mapper.t_script =  MapperScript::parse(&mapper.script, templates).ok();
                            }
                        }
                    }

                }

                Some(Rc::new(app_config))
            },
            Ok(None) => Some(Rc::new(AppConfigDto::default())),
            Err(err) => {
                error!("{err}");
                None
            }
        };
        self.server_config.replace(result.clone());
        self.config_channel.set(result);
        self.is_fetching.store(false, Ordering::Release);
    }

    pub async fn get_ip_info(&self) -> Option<IpCheckDto> {
        request_get::<IpCheckDto>(&self.ip_check_path, None, None).await.unwrap_or_else(|err| {
            error!("{err}");
            None
        })
    }

    pub async fn get_batch_input_content(&self, input: &ConfigInputDto) -> Option<String> {
        let id = input.id.to_string();
        let path = concat_path(&self.batch_input_content_path, &id);
        request_get::<String>(&path, None, Some("text/plain".to_owned())).await.unwrap_or_else(|err| {
            error!("{err}");
            None
        })
    }

    pub async fn save_config(&self, dto: ConfigDto) -> Result<(), Error> {
        let path = concat_path(&self.config_path, "main");
        self.event_service.set_config_change_message_blocked(true);
        match request_post::<ConfigDto, ()>(&path, dto, None, None).await {
            Ok(_) => {
                self.event_service.set_config_change_message_blocked(false);
                Ok(())
            },
            Err(err) => {
                self.event_service.set_config_change_message_blocked(false);
                error!("{err}");
                Err(err)
            }
        }
    }

    pub async fn save_sources(&self, dto: SourcesConfigDto) -> Result<(), Error> {
        self.event_service.set_config_change_message_blocked(true);
        match request_post::<SourcesConfigDto, ()>(&self.sources_path, dto, None, None).await {
            Ok(_) => {
                self.event_service.set_config_change_message_blocked(false);
                Ok(())
            }
            Err(err) => {
                self.event_service.set_config_change_message_blocked(false);
                error!("{err}");
                Err(err)
            }
        }
    }

    pub async fn update_geoip(&self) -> Result<Option<()>, Error> {
        request_get::<()>(&self.geoip_path, None, None).await
    }

    pub async fn update_library(&self) -> Result<Option<()>, Error> {
        let path = concat_path(&self.library_path, "scan");
        let params = LibraryScanRequest {
            force_rescan: false,
        };
        request_post::<LibraryScanRequest, ()>(&path, params, None, None).await
    }
}
