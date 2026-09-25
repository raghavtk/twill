use anyhow::{bail, Context, Result};
use ropey::Rope;
use std::collections::VecDeque;
use std::fs;
use std::io::{BufWriter, Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use unicode_segmentation::UnicodeSegmentation;

const MAX_UNDO_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
struct Edit {
    start: usize,
    before: String,
    after: String,
    before_state: u64,
    after_state: u64,
    group: Option<u64>,
}
impl Edit {
    fn memory_cost(&self) -> usize {
        std::mem::size_of::<Self>() + self.before.capacity() + self.after.capacity()
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Fingerprint {
    len: u64,
    hash: u64,
}

impl Fingerprint {
    fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            len: bytes.len() as u64,
            hash: hash_chunk(0xcbf29ce484222325, bytes),
        }
    }
    fn from_path(path: &Path) -> std::io::Result<Self> {
        let mut file = fs::File::open(path)?;
        let mut buf = [0u8; 64 * 1024];
        let mut len = 0u64;
        let mut hash = 0xcbf29ce484222325;
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            len += n as u64;
            hash = hash_chunk(hash, &buf[..n]);
        }
        Ok(Self { len, hash })
    }
}

fn hash_chunk(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
    }
    hash
}

pub struct Document {
    pub id: u64,
    pub path: Option<PathBuf>,
    pub rope: Rope,
    pub revision: u64,
    bom: bool,
    newline: String,
    fingerprint: Option<Fingerprint>,
    history: VecDeque<Edit>,
    history_pos: usize,
    history_bytes: usize,
    state: u64,
    next_state: u64,
    saved_state: u64,
    active_group: Option<u64>,
    changes: VecDeque<(u64, usize, usize, usize)>,
}

