//! Syntax coloring is parsed in a worker and delivered one visible line at a time.
use egui::{
    text::{LayoutJob, TextFormat},
    Color32, FontId,
};
use ropey::Rope;
use std::{
    collections::{HashMap, VecDeque},
    hash::{Hash, Hasher},
    ops::Range,
    sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError, TrySendError},
    thread,
    time::Duration,
};
use syntect::{
    easy::HighlightLines,
    highlighting::{Color, ThemeSet},
    parsing::{SyntaxReference, SyntaxSet},
};

const MAX_CACHE_BYTES: usize = 8 * 1024 * 1024;
const MAX_RENDER_CHARS: usize = 4096;
const WORK_QUANTUM: usize = 128;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct DocumentKey {
    id: u64,
    revision: u64,
    dark: bool,
    language: u64,
}
enum Message {
    Document {
        key: DocumentKey,
        extension: String,
        rope: Rope,
    },
    Visible {
        key: DocumentKey,
        range: Range<usize>,
    },
    Forget(u64),
}
struct LineResult {
    key: DocumentKey,
    index: usize,
    job: LayoutJob,
    bytes: usize,
}

pub struct Highlighter {
    sender: Sender<Message>,
    results: Receiver<LineResult>,
    cache: HashMap<(DocumentKey, usize), (LayoutJob, usize)>,
    order: VecDeque<(DocumentKey, usize)>,
    cache_bytes: usize,
    requested: HashMap<u64, DocumentKey>,
    visible: HashMap<(u64, u64), Range<usize>>,
}
impl Default for Highlighter {
    fn default() -> Self {
        Self::new()
    }
}
impl Highlighter {
    pub fn new() -> Self {
        Self::start(None)
    }
    pub fn with_context(context: egui::Context) -> Self {
        Self::start(Some(context))
    }
    fn start(context: Option<egui::Context>) -> Self {
        let (sender, receiver) = mpsc::channel();
        let (results_sender, results) = mpsc::sync_channel(512);
        let _ = thread::Builder::new()
            .name("twill-syntax".into())
            .spawn(move || worker(receiver, results_sender, context));
        Self {
            sender,
            results,
            cache: HashMap::new(),
            order: VecDeque::new(),
            cache_bytes: 0,
            requested: HashMap::new(),
            visible: HashMap::new(),
        }
    }
    pub fn request(&mut self, id: u64, revision: u64, extension: &str, rope: Rope, dark: bool) {
        self.drain();
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        extension.hash(&mut hash);
        let key = DocumentKey {
            id,
            revision,
            dark,
            language: hash.finish(),
        };
        if self.requested.get(&id) == Some(&key) {
            return;
        }
        self.requested.insert(id, key);
        self.visible.retain(|(doc, _), _| *doc != id);
        self.cache.retain(|(old, _), (_, bytes)| {
            if old.id == id {
                self.cache_bytes -= *bytes;
                false
            } else {
                true
            }
        });
        self.order.retain(|(old, _)| old.id != id);
        let _ = self.sender.send(Message::Document {
            key,
            extension: extension.to_ascii_lowercase(),
            rope,
        });
    }
    /// Call each frame for the viewport. End is exclusive.
    pub fn request_visible(
        &mut self,
        id: u64,
        revision: u64,
        view: u64,
        start_line: usize,
        end_line: usize,
    ) {
        let Some(&key) = self.requested.get(&id) else {
            return;
        };
        if key.revision != revision {
            return;
        }
        let range = start_line..end_line.max(start_line);
        if self.visible.get(&(id, view)) == Some(&range) {
            return;
        }
        self.visible.insert((id, view), range);
        let start = self
            .visible
            .iter()
            .filter(|((d, _), _)| *d == id)
            .map(|(_, r)| r.start)
            .min()
            .unwrap_or(0);
        let end = self
            .visible
            .iter()
            .filter(|((d, _), _)| *d == id)
            .map(|(_, r)| r.end)
            .max()
            .unwrap_or(0);
        let _ = self.sender.send(Message::Visible {
            key,
            range: start..end,
        });
    }
    pub fn forget(&mut self, id: u64) {
        self.requested.remove(&id);
        self.visible.retain(|(doc, _), _| *doc != id);
        self.cache.retain(|(key, _), (_, bytes)| {
            if key.id == id {
                self.cache_bytes -= *bytes;
                false
            } else {
                true
            }
        });
        self.order.retain(|(key, _)| key.id != id);
        let _ = self.sender.send(Message::Forget(id));
    }
    pub fn line(
        &mut self,
        id: u64,
        revision: u64,
        line_index: usize,
        text: &str,
        dark: bool,
    ) -> LayoutJob {
        self.drain();
        let Some(&key) = self
            .requested
            .get(&id)
            .filter(|key| key.revision == revision && key.dark == dark)
        else {
            return plain(text, dark);
        };
        self.cache
            .get(&(key, line_index))
            .filter(|(job, _)| job.text == text)
            .map(|(job, _)| job.clone())
            .unwrap_or_else(|| plain(text, dark))
    }
    fn drain(&mut self) {
        for result in self.results.try_iter().take(256) {
            if self.requested.get(&result.key.id) != Some(&result.key) {
                continue;
            }
            let key = (result.key, result.index);
            if let Some((_, bytes)) = self.cache.insert(key, (result.job, result.bytes)) {
                self.cache_bytes -= bytes;
            } else {
                self.order.push_back(key);
            }
            self.cache_bytes += result.bytes;
            while self.cache_bytes > MAX_CACHE_BYTES {
                let Some(oldest) = self.order.pop_front() else {
                    break;
                };
                if let Some((_, bytes)) = self.cache.remove(&oldest) {
                    self.cache_bytes -= bytes;
                }
            }
        }
    }
}

