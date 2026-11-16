//! Sanitizing of short, server-supplied display strings (chat titles, sender names,
//! document names) so they cannot spoof surrounding UI.

/// Maximum bytes kept for a display name after sanitizing.
pub const MAX_NAME_BYTES: usize = 256;
/// Maximum bytes kept for a one-line chat-list preview.
pub const MAX_PREVIEW_BYTES: usize = 512;
/// Maximum bytes kept for a service-message description.
pub const MAX_SERVICE_BYTES: usize = 1024;

/// Sanitizes a display name. See [`clean`].
pub fn display_name(raw: &str) -> String {
    clean(raw, MAX_NAME_BYTES)
}

/// Whether `s` is already in the form [`clean`] would produce for `max`.
pub fn is_clean(s: &str, max: usize) -> bool {
    s.len() <= max && clean(s, max) == s
}

/// Removes bidi overrides/isolates, other format controls that reorder or hide text,
/// and C0/C1 control characters; collapses newlines/tabs to spaces; trims; and caps
/// the length at `max` bytes on a char boundary.
pub fn clean(raw: &str, max: usize) -> String {
    let mut out = String::with_capacity(raw.len().min(max));
    for ch in raw.chars() {
        let mapped = match ch {
            '\n' | '\r' | '\t' => ' ',
            c if is_stripped(c) => continue,
            c => c,
        };
        if out.len() + mapped.len_utf8() > max {
            break;
        }
        out.push(mapped);
    }
    out.trim().to_owned()
}

fn is_stripped(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200E}' | '\u{200F}' // LRM, RLM
            | '\u{202A}'..='\u{202E}' // LRE, RLE, PDF, LRO, RLO
            | '\u{2066}'..='\u{2069}' // LRI, RLI, FSI, PDI
            | '\u{061C}' // Arabic letter mark
            | '\u{FEFF}' // zero-width no-break space / BOM
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn strips_rlo_spoofing() {
        assert_eq!(display_name("invoice\u{202E}fdp.exe"), "invoicefdp.exe");
    }

    #[test]
    fn strips_isolates_and_controls() {
        assert_eq!(display_name("\u{2067}a\u{0007}b\u{2069}"), "ab");
        assert_eq!(display_name("line1\nline2\t"), "line1 line2");
    }

    #[test]
    fn keeps_legitimate_text() {
        // ZWJ emoji sequences and RTL scripts must survive.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        assert_eq!(display_name(family), family);
        assert_eq!(display_name("שלום"), "שלום");
    }

    proptest! {
        #[test]
        fn output_is_bounded_and_clean(s in "\\PC{0,400}|[\u{2066}-\u{2069}\u{202A}-\u{202E}a]{0,400}") {
            let out = display_name(&s);
            prop_assert!(out.len() <= MAX_NAME_BYTES);
            prop_assert!(!out.chars().any(is_stripped));
        }
    }
}
