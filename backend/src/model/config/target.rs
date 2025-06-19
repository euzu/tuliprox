use crate::foundation::filter::{get_filter, Filter, PatternTemplate, ValueProvider};
use crate::model::mapping::Mapping;
use crate::model::config::trakt::TraktConfig;
use shared::error::{create_tuliprox_error_result, handle_tuliprox_error_result_list, info_err, TuliproxError, TuliproxErrorKind};
use shared::utils::{default_as_default, default_as_true, default_resolve_delay_secs};
use arc_swap::ArcSwapOption;
use shared::model::{ClusterFlags, ProcessingOrder, StrmExportStyle, TargetType};
use shared::model::PlaylistItemType;
use std::sync::Arc;
use crate::model::{ConfigRename, ConfigSort};


#[derive(Clone, Debug)]
pub struct ProcessTargets {
    pub enabled: bool,
    pub inputs: Vec<u16>,
    pub targets: Vec<u16>,
}

impl ProcessTargets {
    pub fn has_target(&self, tid: u16) -> bool {
        !self.enabled || self.targets.is_empty() || self.targets.contains(&tid)
    }

    pub fn has_input(&self, tid: u16) -> bool {
        !self.enabled || self.inputs.is_empty() || self.inputs.contains(&tid)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ConfigTargetOptions {
    #[serde(default)]
    pub ignore_logo: bool,
    #[serde(default)]
    pub share_live_streams: bool,
    #[serde(default)]
    pub remove_duplicates: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force_redirect: Option<ClusterFlags>,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XtreamTargetOutput {
    #[serde(default = "default_as_true")]
    pub skip_live_direct_source: bool,
    #[serde(default = "default_as_true")]
    pub skip_video_direct_source: bool,
    #[serde(default = "default_as_true")]
    pub skip_series_direct_source: bool,
    #[serde(default)]
    pub resolve_series: bool,
    #[serde(default = "default_resolve_delay_secs")]
    pub resolve_series_delay: u16,
    #[serde(default)]
    pub resolve_vod: bool,
    #[serde(default = "default_resolve_delay_secs")]
    pub resolve_vod_delay: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trakt: Option<TraktConfig>,
}

impl XtreamTargetOutput {
    pub fn prepare(&mut self) {
        if let Some(trakt) = &mut self.trakt {
            trakt.prepare();
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct M3uTargetOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default)]
    pub include_type_in_url: bool,
    #[serde(default)]
    pub mask_redirect_url: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrmTargetOutput {
    pub directory: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default)]
    pub style: StrmExportStyle,
    #[serde(default)]
    pub flat: bool,
    #[serde(default)]
    pub underscore_whitespace: bool,
    #[serde(default)]
    pub cleanup: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strm_props: Option<Vec<String>>,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HdHomeRunTargetOutput {
    pub device: String,
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_output: Option<TargetType>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, tag = "type", rename_all = "lowercase")]
pub enum TargetOutput {
    Xtream(XtreamTargetOutput),
    M3u(M3uTargetOutput),
    Strm(StrmTargetOutput),
    HdHomeRun(HdHomeRunTargetOutput),
}

impl TargetOutput {
    pub fn prepare(&mut self) {
        match self {
            TargetOutput::Xtream(output) => output.prepare(),
            TargetOutput::M3u(_)
            | TargetOutput::Strm(_)
            | TargetOutput::HdHomeRun(_) => {}
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ConfigTarget {
    #[serde(skip)]
    pub id: u16,
    #[serde(default = "default_as_true")]
    pub enabled: bool,
    #[serde(default = "default_as_default")]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<ConfigTargetOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<ConfigSort>,
    pub filter: String,
    #[serde(default)]
    pub output: Vec<TargetOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rename: Option<Vec<ConfigRename>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mapping: Option<Vec<String>>,
    #[serde(default)]
    pub processing_order: ProcessingOrder,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<Vec<String>>,
    #[serde(skip)]
    pub t_watch_re: Option<Vec<regex::Regex>>,
    #[serde(skip)]
    pub t_filter: Option<Filter>,
    #[serde(skip)]
    pub t_mapping: Arc<ArcSwapOption<Vec<Mapping>>>,
}

impl ConfigTarget {
    #[allow(clippy::too_many_lines)]
    pub fn prepare(&mut self, id: u16, templates: Option<&Vec<PatternTemplate>>) -> Result<(), TuliproxError> {
        self.id = id;
        if self.output.is_empty() {
            return Err(info_err!(format!("Missing output format for {}", self.name)));
        }
        let mut m3u_cnt = 0;
        let mut xtream_cnt = 0;
        let mut strm_cnt = 0;
        let mut strm_needs_xtream = false;
        let mut hdhr_cnt = 0;
        let mut hdhomerun_needs_m3u = false;
        let mut hdhomerun_needs_xtream = false;

        let mut strm_export_styles = vec![];
        let mut strm_directories: Vec<&str> = vec![];

        for target_output in &mut self.output {
            target_output.prepare();
            match target_output {
                TargetOutput::Xtream(_) => {
                    xtream_cnt += 1;
                    if default_as_default().eq_ignore_ascii_case(&self.name) {
                        return create_tuliprox_error_result!(TuliproxErrorKind::Info, "unique target name is required for xtream type output: {}", self.name);
                    }
                }
                TargetOutput::M3u(m3u_output) => {
                    m3u_cnt += 1;
                    m3u_output.filename = m3u_output.filename.as_ref().map(|s| s.trim().to_string());
                }
                TargetOutput::Strm(strm_output) => {
                    strm_cnt += 1;
                    strm_output.directory = strm_output.directory.trim().to_string();
                    if strm_output.directory.trim().is_empty() {
                        return create_tuliprox_error_result!(TuliproxErrorKind::Info, "directory is required for strm type: {}", self.name);
                    }
                    if let Some(username) = &mut strm_output.username {
                        *username = username.trim().to_string();
                    }
                    let has_username = strm_output.username.as_ref().is_some_and(|u| !u.is_empty());

                    if has_username {
                        strm_needs_xtream = true;
                    }
                    if strm_export_styles.contains(&strm_output.style) {
                        return create_tuliprox_error_result!(TuliproxErrorKind::Info, "strm outputs with same export style are not allowed: {}", self.name);
                    }
                    strm_export_styles.push(strm_output.style);
                    if strm_directories.contains(&strm_output.directory.as_str()) {
                        return create_tuliprox_error_result!(TuliproxErrorKind::Info, "strm outputs with same export directory are not allowed: {}", self.name);
                    }
                    strm_directories.push(strm_output.directory.as_str());
                }
                TargetOutput::HdHomeRun(hdhomerun_output) => {
                    hdhr_cnt += 1;
                    hdhomerun_output.username = hdhomerun_output.username.trim().to_string();
                    if hdhomerun_output.username.is_empty() {
                        return create_tuliprox_error_result!(TuliproxErrorKind::Info, "Username is required for HdHomeRun type: {}", self.name);
                    }

                    hdhomerun_output.device = hdhomerun_output.device.trim().to_string();
                    if hdhomerun_output.device.is_empty() {
                        return create_tuliprox_error_result!(TuliproxErrorKind::Info, "Device is required for HdHomeRun type: {}", self.name);
                    }

                    if let Some(use_output) = hdhomerun_output.use_output.as_ref() {
                        match &use_output {
                            TargetType::M3u => { hdhomerun_needs_m3u = true; }
                            TargetType::Xtream => { hdhomerun_needs_xtream = true; }
                            _ => return create_tuliprox_error_result!(TuliproxErrorKind::Info, "HdHomeRun output option `use_output` only accepts `m3u` or `xtream` for target: {}", self.name),
                        }
                    }
                }
            }
        }

        if m3u_cnt > 1 || xtream_cnt > 1 || hdhr_cnt > 1 {
            return create_tuliprox_error_result!(TuliproxErrorKind::Info, "Multiple output formats with same type : {}", self.name);
        }

        if strm_cnt > 0 && strm_needs_xtream && xtream_cnt == 0 {
            return create_tuliprox_error_result!(TuliproxErrorKind::Info, "strm output with a username is only permitted when used in combination with xtream output: {}", self.name);
        }

        if hdhr_cnt > 0 {
            if xtream_cnt == 0 && m3u_cnt == 0 {
                return create_tuliprox_error_result!(TuliproxErrorKind::Info, "HdHomeRun output is only permitted when used in combination with xtream or m3u output: {}", self.name);
            }
            if hdhomerun_needs_m3u && m3u_cnt == 0 {
                return create_tuliprox_error_result!(TuliproxErrorKind::Info, "HdHomeRun output has `use_output=m3u` but no `m3u` output defined: {}", self.name);
            }
            if hdhomerun_needs_xtream && xtream_cnt == 0 {
                return create_tuliprox_error_result!(TuliproxErrorKind::Info, "HdHomeRun output has `use_output=xtream` but no `xtream` output defined: {}", self.name);
            }
        }

        if let Some(watch) = &self.watch {
            let regexps: Result<Vec<regex::Regex>, _> = watch.iter().map(|s| regex::Regex::new(s)).collect();
            match regexps {
                Ok(watch_re) => self.t_watch_re = Some(watch_re),
                Err(err) => {
                    return create_tuliprox_error_result!(TuliproxErrorKind::Info, "Invalid watch regular expression: {}", err);
                }
            }
        }

        match get_filter(&self.filter, templates) {
            Ok(fltr) => {
                // debug!("Filter: {}", fltr);
                self.t_filter = Some(fltr);
                if let Some(renames) = self.rename.as_mut() {
                    handle_tuliprox_error_result_list!(TuliproxErrorKind::Info, renames.iter_mut().map(|cr|cr.prepare(templates)));
                }
                if let Some(sort) = self.sort.as_mut() {
                    sort.prepare(templates)?;
                }
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    pub fn filter(&self, provider: &ValueProvider) -> bool {
        if let Some(filter) = self.t_filter.as_ref() {
            return filter.filter(provider);
        }
        true
    }

    pub(crate) fn get_xtream_output(&self) -> Option<&XtreamTargetOutput> {
        if let Some(TargetOutput::Xtream(output)) = self.output.iter().find(|o| matches!(o, TargetOutput::Xtream(_))) {
            Some(output)
        } else {
            None
        }
    }

    pub(crate) fn get_m3u_output(&self) -> Option<&M3uTargetOutput> {
        if let Some(TargetOutput::M3u(output)) = self.output.iter().find(|o| matches!(o, TargetOutput::M3u(_))) {
            Some(output)
        } else {
            None
        }
    }

    // pub(crate) fn get_strm_output(&self) -> Option<&StrmTargetOutput> {
    //     if let Some(TargetOutput::Strm(output)) = self.output.iter().find(|o| matches!(o, TargetOutput::Strm(_))) {
    //         Some(output)
    //     } else {
    //         None
    //     }
    // }

    pub(crate) fn get_hdhomerun_output(&self) -> Option<&HdHomeRunTargetOutput> {
        if let Some(TargetOutput::HdHomeRun(output)) = self.output.iter().find(|o| matches!(o, TargetOutput::HdHomeRun(_))) {
            Some(output)
        } else {
            None
        }
    }

    pub fn has_output(&self, tt: &TargetType) -> bool {
        for target_output in &self.output {
            match target_output {
                TargetOutput::Xtream(_) => { if tt == &TargetType::Xtream { return true; } }
                TargetOutput::M3u(_) => { if tt == &TargetType::M3u { return true; } }
                TargetOutput::Strm(_) => { if tt == &TargetType::Strm { return true; } }
                TargetOutput::HdHomeRun(_) => { if tt == &TargetType::HdHomeRun { return true; } }
            }
        }
        false
    }

    pub fn is_force_redirect(&self, item_type: PlaylistItemType) -> bool {
        self.options
            .as_ref()
            .and_then(|options| options.force_redirect.as_ref())
            .is_some_and(|flags| flags.has_cluster(item_type))
    }
}