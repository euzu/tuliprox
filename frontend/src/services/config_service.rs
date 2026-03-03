use crate::{
    error::Error,
    model::WebConfig,
    services::{
        get_base_href, request_get, request_get_meta, request_post, request_post_meta, request_put_meta, EventService,
    },
};
use futures_signals::signal::{Mutable, SignalExt};
use log::error;
use shared::{
    foundation::{get_filter, prepare_templates, MapperScript},
    model::{
        ApiProxyConfigDto, AppConfigDto, ConfigDto, ConfigInputDto, IpCheckDto, LibraryScanRequest, SourcesConfigDto,
        TargetOutputDto,
    },
    utils::{
        concat_path, concat_path_leading_slash, HEADER_CONFIG_API_PROXY_REVISION, HEADER_CONFIG_MAIN_REVISION,
        HEADER_CONFIG_SOURCES_REVISION, HEADER_IF_MATCH,
    },
};
use std::{
    cell::RefCell,
    fmt,
    future::Future,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Default, Debug, Clone)]
struct ConfigRevisions {
    main: Option<String>,
    sources: Option<String>,
    api_proxy: Option<String>,
}

#[derive(Clone, serde::Serialize)]
pub struct SetupWebUserCredentialDto {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for SetupWebUserCredentialDto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SetupWebUserCredentialDto")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SetupCompleteRequestDto {
    pub app_config: AppConfigDto,
    pub web_users: Vec<SetupWebUserCredentialDto>,
}

pub struct ConfigService {
    pub ui_config: Rc<WebConfig>,
    pub server_config: RefCell<Option<Rc<AppConfigDto>>>,
    pub api_proxy_config: RefCell<Option<Rc<ApiProxyConfigDto>>>,
    revisions: RefCell<ConfigRevisions>,
    config_channel: Mutable<Option<Rc<AppConfigDto>>>,
    api_proxy_config_channel: Mutable<Option<Rc<ApiProxyConfigDto>>>,
    is_fetching: AtomicBool,
    config_path: String,
    api_proxy_config_path: String,
    sources_path: String,
    ip_check_path: String,
    batch_input_content_path: String,
    geoip_path: String,
    library_path: String,
    setup_complete_path: String,
    event_service: Rc<EventService>,
}

impl ConfigService {
    pub fn new(config: &WebConfig, event_service: Rc<EventService>) -> Self {
        let base_href = get_base_href();
        let config_path = concat_path_leading_slash(&base_href, "api/v1/config");
        Self {
            ui_config: Rc::new(config.clone()),
            server_config: RefCell::new(None),
            api_proxy_config: RefCell::new(None),
            revisions: RefCell::new(ConfigRevisions::default()),
            config_channel: Mutable::new(None),
            api_proxy_config_channel: Mutable::new(None),
            is_fetching: AtomicBool::new(false),
            config_path: config_path.clone(),
            api_proxy_config_path: concat_path(&config_path, "apiproxy"),
            sources_path: concat_path(&config_path, "sources"),
            ip_check_path: concat_path_leading_slash(&base_href, "api/v1/ipinfo"),
            batch_input_content_path: concat_path_leading_slash(&base_href, "api/v1/config/batchContent"),
            geoip_path: concat_path_leading_slash(&base_href, "api/v1/geoip/update"),
            library_path: concat_path_leading_slash(&base_href, "api/v1/library"),
            setup_complete_path: concat_path_leading_slash(&base_href, "api/v1/setup/complete"),
            event_service,
        }
    }

    pub async fn config_subscribe<F, U>(&self, callback: &mut F)
    where
        U: Future<Output = ()>,
        F: FnMut(Option<Rc<AppConfigDto>>) -> U,
    {
        let fut = self.config_channel.signal_cloned().for_each(callback);
        fut.await
    }

    pub async fn api_proxy_config_subscribe<F, U>(&self, callback: &mut F)
    where
        U: Future<Output = ()>,
        F: FnMut(Option<Rc<ApiProxyConfigDto>>) -> U,
    {
        let fut = self.api_proxy_config_channel.signal_cloned().for_each(callback);
        fut.await
    }

    pub async fn get_server_config(&self) -> (Option<Rc<AppConfigDto>>, Option<Rc<ApiProxyConfigDto>>) {
        self.fetch_server_config().await;
        (self.server_config.borrow().clone(), self.api_proxy_config.borrow().clone())
    }

