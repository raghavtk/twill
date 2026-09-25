use crate::document::Document;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VimMode {
    Normal,
    Insert,
    Visual,
    VisualLine,
}

#[derive(Clone, Debug, Default)]
pub struct VimOutcome {
    pub handled: bool,
    pub changed: bool,
    pub message: Option<String>,
}

pub struct VimState {
    pub mode: VimMode,
    pending: String,
    count: usize,
    pending_count: usize,
    register: String,
    linewise: bool,
    last_change: Vec<String>,
    recording: Vec<String>,
    replaying: bool,
}

impl Default for VimState {
    fn default() -> Self {
        Self::new()
    }
}

impl VimState {
    pub fn new() -> Self {
        Self {
            mode: VimMode::Normal,
            pending: String::new(),
            count: 0,
            pending_count: 1,
            register: String::new(),
            linewise: false,
            last_change: Vec::new(),
            recording: Vec::new(),
            replaying: false,
        }
    }

    pub fn handle_key(
        &mut self,
        key: &str,
        doc: &mut Document,
        cursor: &mut usize,
        anchor: &mut Option<usize>,
    ) -> VimOutcome {
        let before = doc.revision;
        let mut out = VimOutcome {
            handled: true,
            ..Default::default()
        };
        if self.mode == VimMode::Insert {
            match key {
                "Escape" | "Esc" => {
                    self.mode = VimMode::Normal;
                    doc.end_undo_group();
                    *cursor = doc.prev_grapheme(*cursor);
                    self.recording.push("Escape".into());
                    if !self.replaying {
                        self.last_change = self.recording.clone();
                    }
                    self.recording.clear();
                }
                "Backspace" => {
                    if *cursor > 0 {
                        let p = doc.prev_grapheme(*cursor);
                        doc.replace(p..*cursor, "");
                        *cursor = p;
                        self.recording.push(key.into());
                    }
                }
                "Enter" => {
                    let nl = doc.preferred_newline().to_owned();
                    doc.replace(*cursor..*cursor, &nl);
                    *cursor += nl.chars().count();
                    self.recording.push(key.into());
                }
                "Tab" => {
                    doc.replace(*cursor..*cursor, "\t");
                    *cursor += 1;
                    self.recording.push(key.into());
                }
                _ if key.chars().count() == 1 => {
                    doc.replace(*cursor..*cursor, key);
                    *cursor += 1;
                    self.recording.push(key.into());
                }
                _ => out.handled = false,
            }
            out.changed = doc.revision != before;
            return out;
        }
        if key == "Escape" || key == "Esc" {
            self.mode = VimMode::Normal;
            self.pending.clear();
            self.count = 0;
            *anchor = None;
            return out;
        }
        if self.mode == VimMode::Visual || self.mode == VimMode::VisualLine {
            match key {
                "d" | "x" | "y" | "c" => {
                    let (a, b) = self.selection(doc, *cursor, anchor.unwrap_or(*cursor));
                    self.register = doc.slice(a..b);
                    self.linewise = self.mode == VimMode::VisualLine;
                    if key != "y" {
                        doc.replace(a..b, "");
                        *cursor = a.min(doc.len());
                    }
                    self.mode = if key == "c" {
                        VimMode::Insert
                    } else {
                        VimMode::Normal
                    };
                    *anchor = None;
                    out.changed = doc.revision != before;
                    return out;
                }
                _ => {}
            }
        }
        if key.len() == 1 && key.as_bytes()[0].is_ascii_digit() && (key != "0" || self.count > 0) {
            self.count = self
                .count
                .saturating_mul(10)
                .saturating_add(key.parse::<usize>().unwrap())
                .min(100_000);
            return out;
        }
        let explicit_count = self.count > 0;
        let count = self.count.max(1);
        self.count = 0;
        if self.pending == "g" {
            self.pending.clear();
            if key == "g" {
                *cursor = doc.line_start(self.pending_count.saturating_sub(1));
                return out;
            }
        }
        if matches!(self.pending.as_str(), "d" | "c" | "y") {
            let op = self.pending.clone();
            self.pending.clear();
            let count = count.saturating_mul(self.pending_count);
            let (start, end, linewise) = if key == op {
                let line = doc.line_of(*cursor);
                let a = doc.line_start(line);
                let b = if line + count < doc.line_count() {
                    doc.line_start(line + count)
                } else {
                    doc.len()
                };
                (a, b, true)
            } else if key == "G" {
                let current = doc.line_of(*cursor);
                let target = if explicit_count {
                    (count - 1).min(doc.line_count() - 1)
                } else {
                    doc.line_count() - 1
                };
                let first = current.min(target);
                let last = current.max(target);
                (
                    doc.line_start(first),
                    if last + 1 < doc.line_count() {
                        doc.line_start(last + 1)
                    } else {
                        doc.len()
                    },
                    true,
                )
            } else if key == "$" {
                let target = line_end(doc, doc.line_of(*cursor));
                ((*cursor).min(target), (*cursor).max(target), false)
            } else if let Some(target) = motion(key, doc, *cursor, count) {
                let inclusive = matches!(key, "e" | "G");
                let end = if inclusive {
                    doc.next_grapheme(target)
                } else {
                    target
                };
                ((*cursor).min(end), (*cursor).max(end), false)
            } else {
                return out;
            };
            self.register = doc.slice(start..end);
            self.linewise = linewise;
            if op == "c" {
                doc.begin_undo_group();
            }
            if op != "y" {
                let mut delete_start = start;
                if linewise && end == doc.len() && start > 0 && !self.register.ends_with('\n') {
                    delete_start -= 1;
                    if delete_start > 0 && doc.rope.char(delete_start - 1) == '\r' {
                        delete_start -= 1;
                    }
                }
                doc.replace(delete_start..end, "");
                *cursor = delete_start.min(doc.len());
            }
            let mut keys: Vec<String> = count.to_string().chars().map(|c| c.to_string()).collect();
            if count == 1 {
                keys.clear();
            }
            keys.push(op.clone());
            keys.push(key.into());
            if op == "c" {
                self.mode = VimMode::Insert;
                self.recording = keys;
            } else if op == "d" {
                self.last_change = keys;
            }
            out.changed = doc.revision != before;
            return out;
        }
        match key {
            "g" => {
                self.pending = "g".into();
                self.pending_count = count;
            }
            "d" | "c" | "y" => {
                self.pending = key.into();
                self.pending_count = count;
            }
            "i" | "a" | "I" | "A" | "o" | "O" => {
                doc.begin_undo_group();
                match key {
                    "a" => *cursor = doc.next_grapheme(*cursor),
                    "I" => *cursor = first_nonblank(doc, doc.line_of(*cursor)),
                    "A" => *cursor = line_end(doc, doc.line_of(*cursor)),
                    "o" => {
                        *cursor = line_end(doc, doc.line_of(*cursor));
                        let nl = doc.preferred_newline().to_owned();
                        doc.replace(*cursor..*cursor, &nl);
                        *cursor += nl.chars().count();
                    }
                    "O" => {
                        *cursor = doc.line_start(doc.line_of(*cursor));
                        let nl = doc.preferred_newline().to_owned();
                        doc.replace(*cursor..*cursor, &nl);
                    }
                    _ => {}
                }
                self.mode = VimMode::Insert;
                self.recording = vec![key.into()];
            }
            "v" => {
                self.mode = VimMode::Visual;
                *anchor = Some(*cursor);
            }
            "V" => {
                self.mode = VimMode::VisualLine;
                *anchor = Some(*cursor);
            }
            "x" => {
                let mut end = *cursor;
                for _ in 0..count {
                    end = doc.next_grapheme(end);
                }
                if end > *cursor {
                    self.register = doc.slice(*cursor..end);
                    self.linewise = false;
                    doc.replace(*cursor..end, "");
                    self.last_change = if count == 1 {
                        vec![key.into()]
                    } else {
                        count
                            .to_string()
                            .chars()
                            .map(|c| c.to_string())
                            .chain(std::iter::once(key.into()))
                            .collect()
                    };
                }
            }
            "p" | "P" => {
                if !self.register.is_empty() {
                    let pos = if self.linewise {
                        if key == "p" {
                            line_end_with_newline(doc, doc.line_of(*cursor))
                        } else {
                            doc.line_start(doc.line_of(*cursor))
                        }
                    } else if key == "p" {
                        doc.next_grapheme(*cursor)
                    } else {
                        *cursor
                    };
                    let mut pasted = if self.linewise && !self.register.ends_with('\n') {
                        std::iter::repeat_n(self.register.as_str(), count)
                            .collect::<Vec<_>>()
                            .join(doc.preferred_newline())
                    } else {
                        self.register.repeat(count)
                    };
                    if self.linewise {
                        let nl = doc.preferred_newline();
                        if key == "p"
                            && pos == doc.len()
                            && pos > 0
                            && doc.rope.char(pos - 1) != '\n'
                        {
                            pasted.insert_str(0, nl);
                        }
                        if key == "P" && !pasted.ends_with('\n') {
                            pasted.push_str(nl);
                        }
                    }
                    doc.replace(pos..pos, &pasted);
                    *cursor = pos;
                    self.last_change = if count == 1 {
                        vec![key.into()]
                    } else {
                        count
                            .to_string()
                            .chars()
                            .map(|c| c.to_string())
                            .chain(std::iter::once(key.into()))
                            .collect()
                    };
                }
            }
            "u" => {
                if let Some(pos) = doc.undo() {
                    *cursor = pos;
                }
            }
            "Ctrl+r" | "Ctrl-R" => {
                if let Some(pos) = doc.redo() {
                    *cursor = pos;
                }
            }
            "G" => {
                *cursor = doc.line_start(if explicit_count {
                    (count - 1).min(doc.line_count() - 1)
                } else {
                    doc.line_count() - 1
                });
            }
            "." => {
                if !self.replaying {
                    self.replaying = true;
                    let keys = self.last_change.clone();
                    for _ in 0..count {
                        for k in &keys {
                            self.handle_key(k, doc, cursor, anchor);
                        }
                    }
                    self.replaying = false;
                }
            }
            _ => {
                if let Some(pos) = motion(key, doc, *cursor, count) {
                    *cursor = pos;
                } else {
                    out.handled = false;
                }
            }
        }
        out.changed = doc.revision != before;
        out
    }

