// Editor — Vim-like code editor for the TUI agent
//
// Core: buffer management, modes, cursor, editing, diff, save/revert.
// No rendering — that's in the TUI integration.
//
// Architecture:
//   Buffer   — Vec<String> lines, path, original content, dirty flag
//   Cursor   — row, col, scroll offset, preferred column
//   Mode     — Normal, Insert, Command, Visual, VisualLine
//   Diff     — computed between original and current buffer
//   Editor   — owns all state, exposes actions

#![allow(dead_code)]

use similar::{ChangeTag, DiffTag, TextDiff};
use std::path::{Path, PathBuf};

// ═══════════════════════════════════════════════════════════════
// TYPES
// ═══════════════════════════════════════════════════════════════

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Normal,
    Insert,
    Command,
    Visual,
    VisualLine,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize, // 0-indexed line
    pub col: usize, // 0-indexed byte offset (not char — for simplicity)
    pub scroll_row: usize,
    pub scroll_col: usize,
    pub preferred_col: usize, // for j/k movement to remember column
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    pub row: usize,
    pub col: usize,
}

#[derive(Clone, Debug)]
struct EditSnapshot {
    buffer: Vec<String>,
    cursor: Cursor,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DiffLine {
    Unchanged(String),
    Added(String),
    Removed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeContext {
    pub path: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
    pub selected: bool,
    pub symbol: Option<String>,
    pub focus_line: usize,
    pub focus_column: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReviewDecision {
    #[default]
    Pending,
    Accepted,
    Rejected,
}

impl ReviewDecision {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Clone, Debug)]
struct ReviewHunk {
    before: String,
    after: String,
    lines: Vec<DiffLine>,
    decision: ReviewDecision,
}

#[derive(Clone, Debug)]
enum ReviewSegment {
    Unchanged(String),
    Hunk(usize),
}

#[derive(Clone, Debug)]
pub struct ChangeReview {
    hunks: Vec<ReviewHunk>,
    segments: Vec<ReviewSegment>,
    pub selected_hunk: usize,
    pub before_existed: bool,
}

#[derive(Clone, Debug)]
pub struct ReviewDisplayLine {
    pub line: DiffLine,
    pub hunk: Option<usize>,
    pub selected: bool,
    pub decision: ReviewDecision,
}

impl ChangeReview {
    fn new(before: &str, after: &str, before_existed: bool) -> Self {
        let diff = TextDiff::from_lines(before, after);
        let mut hunks: Vec<ReviewHunk> = Vec::new();
        let mut segments = Vec::new();
        for operation in diff.ops() {
            if operation.tag() == DiffTag::Equal {
                let text = diff
                    .iter_changes(operation)
                    .map(|change| change.value())
                    .collect::<String>();
                if !text.is_empty() {
                    segments.push(ReviewSegment::Unchanged(text));
                }
                continue;
            }

            let mut before_text = String::new();
            let mut after_text = String::new();
            let mut lines = Vec::new();
            for change in diff.iter_changes(operation) {
                let display = change
                    .value()
                    .trim_end_matches('\n')
                    .trim_end_matches('\r')
                    .to_string();
                match change.tag() {
                    ChangeTag::Delete => {
                        before_text.push_str(change.value());
                        lines.push(DiffLine::Removed(display));
                    }
                    ChangeTag::Insert => {
                        after_text.push_str(change.value());
                        lines.push(DiffLine::Added(display));
                    }
                    ChangeTag::Equal => {
                        before_text.push_str(change.value());
                        after_text.push_str(change.value());
                        lines.push(DiffLine::Unchanged(display));
                    }
                }
            }

            if let Some(ReviewSegment::Hunk(index)) = segments.last().cloned() {
                let previous = &mut hunks[index];
                previous.before.push_str(&before_text);
                previous.after.push_str(&after_text);
                previous.lines.extend(lines);
            } else {
                let index = hunks.len();
                hunks.push(ReviewHunk {
                    before: before_text,
                    after: after_text,
                    lines,
                    decision: ReviewDecision::Pending,
                });
                segments.push(ReviewSegment::Hunk(index));
            }
        }
        Self {
            hunks,
            segments,
            selected_hunk: 0,
            before_existed,
        }
    }

    pub fn hunk_count(&self) -> usize {
        self.hunks.len()
    }

    pub fn resolved_count(&self) -> usize {
        self.hunks
            .iter()
            .filter(|hunk| hunk.decision != ReviewDecision::Pending)
            .count()
    }

    pub fn current_decision(&self) -> ReviewDecision {
        self.hunks
            .get(self.selected_hunk)
            .map_or(ReviewDecision::Pending, |hunk| hunk.decision)
    }

    pub fn select_relative(&mut self, delta: isize) {
        if self.hunks.is_empty() {
            self.selected_hunk = 0;
            return;
        }
        self.selected_hunk = if delta.is_negative() {
            self.selected_hunk.saturating_sub(delta.unsigned_abs())
        } else {
            self.selected_hunk
                .saturating_add(delta as usize)
                .min(self.hunks.len() - 1)
        };
    }

    pub fn decide_current(&mut self, decision: ReviewDecision) {
        if let Some(hunk) = self.hunks.get_mut(self.selected_hunk) {
            hunk.decision = decision;
        }
        if self.selected_hunk + 1 < self.hunks.len() {
            self.selected_hunk += 1;
        }
    }

    pub fn decide_all(&mut self, decision: ReviewDecision) {
        for hunk in &mut self.hunks {
            hunk.decision = decision;
        }
    }

    pub fn all_resolved(&self) -> bool {
        self.hunks
            .iter()
            .all(|hunk| hunk.decision != ReviewDecision::Pending)
    }

    pub fn result_exists(&self) -> bool {
        self.before_existed
            || self
                .hunks
                .iter()
                .any(|hunk| hunk.decision != ReviewDecision::Rejected)
    }

    pub fn result(&self) -> String {
        let mut output = String::new();
        for segment in &self.segments {
            match segment {
                ReviewSegment::Unchanged(text) => output.push_str(text),
                ReviewSegment::Hunk(index) => {
                    let hunk = &self.hunks[*index];
                    if hunk.decision == ReviewDecision::Rejected {
                        output.push_str(&hunk.before);
                    } else {
                        output.push_str(&hunk.after);
                    }
                }
            }
        }
        output
    }

    pub fn display_lines(&self) -> Vec<ReviewDisplayLine> {
        let mut lines = Vec::new();
        for segment in &self.segments {
            match segment {
                ReviewSegment::Unchanged(text) => {
                    lines.extend(text.lines().map(|line| ReviewDisplayLine {
                        line: DiffLine::Unchanged(line.to_string()),
                        hunk: None,
                        selected: false,
                        decision: ReviewDecision::Accepted,
                    }));
                }
                ReviewSegment::Hunk(index) => {
                    let hunk = &self.hunks[*index];
                    lines.extend(hunk.lines.iter().cloned().map(|line| ReviewDisplayLine {
                        line,
                        hunk: Some(*index),
                        selected: *index == self.selected_hunk,
                        decision: hunk.decision,
                    }));
                }
            }
        }
        lines
    }
}

#[derive(Clone, Debug)]
pub struct Editor {
    pub buffer: Vec<String>,
    pub file_path: Option<PathBuf>,
    pub original: Vec<String>,
    pub dirty: bool,
    pub cursor: Cursor,
    pub mode: Mode,
    pub cmd_buffer: String,
    pub status_msg: String,
    pub diff_lines: Vec<DiffLine>,
    pub show_diff: bool,
    pub search_term: String,
    pub search_matches: Vec<usize>, // line indices
    pub search_idx: usize,
    pub selection_anchor: Option<Position>,
    pub clipboard: String,
    pub change_review: Option<ChangeReview>,
    undo_stack: Vec<EditSnapshot>,
    redo_stack: Vec<EditSnapshot>,
}

impl Editor {
    pub fn new() -> Self {
        Self {
            buffer: vec![String::new()],
            file_path: None,
            original: vec![String::new()],
            dirty: false,
            cursor: Cursor::default(),
            mode: Mode::Normal,
            cmd_buffer: String::new(),
            status_msg: String::from("new file"),
            diff_lines: Vec::new(),
            show_diff: false,
            search_term: String::new(),
            search_matches: Vec::new(),
            search_idx: 0,
            selection_anchor: None,
            clipboard: String::new(),
            change_review: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        }
    }

    // ── File Operations ──────────────────────────────────────

    pub fn open(&mut self, path: &Path) -> std::io::Result<()> {
        let lines = read_file_lines(path)?;
        self.buffer = lines.clone();
        self.original = lines;
        self.file_path = Some(path.to_path_buf());
        self.dirty = false;
        self.cursor = Cursor::default();
        self.mode = Mode::Normal;
        self.selection_anchor = None;
        self.change_review = None;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.status_msg = format!("opened {}", path.display());
        self.clear_diff();
        Ok(())
    }

    pub fn refresh_external(&mut self, path: &Path) -> std::io::Result<()> {
        let next = read_file_lines(path)?;
        let previous = if self.file_path.as_deref() == Some(path) {
            self.buffer.clone()
        } else {
            Vec::new()
        };
        let external_diff = diff_lines(&previous, &next);

        self.buffer = next.clone();
        self.original = next;
        self.file_path = Some(path.to_path_buf());
        self.dirty = false;
        self.cursor.row = self.cursor.row.min(self.buffer.len().saturating_sub(1));
        self.cursor.col = self.cursor.col.min(self.buffer[self.cursor.row].len());
        self.clamp_cursor_to_boundary();
        self.mode = Mode::Normal;
        self.selection_anchor = None;
        self.change_review = None;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.diff_lines = external_diff;
        self.show_diff = self
            .diff_lines
            .iter()
            .any(|line| !matches!(line, DiffLine::Unchanged(_)));
        self.status_msg = format!("agent updated {} · :diff to close", path.display());
        Ok(())
    }

    pub fn reload(&mut self) -> std::io::Result<()> {
        let path = self.file_path.clone();
        if let Some(ref p) = path {
            self.open(p)?;
            self.status_msg = "reloaded".into();
        }
        Ok(())
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        if self.change_review.is_some() {
            self.status_msg = "resolve the agent change review before saving".into();
            return Ok(());
        }
        if let Some(ref path) = self.file_path {
            let content = self.buffer.join("\n");
            std::fs::write(path, content)?;
            self.original = self.buffer.clone();
            self.dirty = false;
            self.status_msg = format!("saved {}", path.display());
            self.clear_diff();
        } else {
            self.status_msg = "no file name; use :w <path>".into();
        }
        Ok(())
    }

    pub fn save_as(&mut self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        self.file_path = Some(path.to_path_buf());
        self.save()
    }

    pub fn revert(&mut self) {
        self.buffer = self.original.clone();
        self.dirty = false;
        self.cursor = Cursor::default();
        self.mode = Mode::Normal;
        self.selection_anchor = None;
        self.change_review = None;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.status_msg = "reverted to disk".into();
        self.clear_diff();
    }

    // ── Editing ──────────────────────────────────────────────

    pub fn insert_char(&mut self, c: char) {
        self.ensure_row();
        self.clamp_cursor_to_boundary();
        self.record_undo();
        let line = &mut self.buffer[self.cursor.row];
        if self.cursor.col <= line.len() {
            line.insert(self.cursor.col, c);
        } else {
            line.push(c);
        }
        self.cursor.col += c.len_utf8();
        self.cursor.preferred_col = self.cursor.col;
        self.mark_dirty();
    }

    pub fn insert_newline(&mut self) {
        self.ensure_row();
        self.clamp_cursor_to_boundary();
        self.record_undo();
        let line = &self.buffer[self.cursor.row];
        let rest = line[self.cursor.col..].to_string();
        self.buffer[self.cursor.row] = line[..self.cursor.col].to_string();
        self.buffer.insert(self.cursor.row + 1, rest);
        self.cursor.row += 1;
        self.cursor.col = 0;
        self.cursor.preferred_col = 0;
        self.mark_dirty();
    }

    pub fn delete_char(&mut self) {
        self.ensure_row();
        self.clamp_cursor_to_boundary();
        let line_len = self.buffer[self.cursor.row].len();
        if self.cursor.col < line_len {
            self.record_undo();
            self.buffer[self.cursor.row].remove(self.cursor.col);
            self.mark_dirty();
        } else if self.cursor.row + 1 < self.buffer.len() {
            self.record_undo();
            let next = self.buffer.remove(self.cursor.row + 1);
            self.buffer[self.cursor.row].push_str(&next);
            self.mark_dirty();
        }
    }

    pub fn backspace(&mut self) {
        self.ensure_row();
        self.clamp_cursor_to_boundary();
        if self.cursor.col > 0 {
            self.record_undo();
            let previous = previous_char_boundary(&self.buffer[self.cursor.row], self.cursor.col);
            self.buffer[self.cursor.row].remove(previous);
            self.cursor.col = previous;
            self.cursor.preferred_col = previous;
            self.mark_dirty();
        } else if self.cursor.row > 0 {
            self.record_undo();
            let current = self.buffer.remove(self.cursor.row);
            self.cursor.row -= 1;
            self.cursor.col = self.buffer[self.cursor.row].len();
            self.buffer[self.cursor.row].push_str(&current);
            self.mark_dirty();
        }
    }

    pub fn delete_line(&mut self) {
        self.ensure_row();
        if self.buffer.len() == 1 && self.buffer[0].is_empty() {
            return;
        }
        self.record_undo();
        if self.buffer.len() > 1 {
            self.buffer.remove(self.cursor.row);
            if self.cursor.row >= self.buffer.len() {
                self.cursor.row = self.buffer.len() - 1;
            }
            self.cursor.col = 0;
            self.mark_dirty();
        } else {
            self.buffer[0].clear();
            self.cursor.col = 0;
            self.mark_dirty();
        }
    }

    pub fn indent_line(&mut self) {
        self.ensure_row();
        self.record_undo();
        self.buffer[self.cursor.row].insert_str(0, "    ");
        self.cursor.col += 4;
        self.mark_dirty();
    }

    pub fn dedent_line(&mut self) {
        self.ensure_row();
        if self.buffer[self.cursor.row].starts_with("    ") {
            self.record_undo();
            let line = &self.buffer[self.cursor.row];
            let stripped = line.strip_prefix("    ").unwrap_or(line);
            self.buffer[self.cursor.row] = stripped.to_string();
            self.cursor.col = self.cursor.col.saturating_sub(4);
            self.mark_dirty();
        } else if self.buffer[self.cursor.row].starts_with('\t') {
            self.record_undo();
            self.buffer[self.cursor.row].remove(0);
            self.cursor.col = self.cursor.col.saturating_sub(1);
            self.mark_dirty();
        }
    }

    // ── Movement ─────────────────────────────────────────────

    pub fn move_left(&mut self) {
        if self.cursor.col > 0 {
            self.cursor.col = previous_char_boundary(self.current_line(), self.cursor.col);
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
            self.cursor.col = self.buffer[self.cursor.row].len();
        }
        self.cursor.preferred_col = self.cursor.col;
    }

    pub fn move_right(&mut self) {
        let line = self.current_line();
        let line_len = line.len();
        if self.cursor.col < line_len {
            self.cursor.col = next_char_boundary(line, self.cursor.col);
        } else if self.cursor.row + 1 < self.buffer.len() {
            self.cursor.row += 1;
            self.cursor.col = 0;
        }
        self.cursor.preferred_col = self.cursor.col;
    }

    pub fn move_up(&mut self, n: usize) {
        for _ in 0..n {
            if self.cursor.row > 0 {
                self.cursor.row -= 1;
            }
        }
        self.snap_col();
    }

    pub fn move_down(&mut self, n: usize) {
        for _ in 0..n {
            if self.cursor.row + 1 < self.buffer.len() {
                self.cursor.row += 1;
            }
        }
        self.snap_col();
    }

    pub fn move_to_start_of_line(&mut self) {
        self.cursor.col = 0;
        self.cursor.preferred_col = 0;
    }

    pub fn move_to_end_of_line(&mut self) {
        self.cursor.col = self.current_line().len();
        self.cursor.preferred_col = self.cursor.col;
    }

    pub fn move_to_first_line(&mut self) {
        self.cursor.row = 0;
        self.snap_col();
    }

    pub fn move_to_last_line(&mut self) {
        self.cursor.row = self.buffer.len().saturating_sub(1);
        self.snap_col();
    }

    pub fn move_to_line(&mut self, line: usize) {
        self.cursor.row = line.min(self.buffer.len().saturating_sub(1));
        self.snap_col();
    }

    pub fn page_up(&mut self, page_size: usize) {
        self.move_up(page_size);
    }

    pub fn page_down(&mut self, page_size: usize) {
        self.move_down(page_size);
    }

    pub fn word_forward(&mut self) {
        let line = self.current_line();
        let mut offset = self.cursor.col.min(line.len());
        if offset == line.len() && self.cursor.row + 1 < self.buffer.len() {
            self.cursor.row += 1;
            self.cursor.col = 0;
            self.cursor.preferred_col = 0;
            return;
        }
        while offset < line.len()
            && line[offset..]
                .chars()
                .next()
                .is_some_and(|character| !character.is_whitespace())
        {
            offset = next_char_boundary(line, offset);
        }
        while offset < line.len()
            && line[offset..]
                .chars()
                .next()
                .is_some_and(char::is_whitespace)
        {
            offset = next_char_boundary(line, offset);
        }
        self.cursor.col = offset;
        self.cursor.preferred_col = offset;
    }

    pub fn word_backward(&mut self) {
        let line = self.current_line();
        let mut offset = self.cursor.col.min(line.len());
        if offset == 0 && self.cursor.row > 0 {
            self.cursor.row -= 1;
            self.cursor.col = self.buffer[self.cursor.row].len();
            self.cursor.preferred_col = self.cursor.col;
            return;
        }
        while offset > 0 {
            let previous = previous_char_boundary(line, offset);
            let character = line[previous..offset].chars().next().unwrap_or(' ');
            if !character.is_whitespace() {
                break;
            }
            offset = previous;
        }
        while offset > 0 {
            let previous = previous_char_boundary(line, offset);
            let character = line[previous..offset].chars().next().unwrap_or(' ');
            if character.is_whitespace() {
                break;
            }
            offset = previous;
        }
        self.cursor.col = offset;
        self.cursor.preferred_col = offset;
    }

    pub fn set_cursor(&mut self, row: usize, col: usize) {
        self.cursor.row = row.min(self.buffer.len().saturating_sub(1));
        self.cursor.col = col.min(self.buffer[self.cursor.row].len());
        self.clamp_cursor_to_boundary();
        self.cursor.preferred_col = self.cursor.col;
    }

    // ── Selection and History ────────────────────────────────

    pub fn position(&self) -> Position {
        Position {
            row: self.cursor.row,
            col: self.cursor.col,
        }
    }

    pub fn begin_selection(&mut self, linewise: bool) {
        self.clamp_cursor_to_boundary();
        self.selection_anchor = Some(self.position());
        self.mode = if linewise {
            Mode::VisualLine
        } else {
            Mode::Visual
        };
        self.status_msg = if linewise {
            "VISUAL LINE".into()
        } else {
            "VISUAL".into()
        };
    }

    pub fn clear_selection(&mut self) {
        self.selection_anchor = None;
        if matches!(self.mode, Mode::Visual | Mode::VisualLine) {
            self.mode = Mode::Normal;
        }
    }

    pub fn selection_byte_range(&self, row: usize) -> Option<(usize, usize)> {
        let (start, end, linewise) = self.selection_bounds()?;
        if row < start.row || row > end.row {
            return None;
        }
        let line_len = self.buffer.get(row)?.len();
        if linewise {
            return Some((0, line_len));
        }
        let range_start = if row == start.row { start.col } else { 0 };
        let range_end = if row == end.row { end.col } else { line_len };
        Some((range_start.min(line_len), range_end.min(line_len)))
    }

    pub fn copy_selection(&mut self) -> bool {
        let Some(text) = self.selected_text() else {
            return false;
        };
        let line_count = text.matches('\n').count() + 1;
        self.clipboard = text;
        self.clear_selection();
        self.status_msg = format!("yanked {line_count} line(s)");
        true
    }

    pub fn delete_selection(&mut self) -> bool {
        let Some((start, end, linewise)) = self.selection_bounds() else {
            return false;
        };
        let Some(selected) = self.selected_text() else {
            return false;
        };

        self.record_undo();
        self.clipboard = selected;
        if linewise {
            self.buffer.drain(start.row..=end.row);
            if self.buffer.is_empty() {
                self.buffer.push(String::new());
            }
            self.set_cursor(start.row.min(self.buffer.len() - 1), 0);
        } else if start.row == end.row {
            self.buffer[start.row].replace_range(start.col..end.col, "");
            self.set_cursor(start.row, start.col);
        } else {
            let prefix = self.buffer[start.row][..start.col].to_string();
            let suffix = self.buffer[end.row][end.col..].to_string();
            self.buffer.drain(start.row..=end.row);
            self.buffer.insert(start.row, format!("{prefix}{suffix}"));
            self.set_cursor(start.row, start.col);
        }
        self.clear_selection();
        self.mark_dirty();
        self.status_msg = "selection deleted".into();
        true
    }

    pub fn paste(&mut self) -> bool {
        if self.clipboard.is_empty() {
            self.status_msg = "clipboard is empty".into();
            return false;
        }
        self.ensure_row();
        self.clamp_cursor_to_boundary();
        self.record_undo();

        let text = self.clipboard.clone();
        let parts: Vec<&str> = text.split('\n').collect();
        let row = self.cursor.row;
        let col = self.cursor.col;
        let suffix = self.buffer[row][col..].to_string();
        self.buffer[row].truncate(col);
        self.buffer[row].push_str(parts[0]);

        if parts.len() == 1 {
            self.cursor.col = col + parts[0].len();
            self.buffer[row].push_str(&suffix);
        } else {
            let mut insert_at = row + 1;
            for part in &parts[1..parts.len() - 1] {
                self.buffer.insert(insert_at, (*part).into());
                insert_at += 1;
            }
            let last = parts.last().copied().unwrap_or_default();
            self.buffer.insert(insert_at, format!("{last}{suffix}"));
            self.cursor.row = insert_at;
            self.cursor.col = last.len();
        }
        self.cursor.preferred_col = self.cursor.col;
        self.mark_dirty();
        self.status_msg = "pasted".into();
        true
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    pub fn undo(&mut self) -> bool {
        let Some(snapshot) = self.undo_stack.pop() else {
            self.status_msg = "nothing to undo".into();
            return false;
        };
        self.redo_stack.push(self.snapshot());
        self.restore_snapshot(snapshot);
        self.status_msg = "undo".into();
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(snapshot) = self.redo_stack.pop() else {
            self.status_msg = "nothing to redo".into();
            return false;
        };
        self.undo_stack.push(self.snapshot());
        self.restore_snapshot(snapshot);
        self.status_msg = "redo".into();
        true
    }

    // ── Search ───────────────────────────────────────────────

    pub fn search(&mut self, term: &str) {
        self.search_term = term.to_string();
        self.search_matches.clear();
        let lower = term.to_lowercase();
        for (i, line) in self.buffer.iter().enumerate() {
            if line.to_lowercase().contains(&lower) {
                self.search_matches.push(i);
            }
        }
        self.search_idx = 0;
        if !self.search_matches.is_empty() {
            self.cursor.row = self.search_matches[0];
            self.snap_col();
        }
    }

    pub fn search_next(&mut self) {
        if self.search_matches.is_empty() {
            return;
        }
        self.search_idx = (self.search_idx + 1) % self.search_matches.len();
        self.cursor.row = self.search_matches[self.search_idx];
        self.snap_col();
    }

    pub fn search_prev(&mut self) {
        if self.search_matches.is_empty() {
            return;
        }
        if self.search_idx == 0 {
            self.search_idx = self.search_matches.len() - 1;
        } else {
            self.search_idx -= 1;
        }
        self.cursor.row = self.search_matches[self.search_idx];
        self.snap_col();
    }

    // ── Diff ─────────────────────────────────────────────────

    pub fn compute_diff(&mut self) {
        self.diff_lines = diff_lines(&self.original, &self.buffer);
        self.show_diff = true;
    }

    pub fn clear_diff(&mut self) {
        self.diff_lines.clear();
        self.show_diff = false;
    }

    // ── Agent Change Review ─────────────────────────────────

    pub fn begin_change_review(&mut self, path: &Path, before: Option<&str>, after: &str) {
        let before_text = before.unwrap_or_default();
        let review = ChangeReview::new(before_text, after, before.is_some());
        self.buffer = text_lines(after);
        self.original = text_lines(before_text);
        self.file_path = Some(path.to_path_buf());
        self.dirty = false;
        self.cursor = Cursor::default();
        self.mode = Mode::Normal;
        self.selection_anchor = None;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.diff_lines = review
            .display_lines()
            .into_iter()
            .map(|line| line.line)
            .collect();
        self.show_diff = true;
        self.status_msg = format!(
            "agent change review · {} hunk(s) · a accept · r reject",
            review.hunk_count()
        );
        self.change_review = Some(review);
    }

    pub fn review_display_lines(&self) -> Option<Vec<ReviewDisplayLine>> {
        self.change_review.as_ref().map(ChangeReview::display_lines)
    }

    pub fn review_result(&self) -> Option<String> {
        self.change_review.as_ref().map(ChangeReview::result)
    }

    pub fn review_all_resolved(&self) -> bool {
        self.change_review
            .as_ref()
            .is_some_and(ChangeReview::all_resolved)
    }

    pub fn review_result_exists(&self) -> bool {
        self.change_review
            .as_ref()
            .is_some_and(ChangeReview::result_exists)
    }

    pub fn decide_review_hunk(&mut self, decision: ReviewDecision) -> bool {
        let Some(review) = &mut self.change_review else {
            return false;
        };
        review.decide_current(decision);
        self.status_msg = format!(
            "review {}/{} resolved · current {}",
            review.resolved_count(),
            review.hunk_count(),
            review.current_decision().label()
        );
        true
    }

    pub fn decide_all_review_hunks(&mut self, decision: ReviewDecision) -> bool {
        let Some(review) = &mut self.change_review else {
            return false;
        };
        review.decide_all(decision);
        self.status_msg = format!("all {} hunk(s) {}", review.hunk_count(), decision.label());
        true
    }

    pub fn move_review_hunk(&mut self, delta: isize) -> bool {
        let Some(review) = &mut self.change_review else {
            return false;
        };
        review.select_relative(delta);
        self.status_msg = format!(
            "hunk {}/{} · {}",
            review.selected_hunk.saturating_add(1),
            review.hunk_count(),
            review.current_decision().label()
        );
        true
    }

    pub fn finish_change_review(&mut self, final_text: &str, status: impl Into<String>) {
        self.buffer = text_lines(final_text);
        self.original = self.buffer.clone();
        self.dirty = false;
        self.cursor.row = self.cursor.row.min(self.buffer.len().saturating_sub(1));
        self.cursor.col = self.cursor.col.min(self.buffer[self.cursor.row].len());
        self.clamp_cursor_to_boundary();
        self.change_review = None;
        self.clear_diff();
        self.status_msg = status.into();
    }

    // ── Agent Context and Language Intelligence ─────────────

    pub fn text(&self) -> String {
        self.buffer.join("\n")
    }

    pub fn code_context(&self) -> Option<CodeContext> {
        let path = self.file_path.clone()?;
        let selected =
            matches!(self.mode, Mode::Visual | Mode::VisualLine) && self.selection_anchor.is_some();
        let (start_line, end_line, content) = if selected {
            let (start, end, _) = self.selection_bounds()?;
            (start.row + 1, end.row + 1, self.selected_text()?)
        } else {
            (1, self.buffer.len().max(1), self.text())
        };
        let symbol_row = if selected {
            start_line.saturating_sub(1)
        } else {
            self.cursor.row
        };
        Some(CodeContext {
            path,
            start_line,
            end_line,
            content,
            selected,
            symbol: symbol_near(&self.buffer, symbol_row),
            focus_line: self.cursor.row + 1,
            focus_column: self.buffer[self.cursor.row][..self.cursor.col]
                .chars()
                .count()
                + 1,
        })
    }

    pub fn apply_completion(
        &mut self,
        completion: &crate::lsp::CompletionItem,
    ) -> Result<(), String> {
        let range = completion.range.unwrap_or_else(|| {
            let line = &self.buffer[self.cursor.row];
            let end = self.cursor.col.min(line.len());
            let start = line[..end]
                .char_indices()
                .next_back()
                .filter(|(_, character)| !character.is_alphanumeric() && *character != '_')
                .map_or(0, |(index, character)| index + character.len_utf8());
            crate::lsp::Range {
                start: crate::lsp::Position {
                    line: self.cursor.row,
                    character: crate::lsp::utf16_column(line, start),
                },
                end: crate::lsp::Position {
                    line: self.cursor.row,
                    character: crate::lsp::utf16_column(line, end),
                },
            }
        });
        self.replace_utf16_range(range, &completion.new_text)?;
        self.status_msg = format!("completed {}", completion.label);
        Ok(())
    }

    pub fn replace_utf16_range(
        &mut self,
        range: crate::lsp::Range,
        replacement: &str,
    ) -> Result<(), String> {
        if range.start.line > range.end.line || range.end.line >= self.buffer.len() {
            return Err("language-server edit is outside the current buffer".into());
        }
        let start_col =
            crate::lsp::byte_column(&self.buffer[range.start.line], range.start.character);
        let end_col = crate::lsp::byte_column(&self.buffer[range.end.line], range.end.character);
        if range.start.line == range.end.line && start_col > end_col {
            return Err("language-server edit range is reversed".into());
        }

        self.record_undo();
        let prefix = self.buffer[range.start.line][..start_col].to_string();
        let suffix = self.buffer[range.end.line][end_col..].to_string();
        let mut replacement_lines = replacement
            .split('\n')
            .map(str::to_string)
            .collect::<Vec<_>>();
        if replacement_lines.is_empty() {
            replacement_lines.push(String::new());
        }
        replacement_lines[0] = format!("{prefix}{}", replacement_lines[0]);
        let last = replacement_lines.len() - 1;
        replacement_lines[last].push_str(&suffix);
        let replacement_end_col = replacement_lines[last].len().saturating_sub(suffix.len());
        self.buffer
            .splice(range.start.line..=range.end.line, replacement_lines);
        self.cursor.row = range.start.line + last;
        self.cursor.col = replacement_end_col;
        self.cursor.preferred_col = self.cursor.col;
        self.mark_dirty();
        Ok(())
    }

    // ── AI Edit Support ──────────────────────────────────────

    /// Proposed edit: (start_row, start_col, end_row, end_col, replacement_text).
    /// Does NOT apply yet — returns the diff for preview.
    pub fn propose_edit(
        &mut self,
        start_row: usize,
        _start_col: usize,
        end_row: usize,
        _end_col: usize,
        replacement: &str,
    ) -> Vec<DiffLine> {
        let mut preview = self.buffer.clone();
        let end = end_row.min(preview.len().saturating_sub(1));
        // Remove old lines
        if start_row <= end {
            let drain_end = (end + 1).min(preview.len());
            preview.drain(start_row..drain_end);
        }
        // Insert new lines
        let new_lines: Vec<String> = replacement.lines().map(|s| s.to_string()).collect();
        for (i, line) in new_lines.iter().enumerate() {
            preview.insert(start_row + i, line.clone());
        }
        if preview.is_empty() {
            preview.push(String::new());
        }
        diff_lines(&self.buffer, &preview)
    }

    /// Accept the proposed edit: applies it to the buffer.
    pub fn accept_edit(
        &mut self,
        start_row: usize,
        _start_col: usize,
        end_row: usize,
        _end_col: usize,
        replacement: &str,
    ) {
        self.record_undo();
        let end = end_row.min(self.buffer.len().saturating_sub(1));
        if start_row <= end {
            let drain_end = (end + 1).min(self.buffer.len());
            self.buffer.drain(start_row..drain_end);
        }
        let new_lines: Vec<String> = replacement.lines().map(|s| s.to_string()).collect();
        if new_lines.is_empty() {
            if self.buffer.is_empty() {
                self.buffer.push(String::new());
            }
        } else {
            for (i, line) in new_lines.iter().enumerate() {
                self.buffer.insert(start_row + i, line.clone());
            }
        }
        self.cursor.row = start_row;
        self.cursor.col = 0;
        self.mark_dirty();
    }

    /// Reject edit: does nothing, just clears any preview state.
    pub fn reject_edit(&mut self) {
        self.clear_diff();
    }

    // ── Word Completion ──────────────────────────────────────

    /// Returns words from the buffer that start with the given prefix.
    pub fn complete_word(&self, prefix: &str) -> Vec<String> {
        if prefix.is_empty() {
            return Vec::new();
        }
        let lower = prefix.to_lowercase();
        let mut words: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for line in &self.buffer {
            for word in line.split_whitespace() {
                let w = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
                if w.to_lowercase().starts_with(&lower) && w != prefix && seen.insert(w.to_string())
                {
                    words.push(w.to_string());
                }
            }
        }
        words.sort();
        words.truncate(20);
        words
    }

    pub fn complete_at_cursor(&mut self) -> Option<String> {
        let line = self.current_line();
        let end = self.cursor.col.min(line.len());
        if !line.is_char_boundary(end) {
            self.status_msg = "completion unavailable at this cursor position".into();
            return None;
        }
        let start = line[..end]
            .rfind(|c: char| !c.is_alphanumeric() && c != '_')
            .map(|index| index + 1)
            .unwrap_or(0);
        let prefix = line[start..end].to_string();
        let completion = self.complete_word(&prefix).into_iter().next()?;
        let suffix = completion.strip_prefix(&prefix)?.to_string();
        self.record_undo();
        self.buffer[self.cursor.row].insert_str(self.cursor.col, &suffix);
        self.cursor.col += suffix.len();
        self.cursor.preferred_col = self.cursor.col;
        self.mark_dirty();
        self.status_msg = format!("completed {completion}");
        Some(completion)
    }

    // ── Helpers ──────────────────────────────────────────────

    fn current_line(&self) -> &str {
        &self.buffer[self.cursor.row]
    }

    fn ensure_row(&mut self) {
        if self.cursor.row >= self.buffer.len() {
            self.cursor.row = self.buffer.len().saturating_sub(1);
        }
    }

    fn snap_col(&mut self) {
        let line_len = self.buffer[self.cursor.row].len();
        self.cursor.col = clamp_to_char_boundary(
            &self.buffer[self.cursor.row],
            self.cursor.preferred_col.min(line_len),
        );
    }

    fn selection_bounds(&self) -> Option<(Position, Position, bool)> {
        if !matches!(self.mode, Mode::Visual | Mode::VisualLine) {
            return None;
        }
        let anchor = self.selection_anchor?;
        let current = self.position();
        let (mut start, mut end) = if anchor <= current {
            (anchor, current)
        } else {
            (current, anchor)
        };
        start.row = start.row.min(self.buffer.len().saturating_sub(1));
        end.row = end.row.min(self.buffer.len().saturating_sub(1));

        let linewise = self.mode == Mode::VisualLine;
        if linewise {
            start.col = 0;
            end.col = self.buffer[end.row].len();
        } else {
            start.col = clamp_to_char_boundary(
                &self.buffer[start.row],
                start.col.min(self.buffer[start.row].len()),
            );
            end.col = clamp_to_char_boundary(
                &self.buffer[end.row],
                end.col.min(self.buffer[end.row].len()),
            );
            if end.col < self.buffer[end.row].len() {
                end.col = next_char_boundary(&self.buffer[end.row], end.col);
            }
        }
        Some((start, end, linewise))
    }

    fn selected_text(&self) -> Option<String> {
        let (start, end, _) = self.selection_bounds()?;
        let mut lines = Vec::with_capacity(end.row.saturating_sub(start.row) + 1);
        for row in start.row..=end.row {
            let (range_start, range_end) = self.selection_byte_range(row)?;
            lines.push(self.buffer[row][range_start..range_end].to_string());
        }
        Some(lines.join("\n"))
    }

    fn snapshot(&self) -> EditSnapshot {
        EditSnapshot {
            buffer: self.buffer.clone(),
            cursor: self.cursor.clone(),
        }
    }

    fn record_undo(&mut self) {
        const HISTORY_LIMIT: usize = 200;
        let snapshot = self.snapshot();
        if self
            .undo_stack
            .last()
            .is_none_or(|previous| previous.buffer != snapshot.buffer)
        {
            self.undo_stack.push(snapshot);
            if self.undo_stack.len() > HISTORY_LIMIT {
                self.undo_stack.remove(0);
            }
        }
        self.redo_stack.clear();
    }

    fn restore_snapshot(&mut self, snapshot: EditSnapshot) {
        self.buffer = snapshot.buffer;
        if self.buffer.is_empty() {
            self.buffer.push(String::new());
        }
        self.cursor = snapshot.cursor;
        self.cursor.row = self.cursor.row.min(self.buffer.len() - 1);
        self.clamp_cursor_to_boundary();
        self.mode = Mode::Normal;
        self.selection_anchor = None;
        self.dirty = self.buffer != self.original;
        self.clear_diff();
    }

    fn clamp_cursor_to_boundary(&mut self) {
        self.ensure_row();
        let line = &self.buffer[self.cursor.row];
        self.cursor.col = clamp_to_char_boundary(line, self.cursor.col.min(line.len()));
        self.cursor.preferred_col = self.cursor.col;
    }

    fn mark_dirty(&mut self) {
        self.dirty = self.buffer != self.original;
        self.clear_diff();
    }
}

fn clamp_to_char_boundary(line: &str, mut col: usize) -> usize {
    col = col.min(line.len());
    while col > 0 && !line.is_char_boundary(col) {
        col -= 1;
    }
    col
}

fn previous_char_boundary(line: &str, col: usize) -> usize {
    let col = clamp_to_char_boundary(line, col);
    line[..col]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next_char_boundary(line: &str, col: usize) -> usize {
    let col = clamp_to_char_boundary(line, col);
    line[col..]
        .chars()
        .next()
        .map_or(line.len(), |character| col + character.len_utf8())
}

fn read_file_lines(path: &Path) -> std::io::Result<Vec<String>> {
    let content = std::fs::read_to_string(path)?;
    Ok(text_lines(&content))
}

fn text_lines(content: &str) -> Vec<String> {
    if content.is_empty() {
        vec![String::new()]
    } else {
        content.lines().map(str::to_string).collect()
    }
}

fn symbol_near(lines: &[String], row: usize) -> Option<String> {
    const PREFIXES: [&str; 9] = [
        "fn ", "struct ", "enum ", "trait ", "impl ", "mod ", "type ", "const ", "static ",
    ];
    for line in lines.iter().take(row.saturating_add(1)).rev() {
        let mut candidate = line.trim_start();
        for qualifier in ["pub(crate) ", "pub(super) ", "pub ", "async ", "unsafe "] {
            candidate = candidate.strip_prefix(qualifier).unwrap_or(candidate);
        }
        if let Some(prefix) = PREFIXES
            .iter()
            .find(|prefix| candidate.starts_with(**prefix))
        {
            let name = candidate[prefix.len()..]
                .split(|character: char| {
                    character.is_whitespace()
                        || matches!(character, '(' | '<' | '{' | ':' | '=' | ';')
                })
                .next()
                .unwrap_or_default();
            if !name.is_empty() {
                return Some(format!("{} {name}", prefix.trim_end()));
            }
        }
    }
    None
}

// ═══════════════════════════════════════════════════════════════
// DIFF ALGORITHM (line-by-line)
// ═══════════════════════════════════════════════════════════════

fn diff_lines(original: &[String], current: &[String]) -> Vec<DiffLine> {
    let mut result = Vec::new();
    let mut o = 0;
    let mut c = 0;

    while o < original.len() || c < current.len() {
        if o < original.len() && c < current.len() && original[o] == current[c] {
            result.push(DiffLine::Unchanged(current[c].clone()));
            o += 1;
            c += 1;
        } else if o < original.len() && c < current.len() {
            // Look ahead for resync
            let mut found = false;
            for look in 1..=5 {
                if o + look < original.len() && original[o + look] == current[c] {
                    for i in 0..look {
                        result.push(DiffLine::Removed(original[o + i].clone()));
                    }
                    o += look;
                    found = true;
                    break;
                }
                if c + look < current.len() && original[o] == current[c + look] {
                    for i in 0..look {
                        result.push(DiffLine::Added(current[c + i].clone()));
                    }
                    c += look;
                    found = true;
                    break;
                }
            }
            if !found {
                result.push(DiffLine::Removed(original[o].clone()));
                result.push(DiffLine::Added(current[c].clone()));
                o += 1;
                c += 1;
            }
        } else if o < original.len() {
            result.push(DiffLine::Removed(original[o].clone()));
            o += 1;
        } else {
            result.push(DiffLine::Added(current[c].clone()));
            c += 1;
        }
    }

    result
}

// ═══════════════════════════════════════════════════════════════
// FILE TREE
// ═══════════════════════════════════════════════════════════════

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub children: Vec<FileEntry>,
    pub expanded: bool,
}

pub fn build_file_tree(root: &Path, depth: usize) -> Vec<FileEntry> {
    if depth > 4 {
        return Vec::new();
    } // limit recursion
    let mut entries = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(root) {
        for entry in read_dir.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            // Skip hidden files/dirs
            if name.starts_with('.') && name != "." {
                continue;
            }
            // Skip target, node_modules, .git
            if name == "target" || name == "node_modules" || name == ".git" {
                continue;
            }
            let is_dir = path.is_dir();
            let children = if is_dir {
                build_file_tree(&path, depth + 1)
            } else {
                Vec::new()
            };
            entries.push(FileEntry {
                name,
                path,
                is_dir,
                children,
                expanded: false,
            });
        }
    }
    entries.sort_by(|a, b| {
        if a.is_dir != b.is_dir {
            b.is_dir.cmp(&a.is_dir)
        }
        // dirs first
        else {
            a.name.to_lowercase().cmp(&b.name.to_lowercase())
        }
    });
    entries
}

/// Flatten tree to display list: returns (indent_level, entry) pairs
pub fn flatten_tree(entries: &[FileEntry], depth: usize) -> Vec<(usize, FileEntry)> {
    let mut result = Vec::new();
    for entry in entries {
        result.push((depth, entry.clone()));
        if entry.expanded && !entry.children.is_empty() {
            result.extend(flatten_tree(&entry.children, depth + 1));
        }
    }
    result
}

pub fn set_tree_expanded(entries: &mut [FileEntry], path: &Path, expanded: bool) -> bool {
    for entry in entries {
        if entry.path == path {
            if entry.is_dir {
                entry.expanded = expanded;
            }
            return entry.is_dir;
        }
        if set_tree_expanded(&mut entry.children, path, expanded) {
            return true;
        }
    }
    false
}

// ═══════════════════════════════════════════════════════════════
// TESTS
// ═══════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn editor_with(text: &str) -> Editor {
        let lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
        let mut ed = Editor::new();
        ed.buffer = lines.clone();
        ed.original = lines;
        ed.dirty = false;
        ed
    }

