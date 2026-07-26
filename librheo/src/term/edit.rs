//! The line editor (docs/LIBRHEO.md Phase D): the reedline/rustyline-class core
//! a shell drives - character insertion, cursor movement, word/line kill,
//! history recall, and a completion hook. Pure logic over a `Vec<char>` buffer;
//! rendering is the [`render`](super::render) layer's job. Feed it decoded
//! [`Key`]s from [`KeyReader`](super::input::KeyReader) and it returns an [`Edit`]
//! telling the caller what changed.

use alloc::string::String;
use alloc::vec::Vec;

use super::input::Key;

/// What applying a [`Key`] did to the editor.
pub enum Edit {
    /// The line was committed (Enter). Carries the finished line.
    Commit(String),
    /// The buffer or cursor changed; the caller should repaint.
    Redraw,
    /// Nothing visible changed.
    Noop,
    /// End of input (Ctrl-D on an empty line).
    Eof,
}

/// A single-line editor with history and an optional completion hook. Holds the
/// line as `Vec<char>` so cursor moves and edits are by character, not byte.
pub struct LineEditor {
    buf: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    /// `None` while editing a fresh line; `Some(i)` while browsing `history[i]`.
    hist_pos: Option<usize>,
    /// The fresh line stashed while browsing history (restored on Down past the
    /// newest entry).
    stash: Vec<char>,
    /// Completion hook: given the current line, return the completed line.
    completer: Option<fn(&str) -> Option<String>>,
}

impl Default for LineEditor {
    fn default() -> Self {
        Self::new()
    }
}

impl LineEditor {
    pub fn new() -> LineEditor {
        LineEditor {
            buf: Vec::new(),
            cursor: 0,
            history: Vec::new(),
            hist_pos: None,
            stash: Vec::new(),
            completer: None,
        }
    }

    /// Install a completion hook (invoked on Tab).
    pub fn with_completer(mut self, f: fn(&str) -> Option<String>) -> LineEditor {
        self.completer = Some(f);
        self
    }

    /// The current line as a `String`.
    pub fn line(&self) -> String {
        self.buf.iter().collect()
    }

    /// The cursor position (character index within the line).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Apply one decoded key and report what changed.
    pub fn apply(&mut self, key: Key) -> Edit {
        match key {
            Key::Char(c) => {
                self.buf.insert(self.cursor, c);
                self.cursor += 1;
                Edit::Redraw
            }
            Key::Enter => {
                let line: String = self.buf.iter().collect();
                if !line.is_empty() {
                    self.history.push(line.clone());
                }
                self.buf.clear();
                self.cursor = 0;
                self.hist_pos = None;
                self.stash.clear();
                Edit::Commit(line)
            }
            Key::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.buf.remove(self.cursor);
                    Edit::Redraw
                } else {
                    Edit::Noop
                }
            }
            Key::Delete => {
                if self.cursor < self.buf.len() {
                    self.buf.remove(self.cursor);
                    Edit::Redraw
                } else {
                    Edit::Noop
                }
            }
            Key::Left => self.move_cursor(-1),
            Key::Right => self.move_cursor(1),
            Key::Home | Key::Ctrl('a') => {
                self.cursor = 0;
                Edit::Redraw
            }
            Key::End | Key::Ctrl('e') => {
                self.cursor = self.buf.len();
                Edit::Redraw
            }
            Key::Up => self.history_prev(),
            Key::Down => self.history_next(),
            // Kill to line start / end (readline ^U / ^K).
            Key::Ctrl('u') => {
                self.buf.drain(..self.cursor);
                self.cursor = 0;
                Edit::Redraw
            }
            Key::Ctrl('k') => {
                self.buf.truncate(self.cursor);
                Edit::Redraw
            }
            Key::Ctrl('w') => self.kill_word(),
            Key::Ctrl('d') => {
                if self.buf.is_empty() {
                    Edit::Eof
                } else if self.cursor < self.buf.len() {
                    self.buf.remove(self.cursor);
                    Edit::Redraw
                } else {
                    Edit::Noop
                }
            }
            Key::Tab => self.complete(),
            _ => Edit::Noop,
        }
    }

    fn move_cursor(&mut self, delta: isize) -> Edit {
        let new = self.cursor as isize + delta;
        if new >= 0 && new <= self.buf.len() as isize {
            self.cursor = new as usize;
            Edit::Redraw
        } else {
            Edit::Noop
        }
    }

    fn load_hist(&mut self, i: usize) {
        self.buf = self.history[i].chars().collect();
        self.cursor = self.buf.len();
    }

    /// Recall the previous (older) history entry.
    fn history_prev(&mut self) -> Edit {
        if self.history.is_empty() {
            return Edit::Noop;
        }
        let new = match self.hist_pos {
            None => {
                self.stash = core::mem::take(&mut self.buf);
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.hist_pos = Some(new);
        self.load_hist(new);
        Edit::Redraw
    }

    /// Recall the next (newer) history entry, or the stashed fresh line.
    fn history_next(&mut self) -> Edit {
        match self.hist_pos {
            None => Edit::Noop,
            Some(i) if i + 1 < self.history.len() => {
                self.hist_pos = Some(i + 1);
                self.load_hist(i + 1);
                Edit::Redraw
            }
            Some(_) => {
                self.hist_pos = None;
                self.buf = core::mem::take(&mut self.stash);
                self.cursor = self.buf.len();
                Edit::Redraw
            }
        }
    }

    /// Delete the word before the cursor (skip trailing spaces, then non-spaces).
    fn kill_word(&mut self) -> Edit {
        let mut i = self.cursor;
        while i > 0 && self.buf[i - 1] == ' ' {
            i -= 1;
        }
        while i > 0 && self.buf[i - 1] != ' ' {
            i -= 1;
        }
        if i == self.cursor {
            return Edit::Noop;
        }
        self.buf.drain(i..self.cursor);
        self.cursor = i;
        Edit::Redraw
    }

    fn complete(&mut self) -> Edit {
        if let Some(f) = self.completer {
            let line: String = self.buf.iter().collect();
            if let Some(sugg) = f(&line) {
                self.buf = sugg.chars().collect();
                self.cursor = self.buf.len();
                return Edit::Redraw;
            }
        }
        Edit::Noop
    }
}
