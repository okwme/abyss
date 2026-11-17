//! A masked secret's only plaintext home (the 2FA password, the `api_hash`):
//! a fixed-capacity buffer that never reallocates, edited in place, wiped on
//! submit, cancel and drop (plan section 10). No GPUI here, so it is
//! unit-tested directly.
//!
//! Positions are in chars. The field displays one `•` per char and `•` is one
//! UTF-16 unit, so a char index is also the UTF-16 offset the platform sees
//! and `BULLET.len_utf8()` times it is the byte offset in the masked layout.

use std::ops::Range;

use zeroize::{Zeroize, Zeroizing};

/// Capacity in UTF-8 bytes. Telegram's 2FA passwords are far shorter.
pub(crate) const SECRET_CAPACITY: usize = 256;

/// What the field shows for each char.
pub(crate) const BULLET: char = '•';

/// An insert was rejected because it would not fit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Overflow;

/// What a secret field accepts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Filter {
    /// Any non-control char, up to [`SECRET_CAPACITY`] bytes.
    #[default]
    Password,
    /// ASCII hex digits, at most [`API_HASH_LEN`] (a pasted hash loses its
    /// spaces, quotes and line breaks).
    ApiHash,
}

/// Length of an `api_hash` in hex digits.
pub(crate) const API_HASH_LEN: usize = 32;

impl Filter {
    fn admits(self, ch: char) -> bool {
        match self {
            Self::Password => !ch.is_control(),
            Self::ApiHash => ch.is_ascii_hexdigit(),
        }
    }

    fn max_chars(self) -> usize {
        match self {
            Self::Password => SECRET_CAPACITY,
            Self::ApiHash => API_HASH_LEN,
        }
    }
}

pub(crate) struct SecretBuf {
    bytes: Box<[u8; SECRET_CAPACITY]>,
    /// Bytes in use; always a valid UTF-8 prefix.
    len: usize,
    filter: Filter,
}

impl std::fmt::Debug for SecretBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretBuf(<redacted>)")
    }
}

impl Default for SecretBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretBuf {
    pub(crate) fn new() -> Self {
        Self::with_filter(Filter::Password)
    }

    pub(crate) fn with_filter(filter: Filter) -> Self {
        Self {
            bytes: Box::new([0; SECRET_CAPACITY]),
            len: 0,
            filter,
        }
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or_default()
    }

    fn as_str(&self) -> &str {
        // Only whole chars are ever written, so this cannot fail; an empty
        // fallback keeps it panic-free regardless.
        std::str::from_utf8(self.as_bytes()).unwrap_or_default()
    }

    pub(crate) fn char_len(&self) -> usize {
        self.as_str().chars().count()
    }

    /// Byte offset of char `index`, clamped to the end.
    fn byte_offset(&self, index: usize) -> usize {
        self.as_str()
            .char_indices()
            .nth(index)
            .map_or(self.len, |(i, _)| i)
    }

    /// Replaces the chars in `range` with the admitted chars of `text`, in
    /// place. Returns the number of chars inserted. An edit that would not fit
    /// changes nothing.
    pub(crate) fn replace(&mut self, range: &Range<usize>, text: &str) -> Result<usize, Overflow> {
        let filter = self.filter;
        let admit = |ch: &char| filter.admits(*ch);
        let start = self.byte_offset(range.start);
        let end = self.byte_offset(range.end.max(range.start));
        let insert: usize = text.chars().filter(admit).map(char::len_utf8).sum();
        let new_len = self.len - (end - start) + insert;
        let removed = self
            .as_str()
            .get(start..end)
            .map_or(0, |s| s.chars().count());
        let new_chars = self.char_len() - removed + text.chars().filter(admit).count();
        if new_len > SECRET_CAPACITY || new_chars > filter.max_chars() {
            return Err(Overflow);
        }
        // Shift the tail into place, then write the new chars over the gap.
        self.bytes.copy_within(end..self.len, start + insert);
        let mut at = start;
        let mut inserted = 0;
        for ch in text.chars().filter(admit) {
            let n = ch.len_utf8();
            if let Some(dst) = self.bytes.get_mut(at..at + n) {
                ch.encode_utf8(dst);
            }
            at += n;
            inserted += 1;
        }
        // Shrinking leaves stale plaintext past the new end.
        if let Some(stale) = self.bytes.get_mut(new_len..self.len) {
            stale.zeroize();
        }
        self.len = new_len;
        Ok(inserted)
    }

