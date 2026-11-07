use crate::{
    defaults::{default_as_true, is_false, is_true},
    error::TuliproxError,
    foundation::{get_filter, Filter},
    model::{is_valid_plugin_id, PatternTemplate, Prepare, StrmExportStyle, TargetType, TraktConfigDto},
    utils::is_blank_optional_string,
};
use serde_json::{Map, Value};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct XtreamTargetOutputDto {
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub skip_live_direct_source: bool,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub skip_video_direct_source: bool,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub skip_series_direct_source: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trakt: Option<TraktConfigDto>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub filter: Option<String>,
    #[serde(skip)]
    pub t_filter: Option<Filter>,
}

impl Default for XtreamTargetOutputDto {
    fn default() -> Self {
        XtreamTargetOutputDto {
            skip_live_direct_source: default_as_true(),
            skip_video_direct_source: default_as_true(),
            skip_series_direct_source: default_as_true(),
            trakt: None,
            filter: None,
            t_filter: None,
        }
    }
}

impl XtreamTargetOutputDto {
    pub fn has_any_option(&self) -> bool {
        self.skip_live_direct_source
            || self.skip_video_direct_source
            || self.skip_series_direct_source
            || self.trakt.is_some()
            || self.filter.is_some()
    }
}

impl Prepare for XtreamTargetOutputDto {
    type Ctx<'a> = Option<&'a [PatternTemplate]>;