impl Document {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            path: None,
            rope: Rope::new(),
            revision: 0,
            bom: false,
            newline: "\n".into(),
            fingerprint: None,
            history: VecDeque::new(),
            history_pos: 0,
            history_bytes: 0,
            state: 0,
            next_state: 1,
            saved_state: 0,
            active_group: None,
            changes: VecDeque::new(),
        }
    }

    pub fn open(id: u64, path: &Path) -> Result<Self> {
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let bom = bytes.starts_with(&[0xef, 0xbb, 0xbf]);
        let content = std::str::from_utf8(if bom { &bytes[3..] } else { &bytes })
            .with_context(|| format!("{} is not valid UTF-8", path.display()))?;
        let mut doc = Self::new(id);
        doc.path = Some(path.to_path_buf());
        doc.rope = Rope::from_str(content);
        doc.bom = bom;
        let crlf = content.matches("\r\n").count();
        let lf = content.bytes().filter(|b| *b == b'\n').count();
        doc.newline = if crlf > lf - crlf {
            "\r\n".into()
        } else {
            "\n".into()
        };
        doc.fingerprint = Some(Fingerprint::from_bytes(&bytes));
        Ok(doc)
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }
    pub fn len(&self) -> usize {
        self.rope.len_chars()
    }
    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }
    pub fn line_of(&self, char_idx: usize) -> usize {
        self.rope.char_to_line(char_idx.min(self.len()))
    }
    pub fn line_start(&self, line: usize) -> usize {
        self.rope
            .line_to_char(line.min(self.line_count().saturating_sub(1)))
    }
    pub fn line_text(&self, line: usize) -> String {
        if line >= self.line_count() {
            return String::new();
        }
        self.rope
            .line(line)
            .to_string()
            .trim_end_matches(['\r', '\n'])
            .to_string()
    }
    pub fn slice(&self, range: Range<usize>) -> String {
        self.rope
            .slice(range.start.min(self.len())..range.end.min(self.len()))
            .to_string()
    }
    pub fn preferred_newline(&self) -> &str {
        &self.newline
    }
    pub fn has_bom(&self) -> bool {
        self.bom
    }
    pub fn set_format(&mut self, bom: bool, newline: &str) {
        self.bom = bom;
        self.newline = if newline == "\r\n" { "\r\n" } else { "\n" }.to_owned();
    }
    pub fn prev_grapheme(&self, idx: usize) -> usize {
        let idx = idx.min(self.len());
        if idx == 0 {
            return 0;
        }
        let start = self.line_start(self.line_of(idx - 1));
        start + prev_grapheme(&self.slice(start..idx), idx - start)
    }
    pub fn next_grapheme(&self, idx: usize) -> usize {
        let idx = idx.min(self.len());
        if idx == self.len() {
            return idx;
        }
        let start = self.line_start(self.line_of(idx));
        let end = if self.line_of(idx) + 1 < self.line_count() {
            self.line_start(self.line_of(idx) + 1)
        } else {
            self.len()
        };
        start + next_grapheme(&self.slice(start..end), idx - start)
    }
    pub fn is_dirty(&self) -> bool {
        self.state != self.saved_state
    }
    pub fn changes_since(&self, revision: u64) -> Option<Vec<(usize, usize, usize)>> {
        if revision == self.revision {
            return Some(Vec::new());
        }
        if revision > self.revision {
            return None;
        }
        let mut expected = revision.wrapping_add(1);
        let mut result = Vec::new();
        for &(rev, start, removed, inserted) in &self.changes {
            if rev < expected {
                continue;
            }
            if rev != expected {
                return None;
            }
            result.push((start, removed, inserted));
            expected = expected.wrapping_add(1);
        }
        (expected == self.revision.wrapping_add(1)).then_some(result)
    }
    pub fn begin_undo_group(&mut self) {
        self.active_group = Some(self.next_state);
    }
    pub fn end_undo_group(&mut self) {
        self.active_group = None;
    }

    pub fn replace(&mut self, range: Range<usize>, text: &str) {
        assert!(range.start <= range.end && range.end <= self.len());
        let before = self.slice(range.clone());
        if before == text {
            return;
        }
        for edit in self.history.drain(self.history_pos..) {
            self.history_bytes -= edit.memory_cost();
        }
        let edit = Edit {
            start: range.start,
            before,
            after: text.to_owned(),
            before_state: self.state,
            after_state: self.next_state,
            group: self.active_group,
        };
        self.next_state += 1;
        self.apply(range, text);
        self.state = edit.after_state;
        self.history_bytes += edit.memory_cost();
        self.history.push_back(edit);
        self.history_pos = self.history.len();
        while self.history_bytes > MAX_UNDO_BYTES && self.history.len() > 1 {
            let edit = self.history.pop_front().unwrap();
            self.history_bytes -= edit.memory_cost();
            self.history_pos -= 1;
        }
    }

    fn apply(&mut self, range: Range<usize>, text: &str) {
        let removed = range.end - range.start;
        let inserted = text.chars().count();
        self.rope.remove(range.clone());
        self.rope.insert(range.start, text);
        self.revision = self.revision.wrapping_add(1);
        self.changes
            .push_back((self.revision, range.start, removed, inserted));
        if self.changes.len() > 256 {
            self.changes.pop_front();
        }
    }

    pub fn undo(&mut self) -> Option<usize> {
        if self.history_pos == 0 {
            return None;
        }
        let group = self.history[self.history_pos - 1].group;
        loop {
            self.history_pos -= 1;
            let edit = self.history[self.history_pos].clone();
            self.apply(
                edit.start..edit.start + edit.after.chars().count(),
                &edit.before,
            );
            self.state = edit.before_state;
            if group.is_none()
                || self.history_pos == 0
                || self.history[self.history_pos - 1].group != group
            {
                return Some(edit.start);
            }
        }
    }

    pub fn redo(&mut self) -> Option<usize> {
        let group = self.history.get(self.history_pos)?.group;
        loop {
            let edit = self.history[self.history_pos].clone();
            self.apply(
                edit.start..edit.start + edit.before.chars().count(),
                &edit.after,
            );
            self.state = edit.after_state;
            self.history_pos += 1;
            if group.is_none()
                || self.history_pos >= self.history.len()
                || self.history[self.history_pos].group != group
            {
                return Some(edit.start + edit.after.chars().count());
            }
        }
    }

    pub fn external_changed(&self) -> bool {
        match (&self.path, &self.fingerprint) {
            (Some(path), Some(expected)) => {
                Fingerprint::from_path(path).map_or(true, |actual| actual != *expected)
            }
            _ => false,
        }
    }

    pub fn save(&mut self, path: Option<&Path>) -> Result<()> {
        self.save_impl(path, false)
    }
    pub fn force_save(&mut self, path: Option<&Path>) -> Result<()> {
        self.save_impl(path, true)
    }

    fn save_impl(&mut self, path: Option<&Path>, force: bool) -> Result<()> {
        let target = path
            .or(self.path.as_deref())
            .context("no file path selected")?
            .to_path_buf();
        if !force && self.path.as_deref() == Some(target.as_path()) && self.external_changed() {
            bail!("file changed on disk: {}", target.display());
        }
        if !force && self.path.as_deref() != Some(target.as_path()) && target.exists() {
            bail!("destination already exists: {}", target.display());
        }
        let parent = target.parent().context("file has no parent directory")?;
        let stem = target
            .file_name()
            .context("file has no name")?
            .to_string_lossy();
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let temp = parent.join(format!(".{stem}.{}.{}.tmp", std::process::id(), nonce));
        let result = (|| -> Result<Fingerprint> {
            let file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            let mut writer = BufWriter::new(file);
            let mut fingerprint = Fingerprint::from_bytes(&[]);
            if self.bom {
                writer.write_all(&[0xef, 0xbb, 0xbf])?;
                fingerprint = Fingerprint::from_bytes(&[0xef, 0xbb, 0xbf]);
            }
            for chunk in self.rope.chunks() {
                writer.write_all(chunk.as_bytes())?;
                fingerprint.len += chunk.len() as u64;
                fingerprint.hash = hash_chunk(fingerprint.hash, chunk.as_bytes());
            }
            writer.flush()?;
            writer.get_ref().sync_all()?;
            drop(writer);
            // Catch changes made while the temporary file was being written.
            if !force && self.path.as_deref() == Some(target.as_path()) && self.external_changed() {
                bail!("file changed on disk while saving: {}", target.display());
            }
            fs::rename(&temp, &target)?;
            Ok(fingerprint)
        })();
        let fingerprint = match result {
            Ok(fingerprint) => fingerprint,
            Err(err) => {
                let _ = fs::remove_file(&temp);
                return Err(err).with_context(|| format!("replacing {}", target.display()));
            }
        };
        self.path = Some(target);
        self.fingerprint = Some(fingerprint);
        self.saved_state = self.state;
        self.end_undo_group();
        Ok(())
    }

    pub fn reload(&mut self) -> Result<()> {
        let path = self.path.clone().context("no file path selected")?;
        let fresh = Self::open(self.id, &path)?;
        self.rope = fresh.rope;
        self.bom = fresh.bom;
        self.newline = fresh.newline;
        self.fingerprint = fresh.fingerprint;
        self.history.clear();
        self.history.shrink_to_fit();
        self.history_pos = 0;
        self.history_bytes = 0;
        self.end_undo_group();
        self.state = self.next_state;
        self.next_state += 1;
        self.saved_state = self.state;
        self.revision = self.revision.wrapping_add(1);
        self.changes.clear();
        Ok(())
    }
}

