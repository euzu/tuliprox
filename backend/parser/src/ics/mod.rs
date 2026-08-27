mod dummy;
mod event;
mod time;

pub use dummy::fill_dummy_gaps;
pub use event::IcsEvent;
use shared::{
    defaults::{MAX_ICS_DESCRIPTION_LENGTH, MAX_ICS_SUMMARY_LENGTH},
    error::TuliproxError,
    model::{EpgCategory, EpgChannel, EpgProgramme},
    utils::Internable,
};
use std::{path::Path, sync::Arc};
use tokio::io::AsyncReadExt;
use tuliprox_core::{model::IcsEpgSourceConfig, utils::compressed_file_reader_async::CompressedFileReaderAsync};

pub async fn parse_ics_file_to_channel(
    file_path: &Path,
    channel_id: Arc<str>,
    channel_title: Option<Arc<str>>,
    config: &IcsEpgSourceConfig,
) -> Result<EpgChannel, TuliproxError> {
    let content = read_ics_file_limited(file_path, config.max_decompressed_bytes).await?;
    let events = event::parse_ics_events(&content, config)?;
    let mut programmes = events_to_programmes(events, &channel_id, config);
    programmes.sort_by_key(|programme| (programme.start, programme.stop));
    let channel_title = Some(channel_title.unwrap_or_else(|| Arc::clone(&channel_id)));

    Ok(EpgChannel { id: channel_id, title: channel_title, icon: None, programmes })
}

async fn read_ics_file_limited(file_path: &Path, max_bytes: usize) -> Result<String, TuliproxError> {
    let mut reader = CompressedFileReaderAsync::new(file_path)
        .await
        .map_err(|err| TuliproxError::Io(format!("Failed to open ICS file {}: {err}", file_path.display())))?;
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];

    loop {
        let read = reader
            .read(&mut chunk)
            .await
            .map_err(|err| TuliproxError::Io(format!("Failed to read ICS file {}: {err}", file_path.display())))?;
        if read == 0 {
            break;
        }

        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > max_bytes {
            return Err(TuliproxError::Parse(format!(
                "ICS file {} exceeds max_decompressed_bytes={max_bytes}",
                file_path.display()
            )));
        }
    }

    String::from_utf8(buffer)
        .map_err(|err| TuliproxError::Parse(format!("ICS file {} is not valid UTF-8: {err}", file_path.display())))
}

fn events_to_programmes(
    events: Vec<IcsEvent>,
    channel_id: &Arc<str>,
    config: &IcsEpgSourceConfig,
) -> Vec<EpgProgramme> {
    let mut result = Vec::with_capacity(events.len());

    for event in events {
        if event.cancelled && !config.include_cancelled {
            continue;
        }

        let Some(start) = event.start else {
            continue;
        };
        let Some(stop) = event.stop else {
            continue;
        };
        if start >= stop {
            continue;
        }

        let categories_text = event.categories.join(",");
        let title = render_event_template(&config.event.title, &event, &categories_text, MAX_ICS_SUMMARY_LENGTH);
        let mut desc =
            render_event_template(&config.event.description, &event, &categories_text, MAX_ICS_DESCRIPTION_LENGTH);

        if config.event.include_location {
            if let Some(location) = event.location.as_deref().filter(|value| !value.is_empty()) {
                append_description_metadata(&mut desc, "Location: ", location);
            }
        }

        if config.event.include_categories && !categories_text.is_empty() {
            append_description_metadata(&mut desc, "Categories: ", &categories_text);
        }

        let categories: Vec<EpgCategory> =
            event.categories.into_iter().map(|value| EpgCategory { value: value.intern(), lang: None }).collect();

        let mut programme = EpgProgramme::new_all(
            start,
            stop,
            Arc::clone(channel_id),
            (!title.is_empty()).then(|| title.intern()),
            (!desc.is_empty()).then(|| desc.intern()),
            None,
        );
        programme.categories = categories;
        result.push(programme);
    }

    result
}

fn render_event_template(template: &str, event: &IcsEvent, categories: &str, max_bytes: usize) -> String {
    let replacements = [
        ("{summary}", event.summary.as_deref().unwrap_or_default()),
        ("{description}", event.description.as_deref().unwrap_or_default()),
        ("{location}", event.location.as_deref().unwrap_or_default()),
        ("{categories}", categories),
        ("{uid}", event.uid.as_deref().unwrap_or_default()),
        ("{start}", event.start_display.as_deref().unwrap_or_default()),
        ("{end}", event.stop_display.as_deref().unwrap_or_default()),
    ];
    let mut rendered = String::with_capacity(template.len().min(max_bytes));
    let mut remaining = template;

    while !remaining.is_empty() && rendered.len() < max_bytes {
        let Some(open_brace) = remaining.find('{') else {
            append_utf8_bounded(&mut rendered, remaining, max_bytes);
            break;
        };
        append_utf8_bounded(&mut rendered, &remaining[..open_brace], max_bytes);
        remaining = &remaining[open_brace..];

        if let Some((placeholder, value)) =
            replacements.iter().find(|(placeholder, _)| remaining.starts_with(*placeholder))
        {
            append_utf8_bounded(&mut rendered, value, max_bytes);
            remaining = &remaining[placeholder.len()..];
        } else {
            append_utf8_bounded(&mut rendered, "{", max_bytes);
            remaining = &remaining[1..];
        }
    }

    rendered
}

fn append_description_metadata(description: &mut String, label: &str, value: &str) {
    if !description.is_empty() {
        append_utf8_bounded(description, "\n", MAX_ICS_DESCRIPTION_LENGTH);
    }
    append_utf8_bounded(description, label, MAX_ICS_DESCRIPTION_LENGTH);
    append_utf8_bounded(description, value, MAX_ICS_DESCRIPTION_LENGTH);
}