    fn selection(&self, doc: &Document, cursor: usize, anchor: usize) -> (usize, usize) {
        if self.mode == VimMode::VisualLine {
            let a = doc.line_of(cursor).min(doc.line_of(anchor));
            let b = doc.line_of(cursor).max(doc.line_of(anchor));
            (
                doc.line_start(a),
                if b + 1 < doc.line_count() {
                    doc.line_start(b + 1)
                } else {
                    doc.len()
                },
            )
        } else {
            let start = cursor.min(anchor);
            let end = doc.next_grapheme(cursor.max(anchor));
            (start, end)
        }
    }
}

fn motion(key: &str, doc: &Document, cursor: usize, count: usize) -> Option<usize> {
    let len = doc.len();
    let ch = |idx: usize| doc.rope.char(idx);
    let mut pos = cursor.min(len);
    match key {
        "h" | "Left" => {
            let start = doc.line_start(doc.line_of(pos));
            for _ in 0..count {
                pos = doc.prev_grapheme(pos).max(start);
            }
        }
        "l" | "Right" => {
            let end = line_end(doc, doc.line_of(pos));
            let last = if end > doc.line_start(doc.line_of(pos)) {
                doc.prev_grapheme(end)
            } else {
                end
            };
            for _ in 0..count {
                pos = doc.next_grapheme(pos).min(last);
            }
        }
        "j" | "Down" | "k" | "Up" => {
            let line = doc.line_of(pos);
            let col = pos - doc.line_start(line);
            let target = if key == "j" || key == "Down" {
                (line + count).min(doc.line_count() - 1)
            } else {
                line.saturating_sub(count)
            };
            pos = (doc.line_start(target) + col).min(line_end(doc, target));
        }
        "0" => pos = doc.line_start(doc.line_of(pos)),
        "^" => pos = first_nonblank(doc, doc.line_of(pos)),
        "$" => {
            let end = line_end(doc, doc.line_of(pos));
            pos = if end > doc.line_start(doc.line_of(pos)) {
                doc.prev_grapheme(end)
            } else {
                end
            };
        }
        "G" => {
            pos = doc.line_start(if count == 1 {
                doc.line_count() - 1
            } else {
                (count - 1).min(doc.line_count() - 1)
            })
        }
        "w" => {
            for _ in 0..count {
                if pos < len {
                    pos += 1;
                }
                while pos < len && same_word(ch(pos - 1), ch(pos)) {
                    pos += 1;
                }
                while pos < len && ch(pos).is_whitespace() {
                    pos += 1;
                }
            }
        }
        "b" => {
            for _ in 0..count {
                pos = pos.saturating_sub(1);
                while pos > 0 && ch(pos).is_whitespace() {
                    pos -= 1;
                }
                while pos > 0 && same_word(ch(pos - 1), ch(pos)) {
                    pos -= 1;
                }
            }
        }
        "e" => {
            for _ in 0..count {
                if pos + 1 < len {
                    pos += 1;
                }
                while pos + 1 < len && ch(pos).is_whitespace() {
                    pos += 1;
                }
                while pos + 1 < len && same_word(ch(pos), ch(pos + 1)) {
                    pos += 1;
                }
            }
        }
        _ => return None,
    }
    if matches!(key, "w" | "b" | "e" | "j" | "k" | "Down" | "Up") && pos < len {
        pos = doc.prev_grapheme(pos + 1);
    }
    Some(pos.min(len))
}