    /// Zeroes every byte of the buffer, not only the used prefix.
    pub(crate) fn wipe(&mut self) {
        self.bytes.zeroize();
        self.len = 0;
    }

    /// Copies the secret into an exactly sized zeroizing buffer for core, then
    /// wipes this one.
    #[cfg(test)]
    pub(crate) fn take(&mut self) -> Zeroizing<Vec<u8>> {
        let out = self.copy();
        self.wipe();
        out
    }

    /// Copies the secret into an exactly sized zeroizing buffer, keeping it
    /// (a submission core may refuse; the field is wiped once it accepts).
    pub(crate) fn copy(&self) -> Zeroizing<Vec<u8>> {
        let mut out = Zeroizing::new(Vec::with_capacity(self.len));
        out.extend_from_slice(self.as_bytes());
        out
    }

    /// The plaintext, for drawing a revealed `api_hash` only.
    pub(crate) fn reveal(&self) -> &str {
        self.as_str()
    }

    #[cfg(test)]
    pub(crate) fn raw(&self) -> &[u8; SECRET_CAPACITY] {
        &self.bytes
    }
}

impl Drop for SecretBuf {
    fn drop(&mut self) {
        self.wipe();
        #[cfg(test)]
        tests::record_drop(self.bytes.as_slice());
    }
}

/// Editing state of the password field: the secret plus selection and IME
/// marked range, all in chars.
#[derive(Debug, Default)]
pub(crate) struct MaskedEdit {
    secret: SecretBuf,
    selected: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
}

impl MaskedEdit {
    pub(crate) fn new(filter: Filter) -> Self {
        Self {
            secret: SecretBuf::with_filter(filter),
            ..Self::default()
        }
    }

    pub(crate) fn filter(&self) -> Filter {
        self.secret.filter
    }

    /// See [`SecretBuf::copy`].
    pub(crate) fn copy(&self) -> Zeroizing<Vec<u8>> {
        self.secret.copy()
    }

    /// See [`SecretBuf::reveal`].
    pub(crate) fn reveal(&self) -> &str {
        self.secret.reveal()
    }

    pub(crate) fn len(&self) -> usize {
        self.secret.char_len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.secret.len == 0
    }

    pub(crate) fn selected(&self) -> Range<usize> {
        self.selected.clone()
    }

    pub(crate) fn reversed(&self) -> bool {
        self.reversed
    }

    pub(crate) fn marked(&self) -> Option<Range<usize>> {
        self.marked.clone()
    }

    pub(crate) fn cursor(&self) -> usize {
        if self.reversed {
            self.selected.start
        } else {
            self.selected.end
        }
    }

    /// Orders and clamps a platform range (UTF-16 units == chars here).
    pub(crate) fn clamp(&self, range: &Range<usize>) -> Range<usize> {
        let len = self.len();
        let (a, b) = (range.start.min(len), range.end.min(len));
        a.min(b)..a.max(b)
    }

    /// The masked text for a platform range: bullets only, never plaintext.
    pub(crate) fn text_for_range(&self, range: &Range<usize>) -> (String, Range<usize>) {
        let r = self.clamp(range);
        (bullets(r.len()), r)
    }

    pub(crate) fn move_to(&mut self, at: usize) {
        let at = at.min(self.len());
        self.selected = at..at;
        self.reversed = false;
    }

    pub(crate) fn select_to(&mut self, at: usize) {
        let at = at.min(self.len());
        if self.reversed {
            self.selected.start = at;
        } else {
            self.selected.end = at;
        }
        if self.selected.end < self.selected.start {
            self.reversed = !self.reversed;
            self.selected = self.selected.end..self.selected.start;
        }
    }

    pub(crate) fn set_selected(&mut self, range: &Range<usize>) {
        self.selected = self.clamp(range);
        self.reversed = false;
    }

    pub(crate) fn unmark(&mut self) {
        self.marked = None;
    }