    #[test]
    fn test_insert_char() {
        let mut ed = editor_with("hello");
        ed.insert_char('!');
        assert_eq!(ed.buffer[0], "!hello");
        assert!(ed.dirty);
    }

    #[test]
    fn test_newline() {
        let mut ed = editor_with("hello world");
        ed.cursor.col = 5;
        ed.insert_newline();
        assert_eq!(ed.buffer, vec!["hello", " world"]);
        assert_eq!(ed.cursor.row, 1);
        assert_eq!(ed.cursor.col, 0);
    }

    #[test]
    fn test_backspace() {
        let mut ed = editor_with("hello");
        ed.cursor.col = 5;
        ed.backspace();
        assert_eq!(ed.buffer[0], "hell");
        assert_eq!(ed.cursor.col, 4);
    }

    #[test]
    fn unicode_cursor_and_backspace_stay_on_character_boundaries() {
        let mut ed = editor_with("aé🙂b");
        ed.set_cursor(0, ed.buffer[0].len());
        ed.move_left();
        assert_eq!(ed.cursor.col, "aé🙂".len());
        ed.move_left();
        assert_eq!(ed.cursor.col, "aé".len());
        ed.move_right();
        ed.backspace();
        assert_eq!(ed.buffer[0], "aéb");
        assert_eq!(ed.cursor.col, "aé".len());
    }

