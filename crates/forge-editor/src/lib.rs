//! Minimal custom modal editor engine. Covers docs/mvp-plan.md editor scope:
//! rope buffer, Normal-mode navigation, Insert mode, basic ops
//! (`dd`/`yy`/`p`/`x`), `:w`/`:q`, single counts only (no counts yet).
//!
//! Deliberately has no GPUI/UI dependency: this crate owns text state and
//! motions only. Key-sequence dispatch (mapping raw keystrokes, including
//! multi-key pending ops like `dd`/`gg`, onto these methods) lives in the
//! `forge` binary crate. Full vim grammar (operator+motion composition,
//! text objects, registers, marks, macros) is a fast-follow — see
//! docs/engineering-plan.md section 4.4.

use std::path::PathBuf;

use ropey::Rope;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Command,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandOutcome {
    Stayed,
    Quit,
}

pub struct Editor {
    pub rope: Rope,
    pub path: PathBuf,
    pub cursor: usize,
    pub mode: Mode,
    pub register: String,
    pub command_line: String,
    pub dirty: bool,
    pub status: Option<String>,
    /// Sticky column for `j`/`k`, like vim's goal column: preserved across a
    /// run of vertical motions even when a shorter line clamps the cursor,
    /// and reset by any horizontal motion.
    desired_col: Option<usize>,
}

