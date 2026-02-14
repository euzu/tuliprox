use std::borrow::Cow;

pub fn strip_port<'a>(input: &'a str) -> Cow<'a, str> {
    // IPv6 with port: [2001:db8::1]:8080
    if let Some(stripped) = input.strip_prefix('[') {
        if let Some(end) = stripped.find(']') {
            return Cow::Owned(stripped[..end].to_string());
        }
        // Invalid IPv6
        return Cow::Borrowed(input);
    }

    // IPv4 or IPv6 without bracket
    if let Some((left, _right)) = input.rsplit_once(':') {
        // If `left` has a colon then its IPv6 without port.
        if left.contains(':') {
            Cow::Borrowed(input)
        } else {
            // IPv4:Port
            Cow::Owned(left.to_string())
        }
    } else {
        Cow::Borrowed(input)
    }
}