    async fn fetch_server_config(&self) {
        if self.is_fetching.swap(true, Ordering::AcqRel) {
            return;
        }

        let (config_response, api_proxy_response) = futures::join!(
            request_get_meta::<AppConfigDto>(
                &self.config_path,
                None,
                None,
                &[HEADER_CONFIG_MAIN_REVISION, HEADER_CONFIG_SOURCES_REVISION, HEADER_CONFIG_API_PROXY_REVISION,]
            ),
            request_get_meta::<ApiProxyConfigDto>(
                &self.api_proxy_config_path,
                None,
                None,
                &[HEADER_CONFIG_API_PROXY_REVISION]
            )
        );

        let mut revisions = self.revisions.borrow().clone();
        let config_result = match config_response {
            Ok(response) => {
                revisions.main = response.headers.get(HEADER_CONFIG_MAIN_REVISION).cloned();
                revisions.sources = response.headers.get(HEADER_CONFIG_SOURCES_REVISION).cloned();
                if let Some(rev) = response.headers.get(HEADER_CONFIG_API_PROXY_REVISION) {
                    revisions.api_proxy = Some(rev.clone());
                }
                if let Some(mut app_config) = response.body {
                    let templates = {
                        if let Some(template_definition) = app_config.templates.as_mut() {
                            match prepare_templates(&mut template_definition.templates) {
                                Ok(prepared) => Some(prepared),
                                Err(err) => {
                                    error!("Failed to prepare global templates: {err}");
                                    if let Some(templ) = app_config.sources.templates.as_mut() {
                                        match prepare_templates(templ) {
                                            Ok(prepared) => Some(prepared),
                                            Err(fallback_err) => {
                                                error!(
                                                    "Failed to prepare fallback source inline templates: {fallback_err}"
                                                );
                                                None
                                            }
                                        }
                                    } else {
                                        None
                                    }
                                }
                            }
                        } else if let Some(templ) = app_config.sources.templates.as_mut() {
                            match prepare_templates(templ) {
                                Ok(prepared) => Some(prepared),
                                Err(err) => {
                                    error!("Failed to prepare source inline templates: {err}");
                                    None
                                }
                            }
                        } else {
                            None
                        }
                    };

                    for source in app_config.sources.sources.iter_mut() {
                        for target in source.targets.iter_mut() {
                            let prepared_templates = templates.as_deref();
                            target.t_filter = get_filter(target.filter.as_str(), prepared_templates).ok();
                            if let Some(sort) = target.sort.as_mut() {
                                for rule in sort.rules.iter_mut() {
                                    rule.t_filter = get_filter(&rule.filter, prepared_templates)
                                        .map_err(|e| error!("Failed to parse sort rule filter: {}", e))
                                        .ok();
                                }
                            }
                            for output in target.output.iter_mut() {
                                match output {
                                    TargetOutputDto::Xtream(o) => {
                                        o.t_filter = o.filter.as_ref().and_then(|flt| {
                                            get_filter(flt, prepared_templates)
                                                .map_err(|e| error!("Failed to parse Xtream output filter: {}", e))
                                                .ok()
                                        })
                                    }
                                    TargetOutputDto::M3u(o) => {
                                        o.t_filter = o.filter.as_ref().and_then(|flt| {
                                            get_filter(flt, prepared_templates)
                                                .map_err(|e| error!("Failed to parse M3U output filter: {}", e))
                                                .ok()
                                        })
                                    }
                                    TargetOutputDto::Strm(o) => {
                                        o.t_filter = o.filter.as_ref().and_then(|flt| {
                                            get_filter(flt, prepared_templates)
                                                .map_err(|e| error!("Failed to parse Strm output filter: {}", e))
                                                .ok()
                                        })
                                    }
                                    TargetOutputDto::HdHomeRun(_) => {}
                                }
                            }
                        }
                    }

                    if let Some(mappings) = app_config.mappings.as_mut() {
                        for mapping in mappings.mappings.mapping.iter_mut() {
                            let templates = mapping.templates.as_deref();
                            if let Some(mappers) = mapping.mapper.as_mut() {
                                for mapper in mappers.iter_mut() {
                                    mapper.t_filter = get_filter(mapper.filter.as_str(), templates).ok();
                                    mapper.t_script = MapperScript::parse(&mapper.script, templates).ok();
                                }
                            }
                        }
                    }

                    Some(Rc::new(app_config))
                } else {
                    Some(Rc::new(AppConfigDto::default()))
                }
            }
            Err(err) => {
                error!("{err}");
                None
            }
        };

        let api_proxy_result = match api_proxy_response {
            Ok(response) => {
                if let Some(rev) = response.headers.get(HEADER_CONFIG_API_PROXY_REVISION) {
                    revisions.api_proxy = Some(rev.clone());
                }
                response.body.map(Rc::new).or_else(|| Some(Rc::new(ApiProxyConfigDto::default())))
            }
            Err(err) => {
                error!("{err}");
                None
            }
        };

        self.server_config.replace(config_result.clone());
        self.config_channel.set(config_result);
        self.api_proxy_config.replace(api_proxy_result.clone());
        self.api_proxy_config_channel.set(api_proxy_result);
        self.revisions.replace(revisions);
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
        let mut revision = self.revisions.borrow().main.clone();
        if revision.is_none() {
            self.fetch_server_config().await;
            revision = self.revisions.borrow().main.clone();
        }
        let Some(revision) = revision else {
            return Err(Error::PreconditionRequired(
                "Missing config revision. Reload configuration and retry.".to_string(),
            ));
        };

        let path = concat_path(&self.config_path, "main");
        let request_headers = vec![(HEADER_IF_MATCH.to_string(), revision)];
        self.event_service.set_config_change_message_blocked(true);
        match request_post_meta::<ConfigDto, ()>(
            &path,
            dto,
            None,
            None,
            Some(&request_headers),
            &[HEADER_CONFIG_MAIN_REVISION],
        )
        .await
        {
            Ok(response) => {
                if let Some(rev) = response.headers.get(HEADER_CONFIG_MAIN_REVISION) {
                    self.revisions.borrow_mut().main = Some(rev.clone());
                }
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

    pub async fn save_api_proxy_config(&self, dto: ApiProxyConfigDto) -> Result<(), Error> {
        let mut revision = self.revisions.borrow().api_proxy.clone();
        if revision.is_none() {
            self.fetch_server_config().await;
            revision = self.revisions.borrow().api_proxy.clone();
        }
        let Some(revision) = revision else {
            return Err(Error::PreconditionRequired(
                "Missing api-proxy revision. Reload configuration and retry.".to_string(),
            ));
        };

        let request_headers = vec![(HEADER_IF_MATCH.to_string(), revision)];
        self.event_service.set_config_change_message_blocked(true);
        match request_put_meta::<ApiProxyConfigDto, ()>(
            &self.api_proxy_config_path,
            dto,
            None,
            None,
            Some(&request_headers),
            &[HEADER_CONFIG_API_PROXY_REVISION],
        )
        .await
        {
            Ok(response) => {
                if let Some(rev) = response.headers.get(HEADER_CONFIG_API_PROXY_REVISION) {
                    self.revisions.borrow_mut().api_proxy = Some(rev.clone());
                }
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

    pub async fn save_sources(&self, dto: SourcesConfigDto) -> Result<(), Error> {
        let mut revision = self.revisions.borrow().sources.clone();
        if revision.is_none() {
            self.fetch_server_config().await;
            revision = self.revisions.borrow().sources.clone();
        }
        let Some(revision) = revision else {
            return Err(Error::PreconditionRequired(
                "Missing source revision. Reload configuration and retry.".to_string(),
            ));
        };

        let request_headers = vec![(HEADER_IF_MATCH.to_string(), revision)];
        self.event_service.set_config_change_message_blocked(true);
        match request_post_meta::<SourcesConfigDto, ()>(
            &self.sources_path,
            dto,
            None,
            None,
            Some(&request_headers),
            &[HEADER_CONFIG_SOURCES_REVISION],
        )
        .await
        {
            Ok(response) => {
                if let Some(rev) = response.headers.get(HEADER_CONFIG_SOURCES_REVISION) {
                    self.revisions.borrow_mut().sources = Some(rev.clone());
                }
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
        let params = LibraryScanRequest { force_rescan: false };
        request_post::<LibraryScanRequest, ()>(&path, params, None, None).await
    }

    pub async fn complete_setup(&self, payload: SetupCompleteRequestDto) -> Result<(), Error> {
        self.event_service.set_config_change_message_blocked(true);
        match request_post::<SetupCompleteRequestDto, ()>(&self.setup_complete_path, payload, None, None).await {
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
}
