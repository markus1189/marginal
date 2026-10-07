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
//!
//! # History and the draft slot
//!
//! readline keeps the line you are composing in a slot of its own below the
//! oldest… newest history entries, so recalling an entry can never cost you
//! what you had typed. That slot is `stash`, and the model here is:
//!
//! * The draft is parked on the first `C-p` that leaves it, and **only** then.
//!   Nothing typed afterwards overwrites it.
//! * `C-n` past the newest entry puts it back and empties the slot; so does
//!   `submit` / `start_fresh`, which throw the whole buffer away anyway.
//! * `cancel` (Esc, `C-c`) empties it too, but into the history: the parked
//!   draft and any composed buffer become the newest entries, so a cancelled
//!   comment is one `C-p` away rather than gone.
//! * Editing a recalled entry ends browsing but does not touch the slot. The
//!   *edit* has no slot of its own: it lives as long as you stay on it and is
//!   gone the moment you walk away with `C-p`. The next `C-p` resumes from the
//!   newest entry rather than from the one that was edited.
//!
//! Losing an edit to a recalled comment is a simplification, deliberately
//! taken; losing the draft is data loss, and one `C-p` used to be enough.

#[derive(Debug, Default)]
pub struct Editor {
    text: String,
    /// Byte index, always on a character boundary.
    cursor: usize,
    history: Vec<String>,
    /// `None` while editing; `Some(i)` while browsing history.
    browsing: Option<usize>,
    /// The draft slot: `Some` from the `C-p` that parked the user's own line
    /// until the `C-n` that puts it back. Not the same question as `browsing`,
    /// which an edit turns off while the draft is still parked.
    stash: Option<String>,
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
        self.stash = None;
    }

    /// Record a submitted comment for `C-p` recall and reset for the next one.
    pub fn submit(&mut self) -> String {
        let out = self.text.clone();
        self.remember(&out);
        self.start_fresh();
        out
    }

    /// File `text` for `C-p` recall without it ever being in the buffer: the
    /// one-key answers commit text nobody typed here.
    pub fn remember(&mut self, text: &str) {
        if !text.trim().is_empty() && self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_string());
        }
    }

    /// Abandon the comment without losing it: whatever the user composed goes
    /// into the history, so the next comment's `C-p` brings it back. Returns
    /// whether anything was kept.
    ///
    /// Esc and `C-c` used to be `start_fresh`, and a cancel is one keystroke
    /// away from the keys it shares a hand with — a long multi-line draft went
    /// with no way back. What counts as "composed" is the draft slot's rule
    /// turned outward: the parked draft if one is parked, and the buffer unless
    /// it is a recalled entry nobody touched (that one is in the history
    /// already). Blanks and an immediate repeat are skipped, as in `submit`.
    pub fn cancel(&mut self) -> bool {
        let untouched_recall = self
            .browsing
            .is_some_and(|i| self.history.get(i) == Some(&self.text));
        let mut kept = false;
        let draft = self.stash.take();
        let shown = (!untouched_recall).then(|| std::mem::take(&mut self.text));
        for text in [draft, shown].into_iter().flatten() {
            if !text.trim().is_empty() && self.history.last() != Some(&text) {
                self.history.push(text);
                kept = true;
            }
        }
        self.start_fresh();
        kept
    }

    pub fn set(&mut self, s: &str) {
        self.text = s.to_string();
        self.cursor = self.text.len();
        self.browsing = None;
        self.stash = None;
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
    // keeps the parked draft one `C-n` away. Clearing it here took that away:
    // `history_next` returns early without a `browsing` index, so `C-n`
    // answered with nothing at all.
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

    /// Up: to the row above, at the same column counted in characters, or at
    /// that row's end if it is shorter. `false` on the first row, where there
    /// is no row to go to — the caller falls back to the history there, as
    /// fish and readline's multi-line mode do.
    pub fn up(&mut self) -> bool {
        let start = self.line_start();
        if start == 0 {
            return false;
        }
        let col = self.text[start..self.cursor].chars().count();
        let above = self.text[..start - 1].rfind('\n').map_or(0, |i| i + 1);
        self.cursor = self.at_column(above, start - 1, col);
        true
    }

    /// Down: the mirror of `up`; `false` on the last row.
    pub fn down(&mut self) -> bool {
        let end = self.line_end();
        if end == self.text.len() {
            return false;
        }
        let col = self.text[self.line_start()..self.cursor].chars().count();
        let below = end + 1;
        let below_end = self.text[below..]
            .find('\n')
            .map_or(self.text.len(), |i| below + i);
        self.cursor = self.at_column(below, below_end, col);
        true
    }

    /// The byte index of character `col` in the row `start..end`, or `end`.
    fn at_column(&self, start: usize, end: usize, col: usize) -> usize {
        self.text[start..end]
            .char_indices()
            .nth(col)
            .map_or(end, |(i, _)| start + i)
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

    /// Delete `start..end` and leave the cursor where the text was, ending
    /// history browsing — but only if there was anything there to delete.
    ///
    /// The invariant is that *editing* ends browsing, not that pressing an
    /// edit key does. Every kill below can be aimed at an empty range, and
    /// `history_prev` parks the cursor at `text.len()`, which is exactly where
    /// `C-k` and `M-d` have nothing to take. Clearing `browsing` there changed
    /// the screen not at all and stranded the draft: `history_next` returns
    /// early without a `browsing` index, so `C-n` answered with nothing.
    fn kill_range(&mut self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        self.text.replace_range(start..end, "");
        self.cursor = start;
        self.browsing_off();
    }

    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.browsing_off();
    }

    /// A bracketed paste, arriving as one string rather than as keys.
    ///
    /// Line endings are normalised to `\n` — a terminal sends `\r` for the
    /// Enter inside a paste, and CRLF text keeps both — and every other control
    /// character except the tab is dropped: an escape sequence copied out of a
    /// terminal has no business in a comment the agent will read as markdown.
    /// Like any insert it ends history browsing, but only if something was
    /// actually inserted.
    pub fn paste(&mut self, s: &str) {
        let clean: String = s
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .chars()
            .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
            .collect();
        if clean.is_empty() {
            return;
        }
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
        self.browsing_off();
    }

    /// `C-j`
    pub fn newline(&mut self) {
        self.insert('\n');
    }

    pub fn backspace(&mut self) {
        self.kill_range(self.prev(self.cursor), self.cursor);
    }

    /// `C-d`
    pub fn delete_forward(&mut self) {
        self.kill_range(self.cursor, self.next(self.cursor));
    }

    /// `C-k` — to end of line, or swallow the newline when already there.
    pub fn kill_to_end(&mut self) {
        let e = self.line_end();
        if e == self.cursor {
            self.kill_range(e, self.next(e));
        } else {
            self.kill_range(self.cursor, e);
        }
    }

    /// `C-u` — readline's `unix-line-discard`: back to the start of the line,
    /// leaving whatever sits after the cursor.
    pub fn kill_to_start(&mut self) {
        self.kill_range(self.line_start(), self.cursor);
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
        self.kill_range(i, self.cursor);
    }

    /// `M-DEL`
    pub fn kill_word_back(&mut self) {
        let start = self.cursor;
        self.word_left();
        self.kill_range(self.cursor, start);
    }

    /// `M-d`
    pub fn kill_word_forward(&mut self) {
        let start = self.cursor;
        self.word_right();
        let end = self.cursor;
        self.cursor = start;
        self.kill_range(start, end);
    }

    // ---- history --------------------------------------------------------

    /// `C-p`
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let i = match self.browsing {
            None => {
                // Park the draft — but only if the buffer still *is* the
                // draft. After an edit to a recalled comment `browsing` is
                // `None` again while the slot is still full, and what is on
                // screen is that comment, not anything the user composed.
                // Stashing it here overwrote the draft with an entry the
                // history already holds, and `C-n` handed that back instead:
                // one `C-p`, one keystroke, one more `C-p` and the draft was
                // gone with no key left to reach it.
                if self.stash.is_none() {
                    self.stash = Some(self.text.clone());
                }
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
            self.text = self.stash.take().unwrap_or_default();
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

    /// The other half of the same invariant. A kill aimed at an empty range
    /// deletes nothing and redraws the same screen, so it is not an edit — but
    /// all five kills cleared `browsing` unconditionally, and `history_prev`
    /// leaves the cursor at `text.len()`, which is precisely where `C-k` and
    /// `M-d` have nothing to take. `backspace` and `delete_forward` were the
    /// only two that already returned early. Each pair below is a real key
    /// sequence: the cursor sits where the recall put it, or on the line start
    /// after a `C-a`.
    #[test]
    fn a_kill_that_kills_nothing_does_not_throw_away_the_stashed_draft() {
        let home = Editor::home as fn(&mut Editor);
        for (name, aim, kill) in [
            (
                "C-k at the end",
                None,
                Editor::kill_to_end as fn(&mut Editor),
            ),
            ("M-d at the end", None, Editor::kill_word_forward),
            ("C-u at the start", Some(home), Editor::kill_to_start),
            ("M-DEL at the start", Some(home), Editor::kill_word_back),
            ("C-w at the start", Some(home), Editor::kill_word_back_ws),
            ("C-d at the end", None, Editor::delete_forward),
            ("BS at the start", Some(home), Editor::backspace),
        ] {
            let mut e = Editor::default();
            e.set("old comment");
            e.submit();

            e.set("my new draft");
            e.history_prev();
            assert_eq!(e.text(), "old comment");
            if let Some(motion) = aim {
                motion(&mut e);
            }
            kill(&mut e);
            assert_eq!(e.text(), "old comment", "{name} was not a no-op");
            e.history_next();
            assert_eq!(e.text(), "my new draft", "draft lost by {name}");
        }
    }

    /// …and a kill that does kill still ends browsing, so `C-n` restores
    /// nothing behind the user's back. This is the line the guard must not
    /// move: every one of these leaves visibly different text.
    #[test]
    fn a_kill_that_kills_something_still_ends_browsing() {
        for (name, kill) in [
            ("C-k", Editor::kill_to_end as fn(&mut Editor)),
            ("C-u", Editor::kill_to_start),
            ("C-w", Editor::kill_word_back_ws),
            ("M-DEL", Editor::kill_word_back),
            ("C-d", Editor::delete_forward),
            ("BS", Editor::backspace),
        ] {
            let mut e = Editor::default();
            e.set("old comment");
            e.submit();

            e.set("my new draft");
            e.history_prev();
            e.word_left(); // "old |comment" — every kill above has a target
            kill(&mut e);
            assert_ne!(e.text(), "old comment", "{name} killed nothing");
            let after = e.text().to_string();
            e.history_next();
            assert_eq!(e.text(), after, "{name} left browsing on");
        }

        // `M-d` needs the cursor before the word rather than after it.
        let mut e = Editor::default();
        e.set("old comment");
        e.submit();
        e.set("my new draft");
        e.history_prev();
        e.home();
        e.kill_word_forward();
        assert_eq!(e.text(), " comment");
        e.history_next();
        assert_eq!(e.text(), " comment", "M-d left browsing on");
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

    /// The draft slot, and the reason it is a slot rather than "whatever was in
    /// the buffer last time browsing started". `history_prev` stashed
    /// `self.text` on every entry with `browsing == None`, which is also the
    /// state one keystroke into an edit of a recalled comment — so the second
    /// `C-p` parked that comment over the draft, and `C-n` handed it straight
    /// back as if it were the user's own line:
    ///
    ///     "my draft" -> C-p -> "!" -> C-p -> C-n     used to yield "old!"
    ///
    /// with the draft gone and no key left to reach it. The slot now belongs to
    /// the draft alone: it is filled once, by the `C-p` that parks it.
    #[test]
    fn an_edit_to_a_recalled_comment_never_takes_the_drafts_slot() {
        let mut e = Editor::default();
        e.set("old");
        e.submit();

        e.set("my draft");
        e.history_prev();
        assert_eq!(e.text(), "old");
        e.insert('!'); // browsing off, draft still parked
        e.history_prev();
        assert_eq!(e.text(), "old", "the edit went into the history");
        e.history_next();
        assert_eq!(e.text(), "my draft", "the draft was overwritten");
    }

    /// Restoring the draft empties the slot, so the next walk parks the line as
    /// it stands now rather than handing back a stale copy of it.
    #[test]
    fn the_draft_slot_is_refilled_by_the_next_walk_through_the_history() {
        let mut e = Editor::default();
        e.set("first");
        e.submit();
        e.set("second");
        e.submit();

        e.set("draft one");
        e.history_prev();
        e.history_prev();
        assert_eq!(e.text(), "first");
        e.history_next();
        e.history_next();
        assert_eq!(e.text(), "draft one");

        // Same buffer, edited, and away again: the slot holds the new text.
        e.insert('!');
        e.history_prev();
        assert_eq!(e.text(), "second");
        e.history_next();
        assert_eq!(e.text(), "draft one!");
    }

    /// The half that is deliberately *not* kept. An edit to a recalled comment
    /// has no slot of its own — walking away with `C-p` discards it, and the
    /// walk restarts from the newest entry rather than resuming where the edit
    /// happened. Both are simplifications a full readline would not make; they
    /// cost an edit that is still on screen, not a draft that is not.
    #[test]
    fn walking_away_from_an_edited_comment_discards_the_edit() {
        let mut e = Editor::default();
        for c in ["first", "second"] {
            e.set(c);
            e.submit();
        }

        e.set("draft");
        e.history_prev();
        e.history_prev();
        assert_eq!(e.text(), "first");
        e.insert('!');
        assert_eq!(e.text(), "first!");

        e.history_prev();
        assert_eq!(e.text(), "second", "the walk did not restart at the newest");
        e.history_next();
        assert_eq!(e.text(), "draft");
        assert!(!e.history.iter().any(|h| h == "first!"));
    }

    /// A paste is text, whatever it contains: its line breaks become rows, not
    /// Enter, and it lands at the cursor as one insertion.
    #[test]
    fn a_paste_inserts_at_the_cursor_with_its_line_breaks_normalised() {
        let mut e = ed("see: .", 5);
        e.paste("one\r\ntwo\rthree\nfour");
        assert_eq!(show(&e), "see: one\ntwo\nthree\nfour|.");
        assert_eq!(e.rows().len(), 4);

        // Escape sequences and other controls go; tabs and non-ASCII stay.
        let mut e = Editor::default();
        e.paste("a\u{1b}[31mb\tü\u{7}");
        assert_eq!(show(&e), "a[31mb\tü|");
    }

    /// An empty paste is not an edit, so it cannot strand a parked draft —
    /// the same rule the kill keys follow.
    #[test]
    fn an_empty_paste_does_not_end_history_browsing() {
        let mut e = Editor::default();
        e.set("old");
        e.submit();
        e.set("draft");
        e.history_prev();
        e.paste("\u{1b}");
        e.history_next();
        assert_eq!(e.text(), "draft");

        e.history_prev();
        e.paste("!");
        e.history_next();
        assert_eq!(e.text(), "old!", "a real paste is an edit");
    }

    /// Esc threw a draft away for good. Now it is the newest history entry,
    /// line breaks and all, one `C-p` away in the next comment.
    #[test]
    fn a_cancelled_draft_is_one_recall_away() {
        let mut e = Editor::default();
        e.set("older");
        e.submit();
        e.set("a long\nmulti-line\ndraft");
        assert!(e.cancel());
        assert!(e.text().is_empty(), "cancel must still clear the buffer");

        e.history_prev();
        assert_eq!(e.text(), "a long\nmulti-line\ndraft");
        e.history_prev();
        assert_eq!(e.text(), "older", "the cancelled draft displaced history");
    }

    /// Cancelling while a recalled entry is on screen: the draft parked behind
    /// it is what the user composed, and it is kept; the recalled entry is
    /// already in the history and is not added twice. An *edited* recall is
    /// composition too, and is kept after the draft.
    #[test]
    fn cancelling_a_recall_keeps_the_parked_draft_not_a_duplicate() {
        let mut e = Editor::default();
        for c in ["first", "second"] {
            e.set(c);
            e.submit();
        }
        e.set("my draft");
        e.history_prev();
        e.history_prev();
        assert_eq!(e.text(), "first");
        assert!(e.cancel());
        assert_eq!(e.history, ["first", "second", "my draft"]);

        e.set("draft two");
        e.history_prev();
        e.insert('!');
        assert_eq!(e.text(), "my draft!");
        assert!(e.cancel());
        assert_eq!(
            e.history,
            ["first", "second", "my draft", "draft two", "my draft!"]
        );
    }

    /// Nothing composed, nothing kept: a blank buffer, an untouched recall of
    /// the newest entry, and a repeat of it all leave the history as it was.
    #[test]
    fn cancelling_nothing_keeps_nothing() {
        let mut e = Editor::default();
        e.set("  \n ");
        assert!(!e.cancel());
        e.set("same");
        e.submit();
        e.set("same");
        assert!(!e.cancel());
        e.history_prev();
        assert!(!e.cancel());
        assert_eq!(e.history, ["same"]);
    }

    /// Up/Down keep the column, clamp to a shorter row, count characters
    /// rather than bytes, and report `false` at the edges instead of moving.
    #[test]
    fn up_and_down_move_between_rows_and_stop_at_the_edges() {
        let mut e = ed("first row\nab\nthird row", 21); // "third ro|w"
        assert!(e.up());
        assert_eq!(
            show(&e),
            "first row\nab|\nthird row",
            "clamped to the row end"
        );
        assert!(e.up());
        assert_eq!(show(&e), "fi|rst row\nab\nthird row");
        assert!(!e.up(), "moved above the first row");
        assert_eq!(show(&e), "fi|rst row\nab\nthird row");
        assert!(e.down());
        assert!(e.down());
        assert_eq!(show(&e), "first row\nab\nth|ird row");
        assert!(!e.down(), "moved below the last row");

        // Characters, not bytes: two umlauts are two columns, four bytes.
        let mut e = ed("äöx\nabc", 4); // "äö|x"
        assert!(e.down());
        assert_eq!(show(&e), "äöx\nab|c");
        assert!(e.up());
        assert_eq!(show(&e), "äö|x\nabc");

        // Empty rows are rows.
        let mut e = ed("a\n\nb", 4);
        assert!(e.up());
        assert_eq!(show(&e), "a\n|\nb");
        assert!(!Editor::default().up() && !Editor::default().down());
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