fn same_word(a: char, b: char) -> bool {
    (a.is_alphanumeric() || a == '_') && (b.is_alphanumeric() || b == '_')
}
fn line_end(doc: &Document, line: usize) -> usize {
    doc.line_start(line) + doc.line_text(line).chars().count()
}
fn line_end_with_newline(doc: &Document, line: usize) -> usize {
    if line + 1 < doc.line_count() {
        doc.line_start(line + 1)
    } else {
        doc.len()
    }
}
fn first_nonblank(doc: &Document, line: usize) -> usize {
    doc.line_start(line)
        + doc
            .line_text(line)
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn keys(v: &mut VimState, d: &mut Document, cursor: &mut usize, input: &[&str]) {
        let mut anchor = None;
        for key in input {
            v.handle_key(key, d, cursor, &mut anchor);
        }
    }
    #[test]
    fn counted_lines_and_grouped_insert() {
        let mut d = Document::new(1);
        d.replace(0..0, "one\ntwo\nthree\nfour\n");
        let mut v = VimState::new();
        let mut c = 0;
        keys(&mut v, &mut d, &mut c, &["2", "d", "d"]);
        assert_eq!(d.text(), "three\nfour\n");
        keys(&mut v, &mut d, &mut c, &["i", "A", "B", "Escape"]);
        assert_eq!(d.text(), "ABthree\nfour\n");
        keys(&mut v, &mut d, &mut c, &["u"]);
        assert_eq!(d.text(), "three\nfour\n");
    }
    #[test]
    fn dollar_delete_keeps_newline_and_horizontal_motion_stays_on_line() {
        let mut d = Document::new(1);
        d.replace(0..0, "abc\r\ndef");
        let mut v = VimState::new();
        let mut c = 0;
        keys(&mut v, &mut d, &mut c, &["$", "l"]);
        assert_eq!(c, 2);
        keys(&mut v, &mut d, &mut c, &["d", "$"]);
        assert_eq!(d.text(), "ab\r\ndef");
        keys(&mut v, &mut d, &mut c, &["j", "h"]);
        assert_eq!(d.line_of(c), 1);
    }
    #[test]
    fn dot_repeats_counted_operator_and_insert_is_one_undo() {
        let mut d = Document::new(1);
        d.replace(0..0, "a b c d e f");
        let mut v = VimState::new();
        let mut c = 0;
        keys(&mut v, &mut d, &mut c, &["2", "d", "w"]);
        assert_eq!(d.text(), "c d e f");
        keys(&mut v, &mut d, &mut c, &["."]);
        assert_eq!(d.text(), "e f");
        keys(&mut v, &mut d, &mut c, &["o", "X", "Escape"]);
        assert_eq!(d.text(), "e f\nX");
        keys(&mut v, &mut d, &mut c, &["u"]);
        assert_eq!(d.text(), "e f");
    }
    #[test]
    fn linewise_paste_separates_unterminated_final_line() {
        let mut d = Document::new(1);
        d.replace(0..0, "first\nlast");
        let mut v = VimState::new();
        let mut c = d.line_start(1);
        keys(&mut v, &mut d, &mut c, &["y", "y", "p"]);
        assert_eq!(d.text(), "first\nlast\nlast");
    }
    #[test]
    fn delete_last_unterminated_line_removes_separator() {
        let mut d = Document::new(1);
        d.replace(0..0, "first\r\nlast");
        let mut v = VimState::new();
        let mut c = d.line_start(1);
        keys(&mut v, &mut d, &mut c, &["d", "d"]);
        assert_eq!(d.text(), "first");
        keys(&mut v, &mut d, &mut c, &["p"]);
        assert_eq!(d.text(), "first\nlast");
    }
}