    #[test]
    fn test_delete_line() {
        let mut ed = editor_with("line1\nline2\nline3");
        ed.cursor.row = 1;
        ed.delete_line();
        assert_eq!(ed.buffer, vec!["line1", "line3"]);
    }

    #[test]
    fn test_move_up_down() {
        let mut ed = editor_with("a\nb\nc");
        ed.move_down(2);
        assert_eq!(ed.cursor.row, 2);
        ed.move_up(2);
        assert_eq!(ed.cursor.row, 0);
    }

    #[test]
    fn test_dirty_flag() {
        let mut ed = editor_with("clean");
        assert!(!ed.dirty);
        ed.insert_char('x');
        assert!(ed.dirty);
    }

    #[test]
    fn undo_and_redo_restore_buffer_cursor_and_dirty_state() {
        let mut ed = editor_with("clean");
        ed.set_cursor(0, 5);
        ed.insert_char('!');
        assert!(ed.can_undo());
        assert!(ed.dirty);

        assert!(ed.undo());
        assert_eq!(ed.buffer, vec!["clean"]);
        assert_eq!(ed.cursor.col, 5);
        assert!(!ed.dirty);
        assert!(ed.can_redo());

        assert!(ed.redo());
        assert_eq!(ed.buffer, vec!["clean!"]);
        assert_eq!(ed.cursor.col, 6);
        assert!(ed.dirty);
    }

