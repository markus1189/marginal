//! The comment editor: a small multi-line buffer with readline bindings.
//!
//! Byte-indexed like everything else here, and every cursor move lands on a
//! character boundary. Two different word definitions are implemented on
//! purpose, matching readline:
//!
//! * `C-w` (`unix-word-rubout`) — whitespace-delimited, so it eats punctuation
//! * `M-DEL` / `M-d` / `M-b` / `M-f` (`*-word`) — alphanumeric words
//!
//! No kill ring: killed text is gone.

#[derive(Debug, Default)]
pub struct Editor {
    text: String,
    /// Byte index, always on a character boundary.
    cursor: usize,
    history: Vec<String>,
    /// `None` while editing; `Some(i)` while browsing history.
    browsing: Option<usize>,
    stash: String,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl Editor {
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Cursor as `(row, byte column)`, both 0-based.
    pub fn row_col(&self) -> (usize, usize) {
        let before = &self.text[..self.cursor];
        let row = before.matches('\n').count();
        let col = self.cursor - before.rfind('\n').map_or(0, |i| i + 1);
        (row, col)
    }

    pub fn rows(&self) -> Vec<&str> {
        self.text.split('\n').collect()
    }

    pub fn start_fresh(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.browsing = None;
        self.stash.clear();
    }

    /// Record a submitted comment for `C-p` recall and reset for the next one.
    pub fn submit(&mut self) -> String {
        let out = self.text.clone();
        if !out.trim().is_empty() && self.history.last() != Some(&out) {
            self.history.push(out.clone());
        }
        self.start_fresh();
        out
    }

    #[cfg(test)]
    pub fn set(&mut self, s: &str) {
        self.text = s.to_string();
        self.cursor = self.text.len();
        self.browsing = None;
    }

    // ---- boundaries -----------------------------------------------------

    fn prev(&self, i: usize) -> usize {
        let mut i = i.min(self.text.len());
        if i == 0 {
            return 0;
        }
        i -= 1;
        while i > 0 && !self.text.is_char_boundary(i) {
            i -= 1;
        }
        i
    }

    fn next(&self, i: usize) -> usize {
        let mut i = i.min(self.text.len());
        if i >= self.text.len() {
            return self.text.len();
        }
        i += 1;
        while i < self.text.len() && !self.text.is_char_boundary(i) {
            i += 1;
        }
        i
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i)
    }

    fn char_before(&self, i: usize) -> Option<char> {
        self.text[..i].chars().next_back()
    }

    fn char_at(&self, i: usize) -> Option<char> {
        self.text[i..].chars().next()
    }

    // ---- movement -------------------------------------------------------

    // Movement does not end history browsing. `browsing` is documented as
    // "`None` while editing", and moving the cursor is not editing — the four
    // motions below never cleared it, and these two agreeing with them is what
    // keeps the stashed draft reachable. Clearing it here stranded the draft:
    // `history_next` returns early without a `browsing` index, and the next
    // `history_prev` overwrote the stash with whatever was on display.
    pub fn left(&mut self) {
        self.cursor = self.prev(self.cursor);
    }

    pub fn right(&mut self) {
        self.cursor = self.next(self.cursor);
    }

    /// `C-a`
    pub fn home(&mut self) {
        self.cursor = self.line_start();
    }

    /// `C-e`
    pub fn end(&mut self) {
        self.cursor = self.line_end();
    }

    /// `M-b`
    pub fn word_left(&mut self) {
        let mut i = self.cursor;
        while i > 0 && !self.char_before(i).is_some_and(is_word) {
            i = self.prev(i);
        }
        while i > 0 && self.char_before(i).is_some_and(is_word) {
            i = self.prev(i);
        }
        self.cursor = i;
    }

    /// `M-f`
    pub fn word_right(&mut self) {
        let mut i = self.cursor;
        let n = self.text.len();
        while i < n && !self.char_at(i).is_some_and(is_word) {
            i = self.next(i);
        }
        while i < n && self.char_at(i).is_some_and(is_word) {
            i = self.next(i);
        }
        self.cursor = i;
    }

    // ---- editing --------------------------------------------------------