    fn target(&self, range: Option<&Range<usize>>) -> Range<usize> {
        let r = range
            .cloned()
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selected.clone());
        self.clamp(&r)
    }

    pub(crate) fn replace(
        &mut self,
        range: Option<&Range<usize>>,
        text: &str,
    ) -> Result<(), Overflow> {
        let r = self.target(range);
        let n = self.secret.replace(&r, text)?;
        let end = r.start + n;
        self.selected = end..end;
        self.reversed = false;
        self.marked = None;
        Ok(())
    }

    /// A composition step (dead key or IME). The marked chars live in the
    /// secret buffer and display as bullets like the rest.
    pub(crate) fn replace_and_mark(
        &mut self,
        range: Option<&Range<usize>>,
        text: &str,
        new_selected: Option<&Range<usize>>,
    ) -> Result<(), Overflow> {
        let r = self.target(range);
        let n = self.secret.replace(&r, text)?;
        let end = r.start + n;
        self.marked = (n > 0).then_some(r.start..end);
        self.selected = match new_selected {
            Some(sel) => r.start + sel.start.min(n)..r.start + sel.end.min(n),
            None => end..end,
        };
        self.reversed = false;
        Ok(())
    }

    /// Wipes the secret (cancel).
    pub(crate) fn clear(&mut self) {
        self.secret.wipe();
        self.selected = 0..0;
        self.reversed = false;
        self.marked = None;
    }

    /// Hands the secret to the caller and wipes the field (submit).
    #[cfg(test)]
    pub(crate) fn take(&mut self) -> Zeroizing<Vec<u8>> {
        let out = self.secret.take();
        self.clear();
        out
    }

    #[cfg(test)]
    pub(crate) fn secret(&self) -> &SecretBuf {
        &self.secret
    }
}

#[cfg(test)]
mod hash_tests {
    use super::*;

    #[test]
    fn api_hash_filter_keeps_hex_and_caps_at_32() {
        let mut m = MaskedEdit::new(Filter::ApiHash);
        m.replace(None, " 0123456789abcdef\n0123456789ABCDEF \"")
            .unwrap();
        assert_eq!(m.reveal(), "0123456789abcdef0123456789ABCDEF");
        assert_eq!(m.replace(None, "a"), Err(Overflow), "33 digits");
        assert_eq!(m.len(), API_HASH_LEN);
        let (masked, _) = m.text_for_range(&(0..API_HASH_LEN));
        assert_eq!(masked, bullets(API_HASH_LEN), "platform text is bullets");
        let copy = m.copy();
        assert_eq!(
            copy.as_slice(),
            m.reveal().as_bytes(),
            "copy keeps the field"
        );
        assert!(!format!("{m:?}").contains("abcdef"), "Debug is redacted");
        m.clear();
        assert!(m.secret().raw().iter().all(|b| *b == 0), "cleared is wiped");
    }
}