struct WorkerDocument<'a> {
    key: DocumentKey,
    rope: Rope,
    syntax: &'a SyntaxReference,
    theme: &'a syntect::highlighting::Theme,
    lines: HighlightLines<'a>,
    next: usize,
    visible: Range<usize>,
    pending: Option<LineResult>,
}
impl<'a> WorkerDocument<'a> {
    fn reset(&mut self) {
        self.lines = HighlightLines::new(self.syntax, self.theme);
        self.next = 0;
        self.pending = None;
    }
}

fn worker(
    receiver: Receiver<Message>,
    results: SyncSender<LineResult>,
    context: Option<egui::Context>,
) {
    let syntaxes = syntax_set();
    let themes = ThemeSet::load_defaults();
    let mut docs: HashMap<u64, WorkerDocument<'_>> = HashMap::new();
    let mut round_robin = VecDeque::new();
    loop {
        let first = if docs.values().any(|doc| {
            doc.next < doc.visible.end.min(doc.rope.len_lines()) || doc.pending.is_some()
        }) {
            match receiver.try_recv() {
                Ok(message) => Some(message),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => break,
            }
        } else {
            match receiver.recv() {
                Ok(message) => Some(message),
                Err(_) => break,
            }
        };
        if let Some(message) = first {
            apply(message, &syntaxes, &themes, &mut docs, &mut round_robin);
        }
        for message in receiver.try_iter().take(64) {
            apply(message, &syntaxes, &themes, &mut docs, &mut round_robin);
        }
        if let Some(id) = round_robin.pop_front() {
            if let Some(doc) = docs.get_mut(&id) {
                let delivered = process_quantum(doc, &syntaxes, &results);
                if delivered > 0 {
                    if let Some(context) = &context {
                        // Coalesce completed lines into a frame instead of waiting for idle polling.
                        context.request_repaint_after(Duration::from_millis(16));
                    }
                } else if doc.pending.is_some() {
                    // A full results queue must not turn this worker into a busy loop.
                    thread::sleep(Duration::from_millis(2));
                }
                round_robin.push_back(id);
            }
        }
    }
}

fn apply<'a>(
    message: Message,
    syntaxes: &'a SyntaxSet,
    themes: &'a ThemeSet,
    docs: &mut HashMap<u64, WorkerDocument<'a>>,
    queue: &mut VecDeque<u64>,
) {
    match message {
        Message::Forget(id) => {
            docs.remove(&id);
            queue.retain(|d| *d != id);
        }
        Message::Document {
            key,
            extension,
            rope,
        } => {
            let syntax = syntax_for(syntaxes, &extension);
            let theme = if key.dark {
                &themes.themes["base16-ocean.dark"]
            } else {
                &themes.themes["base16-ocean.light"]
            };
            docs.insert(
                key.id,
                WorkerDocument {
                    key,
                    rope,
                    syntax,
                    theme,
                    lines: HighlightLines::new(syntax, theme),
                    next: 0,
                    visible: 0..0,
                    pending: None,
                },
            );
            if !queue.contains(&key.id) {
                queue.push_back(key.id);
            }
        }
        Message::Visible { key, range } => {
            if let Some(doc) = docs.get_mut(&key.id) {
                if doc.key != key {
                    return;
                }
                if range.start < doc.next {
                    doc.reset();
                }
                doc.visible = range;
            }
        }
    }
}

