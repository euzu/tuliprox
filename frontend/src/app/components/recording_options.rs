use crate::app::components::DropDownOption;
use shared::model::PlayerRecordingLevel;
use std::str::FromStr;
use yew::prelude::*;

pub fn recording_level_label(level: PlayerRecordingLevel, t: impl Fn(&str) -> String) -> String {
    match level {
        PlayerRecordingLevel::None => t("LABEL.PLAYER_RECORDING_NONE"),
        PlayerRecordingLevel::Read => t("LABEL.PLAYER_RECORDING_READ"),
        PlayerRecordingLevel::Private => t("LABEL.PLAYER_RECORDING_PRIVATE"),
        PlayerRecordingLevel::Shared => t("LABEL.PLAYER_RECORDING_SHARED"),
    }
}

pub fn recording_level_from_id(id: &str) -> Option<PlayerRecordingLevel> { id.parse().ok() }

pub fn parse_user_recording_level(id: Option<&str>) -> Option<PlayerRecordingLevel> {
    let id = id.filter(|s| !s.is_empty())?;
    recording_level_from_id(id)
}

pub fn parse_plan_recording_level(id: Option<&str>) -> Option<PlayerRecordingLevel> {
    match parse_user_recording_level(id) {
        Some(PlayerRecordingLevel::None) | None => None,
        Some(other) => Some(other),
    }
}

pub fn build_recording_options(
    current: Option<PlayerRecordingLevel>,
    include_inherit: bool,
    t: impl Fn(&str) -> String,
) -> Vec<DropDownOption> {
    let mut options = Vec::with_capacity(PlayerRecordingLevel::ALL.len() + usize::from(include_inherit));
    if include_inherit {
        options.push(DropDownOption {
            id: String::new(),
            label: html! { t("LABEL.INHERIT") },
            selected: current.is_none(),
        });
    }
    options.extend(PlayerRecordingLevel::ALL.iter().map(|&level| DropDownOption {
        id: level.to_string(),
        label: html! { recording_level_label(level, &t) },
        selected: current == Some(level),
    }));
    options
}

pub fn parse_optional_input<T: FromStr>(value: &str) -> Result<Option<T>, ()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        trimmed.parse::<T>().map(Some).map_err(|_| ())
    }
}

pub const MSG_BYTE_SIZE_INVALID: &str = "MESSAGES.VALIDATION.BYTE_SIZE_INVALID";

/// Parse and validate an optional byte size string.
///
/// Returns `Ok(None)` for blank input, `Ok(Some(bytes))` for a valid size greater than 0,
/// and `Err(())` if parsing fails or the resolved byte count is 0.
pub fn parse_positive_byte_size(value: &str) -> Result<Option<u64>, ()> {
    if value.trim().is_empty() {
        Ok(None)
    } else {
        shared::model::parse_positive_size(value).map(Some).ok_or(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_recording_levels() {
        assert_eq!(recording_level_from_id("none"), Some(PlayerRecordingLevel::None));
        assert_eq!(recording_level_from_id("read"), Some(PlayerRecordingLevel::Read));
        assert_eq!(recording_level_from_id("private"), Some(PlayerRecordingLevel::Private));
        assert_eq!(recording_level_from_id("shared"), Some(PlayerRecordingLevel::Shared));
        assert_eq!(recording_level_from_id("unknown"), None);

        assert_eq!(parse_user_recording_level(Some("private")), Some(PlayerRecordingLevel::Private));
        assert_eq!(parse_user_recording_level(Some("")), None);
        assert_eq!(parse_user_recording_level(None), None);

        assert_eq!(parse_plan_recording_level(Some("private")), Some(PlayerRecordingLevel::Private));
        assert_eq!(parse_plan_recording_level(Some("none")), None);
        assert_eq!(parse_plan_recording_level(None), None);
    }

    #[test]
    fn parse_positive_byte_size_valid_zero_invalid() {
        // valid
        assert_eq!(parse_positive_byte_size("20GB"), Ok(Some(21_474_836_480)));
        assert_eq!(parse_positive_byte_size("500MB"), Ok(Some(524_288_000)));
        assert_eq!(parse_positive_byte_size("1024"), Ok(Some(1024)));
        assert_eq!(parse_positive_byte_size(" 1GB "), Ok(Some(1_073_741_824)));

        // blank
        assert_eq!(parse_positive_byte_size(""), Ok(None));
        assert_eq!(parse_positive_byte_size("   "), Ok(None));

        // zero
        assert_eq!(parse_positive_byte_size("0"), Err(()));
        assert_eq!(parse_positive_byte_size("0GB"), Err(()));
        assert_eq!(parse_positive_byte_size("0MB"), Err(()));

        // invalid
        assert_eq!(parse_positive_byte_size("abc"), Err(()));
        assert_eq!(parse_positive_byte_size("20G"), Err(()));
        assert_eq!(parse_positive_byte_size("-1"), Err(()));
    }

    #[test]
    fn invalid_byte_size_error_message_key() {
        assert_eq!(MSG_BYTE_SIZE_INVALID, "MESSAGES.VALIDATION.BYTE_SIZE_INVALID");
    }
}