/// `n` bullets.
pub(crate) fn bullets(n: usize) -> String {
    std::iter::repeat_n(BULLET, n).collect()
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    thread_local! {
        /// Non-zero bytes seen in the last dropped buffer, after its wipe.
        static LAST_DROP_NONZERO: Cell<Option<usize>> = const { Cell::new(None) };
    }

    pub(super) fn record_drop(bytes: &[u8]) {
        let nonzero = bytes.iter().filter(|b| **b != 0).count();
        LAST_DROP_NONZERO.with(|c| c.set(Some(nonzero)));
    }

    const FAMILY: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";

    fn typed(text: &str) -> MaskedEdit {
        let mut m = MaskedEdit::default();
        m.replace(None, text).unwrap();
        m
    }

    fn plain(m: &MaskedEdit) -> &str {
        std::str::from_utf8(m.secret().as_bytes()).unwrap()
    }

    #[test]
    fn non_ascii_emoji_and_zwj_map_one_bullet_per_char() {
        let text = format!("éß🙂{FAMILY}");
        let m = typed(&text);
        let chars = text.chars().count();
        assert_eq!(chars, 3 + 5);
        assert_eq!(m.len(), chars);
        let (masked, actual) = m.text_for_range(&(0..999));
        assert_eq!(actual, 0..chars);
        assert_eq!(masked.chars().count(), chars);
        assert_eq!(masked.encode_utf16().count(), chars);
        assert_eq!(masked.len(), chars * BULLET.len_utf8());
    }

    #[test]
    fn edits_in_the_middle_map_char_offsets_to_bytes() {
        let mut m = typed("a🙂c");
        m.set_selected(&(1..2));
        m.replace(None, "ßé").unwrap();
        assert_eq!(plain(&m), "aßéc");
        assert_eq!(m.selected(), 3..3);
        m.set_selected(&(0..1));
        m.replace(None, "").unwrap();
        assert_eq!(plain(&m), "ßéc");
        // A ZWJ sequence is several chars; deleting one char at a time works.
        let mut m = typed(FAMILY);
        m.set_selected(&(4..5));
        m.replace(None, "").unwrap();
        assert_eq!(m.len(), 4);
        assert!(FAMILY.starts_with(plain(&m)));
    }

    #[test]
    fn text_for_range_never_returns_plaintext() {
        let m = typed("hunter2é🙂");
        for start in 0..12 {
            for end in 0..12 {
                let (text, _) = m.text_for_range(&(start..end));
                assert!(text.chars().all(|c| c == BULLET), "{text:?}");
            }
        }
    }

    #[test]
    fn dead_key_sequence_composes() {
        let mut m = typed("x");
        m.replace_and_mark(None, "´", Some(&(1..1))).unwrap();
        assert_eq!(m.marked(), Some(1..2));
        assert_eq!(m.selected(), 2..2);
        m.replace(None, "é").unwrap();
        assert_eq!(plain(&m), "xé");
        assert_eq!(m.marked(), None);
        assert_eq!(m.selected(), 2..2);
    }

    #[test]
    fn ime_composition_and_cancel() {
        let mut m = typed("ab");
        m.move_to(1);
        m.replace_and_mark(None, "に", Some(&(1..1))).unwrap();
        m.replace_and_mark(None, "にほ", Some(&(2..2))).unwrap();
        assert_eq!(plain(&m), "aにほb");
        assert_eq!(m.marked(), Some(1..3));
        assert_eq!(m.selected(), 3..3);
        m.replace_and_mark(None, "", None).unwrap();
        assert_eq!(plain(&m), "ab");
        assert_eq!(m.marked(), None);
        assert_eq!(m.selected(), 1..1);
    }

    #[test]
    fn overflow_is_rejected_whole() {
        let mut m = typed(&"a".repeat(SECRET_CAPACITY - 2));
        assert_eq!(m.replace(None, "🙂"), Err(Overflow));
        assert_eq!(m.len(), SECRET_CAPACITY - 2);
        assert_eq!(m.replace(None, "é"), Ok(()));
        assert_eq!(m.secret().as_bytes().len(), SECRET_CAPACITY);
        assert_eq!(m.replace(None, "x"), Err(Overflow));
        // Replacing a selection frees its room first.
        m.set_selected(&(0..1));
        assert_eq!(m.replace(None, "b"), Ok(()));
    }

    #[test]
    fn control_chars_are_dropped() {
        let m = typed("a\nb\r\tc");
        assert_eq!(plain(&m), "abc");
    }

    #[test]
    fn never_reallocates() {
        let mut buf = SecretBuf::new();
        let before = std::ptr::from_ref(buf.raw());
        for i in 0..200 {
            buf.replace(&(0..0), "🙂é").ok();
            if i % 3 == 0 {
                buf.replace(&(1..4), "").unwrap();
            }
        }
        assert_eq!(std::ptr::from_ref(buf.raw()), before);
    }

    #[test]
    fn shrinking_zeroes_the_stale_tail() {
        let mut buf = SecretBuf::new();
        buf.replace(&(0..0), "secret").unwrap();
        buf.replace(&(2..6), "").unwrap();
        assert_eq!(buf.as_bytes(), b"se");
        assert!(buf.raw()[2..].iter().all(|b| *b == 0));
    }

    #[test]
    fn take_and_clear_wipe() {
        let mut m = typed("p@ss🙂");
        let out = m.take();
        assert_eq!(&out[..], "p@ss🙂".as_bytes());
        assert_eq!(out.capacity(), out.len());
        assert!(m.is_empty());
        assert!(m.secret().raw().iter().all(|b| *b == 0));
        m.replace(None, "again").unwrap();
        m.clear();
        assert!(m.secret().raw().iter().all(|b| *b == 0));
    }

    #[test]
    fn drop_wipes() {
        LAST_DROP_NONZERO.with(|c| c.set(None));
        drop(typed("hunter2"));
        assert_eq!(LAST_DROP_NONZERO.with(Cell::get), Some(0));
    }

    #[test]
    fn debug_is_redacted() {
        let m = typed("hunter2");
        assert!(!format!("{m:?}").contains("hunter2"));
    }
}
