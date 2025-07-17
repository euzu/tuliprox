use log::warn;
use crate::error::{TuliproxError, TuliproxErrorKind};
use crate::{create_tuliprox_error_result, handle_tuliprox_error_result_list, info_err};
use crate::foundation::filter::{get_filter, Filter};
use crate::model::{ClusterFlags, ConfigRenameDto, ConfigSortDto, HdHomeRunDeviceOverview, PatternTemplate, ProcessingOrder, StrmExportStyle, TargetType, TraktConfigDto};
use crate::utils::{default_as_true, default_resolve_delay_secs, default_as_default};
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct XtreamTargetOutputDto {
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
    pub trakt: Option<TraktConfigDto>,
}

impl XtreamTargetOutputDto {
    pub fn prepare(&mut self) {
        if let Some(trakt) = &mut self.trakt {
            trakt.prepare();
        }
    }

    pub fn has_any_option(&self) -> bool {
        self.skip_live_direct_source
            || self.skip_video_direct_source
            || self.skip_series_direct_source
            || self.resolve_series
            || self.resolve_vod
            || self.trakt.is_some()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct M3uTargetOutputDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default)]
    pub include_type_in_url: bool,
    #[serde(default)]
    pub mask_redirect_url: bool,
}

impl M3uTargetOutputDto {
    pub fn has_any_option(&self) -> bool {
        self.filename.is_some()
            || self.include_type_in_url
            || self.mask_redirect_url
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StrmTargetOutputDto {
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HdHomeRunTargetOutputDto {
    pub device: String,
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_output: Option<TargetType>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields, tag = "type", rename_all = "lowercase")]
pub enum TargetOutputDto {
    Xtream(XtreamTargetOutputDto),
    M3u(M3uTargetOutputDto),
    Strm(StrmTargetOutputDto),
    HdHomeRun(HdHomeRunTargetOutputDto),
}

impl TargetOutputDto {
    pub fn prepare(&mut self) {
        match self {
            TargetOutputDto::Xtream(output) => output.prepare(),
            TargetOutputDto::M3u(_)
            | TargetOutputDto::Strm(_)
            | TargetOutputDto::HdHomeRun(_) => {}
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigTargetDto {
    #[serde(skip)]
    pub id: u16,
    #[serde(default = "default_as_true")]
    pub enabled: bool,
    #[serde(default = "default_as_default")]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<ConfigTargetOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<ConfigSortDto>,
    pub filter: String,
    #[serde(default)]
    pub output: Vec<TargetOutputDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rename: Option<Vec<ConfigRenameDto>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mapping: Option<Vec<String>>,
    #[serde(default)]
    pub processing_order: ProcessingOrder,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<Vec<String>>,
    #[serde(skip)]
    pub t_filter: Option<Filter>,
}

impl ConfigTargetDto {
    #[allow(clippy::too_many_lines)]
    pub fn prepare(&mut self, id: u16, templates: Option<&Vec<PatternTemplate>>, hdhr_config: Option<&HdHomeRunDeviceOverview>) -> Result<(), TuliproxError> {
        self.id = id;
        if self.output.is_empty() {
            return Err(info_err!(format!("Missing output format for {}", self.name)));
        }
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            return create_tuliprox_error_result!(TuliproxErrorKind::Info, "target name required");
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
                TargetOutputDto::Xtream(_) => {
                    xtream_cnt += 1;
                    if default_as_default().eq_ignore_ascii_case(&self.name) {
                        return create_tuliprox_error_result!(TuliproxErrorKind::Info, "unique target name is required for xtream type output: {}", self.name);
                    }
                }
                TargetOutputDto::M3u(m3u_output) => {
                    m3u_cnt += 1;
                    m3u_output.filename = m3u_output.filename.as_ref().map(|s| s.trim().to_string());
                }
                TargetOutputDto::Strm(strm_output) => {
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
                TargetOutputDto::HdHomeRun(hdhomerun_output) => {
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
                    if let Some(hdhr_devices) = hdhr_config {
                        if !hdhr_devices.devices.contains(&hdhomerun_output.device) {
                            return create_tuliprox_error_result!(TuliproxErrorKind::Info, "HdHomeRun output device is not defined: {}", hdhomerun_output.device);
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

            if let Some(hdhr_devices) = hdhr_config {
                if !hdhr_devices.enabled {
                    warn!("You have defined an HDHomeRun output, but HDHomeRun devices are disabled.");
                }
            }
        }

        if let Some(watch) = &self.watch {
            for pat in watch {
                if let Err(err) = regex::Regex::new(pat) {
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
}