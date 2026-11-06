//! Text being typed, as input methods see it: a caret, plus the marked
//! (pre-edit) range an IME is composing. Offsets are bytes; the platform
//! speaks UTF-16, so conversions live here too. Pure, so the IME protocol
//! semantics are unit-tested without a compositor.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct EditBuffer {
    pub text: String,
    /// Caret, a byte offset on a char boundary.
    pub cursor: usize,
    /// Text the IME is still composing, shown underlined.
    pub marked: Option<Range<usize>>,
}

impl EditBuffer {
    /// `text` with the caret at its end.
    pub fn new(text: String) -> EditBuffer {
        EditBuffer {
            cursor: text.len(),
            text,
            marked: None,
        }
    }

    /// The caret as a char index (what text layouts index by).
    pub fn cursor_chars(&self) -> usize {
        self.text[..self.cursor].chars().count()
    }

    pub fn to_utf16(&self, byte: usize) -> usize {
        self.text[..byte.min(self.text.len())]
            .chars()
            .map(char::len_utf16)
            .sum()
    }

    /// The byte offset of UTF-16 offset `u16`, clamped to the text and
    /// snapped back to a char boundary.
    pub fn from_utf16(&self, u16: usize) -> usize {
        let mut units = 0;
        for (i, c) in self.text.char_indices() {
            if units + c.len_utf16() > u16 {
                return i;
            }
            units += c.len_utf16();
        }
        self.text.len()
    }

    pub fn range_to_utf16(&self, r: &Range<usize>) -> Range<usize> {
        self.to_utf16(r.start)..self.to_utf16(r.end)
    }

    pub fn range_from_utf16(&self, r: &Range<usize>) -> Range<usize> {
        let start = self.from_utf16(r.start);
        start..self.from_utf16(r.end).max(start)
    }

    /// What an unspecified IME range means: the marked text, else the caret.
    fn target(&self, range: Option<Range<usize>>) -> Range<usize> {
        range
            .or_else(|| self.marked.clone())
            .unwrap_or(self.cursor..self.cursor)
    }

    /// Replaces `range` (bytes; `None`: the marked text or the caret) with
    /// committed `text`, ending composition. Control characters other than
    /// newlines are dropped.
    pub fn replace(&mut self, range: Option<Range<usize>>, text: &str) {
        let r = self.target(range);
        let clean: String = text
            .chars()
            .filter(|&c| c == '\n' || !c.is_control())
            .collect();
        self.text.replace_range(r.clone(), &clean);
        self.cursor = r.start + clean.len();
        self.marked = None;
    }

    /// Replaces `range` with IME pre-edit `text` and marks it. `selected`
    /// (bytes within `text`) is where the IME puts the caret; default end.
    pub fn set_marked(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
    ) {
        let r = self.target(range);
        self.text.replace_range(r.clone(), text);
        self.marked = (!text.is_empty()).then(|| r.start..r.start + text.len());
        let caret = selected.map_or(text.len(), |s| s.end.min(text.len()));
        let caret = (0..=caret)
            .rev()
            .find(|&i| text.is_char_boundary(i))
            .unwrap_or(0);
        self.cursor = r.start + caret;
    }

    /// Typed text at the caret.
    pub fn insert(&mut self, text: &str) {
        self.replace(None, text);
    }

    fn prev_boundary(&self) -> usize {
        self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self) -> usize {
        self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(self.text.len(), |g| self.cursor + g.len())
    }

    /// Deletes the grapheme before the caret (a whole emoji or flag, a
    /// letter with its accents).
    pub fn backspace(&mut self) {
        let start = self.prev_boundary();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.marked = None;
    }

    pub fn delete(&mut self) {
        let end = self.next_boundary();
        self.text.replace_range(self.cursor..end, "");
        self.marked = None;
    }

    /// One grapheme towards the end (`forward`) or the start of the text.
    pub fn step(&mut self, forward: bool) {
        self.cursor = if forward {
            self.next_boundary()
        } else {
            self.prev_boundary()
        };
    }

