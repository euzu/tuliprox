use super::TransientResourceId;

const MIN_PROXY_ID_DIGITS: usize = 6;
const SEGMENT_EXTENSIONS: &[&str] = &["ts", "mp4", "m4s", "m4v"];
const MAP_EXTENSIONS: &[&str] = &["mp4", "m4s", "m4v"];

/// Parsed normal timeline segment file from `/hls/shared/live/{id}/{segment_file}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsSegmentFile {
    pub proxy_seq: u64,
    pub extension: String,
}

/// Parsed EXT-X-MAP file from `/hls/shared/live/{id}/map/{map_file}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsMapFile {
    pub proxy_map_id: u64,
    pub extension: String,
}

/// Parsed transient passthrough resource from `/hls/shared/live/{id}/r/{resource_file}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransientResourceFile {
    pub resource_id: TransientResourceId,
    pub extension: String,
}

impl HlsSegmentFile {
    pub fn parse(file_name: &str) -> Option<Self> {
        let (id, extension) = parse_numeric_file(file_name, SEGMENT_EXTENSIONS)?;
        Some(Self { proxy_seq: id, extension: extension.to_string() })
    }
}

impl HlsMapFile {
    pub fn parse(file_name: &str) -> Option<Self> {
        let (id, extension) = parse_numeric_file(file_name, MAP_EXTENSIONS)?;
        Some(Self { proxy_map_id: id, extension: extension.to_string() })
    }
}

impl TransientResourceFile {
    pub fn parse(file_name: &str) -> Option<Self> {
        if file_name.contains('/') || file_name.contains("://") {
            return None;
        }
        let (resource_id, extension) = file_name.rsplit_once('.')?;
        if resource_id.is_empty() || extension.is_empty() || !is_opaque_resource_id(resource_id) {
            return None;
        }
        Some(Self { resource_id: TransientResourceId(resource_id.to_string()), extension: extension.to_string() })
    }
}

fn parse_numeric_file<'a>(file_name: &'a str, allowed_extensions: &[&str]) -> Option<(u64, &'a str)> {
    let (digits, extension) = file_name.rsplit_once('.')?;
    if digits.len() < MIN_PROXY_ID_DIGITS || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if !allowed_extensions.contains(&extension) {
        return None;
    }
    Some((digits.parse().ok()?, extension))
}

fn is_opaque_resource_id(resource_id: &str) -> bool {
    resource_id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::{HlsMapFile, HlsSegmentFile, TransientResourceFile};

    #[test]
    fn segment_file_parser_accepts_supported_extensions_and_minimum_width() {
        assert_eq!(HlsSegmentFile::parse("000123.ts").expect("valid segment").proxy_seq, 123);
        assert_eq!(HlsSegmentFile::parse("000123.mp4").expect("valid segment").extension, "mp4");
        assert_eq!(HlsSegmentFile::parse("000123.m4s").expect("valid segment").extension, "m4s");
        assert_eq!(HlsSegmentFile::parse("000123.m4v").expect("valid segment").extension, "m4v");
        assert_eq!(HlsSegmentFile::parse("000000123.ts").expect("valid segment").proxy_seq, 123);
    }

    #[test]
    fn map_file_parser_accepts_supported_extensions_and_minimum_width() {
        assert_eq!(HlsMapFile::parse("000001.mp4").expect("valid map").proxy_map_id, 1);
        assert_eq!(HlsMapFile::parse("000001.m4s").expect("valid map").extension, "m4s");
        assert_eq!(HlsMapFile::parse("000001.m4v").expect("valid map").extension, "m4v");
        assert_eq!(HlsMapFile::parse("000000001.mp4").expect("valid map").proxy_map_id, 1);
    }

    #[test]
    fn segment_file_parser_rejects_invalid_names() {
        for file_name in ["123.ts", "abc123.ts", "000123.exe", "000123"] {
            assert!(HlsSegmentFile::parse(file_name).is_none(), "{file_name} should be rejected");
        }
    }

    #[test]
    fn map_file_parser_rejects_invalid_names() {
        for file_name in ["123.ts", "abc123.ts", "000123.exe", "000123"] {
            assert!(HlsMapFile::parse(file_name).is_none(), "{file_name} should be rejected");
        }
    }

    #[test]
    fn transient_resource_parser_accepts_opaque_resource_ids() {
        assert_eq!(
            TransientResourceFile::parse("f91ac2.ts").expect("valid transient resource").resource_id.0,
            "f91ac2"
        );
        assert_eq!(TransientResourceFile::parse("resource_1.m4s").expect("valid transient resource").extension, "m4s");
        assert_eq!(TransientResourceFile::parse("abc-123.key").expect("valid transient resource").extension, "key");
    }

    #[test]
    fn transient_resource_parser_rejects_missing_parts_and_origin_urls() {
        for file_name in [".ts", "f91ac2", "f91ac2.", "http://origin/seg.ts", "provider://demo/seg.ts"] {
            assert!(TransientResourceFile::parse(file_name).is_none(), "{file_name} should be rejected");
        }
    }
}