impl Editor {
    pub fn open(path: PathBuf) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        Ok(Self {
            rope: Rope::from_str(&text),
            path,
            cursor: 0,
            mode: Mode::Normal,
            register: String::new(),
            command_line: String::new(),
            dirty: false,
            status: None,
            desired_col: None,
        })
    }

    pub fn save(&mut self) {
        match std::fs::write(&self.path, self.rope.to_string()) {
            Ok(()) => {
                self.dirty = false;
                self.status = Some(format!("\"{}\" written", self.path.display()));
            }
            Err(err) => {
                self.status = Some(format!("write failed: {err}"));
            }
        }
    }

    // -- position helpers --------------------------------------------------

    fn line_idx(&self) -> usize {
        self.rope
            .char_to_line(self.cursor.min(self.rope.len_chars()))
    }

    fn line_start(&self, line: usize) -> usize {
        self.rope.line_to_char(line)
    }

    /// Length of a line's content, excluding its trailing newline.
    fn line_len(&self, line: usize) -> usize {
        let slice = self.rope.line(line);
        let len = slice.len_chars();
        if len > 0 && slice.char(len - 1) == '\n' {
            len - 1
        } else {
            len
        }
    }

    fn col(&self) -> usize {
        self.cursor - self.line_start(self.line_idx())
    }

    /// Max column the cursor may sit at on `line`: one past the end while
    /// inserting (to allow appending), clamped to the last character
    /// otherwise (vim's normal-mode behavior).
    fn max_col(&self, line: usize) -> usize {
        let len = self.line_len(line);
        if self.mode == Mode::Insert {
            len
        } else {
            len.saturating_sub(1)
        }
    }

    fn clamp_col(&self, line: usize, col: usize) -> usize {
        col.min(self.max_col(line))
    }

    pub fn cursor_line_col(&self) -> (usize, usize) {
        (self.line_idx(), self.col())
    }

    // -- motions -------------------------------------------------------------

    pub fn move_left(&mut self) {
        self.desired_col = None;
        if self.col() > 0 {
            self.cursor -= 1;
        }
    }

    pub fn move_right(&mut self) {
        self.desired_col = None;
        let line = self.line_idx();
        if self.col() < self.max_col(line) {
            self.cursor += 1;
        }
    }

    pub fn move_down(&mut self) {
        let line = self.line_idx();
        if line + 1 < self.rope.len_lines() {
            let current_col = self.col();
            let goal = *self.desired_col.get_or_insert(current_col);
            let new_line = line + 1;
            self.cursor = self.line_start(new_line) + self.clamp_col(new_line, goal);
        }
    }

    pub fn move_up(&mut self) {
        let line = self.line_idx();
        if line > 0 {
            let current_col = self.col();
            let goal = *self.desired_col.get_or_insert(current_col);
            let new_line = line - 1;
            self.cursor = self.line_start(new_line) + self.clamp_col(new_line, goal);
        }
    }

    pub fn move_line_start(&mut self) {
        self.desired_col = None;
        self.cursor = self.line_start(self.line_idx());
    }

    pub fn move_line_end(&mut self) {
        self.desired_col = None;
        let line = self.line_idx();
        self.cursor = self.line_start(line) + self.max_col(line);
    }

    pub fn move_doc_start(&mut self) {
        self.desired_col = None;
        self.cursor = 0;
    }

    pub fn move_doc_end(&mut self) {
        self.desired_col = None;
        let last = self.rope.len_lines().saturating_sub(1);
        self.cursor = self.line_start(last) + self.clamp_col(last, usize::MAX);
    }

    // -- mode transitions ------------------------------------------------

    pub fn enter_insert(&mut self) {
        self.mode = Mode::Insert;
    }

    pub fn enter_insert_after(&mut self) {
        let line = self.line_idx();
        let target = (self.col() + 1).min(self.line_len(line));
        self.cursor = self.line_start(line) + target;
        self.mode = Mode::Insert;
    }

    pub fn enter_insert_line_below(&mut self) {
        let line = self.line_idx();
        let at = self.line_start(line) + self.line_len(line);
        self.rope.insert_char(at, '\n');
        self.cursor = at + 1;
        self.mode = Mode::Insert;
        self.dirty = true;
    }

    pub fn enter_insert_line_above(&mut self) {
        let line = self.line_idx();
        let at = self.line_start(line);
        self.rope.insert_char(at, '\n');
        self.cursor = at;
        self.mode = Mode::Insert;
        self.dirty = true;
    }

    pub fn exit_insert(&mut self) {
        self.mode = Mode::Normal;
        let line = self.line_idx();
        self.cursor = self.line_start(line) + self.clamp_col(line, self.col());
    }

    pub fn enter_command(&mut self) {
        self.mode = Mode::Command;
        self.command_line.clear();
    }

    pub fn exit_command(&mut self) {
        self.mode = Mode::Normal;
        self.command_line.clear();
    }

    // -- editing -----------------------------------------------------------

    pub fn insert_char(&mut self, ch: char) {
        self.rope.insert_char(self.cursor, ch);
        self.cursor += 1;
        self.dirty = true;
    }

    pub fn insert_newline(&mut self) {
        self.rope.insert_char(self.cursor, '\n');
        self.cursor += 1;
        self.dirty = true;
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.rope.remove(self.cursor - 1..self.cursor);
            self.cursor -= 1;
            self.dirty = true;
        }
    }

    pub fn delete_char(&mut self) {
        let line = self.line_idx();
        if self.col() < self.line_len(line) {
            self.rope.remove(self.cursor..self.cursor + 1);
            self.dirty = true;
            let line = self.line_idx();
            self.cursor = self.line_start(line) + self.clamp_col(line, self.col());
        }
    }

    fn line_span_with_newline(&self, line: usize) -> std::ops::Range<usize> {
        let start = self.line_start(line);
        let end = if line + 1 < self.rope.len_lines() {
            self.line_start(line + 1)
        } else {
            self.rope.len_chars()
        };
        start..end
    }

    pub fn delete_line(&mut self) {
        let line = self.line_idx();
        let span = self.line_span_with_newline(line);
        self.register = self.rope.slice(span.clone()).to_string();
        self.rope.remove(span.clone());
        self.dirty = true;
        let line_count = self.rope.len_lines();
        let new_line = line.min(line_count.saturating_sub(1));
        self.cursor = self.line_start(new_line) + self.clamp_col(new_line, 0);
    }

    pub fn yank_line(&mut self) {
        let line = self.line_idx();
        let span = self.line_span_with_newline(line);
        self.register = self.rope.slice(span).to_string();
        self.status = Some("1 line yanked".to_string());
    }

    pub fn paste_after(&mut self) {
        if self.register.is_empty() {
            return;
        }
        let line = self.line_idx();
        let at = if line + 1 < self.rope.len_lines() {
            self.line_start(line + 1)
        } else {
            self.rope.len_chars()
        };
        self.rope.insert(at, &self.register);
        self.cursor = at;
        self.dirty = true;
    }

    // -- command line --------------------------------------------------------

    pub fn command_push(&mut self, ch: char) {
        self.command_line.push(ch);
    }

    pub fn command_backspace(&mut self) {
        self.command_line.pop();
    }

    pub fn execute_command(&mut self) -> CommandOutcome {
        let cmd = self.command_line.trim().to_string();
        let outcome = match cmd.as_str() {
            "w" => {
                self.save();
                CommandOutcome::Stayed
            }
            "q" => CommandOutcome::Quit,
            "wq" | "x" => {
                self.save();
                CommandOutcome::Quit
            }
            "" => CommandOutcome::Stayed,
            other => {
                self.status = Some(format!("unknown command: {other}"));
                CommandOutcome::Stayed
            }
        };
        self.mode = Mode::Normal;
        self.command_line.clear();
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor_with(text: &str) -> Editor {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let dir = std::env::temp_dir().join(format!("forge-editor-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("{id}.txt"));
        std::fs::write(&path, text).unwrap();
        Editor::open(path).unwrap()
    }

    #[test]
    fn opens_existing_content() {
        let editor = editor_with("hello\nworld\n");
        assert_eq!(editor.rope.to_string(), "hello\nworld\n");
        assert_eq!(editor.cursor, 0);
        assert_eq!(editor.mode, Mode::Normal);
    }

    #[test]
    fn normal_mode_horizontal_motion_clamps_to_last_char() {
        let mut editor = editor_with("abc\n");
        for _ in 0..10 {
            editor.move_right();
        }
        assert_eq!(editor.cursor_line_col(), (0, 2));
        editor.move_left();
        assert_eq!(editor.cursor_line_col(), (0, 1));
    }

    #[test]
    fn vertical_motion_clamps_column_to_shorter_line() {
        let mut editor = editor_with("abcdef\nxy\n");
        editor.move_right();
        editor.move_right();
        editor.move_right();
        assert_eq!(editor.cursor_line_col(), (0, 3));
        editor.move_down();
        assert_eq!(editor.cursor_line_col(), (1, 1));
        editor.move_up();
        assert_eq!(editor.cursor_line_col(), (0, 3));
    }

    #[test]
    fn insert_mode_allows_appending_past_last_char() {
        let mut editor = editor_with("ab\n");
        editor.move_right(); // cursor onto 'b' (col 1)
        editor.enter_insert_after();
        editor.insert_char('c');
        editor.exit_insert();
        assert_eq!(editor.rope.to_string(), "abc\n");
        assert_eq!(editor.cursor_line_col(), (0, 2));
    }

    #[test]
    fn dd_then_p_moves_the_line() {
        let mut editor = editor_with("one\ntwo\nthree\n");
        editor.move_down();
        editor.delete_line();
        assert_eq!(editor.rope.to_string(), "one\nthree\n");
        editor.paste_after();
        assert_eq!(editor.rope.to_string(), "one\nthree\ntwo\n");
    }

    #[test]
    fn yy_then_p_duplicates_the_line() {
        let mut editor = editor_with("only\n");
        editor.yank_line();
        editor.paste_after();
        assert_eq!(editor.rope.to_string(), "only\nonly\n");
    }

    #[test]
    fn command_w_saves_and_stays_command_q_quits() {
        let mut editor = editor_with("x\n");
        editor.insert_char('y');
        editor.command_line = "w".to_string();
        assert_eq!(editor.execute_command(), CommandOutcome::Stayed);
        assert!(!editor.dirty);
        let saved = std::fs::read_to_string(&editor.path).unwrap();
        assert_eq!(saved, "yx\n");

        editor.command_line = "q".to_string();
        assert_eq!(editor.execute_command(), CommandOutcome::Quit);
    }
}