    pub fn home(&mut self) {
        self.cursor = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
    }

    pub fn end(&mut self) {
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i);
    }

    /// Index of the line the caret is on.
    pub fn line(&self) -> usize {
        self.text[..self.cursor].matches('\n').count()
    }

    pub fn marked_chars(&self) -> Option<Range<usize>> {
        let m = self.marked.as_ref()?;
        let start = self.text[..m.start].chars().count();
        Some(start..start + self.text[m.clone()].chars().count())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_offsets_round_trip_through_astral_chars() {
        // "a" 1 unit, "日" 1 unit (3 bytes), "😀" 2 units (4 bytes).
        let b = EditBuffer::new("a日😀b".into());
        assert_eq!(b.to_utf16(b.text.len()), 5);
        assert_eq!(b.from_utf16(0), 0);
        assert_eq!(b.from_utf16(1), 1);
        assert_eq!(b.from_utf16(2), 4);
        // Mid-surrogate snaps back to the emoji's start.
        assert_eq!(b.from_utf16(3), 4);
        assert_eq!(b.from_utf16(4), 8);
        assert_eq!(b.from_utf16(99), b.text.len());
        assert_eq!(b.range_from_utf16(&(1..4)), 1..8);
        assert_eq!(b.range_to_utf16(&(1..8)), 1..4);
    }

    #[test]
    fn composition_replaces_its_own_pre_edit_then_commits() {
        let mut b = EditBuffer::new("Hi ".into());
        // Typing "nihon" with a Japanese IME: each keystroke replaces the
        // pre-edit, which stays marked.
        b.set_marked(None, "に", None);
        b.set_marked(None, "にほ", None);
        b.set_marked(None, "にほん", None);
        assert_eq!(b.text, "Hi にほん");
        assert_eq!(b.marked, Some(3..12));
        assert_eq!(b.marked_chars(), Some(3..6));
        assert_eq!(b.cursor, b.text.len());
        // Conversion shows a candidate, then the commit replaces the
        // marked text and ends composition.
        b.set_marked(None, "日本", Some(0..0));
        assert_eq!(b.cursor, 3, "IME put the caret at the candidate start");
        b.replace(None, "日本");
        assert_eq!(b.text, "Hi 日本");
        assert_eq!((b.marked.clone(), b.cursor), (None, b.text.len()));
        // An empty pre-edit (composition cancelled) removes it.
        b.set_marked(None, "x", None);
        b.set_marked(None, "", None);
        assert_eq!((b.text.as_str(), b.marked.clone()), ("Hi 日本", None));
    }

    #[test]
    fn commits_land_at_the_caret_and_drop_control_characters() {
        let mut b = EditBuffer::new("ad".into());
        b.step(false);
        b.insert("b\tc\n");
        assert_eq!(b.text, "abc\nd");
        assert_eq!(b.cursor, 4);
        b.replace(Some(0..1), "Å");
        assert_eq!(b.text, "Åbc\nd");
        assert_eq!(b.cursor, 2);
    }

    #[test]
    fn editing_moves_by_grapheme() {
        // Flag (2 regional indicators), e + combining acute, family ZWJ emoji.
        let mut b = EditBuffer::new("🇯🇵e\u{301}👨\u{200D}👩\u{200D}👧".into());
        b.backspace();
        assert_eq!(b.text, "🇯🇵e\u{301}");
        b.step(false);
        assert_eq!(b.cursor, 8);
        b.step(false);
        assert_eq!(b.cursor, 0);
        b.delete();
        assert_eq!(b.text, "e\u{301}");
        b.step(true);
        assert_eq!(b.cursor, b.text.len());
        b.step(true);
        assert_eq!(b.cursor, b.text.len());
    }

    #[test]
    fn home_and_end_stay_on_the_caret_line() {
        let mut b = EditBuffer::new("one\ntwo\nthree".into());
        b.cursor = 5;
        assert_eq!(b.line(), 1);
        b.home();
        assert_eq!(b.cursor, 4);
        b.end();
        assert_eq!(b.cursor, 7);
    }
}