fn process_quantum(
    doc: &mut WorkerDocument<'_>,
    syntaxes: &SyntaxSet,
    results: &SyncSender<LineResult>,
) -> usize {
    let mut delivered = 0;
    if let Some(result) = doc.pending.take() {
        match results.try_send(result) {
            Ok(()) => delivered += 1,
            Err(TrySendError::Full(result)) => {
                doc.pending = Some(result);
                return delivered;
            }
            Err(TrySendError::Disconnected(_)) => return delivered,
        }
    }
    let end = doc.visible.end.min(doc.rope.len_lines());
    for _ in 0..WORK_QUANTUM {
        if doc.next >= end {
            break;
        }
        let index = doc.next;
        doc.next += 1;
        let source: String = doc
            .rope
            .line(index)
            .chars()
            .take(MAX_RENDER_CHARS + 1)
            .collect();
        if doc.rope.line(index).len_chars() > MAX_RENDER_CHARS {
            // A minified or generated line must not monopolize the parser worker.
            if index >= doc.visible.start {
                let job = plain(&source, doc.key.dark);
                let bytes = job.text.len()
                    + job.sections.len() * std::mem::size_of::<egui::text::LayoutSection>();
                match results.try_send(LineResult {
                    key: doc.key,
                    index,
                    job,
                    bytes,
                }) {
                    Ok(()) => delivered += 1,
                    Err(TrySendError::Full(result)) => {
                        doc.pending = Some(result);
                        return delivered;
                    }
                    Err(TrySendError::Disconnected(_)) => return delivered,
                }
            }
            continue;
        }
        let spans = doc.lines.highlight_line(&source, syntaxes);
        if index < doc.visible.start {
            continue;
        }
        let job = match spans {
            Ok(spans) => styled(spans, doc.key.dark),
            Err(_) => plain(&source, doc.key.dark),
        };
        let bytes =
            job.text.len() + job.sections.len() * std::mem::size_of::<egui::text::LayoutSection>();
        let result = LineResult {
            key: doc.key,
            index,
            job,
            bytes,
        };
        match results.try_send(result) {
            Ok(()) => delivered += 1,
            Err(TrySendError::Full(result)) => {
                doc.pending = Some(result);
                return delivered;
            }
            Err(TrySendError::Disconnected(_)) => return delivered,
        }
    }
    delivered
}

fn syntax_for<'a>(set: &'a SyntaxSet, extension: &str) -> &'a SyntaxReference {
    if let Some(syntax) = set.find_syntax_by_extension(extension) {
        return syntax;
    }
    let fallback = match extension {
        "h" => "c",
        "jsx" => "tsx",
        other => other,
    };
    set.find_syntax_by_extension(fallback)
        .unwrap_or_else(|| set.find_syntax_plain_text())
}

fn syntax_set() -> SyntaxSet {
    syntect::dumps::from_uncompressed_data(include_bytes!(concat!(
        env!("OUT_DIR"),
        "/syntaxes.packdump"
    )))
    .expect("bundled syntax pack")
}