    #[test]
    fn visual_selection_copies_and_deletes_across_lines() {
        let mut ed = editor_with("alpha\nbeta\ngamma");
        ed.set_cursor(0, 2);
        ed.begin_selection(false);
        ed.set_cursor(1, 1);

        assert_eq!(ed.selection_byte_range(0), Some((2, 5)));
        assert_eq!(ed.selection_byte_range(1), Some((0, 2)));
        assert!(ed.delete_selection());
        assert_eq!(ed.clipboard, "pha\nbe");
        assert_eq!(ed.buffer, vec!["alta", "gamma"]);
        assert_eq!(ed.position(), Position { row: 0, col: 2 });

        assert!(ed.undo());
        assert_eq!(ed.buffer, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn visual_line_selection_and_multiline_paste_work() {
        let mut ed = editor_with("one\ntwo\nthree");
        ed.set_cursor(0, 1);
        ed.begin_selection(true);
        ed.set_cursor(1, 2);
        assert!(ed.copy_selection());
        assert_eq!(ed.clipboard, "one\ntwo");

        ed.set_cursor(2, 5);
        assert!(ed.paste());
        assert_eq!(ed.buffer, vec!["one", "two", "threeone", "two"]);
        assert_eq!(ed.position(), Position { row: 3, col: 3 });
    }

    #[test]
    fn test_save_and_revert() {
        let tmp = "/tmp/uintell_editor_test.txt";
        let _ = std::fs::remove_file(tmp);
        std::fs::write(tmp, "original").unwrap();

        let mut ed = Editor::new();
        ed.open(Path::new(tmp)).unwrap();
        assert!(!ed.dirty);
        assert_eq!(ed.buffer[0], "original");

        ed.insert_char('!');
        assert!(ed.dirty);
        assert_eq!(ed.buffer[0], "!original");

        ed.revert();
        assert!(!ed.dirty);
        assert_eq!(ed.buffer[0], "original");

        ed.insert_char('!');
        ed.save().unwrap();
        assert!(!ed.dirty);
        assert_eq!(std::fs::read_to_string(tmp).unwrap(), "!original");

        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn external_refresh_shows_agent_changes_without_marking_dirty() {
        let path = std::env::temp_dir().join(format!(
            "uintell-editor-external-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::write(&path, "before\nsecond").unwrap();
        let mut editor = Editor::new();
        editor.open(&path).unwrap();
        std::fs::write(&path, "after\nsecond\nthird").unwrap();

        editor.refresh_external(&path).unwrap();

        assert_eq!(editor.buffer, vec!["after", "second", "third"]);
        assert!(!editor.dirty);
        assert!(editor.show_diff);
        assert!(editor
            .diff_lines
            .iter()
            .any(|line| matches!(line, DiffLine::Added(value) if value == "third")));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_diff_unchanged() {
        let ed = editor_with("a\nb\nc");
        let diff = diff_lines(&ed.original, &ed.buffer);
        assert_eq!(
            diff,
            vec![
                DiffLine::Unchanged("a".into()),
                DiffLine::Unchanged("b".into()),
                DiffLine::Unchanged("c".into()),
            ]
        );
    }

    #[test]
    fn test_diff_added() {
        let original = vec!["a".to_string(), "c".to_string()];
        let current = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let diff = diff_lines(&original, &current);
        assert_eq!(
            diff,
            vec![
                DiffLine::Unchanged("a".into()),
                DiffLine::Added("b".into()),
                DiffLine::Unchanged("c".into()),
            ]
        );
    }

    #[test]
    fn test_diff_removed() {
        let original = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let current = vec!["a".to_string(), "c".to_string()];
        let diff = diff_lines(&original, &current);
        assert_eq!(
            diff,
            vec![
                DiffLine::Unchanged("a".into()),
                DiffLine::Removed("b".into()),
                DiffLine::Unchanged("c".into()),
            ]
        );
    }

    #[test]
    fn test_propose_and_accept_edit() {
        let mut ed = editor_with("line1\nline2\nline3");
        let diff = ed.propose_edit(1, 0, 1, 5, "replaced");
        assert_eq!(diff.len(), 4); // unchanged, removed, added, unchanged
        ed.accept_edit(1, 0, 1, 5, "replaced");
        assert_eq!(ed.buffer, vec!["line1", "replaced", "line3"]);
        assert!(ed.dirty);
    }

    #[test]
    fn test_word_completion() {
        let ed = editor_with("function main\n    println hello\n    function test");
        let completions = ed.complete_word("fun");
        assert!(completions.contains(&"function".to_string()));
        let completions = ed.complete_word("zzz");
        assert!(completions.is_empty());
    }

    #[test]
    fn test_complete_at_cursor_inserts_suffix() {
        let mut ed = editor_with("function main\nfun");
        ed.cursor.row = 1;
        ed.cursor.col = 3;
        assert_eq!(ed.complete_at_cursor().as_deref(), Some("function"));
        assert_eq!(ed.buffer[1], "function");
    }

    #[test]
    fn test_tree_expansion_changes_flattened_rows() {
        let root_path = PathBuf::from("/project/src");
        let mut tree = vec![FileEntry {
            name: "src".into(),
            path: root_path.clone(),
            is_dir: true,
            children: vec![FileEntry {
                name: "main.rs".into(),
                path: root_path.join("main.rs"),
                is_dir: false,
                children: Vec::new(),
                expanded: false,
            }],
            expanded: false,
        }];

        assert_eq!(flatten_tree(&tree, 0).len(), 1);
        assert!(set_tree_expanded(&mut tree, &root_path, true));
        assert_eq!(flatten_tree(&tree, 0).len(), 2);
    }

    #[test]
    fn test_search() {
        let mut ed = editor_with("apple\nbanana\ncherry\nbanana");
        ed.search("banana");
        assert_eq!(ed.search_matches, vec![1, 3]);
        assert_eq!(ed.cursor.row, 1);
        ed.search_next();
        assert_eq!(ed.cursor.row, 3);
        ed.search_next();
        assert_eq!(ed.cursor.row, 1);
    }

    #[test]
    fn change_review_resolves_independent_hunks() {
        let before = "a\nold one\nmiddle\nold two\n";
        let after = "a\nnew one\nmiddle\nnew two\n";
        let mut review = ChangeReview::new(before, after, true);
        assert_eq!(review.hunk_count(), 2);

        review.decide_current(ReviewDecision::Rejected);
        review.decide_current(ReviewDecision::Accepted);

        assert!(review.all_resolved());
        assert_eq!(review.result(), "a\nold one\nmiddle\nnew two\n");
    }

    #[test]
    fn editor_context_prefers_visual_selection_and_tracks_symbol() {
        let mut editor = editor_with("pub fn calculate() {\n    let total = 2;\n    total\n}");
        editor.file_path = Some(PathBuf::from("src/example.rs"));
        editor.set_cursor(1, 4);
        editor.begin_selection(true);
        editor.set_cursor(2, 5);

        let context = editor.code_context().unwrap();
        assert!(context.selected);
        assert_eq!((context.start_line, context.end_line), (2, 3));
        assert_eq!(context.content, "    let total = 2;\n    total");
        assert_eq!(context.symbol.as_deref(), Some("fn calculate"));
    }

    #[test]
    fn lsp_completion_replaces_utf16_range() {
        let mut editor = editor_with("let 😀name = pri;");
        let start = crate::lsp::utf16_column(&editor.buffer[0], "let 😀name = ".len());
        let end = crate::lsp::utf16_column(&editor.buffer[0], "let 😀name = pri".len());
        let completion = crate::lsp::CompletionItem {
            label: "println!".into(),
            detail: Some("macro".into()),
            new_text: "println!()".into(),
            range: Some(crate::lsp::Range {
                start: crate::lsp::Position {
                    line: 0,
                    character: start,
                },
                end: crate::lsp::Position {
                    line: 0,
                    character: end,
                },
            }),
        };

        editor.apply_completion(&completion).unwrap();
        assert_eq!(editor.buffer[0], "let 😀name = println!();");
        assert!(editor.dirty);
    }
}