pub fn prev_grapheme(text: &str, idx: usize) -> usize {
    let byte = char_to_byte(text, idx.min(text.chars().count()));
    text.grapheme_indices(true)
        .take_while(|(i, _)| *i < byte)
        .last()
        .map_or(0, |(i, _)| text[..i].chars().count())
}

pub fn next_grapheme(text: &str, idx: usize) -> usize {
    let byte = char_to_byte(text, idx.min(text.chars().count()));
    text.grapheme_indices(true)
        .find(|(i, _)| *i > byte)
        .map_or(text.chars().count(), |(i, _)| text[..i].chars().count())
}

fn char_to_byte(text: &str, idx: usize) -> usize {
    text.char_indices().nth(idx).map_or(text.len(), |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn streaming_save_preserves_chunks_and_separates_undo_at_saved_state() {
        let path = std::env::temp_dir().join(format!(
            "twill-stream-save-{}-{}.txt",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let text = "a🧵b\r\n".repeat(20_000);
        let mut doc = Document::new(1);
        doc.set_format(true, "\r\n");
        doc.begin_undo_group();
        doc.replace(0..0, &text);
        doc.save(Some(&path)).unwrap();
        let disk = fs::read(&path).unwrap();
        assert_eq!(&disk[..3], &[0xef, 0xbb, 0xbf]);
        assert_eq!(&disk[3..], text.as_bytes());
        assert!(!doc.external_changed());
        doc.replace(doc.len()..doc.len(), "later");
        doc.undo();
        assert_eq!(doc.text(), text);
        assert!(!doc.is_dirty());
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn undo_tracks_saved_state() {
        let mut d = Document::new(1);
        d.replace(0..0, "a");
        assert!(d.is_dirty());
        d.undo();
        assert!(!d.is_dirty());
        d.redo();
        assert!(d.is_dirty());
    }
    #[test]
    fn unicode_graphemes() {
        let s = "a👩‍🚀e\u{301}";
        assert_eq!(next_grapheme(s, 1), 4);
        assert_eq!(prev_grapheme(s, 4), 1);
    }
    #[test]
    fn preserves_bom_mixed_newlines_and_detects_external_edit() {
        let path = std::env::temp_dir().join(format!(
            "twill-document-{}-{}.txt",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let original = b"\xef\xbb\xbfA\r\nB\nC\r\n";
        fs::write(&path, original).unwrap();
        let mut doc = Document::open(1, &path).unwrap();
        assert_eq!(doc.preferred_newline(), "\r\n");
        assert!(!doc.is_dirty());
        doc.force_save(None).unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
        fs::write(&path, b"outside").unwrap();
        assert!(doc.external_changed());
        assert!(doc.save(None).is_err());
        doc.reload().unwrap();
        assert_eq!(doc.text(), "outside");
        let _ = fs::remove_file(path);
    }
    #[test]
    fn save_as_refuses_existing_destination_unless_forced() {
        let dir = std::env::temp_dir();
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let source = dir.join(format!("twill-source-{suffix}.txt"));
        let target = dir.join(format!("twill-target-{suffix}.txt"));
        fs::write(&source, "source").unwrap();
        fs::write(&target, "target").unwrap();
        let mut doc = Document::open(1, &source).unwrap();
        doc.replace(0..doc.len(), "changed");
        assert!(doc.save(Some(&target)).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "target");
        assert_eq!(doc.path.as_deref(), Some(source.as_path()));
        doc.force_save(Some(&target)).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "changed");
        assert!(!doc.is_dirty());
        let _ = fs::remove_file(source);
        let _ = fs::remove_file(target);
    }
    #[test]
    fn graphemes_cross_line_boundaries_without_splitting_crlf() {
        let mut doc = Document::new(1);
        doc.replace(0..0, "a\r\n👩‍🚀e\u{301}");
        assert_eq!(doc.next_grapheme(0), 1);
        assert_eq!(doc.next_grapheme(1), 3);
        assert_eq!(doc.prev_grapheme(3), 1);
        assert_eq!(doc.next_grapheme(3), 6);
        assert_eq!(doc.prev_grapheme(6), 3);
    }
    #[test]
    fn change_journal_tracks_undo_and_evicts_old_revisions() {
        let mut doc = Document::new(1);
        doc.replace(0..0, "ab");
        doc.replace(1..2, "👩‍🚀");
        assert_eq!(doc.changes_since(0), Some(vec![(0, 0, 2), (1, 1, 3)]));
        doc.undo();
        assert_eq!(doc.changes_since(2), Some(vec![(1, 3, 1)]));
        for _ in 0..257 {
            doc.replace(0..0, "x");
        }
        assert!(doc.changes_since(0).is_none());
    }
}