fn styled(spans: Vec<(syntect::highlighting::Style, &str)>, dark: bool) -> LayoutJob {
    let mut job = LayoutJob::default();
    let mut remaining = MAX_RENDER_CHARS;
    for (style, part) in spans {
        let part = part.trim_end_matches(['\r', '\n']);
        let clipped: String = part.chars().take(remaining).collect();
        remaining -= clipped.chars().count();
        if !clipped.is_empty() {
            job.append(
                &clipped,
                0.0,
                TextFormat {
                    font_id: FontId::monospace(14.0),
                    color: rgb(style.foreground),
                    ..Default::default()
                },
            );
        }
        if remaining == 0 {
            break;
        }
    }
    if job.text.is_empty() {
        return plain("", dark);
    }
    job
}
fn rgb(color: Color) -> Color32 {
    Color32::from_rgb(color.r, color.g, color.b)
}
fn plain(text: &str, dark: bool) -> LayoutJob {
    let mut job = LayoutJob::default();
    let clipped: String = text
        .trim_end_matches(['\r', '\n'])
        .chars()
        .take(MAX_RENDER_CHARS)
        .collect();
    job.append(
        &clipped,
        0.0,
        TextFormat {
            font_id: FontId::monospace(14.0),
            color: if dark {
                Color32::LIGHT_GRAY
            } else {
                Color32::DARK_GRAY
            },
            ..Default::default()
        },
    );
    job
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completed_highlighting_requests_a_frame_without_idle_polling() {
        let context = egui::Context::default();
        let (sender, receiver) = mpsc::channel();
        context.set_request_repaint_callback(move |info| {
            let _ = sender.send(info.delay);
        });
        let mut highlighter = Highlighter::with_context(context);
        highlighter.request(1, 1, "rs", Rope::from_str("fn main() {}"), true);
        highlighter.request_visible(1, 1, 1, 0, 1);
        let delay = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(delay <= Duration::from_millis(16));
        highlighter.drain();
        assert_eq!(highlighter.cache.len(), 1);
    }
    #[test]
    fn full_result_queue_preserves_pending_line_until_space_is_available() {
        let syntaxes = syntax_set();
        let themes = ThemeSet::load_defaults();
        let theme = &themes.themes["base16-ocean.dark"];
        let syntax = syntax_for(&syntaxes, "rs");
        let mut doc = WorkerDocument {
            key: DocumentKey {
                id: 1,
                revision: 1,
                dark: true,
                language: 0,
            },
            rope: Rope::from_str("let a = 1;\nlet b = 2;"),
            syntax,
            theme,
            lines: HighlightLines::new(syntax, theme),
            next: 0,
            visible: 0..100,
            pending: None,
        };
        let (sender, receiver) = mpsc::sync_channel(1);
        assert_eq!(process_quantum(&mut doc, &syntaxes, &sender), 1);
        assert!(doc.pending.is_some());
        assert_eq!(process_quantum(&mut doc, &syntaxes, &sender), 0);
        assert_eq!(receiver.try_recv().unwrap().index, 0);
        assert_eq!(process_quantum(&mut doc, &syntaxes, &sender), 1);
        assert_eq!(receiver.try_recv().unwrap().index, 1);
        assert!(doc.pending.is_none());
        assert_eq!(process_quantum(&mut doc, &syntaxes, &sender), 0);
    }
    #[test]
    fn requested_syntaxes_are_available() {
        let set = syntax_set();
        for extension in [
            "html", "md", "c", "cpp", "h", "go", "py", "json", "jsonc", "yaml", "rs", "ts", "js",
            "jsx", "tsx", "css", "sql", "toml", "tex",
        ] {
            assert_ne!(
                syntax_for(&set, extension).name,
                "Plain Text",
                "missing {extension}"
            );
        }
    }
    #[test]
    fn jobs_do_not_include_line_breaks() {
        assert_eq!(plain("text\r\n", true).text, "text");
    }
    #[test]
    fn worker_highlights_both_documents_and_rejects_stale_results() {
        let mut h = Highlighter::new();
        h.request(1, 1, "rs", Rope::from_str("let x = 1;\n"), true);
        h.request_visible(1, 1, 10, 0, 1);
        h.request(2, 1, "jsonc", Rope::from_str("// comment\n"), true);
        h.request_visible(2, 1, 20, 0, 1);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while h.cache.len() < 2 && std::time::Instant::now() < deadline {
            h.drain();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(h.cache.len(), 2);
        let job = h.line(2, 1, 0, "// comment", true);
        assert_eq!(job.text, "// comment");
        h.request(1, 2, "rs", Rope::from_str("let y = 2;\n"), true);
        assert!(!h
            .cache
            .keys()
            .any(|(key, _)| key.id == 1 && key.revision == 1));
        h.forget(2);
        assert!(!h.cache.keys().any(|(key, _)| key.id == 2));
    }
}
