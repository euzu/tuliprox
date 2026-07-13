use shared::utils::Internable;
use std::{borrow::Cow, sync::Arc};

const EPG_ID_STACK_FOLD_LEN: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EpgIdOutputCase {
    Preserve,
    LowercaseAscii,
}

impl EpgIdOutputCase {
    pub(crate) const fn from_lowercase(lowercase: bool) -> Self {
        if lowercase {
            Self::LowercaseAscii
        } else {
            Self::Preserve
        }
    }
}

/// Runs `f` with an ASCII-lowercase EPG ID.
///
/// Non-ASCII characters remain unchanged, matching `eq_ignore_ascii_case` semantics.
pub(crate) fn with_folded_epg_id<R>(id: &str, f: impl FnOnce(&str) -> R) -> R {
    if !id.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return f(id);
    }

    if id.len() <= EPG_ID_STACK_FOLD_LEN {
        let mut buf = [0_u8; EPG_ID_STACK_FOLD_LEN];
        for (out, byte) in buf.iter_mut().zip(id.bytes()) {
            *out = byte.to_ascii_lowercase();
        }
        let folded = std::str::from_utf8(&buf[..id.len()]);
        return f(folded.unwrap_or(id));
    }

    let mut folded = String::with_capacity(id.len());
    folded.extend(id.chars().map(|ch| ch.to_ascii_lowercase()));
    f(&folded)
}

/// Folds a trusted, persisted EPG ID and interns only a changed value.
pub(crate) fn fold_epg_id_arc(id: &Arc<str>) -> Arc<str> {
    with_folded_epg_id(id, |folded| {
        if folded == id.as_ref() {
            Arc::clone(id)
        } else {
            folded.intern()
        }
    })
}

/// Folds an untrusted request ID without adding it to the global interner.
pub(crate) fn fold_untrusted_epg_id(id: &str) -> Arc<str> {
    let mut folded = id.to_owned();
    folded.make_ascii_lowercase();
    Arc::from(folded)
}

/// Applies the target output casing to an untrusted request ID without interning it.
pub(crate) fn canonicalize_untrusted_epg_id(id: &str, output_case: EpgIdOutputCase) -> Arc<str> {
    match output_case {
        EpgIdOutputCase::Preserve => Arc::from(id),
        EpgIdOutputCase::LowercaseAscii => fold_untrusted_epg_id(id),
    }
}

/// Applies the configured casing to a visible, persisted EPG ID.
pub(crate) fn canonicalize_output_epg_id(id: &Arc<str>, output_case: EpgIdOutputCase) -> Arc<str> {
    match output_case {
        EpgIdOutputCase::Preserve => Arc::clone(id),
        EpgIdOutputCase::LowercaseAscii => fold_epg_id_arc(id),
    }
}

/// Applies Unicode lowercase only to visible XMLTV text.
pub(crate) fn lowercase_xmltv_text(value: &str, enabled: bool) -> Cow<'_, str> {
    if !enabled {
        return Cow::Borrowed(value);
    }

    let lowercase = value.to_lowercase();
    if lowercase == value {
        Cow::Borrowed(value)
    } else {
        Cow::Owned(lowercase)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        canonicalize_output_epg_id, canonicalize_untrusted_epg_id, fold_epg_id_arc,
        fold_untrusted_epg_id, lowercase_xmltv_text, EpgIdOutputCase,
    };
    use quick_xml::{
        events::{BytesText, Event},
        Writer,
    };
    use shared::utils::Internable;
    use std::{borrow::Cow, sync::Arc};

    #[test]
    fn fold_epg_id_arc_lowercases_ascii() {
        let id = "Example.Channel".intern();

        let canonical = fold_epg_id_arc(&id);

        assert_eq!(canonical.as_ref(), "example.channel");
    }

    #[test]
    fn fold_epg_id_arc_reuses_already_folded_arc() {
        let id = "example.channel".intern();

        let canonical = fold_epg_id_arc(&id);

        assert!(Arc::ptr_eq(&canonical, &id));
    }

    #[test]
    fn output_epg_id_reuses_arc_when_case_is_preserved() {
        let id = "Example.Channel".intern();

        let canonical = canonicalize_output_epg_id(&id, EpgIdOutputCase::Preserve);

        assert!(Arc::ptr_eq(&canonical, &id));
    }

    #[test]
    fn fold_untrusted_epg_id_handles_long_ids() {
        let id = format!("{}.Example", "A".repeat(129));

        let canonical = fold_untrusted_epg_id(&id);

        assert_eq!(canonical.as_ref(), format!("{}.example", "a".repeat(129)));
    }

    #[test]
    fn fold_epg_id_arc_handles_ids_longer_than_stack_buffer() {
        let id = format!("{}.Example", "A".repeat(129)).intern();

        let canonical = fold_epg_id_arc(&id);

        assert_eq!(canonical.as_ref(), format!("{}.example", "a".repeat(129)));
    }

    #[test]
    fn fold_epg_id_arc_preserves_non_ascii_characters() {
        let id = "ÄBC.Id".intern();

        let canonical = fold_epg_id_arc(&id);

        assert_eq!(canonical.as_ref(), "Äbc.id");
    }

    #[test]
    fn fold_untrusted_epg_id_does_not_reuse_the_global_interner() {
        let interned = "request.example".intern();

        let request_id = fold_untrusted_epg_id("REQUEST.Example");

        assert_eq!(request_id.as_ref(), interned.as_ref());
        assert!(!Arc::ptr_eq(&request_id, &interned));
    }

    #[test]
    fn untrusted_epg_id_preserves_case_without_using_the_global_interner() {
        let interned = "Request.Example".intern();

        let request_id = canonicalize_untrusted_epg_id("Request.Example", EpgIdOutputCase::Preserve);

        assert_eq!(request_id.as_ref(), interned.as_ref());
        assert!(!Arc::ptr_eq(&request_id, &interned));
    }

    #[test]
    fn lowercase_xmltv_text_uses_unicode_lowercase() {
        let lowercase = lowercase_xmltv_text("CAFÉ NETWORK", true);

        assert_eq!(lowercase, "café network");
    }

    #[test]
    fn lowercase_xmltv_text_handles_unicode_titlecase_characters() {
        let lowercase = lowercase_xmltv_text("\u{01C5}", true);

        assert_eq!(lowercase, "\u{01C6}");
    }

    #[test]
    fn lowercase_xmltv_text_borrows_unchanged_values() {
        let disabled = lowercase_xmltv_text("MixedCase", false);
        let already_lowercase = lowercase_xmltv_text("lowercase", true);

        assert!(matches!(disabled, Cow::Borrowed("MixedCase")));
        assert!(matches!(already_lowercase, Cow::Borrowed("lowercase")));
    }

    #[test]
    fn lowercase_xmltv_text_remains_xml_escaped() {
        let display_name = lowercase_xmltv_text("CAFÉ NETWORK & <HD>", true);
        let mut writer = Writer::new(Vec::new());

        writer
            .write_event(Event::Text(BytesText::new(display_name.as_ref())))
            .expect("XMLTV display name should serialize");
        let output = String::from_utf8(writer.into_inner()).expect("XML output should be UTF-8");

        assert_eq!(output, "café network &amp; &lt;hd&gt;");
    }
}