fn append_utf8_bounded(target: &mut String, value: &str, max_bytes: usize) {
    let available = max_bytes.saturating_sub(target.len());
    let mut end = value.len().min(available);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&value[..end]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::{model::EpgCategory, utils::Internable};
    use std::fs;
    use tempfile::tempdir;
    use tuliprox_core::model::{IcsEpgSourceConfig, IcsEventMapping};

    #[tokio::test]
    async fn parses_file_to_channel_and_programmes() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("calendar.ics");
        fs::write(
            &path,
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:1\nSUMMARY:Practice 1\nDESCRIPTION:Session\nLOCATION:Bahrain\nCATEGORIES:F1,Race\nDTSTART:20260306T123000Z\nDTEND:20260306T133000Z\nEND:VEVENT\nEND:VCALENDAR",
        )
        .expect("write");
        let config = IcsEpgSourceConfig {
            event: IcsEventMapping {
                title: "Formula 1: {summary}".to_string(),
                description: "{description}".to_string(),
                include_location: true,
                include_categories: true,
            },
            ..IcsEpgSourceConfig::default()
        };

        let channel = parse_ics_file_to_channel(&path, "f1.calendar".intern(), Some("Formula 1".intern()), &config)
            .await
            .expect("channel");

        assert_eq!(channel.id.as_ref(), "f1.calendar");
        assert_eq!(channel.title.as_deref(), Some("Formula 1"));
        assert_eq!(channel.programmes.len(), 1);
        assert_eq!(channel.programmes[0].title.as_deref(), Some("Formula 1: Practice 1"));
        assert_eq!(channel.programmes[0].desc.as_deref(), Some("Session\nLocation: Bahrain\nCategories: F1,Race"));
        assert_eq!(
            channel.programmes[0].categories,
            vec![EpgCategory { value: "F1".intern(), lang: None }, EpgCategory { value: "Race".intern(), lang: None },],
        );
        assert!(!channel.programmes[0].is_live);
        assert!(!channel.programmes[0].is_new);
    }

    #[tokio::test]
    async fn cancelled_events_are_skipped_by_default_and_imported_when_enabled() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("cancelled.ics");
        fs::write(
            &path,
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:Cancelled\nSTATUS:CANCELLED\nDTSTART:20260306T123000Z\nDTEND:20260306T133000Z\nEND:VEVENT\nEND:VCALENDAR",
        )
        .expect("write");

        let skipped = parse_ics_file_to_channel(&path, "f1.calendar".intern(), None, &IcsEpgSourceConfig::default())
            .await
            .expect("channel");
        assert!(skipped.programmes.is_empty());

        let included_config = IcsEpgSourceConfig { include_cancelled: true, ..IcsEpgSourceConfig::default() };
        let included =
            parse_ics_file_to_channel(&path, "f1.calendar".intern(), None, &included_config).await.expect("channel");
        assert_eq!(included.programmes.len(), 1);
        assert_eq!(included.title.as_deref(), Some("f1.calendar"));
    }

    #[tokio::test]
    async fn rejects_file_above_decompressed_limit() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("large.ics");
        fs::write(&path, "BEGIN:VCALENDAR\nEND:VCALENDAR\n").expect("write");
        let config = IcsEpgSourceConfig { max_decompressed_bytes: 4, ..IcsEpgSourceConfig::default() };

        let err = parse_ics_file_to_channel(&path, "f1.calendar".intern(), None, &config).await.unwrap_err();
        assert!(err.to_string().contains("max_decompressed_bytes"));
    }

    #[test]
    fn bounded_renderer_caps_repeated_placeholders_uid_and_multibyte_text() {
        let event = IcsEvent {
            uid: Some("u".repeat(MAX_ICS_SUMMARY_LENGTH)),
            description: Some("€".repeat(MAX_ICS_DESCRIPTION_LENGTH / 3 + 1)),
            ..IcsEvent::default()
        };
        let repeated_description = "{description}".repeat(MAX_ICS_SUMMARY_LENGTH / "{description}".len());
        let rendered_description = render_event_template(&repeated_description, &event, "", MAX_ICS_SUMMARY_LENGTH);
        assert!(rendered_description.len() <= MAX_ICS_SUMMARY_LENGTH);
        assert!(rendered_description.chars().all(|character| character == '€'));

        let rendered_uid = render_event_template("{uid}{uid}", &event, "", MAX_ICS_SUMMARY_LENGTH);
        assert_eq!(rendered_uid.len(), MAX_ICS_SUMMARY_LENGTH);
    }

    #[test]
    fn final_description_cap_includes_location_and_categories() {
        let event = IcsEvent {
            start: Some(1),
            stop: Some(2),
            description: Some("Description".to_string()),
            location: Some("L".repeat(MAX_ICS_DESCRIPTION_LENGTH / 2)),
            categories: vec!["ä".repeat(MAX_ICS_DESCRIPTION_LENGTH)],
            ..IcsEvent::default()
        };
        let config = IcsEpgSourceConfig {
            event: IcsEventMapping {
                title: "{summary}".to_string(),
                description: "{description}".to_string(),
                include_location: true,
                include_categories: true,
            },
            ..IcsEpgSourceConfig::default()
        };

        let programmes = events_to_programmes(vec![event], &"channel".intern(), &config);
        let description = programmes[0].desc.as_deref().expect("description");
        assert!(description.len() <= MAX_ICS_DESCRIPTION_LENGTH);
        assert!(description.is_char_boundary(description.len()));
    }
}