    const fn browsing_off(&mut self) {
        self.browsing = None;
    }

    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.browsing_off();
    }

    /// `C-j`
    pub fn newline(&mut self) {
        self.insert('\n');
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let p = self.prev(self.cursor);
        self.text.replace_range(p..self.cursor, "");
        self.cursor = p;
        self.browsing_off();
    }

    /// `C-d`
    pub fn delete_forward(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let n = self.next(self.cursor);
        self.text.replace_range(self.cursor..n, "");
        self.browsing_off();
    }

    /// `C-k` — to end of line, or swallow the newline when already there.
    pub fn kill_to_end(&mut self) {
        let e = self.line_end();
        if e == self.cursor && e < self.text.len() {
            self.text.replace_range(e..=e, "");
        } else {
            self.text.replace_range(self.cursor..e, "");
        }
        self.browsing_off();
    }

    /// `C-u` — readline's `unix-line-discard`: back to the start of the line,
    /// leaving whatever sits after the cursor.
    pub fn kill_to_start(&mut self) {
        let s = self.line_start();
        self.text.replace_range(s..self.cursor, "");
        self.cursor = s;
        self.browsing_off();
    }

    /// `C-w` — whitespace-delimited, so it takes punctuation with it.
    pub fn kill_word_back_ws(&mut self) {
        let mut i = self.cursor;
        while i > 0 && self.char_before(i).is_some_and(char::is_whitespace) {
            i = self.prev(i);
        }
        while i > 0 && !self.char_before(i).is_some_and(char::is_whitespace) {
            i = self.prev(i);
        }
        self.text.replace_range(i..self.cursor, "");
        self.cursor = i;
        self.browsing_off();
    }

    /// `M-DEL`
    pub fn kill_word_back(&mut self) {
        let start = self.cursor;
        self.word_left();
        let i = self.cursor;
        self.text.replace_range(i..start, "");
        self.browsing_off();
    }

    /// `M-d`
    pub fn kill_word_forward(&mut self) {
        let start = self.cursor;
        self.word_right();
        let end = self.cursor;
        self.cursor = start;
        self.text.replace_range(start..end, "");
        self.browsing_off();
    }

    // ---- history --------------------------------------------------------

    /// `C-p`
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let i = match self.browsing {
            None => {
                self.stash = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.browsing = Some(i);
        self.text = self.history[i].clone();
        self.cursor = self.text.len();
    }

    /// `C-n`
    pub fn history_next(&mut self) {
        let Some(i) = self.browsing else { return };
        if i + 1 >= self.history.len() {
            self.browsing = None;
            self.text = std::mem::take(&mut self.stash);
        } else {
            self.browsing = Some(i + 1);
            self.text = self.history[i + 1].clone();
        }
        self.cursor = self.text.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ed(text: &str, cursor: usize) -> Editor {
        let mut e = Editor::default();
        e.set(text);
        e.cursor = cursor;
        e
    }

    /// Render as `text` with `|` marking the cursor, for readable assertions.
    fn show(e: &Editor) -> String {
        let mut s = e.text.clone();
        s.insert(e.cursor, '|');
        s
    }

    #[test]
    fn typing_and_backspace() {
        let mut e = Editor::default();
        for c in "abc".chars() {
            e.insert(c);
        }
        assert_eq!(show(&e), "abc|");
        e.backspace();
        assert_eq!(show(&e), "ab|");
        e.left();
        e.insert('X');
        assert_eq!(show(&e), "aX|b");
    }

    #[test]
    fn backspace_at_the_start_is_a_no_op() {
        let mut e = ed("abc", 0);
        e.backspace();
        assert_eq!(show(&e), "|abc");
    }

    #[test]
    fn ctrl_a_and_ctrl_e_are_line_local() {
        let mut e = ed("first line\nsecond line", 15);
        e.home();
        assert_eq!(show(&e), "first line\n|second line");
        e.end();
        assert_eq!(show(&e), "first line\nsecond line|");
        e.home();
        e.left(); // across the newline
        e.home();
        assert_eq!(show(&e), "|first line\nsecond line");
    }

    #[test]
    fn ctrl_j_inserts_a_newline_without_committing() {
        let mut e = Editor::default();
        for c in "one".chars() {
            e.insert(c);
        }
        e.newline();
        for c in "two".chars() {
            e.insert(c);
        }
        assert_eq!(e.text(), "one\ntwo");
        assert_eq!(e.rows(), vec!["one", "two"]);
        assert_eq!(e.row_col(), (1, 3));
    }

    #[test]
    fn ctrl_u_kills_to_line_start_and_keeps_the_tail() {
        let mut e = ed("hello world", 6);
        e.kill_to_start();
        assert_eq!(show(&e), "|world");
    }

    #[test]
    fn ctrl_u_on_the_second_line_leaves_the_first_alone() {
        let mut e = ed("keep me\ndrop this", 13);
        e.kill_to_start();
        assert_eq!(show(&e), "keep me\n|this");
    }

    #[test]
    fn ctrl_k_kills_to_end_then_swallows_the_newline() {
        let mut e = ed("hello world", 5);
        e.kill_to_end();
        assert_eq!(show(&e), "hello|");

        let mut e = ed("one\ntwo", 3);
        e.kill_to_end(); // already at line end -> join the lines
        assert_eq!(show(&e), "one|two");
    }

    #[test]
    fn ctrl_w_is_whitespace_delimited_and_takes_punctuation() {
        let mut e = ed("call foo.bar()", 14);
        e.kill_word_back_ws();
        assert_eq!(show(&e), "call |");
    }

    #[test]
    fn ctrl_w_skips_trailing_whitespace_first() {
        let mut e = ed("one two   ", 10);
        e.kill_word_back_ws();
        assert_eq!(show(&e), "one |");
    }

    #[test]
    fn meta_del_stops_at_punctuation_unlike_ctrl_w() {
        let mut e = ed("call foo.bar()", 14);
        e.kill_word_back();
        assert_eq!(show(&e), "call foo.|");
        e.kill_word_back();
        assert_eq!(show(&e), "call |");
    }

    #[test]
    fn meta_d_kills_the_word_ahead() {
        // From the space before a word, readline takes the space with it.
        let mut e = ed("delete this word", 6);
        e.kill_word_forward();
        assert_eq!(show(&e), "delete| word");

        // Sitting on the word itself, only the word goes.
        let mut e = ed("delete this word", 7);
        e.kill_word_forward();
        assert_eq!(show(&e), "delete | word");
    }

    #[test]
    fn ctrl_d_deletes_forward_and_stops_at_the_end() {
        let mut e = ed("abc", 1);
        e.delete_forward();
        assert_eq!(show(&e), "a|c");
        e.end();
        e.delete_forward();
        assert_eq!(show(&e), "ac|");
    }

    #[test]
    fn word_motions_move_without_editing() {
        let mut e = ed("alpha beta gamma", 16);
        e.word_left();
        assert_eq!(show(&e), "alpha beta |gamma");
        e.word_left();
        assert_eq!(show(&e), "alpha |beta gamma");
        e.word_right();
        assert_eq!(show(&e), "alpha beta| gamma");
    }

    #[test]
    fn every_operation_respects_character_boundaries() {
        let mut e = ed("Prüfen köde ✓", 0);
        e.right();
        e.right();
        e.right(); // P, r, ü
        assert_eq!(e.cursor, 4);
        e.backspace();
        assert_eq!(e.text(), "Prfen köde ✓");
        e.end();
        e.kill_word_back_ws();
        assert_eq!(e.text(), "Prfen köde ");
        e.kill_word_back_ws();
        assert_eq!(e.text(), "Prfen ");
    }

    #[test]
    fn history_recalls_previous_comments_and_returns() {
        let mut e = Editor::default();
        e.set("first");
        e.submit();
        e.set("second");
        e.submit();

        e.set("draft");
        e.history_prev();
        assert_eq!(e.text(), "second");
        e.history_prev();
        assert_eq!(e.text(), "first");
        e.history_prev(); // clamped at the oldest
        assert_eq!(e.text(), "first");
        e.history_next();
        assert_eq!(e.text(), "second");
        e.history_next(); // back to what was being typed
        assert_eq!(e.text(), "draft");
    }

    /// `left`/`right` cancelled browsing; `home`, `end`, `word_left` and
    /// `word_right` did not. Since `history_next` returns early without a
    /// `browsing` index, one `C-f` while browsing made the stashed draft
    /// unreachable by any key, and the next `C-p` overwrote the stash with
    /// whatever was on display — so the draft was gone for good. These are the
    /// keys sitting immediately next to the recall keys.
    #[test]
    fn a_cursor_motion_does_not_throw_away_the_stashed_draft() {
        for motion in [
            Editor::left,
            Editor::right,
            Editor::home,
            Editor::end,
            Editor::word_left,
            Editor::word_right,
        ] {
            let mut e = Editor::default();
            e.set("old comment");
            e.submit();

            e.set("my new draft");
            e.history_prev();
            assert_eq!(e.text(), "old comment");
            motion(&mut e);
            e.history_next();
            assert_eq!(e.text(), "my new draft", "draft lost by a cursor motion");
        }
    }

    #[test]
    fn history_ignores_blanks_and_immediate_repeats() {
        let mut e = Editor::default();
        e.set("  ");
        e.submit();
        e.set("same");
        e.submit();
        e.set("same");
        e.submit();
        e.history_prev();
        assert_eq!(e.text(), "same");
        e.history_prev();
        assert_eq!(e.text(), "same");
        assert_eq!(e.history.len(), 1);
    }

    #[test]
    fn typing_after_a_recall_stops_browsing() {
        let mut e = Editor::default();
        e.set("old");
        e.submit();
        e.set("new draft");
        e.history_prev();
        assert_eq!(e.text(), "old");
        e.insert('!');
        e.history_next(); // no longer browsing, so nothing is restored
        assert_eq!(e.text(), "old!");
    }

    #[test]
    fn submit_clears_the_buffer() {
        let mut e = Editor::default();
        e.set("something");
        assert_eq!(e.submit(), "something");
        assert!(e.text().is_empty());
        assert_eq!(e.cursor, 0);
    }

    #[test]
    fn operations_on_an_empty_buffer_never_panic() {
        let mut e = Editor::default();
        e.left();
        e.right();
        e.home();
        e.end();
        e.backspace();
        e.delete_forward();
        e.kill_to_end();
        e.kill_to_start();
        e.kill_word_back();
        e.kill_word_back_ws();
        e.kill_word_forward();
        e.word_left();
        e.word_right();
        e.history_prev();
        e.history_next();
        assert!(e.text().is_empty());
        assert!(e.text().trim().is_empty());
    }
}
