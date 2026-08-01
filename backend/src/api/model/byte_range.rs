use axum::http::HeaderValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::api) enum SingleByteRange {
    Full,
    Partial { start: u64, end: u64, length: u64 },
    Unsatisfiable,
}

pub(in crate::api) fn resolve_single_byte_range(
    range_header: Option<&HeaderValue>,
    full_size: u64,
) -> SingleByteRange {
    let Some(range_header) = range_header else {
        return SingleByteRange::Full;
    };
    let Ok(range_header) = range_header.to_str() else {
        return SingleByteRange::Full;
    };
    let Some(range_spec) = range_header.strip_prefix("bytes=") else {
        return SingleByteRange::Full;
    };
    if range_spec.contains(',') || full_size == 0 {
        return SingleByteRange::Unsatisfiable;
    }

    let Some((start, end)) = range_spec.split_once('-') else {
        return SingleByteRange::Unsatisfiable;
    };
    if start.is_empty() {
        return resolve_suffix_range(end, full_size);
    }
    resolve_start_range(start, end, full_size)
}

fn resolve_suffix_range(suffix_length: &str, full_size: u64) -> SingleByteRange {
    let Ok(suffix_length) = suffix_length.parse::<u64>() else {
        return SingleByteRange::Unsatisfiable;
    };
    if suffix_length == 0 {
        return SingleByteRange::Unsatisfiable;
    }
    let length = suffix_length.min(full_size);
    let start = full_size - length;
    let end = full_size - 1;
    SingleByteRange::Partial { start, end, length }
}

fn resolve_start_range(start: &str, end: &str, full_size: u64) -> SingleByteRange {
    let Ok(start) = start.parse::<u64>() else {
        return SingleByteRange::Unsatisfiable;
    };
    if start >= full_size {
        return SingleByteRange::Unsatisfiable;
    }
    let end = if end.is_empty() {
        full_size - 1
    } else {
        let Ok(parsed_end) = end.parse::<u64>() else {
            return SingleByteRange::Unsatisfiable;
        };
        parsed_end.min(full_size - 1)
    };
    if end < start {
        return SingleByteRange::Unsatisfiable;
    }
    SingleByteRange::Partial { start, end, length: end - start + 1 }
}

#[cfg(test)]
mod tests {
    use super::{resolve_single_byte_range, SingleByteRange};
    use axum::http::HeaderValue;

    fn header(value: &str) -> HeaderValue { HeaderValue::from_str(value).expect("valid test header") }

    #[test]
    fn resolves_open_closed_and_suffix_ranges() {
        assert_eq!(
            resolve_single_byte_range(Some(&header("bytes=4-")), 10),
            SingleByteRange::Partial { start: 4, end: 9, length: 6 }
        );
        assert_eq!(
            resolve_single_byte_range(Some(&header("bytes=2-5")), 10),
            SingleByteRange::Partial { start: 2, end: 5, length: 4 }
        );
        assert_eq!(
            resolve_single_byte_range(Some(&header("bytes=-3")), 10),
            SingleByteRange::Partial { start: 7, end: 9, length: 3 }
        );
        assert_eq!(
            resolve_single_byte_range(Some(&header("bytes=-20")), 10),
            SingleByteRange::Partial { start: 0, end: 9, length: 10 }
        );
    }

    #[test]
    fn rejects_empty_unsatisfiable_malformed_and_multi_ranges() {
        for value in ["bytes=", "bytes=-0", "bytes=20-", "bytes=5-2", "bytes=a-b", "bytes=0-1,4-5"] {
            assert_eq!(
                resolve_single_byte_range(Some(&header(value)), 10),
                SingleByteRange::Unsatisfiable,
                "unexpected decision for {value}"
            );
        }
        assert_eq!(resolve_single_byte_range(Some(&header("bytes=0-")), 0), SingleByteRange::Unsatisfiable);
    }

    #[test]
    fn absent_invalid_utf8_and_unknown_units_are_ignored() {
        assert_eq!(resolve_single_byte_range(None, 10), SingleByteRange::Full);
        assert_eq!(
            resolve_single_byte_range(Some(&HeaderValue::from_bytes(&[0xFF]).expect("opaque header")), 10),
            SingleByteRange::Full
        );
        assert_eq!(
            resolve_single_byte_range(Some(&header("items=0-1")), 10),
            SingleByteRange::Full
        );
    }
}