    fn prepare(&mut self, templates: Self::Ctx<'_>) -> Result<(), TuliproxError> {
        if let Some(raw_filter) = &self.filter {
            self.t_filter = Some(get_filter(raw_filter, templates)?);
        }
        if let Some(trakt) = &mut self.trakt {
            trakt.prepare()?;
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct M3uTargetOutputDto {
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_type_in_url: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub mask_redirect_url: bool,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub filter: Option<String>,
    #[serde(skip)]
    pub t_filter: Option<Filter>,
}

impl M3uTargetOutputDto {
    pub fn has_any_option(&self) -> bool {
        self.filename.is_some() || self.include_type_in_url || self.mask_redirect_url || self.filter.is_some()
    }
}

impl Prepare for M3uTargetOutputDto {
    type Ctx<'a> = Option<&'a [PatternTemplate]>;

    fn prepare(&mut self, templates: Self::Ctx<'_>) -> Result<(), TuliproxError> {
        if let Some(raw_filter) = &self.filter {
            self.t_filter = Some(get_filter(raw_filter, templates)?);
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StrmTargetOutputDto {
    pub directory: String,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub username: Option<String>,
    #[serde(default)]
    pub style: StrmExportStyle,
    #[serde(default, skip_serializing_if = "is_false")]
    pub flat: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub underscore_whitespace: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub cleanup: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strm_props: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub filter: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub add_quality_to_filename: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub use_metadata: bool,

    // New Fields for Metadata and Probe
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_probe_size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_analyze_duration: Option<u64>,

    #[serde(skip)]
    pub t_filter: Option<Filter>,
}

impl Prepare for StrmTargetOutputDto {
    type Ctx<'a> = Option<&'a [PatternTemplate]>;

    fn prepare(&mut self, templates: Self::Ctx<'_>) -> Result<(), TuliproxError> {
        if let Some(raw_filter) = &self.filter {
            self.t_filter = Some(get_filter(raw_filter, templates)?);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HdHomeRunTargetOutputDto {
    pub device: String,
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_output: Option<TargetType>,
}

impl Default for HdHomeRunTargetOutputDto {
    fn default() -> Self { Self { device: String::new(), username: String::new(), use_output: Some(TargetType::M3u) } }
}

/// A custom output format rendered by a plugin: `type: plugin` + `plugin: <id>`
/// (plan §18 Q6). The host applies `filter`, then streams the items through
/// the plugin's `begin-output -> render-batch… -> finish-output` into one file
/// in the target's storage directory.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PluginTargetOutputDto {
    /// Id of the plugin that renders this output.
    pub plugin: String,
    /// The plugin's `provides.output_format`. Optional: when set it must equal
    /// it, which is checked after plugin load.
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub format: Option<String>,
    /// File name in the target's storage directory (a bare name, no path);
    /// `<plugin>.out` when unset.
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub filename: Option<String>,
    /// The plugin's settings for this output only, as the plugin's
    /// `ui.output_form` schemas describe them. Handed to the plugin as
    /// `output-request.options` in key order.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub plugin_config: Map<String, Value>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub filter: Option<String>,
    #[serde(skip)]
    pub t_filter: Option<Filter>,
}

impl PluginTargetOutputDto {
    /// The file name this output writes, [`Self::filename`] or `<plugin>.out`.
    #[must_use]
    pub fn file_name(&self) -> String { self.filename.clone().unwrap_or_else(|| format!("{}.out", self.plugin)) }
}

impl Prepare for PluginTargetOutputDto {
    type Ctx<'a> = Option<&'a [PatternTemplate]>;

    fn prepare(&mut self, templates: Self::Ctx<'_>) -> Result<(), TuliproxError> {
        self.plugin = self.plugin.trim().to_string();
        if !is_valid_plugin_id(&self.plugin) {
            return Err(TuliproxError::ConfigTarget(format!(
                "plugin output requires a valid `plugin` id, got '{}'",
                self.plugin
            )));
        }
        self.format = self.format.take().map(|format| format.trim().to_string()).filter(|format| !format.is_empty());
        self.filename =
            self.filename.take().map(|filename| filename.trim().to_string()).filter(|filename| !filename.is_empty());
        if let Some(filename) = self.filename.as_deref() {
            if filename.starts_with('.') || filename.contains(['/', '\\', '\0']) {
                return Err(TuliproxError::ConfigTarget(format!(
                    "plugin output filename must be a plain file name inside the target directory, got '{filename}'"
                )));
            }
        }
        // A dot is the nesting separator of `plugin_options`, so a key holding one would be
        // indistinguishable from a nested property.
        if let Some(bad) = self
            .plugin_config
            .keys()
            .find(|key| key.trim().is_empty() || key.trim() != key.as_str() || key.contains('.'))
        {
            return Err(TuliproxError::ConfigTarget(format!(
                "plugin output of {} has an invalid plugin_config key '{bad}': no dots, no surrounding spaces",
                self.plugin
            )));
        }
        if let Some(raw_filter) = &self.filter {
            self.t_filter = Some(get_filter(raw_filter, templates)?);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields, tag = "type", rename_all = "lowercase")]
pub enum TargetOutputDto {
    Xtream(XtreamTargetOutputDto),
    M3u(M3uTargetOutputDto),
    Strm(StrmTargetOutputDto),
    HdHomeRun(HdHomeRunTargetOutputDto),
    Plugin(PluginTargetOutputDto),
}

impl Prepare for TargetOutputDto {
    type Ctx<'a> = Option<&'a [PatternTemplate]>;

    fn prepare(&mut self, templates: Self::Ctx<'_>) -> Result<(), TuliproxError> {
        match self {
            TargetOutputDto::Xtream(output) => output.prepare(templates),
            TargetOutputDto::M3u(output) => output.prepare(templates),
            TargetOutputDto::Strm(output) => output.prepare(templates),
            TargetOutputDto::HdHomeRun(_) => Ok(()),
            TargetOutputDto::Plugin(output) => output.prepare(templates),
        }
    }
}

#[cfg(test)]
mod plugin_output_tests {
    use super::*;
    use crate::model::ConfigTargetDto;

    fn target(outputs: serde_json::Value) -> ConfigTargetDto {
        serde_json::from_value(serde_json::json!({"name": "t1", "output": outputs})).unwrap()
    }

    fn plugin_output(value: serde_json::Value) -> PluginTargetOutputDto {
        match serde_json::from_value::<TargetOutputDto>(value).unwrap() {
            TargetOutputDto::Plugin(output) => output,
            other => panic!("not a plugin output: {other:?}"),
        }
    }

    #[test]
    fn the_shape_round_trips_with_type_plugin() {
        let value = serde_json::json!({
            "type": "plugin", "plugin": "my-plugin", "format": "my-format", "filename": "out.txt",
            "plugin_config": {"b": "2", "a": 1, "flag": true}, "filter": "Group ~ \"News\""
        });
        let dto = plugin_output(value.clone());
        assert_eq!(dto.plugin, "my-plugin");
        assert_eq!(dto.plugin_config.len(), 3);
        assert_eq!(dto.plugin_config["b"], serde_json::json!("2"));
        assert_eq!(dto.plugin_config["flag"], serde_json::json!(true));
        assert_eq!(dto.plugin_config["a"], serde_json::json!(1), "values keep their type");

        let json = serde_json::to_value(TargetOutputDto::Plugin(dto.clone())).unwrap();
        assert_eq!(json["type"], "plugin");
        assert_eq!(serde_json::from_value::<TargetOutputDto>(json).unwrap(), TargetOutputDto::Plugin(dto));

        // Only `plugin` is required; the rest is not serialized when unset.
        let minimal = serde_json::to_value(TargetOutputDto::Plugin(plugin_output(
            serde_json::json!({"type": "plugin", "plugin": "my-plugin"}),
        )))
        .unwrap();
        assert_eq!(minimal, serde_json::json!({"type": "plugin", "plugin": "my-plugin"}));
    }

    #[test]
    fn yaml_unknown_keys_and_a_missing_plugin_are_rejected() {
        assert!(serde_json::from_value::<TargetOutputDto>(serde_json::json!({"type": "plugin"})).is_err());
        assert!(serde_json::from_value::<TargetOutputDto>(
            serde_json::json!({"type": "plugin", "plugin": "my-plugin", "directory": "x"})
        )
        .is_err());
    }

    #[test]
    fn prepare_checks_the_id_trims_and_validates_the_file_name() {
        let mut ok = plugin_output(serde_json::json!(
            {"type": "plugin", "plugin": " my-plugin ", "format": " ", "filename": " out.txt "}
        ));
        ok.prepare(None).unwrap();
        assert_eq!(ok.plugin, "my-plugin");
        assert_eq!(ok.format, None, "a blank format is no format");
        assert_eq!(ok.file_name(), "out.txt");

        let mut defaulted = plugin_output(serde_json::json!({"type": "plugin", "plugin": "my-plugin"}));
        defaulted.prepare(None).unwrap();
        assert_eq!(defaulted.file_name(), "my-plugin.out");

        for bad_id in ["", "X", "no way"] {
            let mut dto = plugin_output(serde_json::json!({"type": "plugin", "plugin": bad_id}));
            assert!(dto.prepare(None).is_err(), "{bad_id:?}");
        }
        for bad_name in ["../up.txt", "a/b.txt", "a\\b.txt", ".hidden"] {
            let mut dto =
                plugin_output(serde_json::json!({"type": "plugin", "plugin": "my-plugin", "filename": bad_name}));
            let err = dto.prepare(None).unwrap_err().to_string();
            assert!(err.contains("plain file name"), "{bad_name:?}: {err}");
        }
    }

    #[test]
    fn prepare_compiles_the_output_filter() {
        let mut dto = plugin_output(serde_json::json!(
            {"type": "plugin", "plugin": "my-plugin", "filter": "Group ~ \"News\""}
        ));
        dto.prepare(None).unwrap();
        assert!(dto.t_filter.is_some());
        let mut broken = plugin_output(serde_json::json!({"type": "plugin", "plugin": "my-plugin", "filter": "((("}));
        assert!(broken.prepare(None).is_err());
    }

    #[test]
    fn a_target_may_have_a_plugin_output_alone_but_not_two_for_one_file() {
        let mut alone = target(serde_json::json!([{"type": "plugin", "plugin": "my-plugin"}]));
        alone.prepare(1, None, None).unwrap();

        let mut two_files = target(serde_json::json!([
            {"type": "plugin", "plugin": "my-plugin"},
            {"type": "plugin", "plugin": "my-plugin", "filename": "second.out"},
        ]));
        two_files.prepare(1, None, None).unwrap();

        let mut clash = target(serde_json::json!([
            {"type": "plugin", "plugin": "my-plugin"},
            {"type": "plugin", "plugin": "my-plugin"},
        ]));
        let err = clash.prepare(1, None, None).unwrap_err().to_string();
        assert!(err.contains("same filename"), "{err}");
    }
}
