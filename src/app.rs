use crate::{
    document::{next_grapheme, prev_grapheme, Document},
    platform::{FileWatcher, Recovery, RecoveryStore, Settings},
    syntax::Highlighter,
    terminal::{Shell, Terminal},
    vim::{VimMode, VimState},
};
use egui::{Color32, FontId, Key, Pos2, Rect, Sense, Stroke, Vec2};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

struct View {
    document: u64,
    cursor: usize,
    anchor: Option<usize>,
    vim: VimState,
    reveal: bool,
    observed_revision: u64,
    typing_until: Option<Instant>,
    language: Option<String>,
}
impl View {
    fn new(document: u64) -> Self {
        Self {
            document,
            cursor: 0,
            anchor: None,
            vim: VimState::new(),
            reveal: false,
            observed_revision: 0,
            typing_until: None,
            language: None,
        }
    }
}
struct Pane {
    id: u64,
    tabs: Vec<View>,
    active: usize,
}
enum Layout {
    Leaf(Pane),
    Split {
        horizontal: bool,
        fraction: f32,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}
impl Layout {
    fn pane_mut(&mut self, id: u64) -> Option<&mut Pane> {
        match self {
            Self::Leaf(p) => (p.id == id).then_some(p),
            Self::Split { first, second, .. } => first.pane_mut(id).or_else(|| second.pane_mut(id)),
        }
    }
    fn each(&mut self, f: &mut impl FnMut(&mut Pane)) {
        match self {
            Self::Leaf(p) => f(p),
            Self::Split { first, second, .. } => {
                first.each(f);
                second.each(f);
            }
        }
    }
    fn split(&mut self, id: u64, new_id: u64, horizontal: bool) -> bool {
        match self {
            Self::Leaf(p) if p.id == id => {
                let doc = p.tabs.get(p.active).map(|v| v.document);
                let next = Pane {
                    id: new_id,
                    tabs: doc.map(|d| vec![View::new(d)]).unwrap_or_default(),
                    active: 0,
                };
                let old = std::mem::replace(self, Self::Leaf(next));
                let new = std::mem::replace(self, old);
                let old = std::mem::replace(
                    self,
                    Self::Leaf(Pane {
                        id: 0,
                        tabs: vec![],
                        active: 0,
                    }),
                );
                *self = Self::Split {
                    horizontal,
                    fraction: 0.5,
                    first: Box::new(old),
                    second: Box::new(new),
                };
                true
            }
            Self::Split { first, second, .. } => {
                first.split(id, new_id, horizontal) || second.split(id, new_id, horizontal)
            }
            _ => false,
        }
    }
    fn remove_pane(&mut self, id: u64) -> bool {
        match self {
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                if matches!(first.as_ref(),Self::Leaf(p) if p.id==id) {
                    let replacement = std::mem::replace(
                        second,
                        Box::new(Self::Leaf(Pane {
                            id: 0,
                            tabs: vec![],
                            active: 0,
                        })),
                    );
                    *self = *replacement;
                    true
                } else if matches!(second.as_ref(),Self::Leaf(p) if p.id==id) {
                    let replacement = std::mem::replace(
                        first,
                        Box::new(Self::Leaf(Pane {
                            id: 0,
                            tabs: vec![],
                            active: 0,
                        })),
                    );
                    *self = *replacement;
                    true
                } else {
                    first.remove_pane(id) || second.remove_pane(id)
                }
            }
        }
    }
    fn close_pane(&mut self, id: u64) -> Option<u64> {
        let mut others = Vec::new();
        self.each(&mut |p| {
            if p.id != id {
                others.push(p.id);
            }
        });
        let target = *others.first()?;
        let tabs = std::mem::take(&mut self.pane_mut(id)?.tabs);
        self.remove_pane(id);
        let pane = self.pane_mut(target)?;
        for view in tabs {
            if !pane.tabs.iter().any(|v| v.document == view.document) {
                pane.tabs.push(view);
            }
        }
        Some(target)
    }
}

enum Action {
    Open(PathBuf),
    Close(u64, usize),
    Save(bool),
    Split(bool),
    Command(String),
    Find(bool),
    Focus(u64),
}
pub struct Twill {
    documents: HashMap<u64, Document>,
    layout: Layout,
    active_pane: u64,
    next_id: u64,
    settings: Settings,
    highlighter: Highlighter,
    watcher: FileWatcher,
    root: Option<PathBuf>,
    tree_cache: HashMap<PathBuf, Vec<PathBuf>>,
    tree_open: HashSet<PathBuf>,
    terminal: Option<Terminal>,
    terminal_visible: bool,
    terminal_close: bool,
    message: Option<String>,
    pending_close: Option<(u64, usize)>,
    quit_requested: bool,
    allow_quit: bool,
    conflicts: HashSet<u64>,
    recovery: RecoveryStore,
    recovered: Vec<(PathBuf, Recovery)>,
    recovery_revision: HashMap<u64, u64>,
    last_recovery: Instant,
    last_edit: Instant,
    last_poll: Instant,
    find_open: bool,
    find: String,
    replacement: String,
    match_case: bool,
    command: Option<String>,
    command_focus: bool,
    search_backwards: bool,
}
impl Twill {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (settings, message) = Settings::load();
        cc.egui_ctx.set_visuals(if settings.dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });
        let recovery = RecoveryStore::new();
        let recovered = recovery.pending();
        let mut app = Self {
            documents: HashMap::new(),
            layout: Layout::Leaf(Pane {
                id: 1,
                tabs: vec![],
                active: 0,
            }),
            active_pane: 1,
            next_id: 2,
            settings,
            highlighter: Highlighter::new(),
            watcher: FileWatcher::new(),
            root: None,
            tree_cache: HashMap::new(),
            tree_open: HashSet::new(),
            terminal: None,
            terminal_visible: false,
            terminal_close: false,
            message,
            pending_close: None,
            quit_requested: false,
            allow_quit: false,
            conflicts: HashSet::new(),
            recovery,
            recovered,
            recovery_revision: HashMap::new(),
            last_recovery: Instant::now(),
            last_edit: Instant::now(),
            last_poll: Instant::now(),
            find_open: false,
            find: String::new(),
            replacement: String::new(),
            match_case: false,
            command: None,
            command_focus: false,
            search_backwards: false,
        };
        for arg in std::env::args_os().skip(1) {
            app.open(PathBuf::from(arg));
        }
        if app.documents.is_empty() {
            app.new_document();
        }
        app
    }
    fn id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
    fn active_id(&mut self) -> Option<u64> {
        self.layout
            .pane_mut(self.active_pane)
            .and_then(|p| p.tabs.get(p.active))
            .map(|v| v.document)
    }
    fn add_document(&mut self, doc: Document) {
        let id = doc.id;
        if let Some(path) = &doc.path {
            self.watcher.watch(path);
        }
        self.documents.insert(id, doc);
        if let Some(p) = self.layout.pane_mut(self.active_pane) {
            p.tabs.push(View::new(id));
            p.active = p.tabs.len() - 1;
        }
    }
    fn new_document(&mut self) {
        let id = self.id();
        self.add_document(Document::new(id));
    }
    fn open(&mut self, path: PathBuf) {
        if path.is_dir() {
            self.root = Some(path);
            self.tree_cache.clear();
            return;
        }
        let path = fs::canonicalize(&path).unwrap_or(path);
        let existing = self
            .documents
            .values()
            .find(|d| d.path.as_ref() == Some(&path))
            .map(|d| d.id);
        if let Some(id) = existing {
            if let Some(p) = self.layout.pane_mut(self.active_pane) {
                if let Some(i) = p.tabs.iter().position(|v| v.document == id) {
                    p.active = i;
                } else {
                    p.tabs.push(View::new(id));
                    p.active = p.tabs.len() - 1;
                }
            }
            return;
        }
        let id = self.id();
        match Document::open(id, &path) {
            Ok(doc) => self.add_document(doc),
            Err(e) => self.message = Some(format!("{e:#}")),
        }
    }
    fn save_id(&mut self, id: u64, save_as: bool, force: bool) -> bool {
        let Some(doc) = self.documents.get(&id) else {
            return false;
        };
        let path = if save_as || doc.path.is_none() {
            let mut dialog = rfd::FileDialog::new();
            if let Some(p) = &doc.path {
                dialog = dialog.set_file_name(p.file_name().unwrap_or_default().to_string_lossy());
            }
            match dialog.save_file() {
                Some(p) => Some(p),
                None => return false,
            }
        } else {
            None
        };
        if let Some(target) = &path {
            let canonical = fs::canonicalize(target).unwrap_or_else(|_| target.clone());
            if self.documents.iter().any(|(other, d)| {
                *other != id
                    && d.path.as_ref().is_some_and(|p| {
                        fs::canonicalize(p).unwrap_or_else(|_| p.clone()) == canonical
                    })
            }) {
                self.message =
                    Some("That file is already open in another tab. Save to another path.".into());
                return false;
            }
        }
        let doc = self.documents.get_mut(&id).unwrap();
        doc.end_undo_group();
        // The native Save As dialog obtains overwrite confirmation for existing destinations.
        let result = if force || path.is_some() {
            doc.force_save(path.as_deref())
        } else {
            doc.save(path.as_deref())
        };
        match result {
            Ok(()) => {
                if let Some(p) = &doc.path {
                    self.watcher.watch(p);
                }
                self.recovery.remove(id);
                self.conflicts.remove(&id);
                true
            }
            Err(e) => {
                self.message = Some(format!("Save failed: {e:#}"));
                false
            }
        }
    }
    fn close_tab(&mut self, pane: u64, index: usize, discard: bool) {
        let Some(id) = self
            .layout
            .pane_mut(pane)
            .and_then(|p| p.tabs.get(index))
            .map(|v| v.document)
        else {
            return;
        };
        let mut views = 0;
        self.layout
            .each(&mut |p| views += p.tabs.iter().filter(|v| v.document == id).count());
        if views == 1 && !discard && self.documents.get(&id).is_some_and(Document::is_dirty) {
            self.pending_close = Some((pane, index));
            return;
        }
        if let Some(p) = self.layout.pane_mut(pane) {
            p.tabs.remove(index);
            p.active = p.active.min(p.tabs.len().saturating_sub(1));
        }
        if views == 1 {
            self.documents.remove(&id);
            self.highlighter.forget(id);
            self.recovery.remove(id);
            self.recovery_revision.remove(&id);
            self.conflicts.remove(&id);
        }
    }
    fn search(&mut self, backwards: bool) {
        if self.find.is_empty() {
            return;
        }
        let Some(p) = self.layout.pane_mut(self.active_pane) else {
            return;
        };
        let Some(v) = p.tabs.get_mut(p.active) else {
            return;
        };
        let Some(d) = self.documents.get(&v.document) else {
            return;
        };
        let hay = d.text();
        let start = d.rope.char_to_byte(if backwards {
            selection(v).start.min(d.len())
        } else {
            v.cursor.min(d.len())
        });
        let found = find_match(&hay, &self.find, self.match_case, start, backwards);
        if let Some((a, b)) = found {
            v.anchor = Some(d.rope.byte_to_char(a));
            v.cursor = d.rope.byte_to_char(b);
            v.reveal = true;
        } else {
            self.message = Some("No matches".into());
        }
    }
    fn replace_selection(&mut self) {
        let Some(p) = self.layout.pane_mut(self.active_pane) else {
            return;
        };
        let Some(v) = p.tabs.get_mut(p.active) else {
            return;
        };
        let Some(d) = self.documents.get_mut(&v.document) else {
            return;
        };
        if let Some(a) = v.anchor {
            let r = a.min(v.cursor)..a.max(v.cursor);
            let selected = d.slice(r.clone());
            if selected == self.find
                || (!self.match_case && selected.to_lowercase() == self.find.to_lowercase())
            {
                d.replace(r.clone(), &self.replacement);
                v.cursor = r.start + self.replacement.chars().count();
                v.anchor = None;
                v.observed_revision = d.revision;
            }
        }
    }
    fn command(&mut self, command: String) {
        match command.as_str() {
            "w" => {
                if let Some(id) = self.active_id() {
                    self.save_id(id, false, false);
                }
            }
            "q" => {
                if let Some(p) = self.layout.pane_mut(self.active_pane) {
                    let index = p.active;
                    self.close_tab(self.active_pane, index, false);
                }
            }
            "q!" => {
                if let Some(p) = self.layout.pane_mut(self.active_pane) {
                    let index = p.active;
                    self.close_tab(self.active_pane, index, true);
                }
            }
            "wq" => {
                if let Some(id) = self.active_id() {
                    if self.save_id(id, false, false) {
                        if let Some(p) = self.layout.pane_mut(self.active_pane) {
                            let index = p.active;
                            self.close_tab(self.active_pane, index, false);
                        }
                    }
                }
            }
            _ => {
                if let Ok(line) = command.parse::<usize>() {
                    if let Some(p) = self.layout.pane_mut(self.active_pane) {
                        if let Some(v) = p.tabs.get_mut(p.active) {
                            if let Some(d) = self.documents.get(&v.document) {
                                v.cursor = d.line_start(line.saturating_sub(1));
                                v.anchor = None;
                                v.reveal = true;
                            }
                        }
                    }
                } else {
                    self.message = Some(format!("Unsupported command: {command}"));
                }
            }
        }
    }
}

impl eframe::App for Twill {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.quit_requested = true;
        }
        for path in ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect::<Vec<_>>()
        }) {
            self.open(path);
        }
        let mut actions = Vec::new();
        let terminal_focused =
            self.terminal_visible && self.terminal.as_ref().is_some_and(|t| t.has_focus(ctx));
        if !terminal_focused {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, Key::N)) {
                self.new_document();
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, Key::O)) {
                if let Some(path) = rfd::FileDialog::new().pick_file() {
                    self.open(path);
                }
            }
            if ctx.input_mut(|i| {
                i.consume_key(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, Key::S)
            }) {
                actions.push(Action::Save(true));
            } else if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, Key::S)) {
                actions.push(Action::Save(false));
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, Key::F)) {
                self.find_open = true;
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, Key::G)) {
                self.command = Some(String::new());
                self.command_focus = true;
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, Key::W)) {
                if let Some(p) = self.layout.pane_mut(self.active_pane) {
                    actions.push(Action::Close(p.id, p.active));
                }
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, Key::Tab)) {
                if let Some(p) = self.layout.pane_mut(self.active_pane) {
                    if !p.tabs.is_empty() {
                        p.active = (p.active + 1) % p.tabs.len();
                    }
                }
            }
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, Key::Backtick)) {
            self.terminal_visible = !self.terminal_visible;
        }
        if ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers::CTRL | egui::Modifiers::ALT,
                Key::ArrowRight,
            )
        }) {
            let mut ids = Vec::new();
            self.layout.each(&mut |p| ids.push(p.id));
            if let Some(i) = ids.iter().position(|id| *id == self.active_pane) {
                self.active_pane = ids[(i + 1) % ids.len()];
            }
        }
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.label(
                    egui::RichText::new("twill")
                        .strong()
                        .color(Color32::from_rgb(122, 190, 174)),
                );
                ui.separator();
                ui.menu_button("File", |ui| {
                    if ui.button("New                 Ctrl+N").clicked() {
                        self.new_document();
                        ui.close_menu();
                    }
                    if ui.button("Open file          Ctrl+O").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_file() {
                            self.open(p);
                        }
                        ui.close_menu();
                    }
                    if ui.button("Open folder...").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            self.open(p);
                        }
                        ui.close_menu();
                    }
                    if ui.button("Save                 Ctrl+S").clicked() {
                        actions.push(Action::Save(false));
                        ui.close_menu();
                    }
                    if ui.button("Save as...").clicked() {
                        actions.push(Action::Save(true));
                        ui.close_menu();
                    }
                });
                ui.menu_button("View", |ui| {
                    if ui.button("Split side by side").clicked() {
                        actions.push(Action::Split(true));
                        ui.close_menu();
                    }
                    if ui.button("Split top and bottom").clicked() {
                        actions.push(Action::Split(false));
                        ui.close_menu();
                    }
                    if ui.button("Close current pane").clicked() {
                        if let Some(id) = self.layout.close_pane(self.active_pane) {
                            self.active_pane = id;
                        }
                        ui.close_menu();
                    }
                    if ui.button("Find / replace").clicked() {
                        self.find_open = true;
                        ui.close_menu();
                    }
                    ui.checkbox(&mut self.terminal_visible, "Terminal");
                });
                ui.menu_button("Settings", |ui| {
                    let mut changed = ui.checkbox(&mut self.settings.dark, "Dark theme").changed();
                    changed |= ui.checkbox(&mut self.settings.vim, "Vim mode").changed();
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut self.settings.font_size, 10.0..=28.0)
                                .text("Font size"),
                        )
                        .changed();
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut self.settings.tab_width, 1..=8)
                                .text("Tab width"),
                        )
                        .changed();
                    changed |= ui
                        .checkbox(&mut self.settings.insert_spaces, "Insert spaces")
                        .changed();
                    if changed {
                        ctx.set_visuals(if self.settings.dark {
                            egui::Visuals::dark()
                        } else {
                            egui::Visuals::light()
                        });
                        if let Err(e) = self.settings.save() {
                            self.message = Some(e.to_string());
                        }
                    }
                });
            });
        });
        if self.find_open {
            egui::TopBottomPanel::top("find").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Find");
                    let r = ui.add(egui::TextEdit::singleline(&mut self.find).desired_width(180.0));
                    if r.has_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        actions.push(Action::Find(false));
                    }
                    ui.checkbox(&mut self.match_case, "Aa");
                    if ui.button("Previous").clicked() {
                        actions.push(Action::Find(true));
                    }
                    if ui.button("Next").clicked() {
                        actions.push(Action::Find(false));
                    }
                    ui.label("Replace");
                    ui.add(egui::TextEdit::singleline(&mut self.replacement).desired_width(140.0));
                    if ui.button("Replace").clicked() {
                        self.replace_selection();
                        actions.push(Action::Find(false));
                    }
                    if ui.button("Close").clicked() {
                        self.find_open = false;
                    }
                });
            });
        }
        if self.command.is_some() {
            egui::TopBottomPanel::bottom("command").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let prefix = if self
                        .command
                        .as_ref()
                        .is_some_and(|s| s.starts_with('/') || s.starts_with('?'))
                    {
                        ""
                    } else {
                        ":"
                    };
                    ui.label(prefix);
                    let r = ui.add(
                        egui::TextEdit::singleline(self.command.as_mut().unwrap())
                            .desired_width(f32::INFINITY),
                    );
                    if self.command_focus {
                        r.request_focus();
                        self.command_focus = false;
                    }
                    if ui.input(|i| i.key_pressed(Key::Escape)) {
                        self.command = None;
                    } else if ui.input(|i| i.key_pressed(Key::Enter)) {
                        if let Some(s) = self.command.take() {
                            if s.starts_with('/') || s.starts_with('?') {
                                self.search_backwards = s.starts_with('?');
                                self.find = s[1..].to_string();
                                actions.push(Action::Find(self.search_backwards));
                            } else {
                                actions.push(Action::Command(s));
                            }
                        }
                    }
                });
            });
        }
        let mut language_change = None;
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some(p) = self.layout.pane_mut(self.active_pane) {
                    if let Some(v) = p.tabs.get_mut(p.active) {
                        if let Some(d) = self.documents.get(&v.document) {
                            ui.label(if self.settings.vim {
                                format!("{:?}", v.vim.mode)
                            } else {
                                "TEXT".into()
                            });
                            ui.separator();
                            ui.label(format!(
                                "Ln {}, Col {}",
                                d.line_of(v.cursor) + 1,
                                v.cursor.saturating_sub(d.line_start(d.line_of(v.cursor))) + 1
                            ));
                            ui.separator();
                            ui.label("UTF-8");
                            ui.label(if d.preferred_newline() == "\r\n" {
                                "CRLF"
                            } else {
                                "LF"
                            });
                            let old_language = v.language.clone();
                            egui::ComboBox::from_id_salt("language")
                                .selected_text(v.language.as_deref().unwrap_or("Auto"))
                                .width(65.0)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut v.language, None, "Auto");
                                    for ext in [
                                        "txt", "html", "md", "c", "cpp", "go", "py", "json",
                                        "jsonc", "yaml", "rs", "ts", "js", "jsx", "tsx", "css",
                                        "sql", "toml", "tex",
                                    ] {
                                        ui.selectable_value(
                                            &mut v.language,
                                            Some(ext.to_owned()),
                                            ext,
                                        );
                                    }
                                });
                            if old_language != v.language {
                                language_change = Some((v.document, v.language.clone()));
                            }
                            if d.rope.len_bytes() > 10 * 1024 * 1024 {
                                ui.label("Large file: syntax off");
                            }
                        }
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label("local · no services");
                });
            });
        });
        if let Some((id, language)) = language_change {
            self.layout.each(&mut |p| {
                for v in &mut p.tabs {
                    if v.document == id {
                        v.language = language.clone();
                    }
                }
            });
        }
        if self.terminal_visible {
            egui::TopBottomPanel::bottom("terminal")
                .resizable(true)
                .default_height(210.0)
                .min_height(80.0)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("TERMINAL");
                        if self.terminal.is_none() {
                            for (name, shell) in [
                                ("PowerShell", Shell::PowerShell),
                                ("Command Prompt", Shell::CommandPrompt),
                                ("WSL", Shell::Wsl),
                            ] {
                                if ui.button(name).clicked() {
                                    match Terminal::spawn(shell, self.root.as_deref()) {
                                        Ok(t) => self.terminal = Some(t),
                                        Err(e) => self.message = Some(format!("Terminal: {e:#}")),
                                    }
                                }
                            }
                        } else if ui.button("End session").clicked() {
                            self.terminal_close = true;
                        }
                        if self.terminal.as_mut().is_some_and(|t| !t.is_running()) {
                            ui.label("Session exited");
                        }
                    });
                    if let Some(t) = self.terminal.as_mut() {
                        t.ui(ui);
                    }
                });
        }
        if let Some(root) = self.root.clone() {
            egui::SidePanel::left("files")
                .resizable(true)
                .default_width(210.0)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.strong(root.file_name().unwrap_or_default().to_string_lossy());
                        if ui.small_button("Refresh").clicked() {
                            self.tree_cache.clear();
                        }
                    });
                    egui::ScrollArea::both().show(ui, |ui| {
                        tree(
                            ui,
                            &root,
                            &mut self.tree_cache,
                            &mut self.tree_open,
                            &mut actions,
                            0,
                        );
                    });
                });
        }
        let modal = self.pending_close.is_some()
            || self.quit_requested
            || !self.recovered.is_empty()
            || self.command.is_some()
            || self.message.is_some()
            || self.terminal_close;
        let before: HashMap<_, _> = self
            .documents
            .iter()
            .map(|(id, d)| (*id, d.revision))
            .collect();
        egui::CentralPanel::default().show(ctx, |ui| {
            draw_layout(
                ui,
                &mut self.layout,
                &mut self.documents,
                &self.settings,
                &mut self.highlighter,
                self.active_pane,
                modal,
                &mut actions,
            );
        });
        if self
            .documents
            .iter()
            .any(|(id, d)| before.get(id) != Some(&d.revision))
        {
            self.last_edit = Instant::now();
        }
        for action in actions {
            match action {
                Action::Open(p) => self.open(p),
                Action::Close(p, i) => self.close_tab(p, i, false),
                Action::Save(save_as) => {
                    if let Some(id) = self.active_id() {
                        self.save_id(id, save_as, false);
                    }
                }
                Action::Split(h) => {
                    let id = self.id();
                    self.layout.split(self.active_pane, id, h);
                    self.active_pane = id;
                }
                Action::Focus(id) => self.active_pane = id,
                Action::Command(c) => {
                    if c == "/" || c == "?" || c == ":" {
                        self.command = Some(if c == ":" { String::new() } else { c });
                        self.command_focus = true;
                    } else if c == "n" {
                        self.search(self.search_backwards);
                    } else if c == "N" {
                        self.search(!self.search_backwards);
                    } else {
                        self.command(c);
                    }
                }
                Action::Find(b) => self.search(b),
            }
        }
        if self.watcher.changed() || self.last_poll.elapsed() > Duration::from_secs(30) {
            self.last_poll = Instant::now();
            self.tree_cache.clear();
            for (id, d) in &mut self.documents {
                if d.external_changed() {
                    if d.is_dirty() || d.reload().is_err() {
                        self.conflicts.insert(*id);
                    }
                }
            }
        }
        if self.last_edit.elapsed() > Duration::from_secs(2)
            && self.last_recovery.elapsed() > Duration::from_secs(2)
        {
            self.last_recovery = Instant::now();
            for (id, d) in &self.documents {
                if d.is_dirty() && self.recovery_revision.get(id) != Some(&d.revision) {
                    match self.recovery.write_document(d) {
                        Ok(()) => {
                            self.recovery_revision.insert(*id, d.revision);
                        }
                        Err(e) => self.message = Some(format!("Recovery: {e:#}")),
                    }
                }
            }
        }
        self.dialogs(ctx);
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

impl Twill {
    fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some(message) = self.message.clone() {
            egui::Window::new("twill")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(message);
                    if ui.button("OK").clicked() {
                        self.message = None;
                    }
                });
        }
        if let Some((pane, index)) = self.pending_close {
            let id = self
                .layout
                .pane_mut(pane)
                .and_then(|p| p.tabs.get(index))
                .map(|v| v.document);
            egui::Window::new("Unsaved changes")
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label("Save changes before closing this file?");
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked()
                            && id.is_some_and(|id| self.save_id(id, false, false))
                        {
                            self.pending_close = None;
                            self.close_tab(pane, index, true);
                        }
                        if ui.button("Discard").clicked() {
                            self.pending_close = None;
                            self.close_tab(pane, index, true);
                        }
                        if ui.button("Cancel").clicked() {
                            self.pending_close = None;
                        }
                    });
                });
        }
        if self.quit_requested {
            let dirty = self.documents.values().any(Document::is_dirty);
            if !dirty && self.terminal.is_none() {
                self.allow_quit = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                egui::Window::new("Close twill?")
                    .collapsible(false)
                    .show(ctx, |ui| {
                        ui.label(
                            "Unsaved files will need saving. Active terminal processes will stop.",
                        );
                        ui.horizontal(|ui| {
                            if ui.button("Save all and quit").clicked() {
                                let ids = self
                                    .documents
                                    .iter()
                                    .filter(|(_, d)| d.is_dirty())
                                    .map(|(id, _)| *id)
                                    .collect::<Vec<_>>();
                                if ids.into_iter().all(|id| self.save_id(id, false, false)) {
                                    self.allow_quit = true;
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                            }
                            if ui.button("Discard and quit").clicked() {
                                for id in self.documents.keys() {
                                    self.recovery.remove(*id);
                                }
                                self.allow_quit = true;
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                            if ui.button("Cancel").clicked() {
                                self.quit_requested = false;
                            }
                        });
                    });
            }
        }
        if self.terminal_close {
            egui::Window::new("End terminal session?")
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label("This stops the shell and its running command.");
                    if ui.button("End session").clicked() {
                        if let Some(mut t) = self.terminal.take() {
                            t.shutdown();
                        }
                        self.terminal_close = false;
                    }
                    if ui.button("Cancel").clicked() {
                        self.terminal_close = false;
                    }
                });
        }
        if let Some(id) = self.conflicts.iter().next().copied() {
            let name = self
                .documents
                .get(&id)
                .and_then(|d| d.path.as_ref())
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            egui::Window::new("File changed on disk")
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label(name);
                    ui.label("Your unsaved edits are still in memory.");
                    if ui.button("Reload and discard local edits").clicked() {
                        if let Some(d) = self.documents.get_mut(&id) {
                            match d.reload() {
                                Ok(()) => {
                                    self.conflicts.remove(&id);
                                    self.recovery.remove(id);
                                }
                                Err(e) => self.message = Some(e.to_string()),
                            }
                        }
                    }
                    if ui.button("Overwrite disk with local edits").clicked() {
                        self.save_id(id, false, true);
                    }
                    if ui.button("Save local edits as...").clicked() {
                        self.save_id(id, true, false);
                    }
                });
        }
        if !self.recovered.is_empty() {
            egui::Window::new("Recover unsaved work")
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label(format!("{} recovery file(s) found.", self.recovered.len()));
                    if ui.button("Restore").clicked() {
                        for (file, r) in std::mem::take(&mut self.recovered) {
                            let id = self.id();
                            let mut d = r
                                .path
                                .as_ref()
                                .and_then(|p| Document::open(id, p).ok())
                                .unwrap_or_else(|| Document::new(id));
                            let len = d.len();
                            d.replace(0..len, &r.text);
                            d.set_format(r.bom, &r.newline);
                            let persisted = self.recovery.write_document(&d);
                            self.add_document(d);
                            if persisted.is_ok() {
                                let _ = fs::remove_file(file);
                            } else if let Err(e) = persisted {
                                self.message =
                                    Some(format!("Recovery retained at {}: {e}", file.display()));
                            }
                        }
                    }
                    if ui.button("Discard recovery files").clicked() {
                        for (file, _) in self.recovered.drain(..) {
                            let _ = fs::remove_file(file);
                        }
                    }
                });
        }
    }
}

fn tree(
    ui: &mut egui::Ui,
    path: &Path,
    cache: &mut HashMap<PathBuf, Vec<PathBuf>>,
    open: &mut HashSet<PathBuf>,
    actions: &mut Vec<Action>,
    depth: usize,
) {
    if depth > 32 {
        return;
    }
    let entries = cache
        .entry(path.to_path_buf())
        .or_insert_with(|| {
            let mut entries = fs::read_dir(path)
                .map(|it| it.flatten().map(|e| e.path()).collect::<Vec<_>>())
                .unwrap_or_default();
            entries.sort_by_key(|p| {
                (
                    !p.is_dir(),
                    p.file_name().unwrap_or_default().to_ascii_lowercase(),
                )
            });
            entries
        })
        .clone();
    for child in entries {
        let name = child.file_name().unwrap_or_default().to_string_lossy();
        let dir = child.is_dir();
        let link = fs::symlink_metadata(&child).is_ok_and(|m| m.file_type().is_symlink());
        ui.horizontal(|ui| {
            ui.add_space(depth as f32 * 10.0);
            if dir {
                let expanded = open.contains(&child);
                if ui
                    .selectable_label(
                        expanded,
                        format!("{} {}", if expanded { "▾" } else { "▸" }, name),
                    )
                    .clicked()
                {
                    if expanded {
                        open.remove(&child);
                    } else if !link {
                        open.insert(child.clone());
                    }
                }
            } else if ui.selectable_label(false, name).clicked() {
                actions.push(Action::Open(child.clone()));
            }
        });
        if dir && open.contains(&child) && !link {
            tree(ui, &child, cache, open, actions, depth + 1);
        }
    }
}

// Explicit borrows keep the UI from borrowing the whole application during rendering.
#[allow(clippy::too_many_arguments)]
fn draw_layout(
    ui: &mut egui::Ui,
    layout: &mut Layout,
    docs: &mut HashMap<u64, Document>,
    settings: &Settings,
    syntax: &mut Highlighter,
    active: u64,
    modal: bool,
    actions: &mut Vec<Action>,
) {
    match layout {
        Layout::Split {
            horizontal,
            fraction,
            first,
            second,
        } => {
            let rect = ui.available_rect_before_wrap();
            let extent = if *horizontal {
                rect.width()
            } else {
                rect.height()
            };
            let split = extent * (*fraction);
            let mut a = rect;
            let mut b = rect;
            let mut divider = rect;
            if *horizontal {
                a.max.x = rect.min.x + split - 3.0;
                b.min.x = rect.min.x + split + 3.0;
                divider.min.x = a.max.x;
                divider.max.x = b.min.x;
            } else {
                a.max.y = rect.min.y + split - 3.0;
                b.min.y = rect.min.y + split + 3.0;
                divider.min.y = a.max.y;
                divider.max.y = b.min.y;
            }
            let r = ui.interact(
                divider,
                ui.id().with((
                    "divider",
                    active,
                    rect.min.x.to_bits(),
                    rect.min.y.to_bits(),
                )),
                Sense::drag(),
            );
            if r.dragged() {
                let delta = if *horizontal {
                    r.drag_delta().x
                } else {
                    r.drag_delta().y
                };
                *fraction = (*fraction + delta / extent).clamp(0.15, 0.85);
            }
            ui.painter()
                .rect_filled(divider, 0.0, ui.visuals().widgets.noninteractive.bg_fill);
            ui.scope_builder(egui::UiBuilder::new().max_rect(a).id_salt("first"), |ui| {
                draw_layout(ui, first, docs, settings, syntax, active, modal, actions)
            });
            ui.scope_builder(egui::UiBuilder::new().max_rect(b).id_salt("second"), |ui| {
                draw_layout(ui, second, docs, settings, syntax, active, modal, actions)
            });
        }
        Layout::Leaf(p) => {
            ui.push_id(p.id, |ui| {
                egui::ScrollArea::horizontal()
                    .id_salt("tabs")
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for (i, v) in p.tabs.iter().enumerate() {
                                if let Some(d) = docs.get(&v.document) {
                                    let title = d
                                        .path
                                        .as_ref()
                                        .and_then(|p| p.file_name())
                                        .map(|s| s.to_string_lossy().into_owned())
                                        .unwrap_or_else(|| "Untitled".into());
                                    let label = format!(
                                        "{}{}",
                                        title,
                                        if d.is_dirty() { " *" } else { "" }
                                    );
                                    if ui.selectable_label(p.active == i, label).clicked() {
                                        p.active = i;
                                        actions.push(Action::Focus(p.id));
                                    }
                                    if ui.small_button("×").clicked() {
                                        actions.push(Action::Close(p.id, i));
                                    }
                                }
                            }
                        });
                    });
                ui.separator();
                if let Some(v) = p.tabs.get_mut(p.active) {
                    if let Some(d) = docs.get_mut(&v.document) {
                        editor(
                            ui,
                            p.id,
                            v,
                            d,
                            settings,
                            syntax,
                            p.id == active,
                            modal,
                            actions,
                        );
                    }
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label("Open a file or press Ctrl+N");
                    });
                }
            });
        }
    }
}

fn selection(v: &View) -> std::ops::Range<usize> {
    let a = v.anchor.unwrap_or(v.cursor);
    a.min(v.cursor)..a.max(v.cursor)
}
fn find_match(
    hay: &str,
    needle: &str,
    match_case: bool,
    start: usize,
    backwards: bool,
) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    let target = if match_case {
        needle.to_owned()
    } else {
        needle.to_lowercase()
    };
    let source = if match_case {
        hay.to_owned()
    } else {
        hay.to_lowercase()
    };
    let mut first = None;
    let mut last = None;
    let mut previous = None;
    for (pos, _) in source.match_indices(&target) {
        let found = (pos, pos + target.len());
        first.get_or_insert(found);
        last = Some(found);
        if backwards {
            if pos < start {
                previous = Some(found);
            }
        } else if pos >= start {
            return Some(found);
        }
    }
    if backwards {
        previous.or(last)
    } else {
        first
    }
}
fn transform_position(p: usize, start: usize, removed: usize, inserted: usize) -> usize {
    if p <= start {
        p
    } else if p >= start + removed {
        p - removed + inserted
    } else {
        start + inserted
    }
}
fn indent_lines(v: &mut View, d: &mut Document, settings: &Settings, outdent: bool) {
    let range = selection(v);
    let first = d.line_of(range.start);
    let last = d.line_of(if range.end > range.start {
        range.end - 1
    } else {
        range.end
    });
    d.begin_undo_group();
    for line in (first..=last).rev() {
        let start = d.line_start(line);
        let text = d.line_text(line);
        let (remove, insert) = if outdent {
            let count = if text.starts_with('\t') {
                1
            } else {
                text.chars()
                    .take(settings.tab_width)
                    .take_while(|c| *c == ' ')
                    .count()
            };
            (count, String::new())
        } else {
            (
                0,
                if settings.insert_spaces {
                    " ".repeat(settings.tab_width)
                } else {
                    "\t".into()
                },
            )
        };
        d.replace(start..start + remove, &insert);
        v.cursor = transform_position(v.cursor, start, remove, insert.chars().count());
        v.anchor = v
            .anchor
            .map(|p| transform_position(p, start, remove, insert.chars().count()));
    }
    d.end_undo_group();
    v.observed_revision = d.revision;
    v.reveal = true;
}
fn insert(v: &mut View, d: &mut Document, text: &str) {
    let r = selection(v);
    d.replace(r.clone(), text);
    v.cursor = r.start + text.chars().count();
    v.anchor = None;
    v.reveal = true;
}

#[allow(clippy::too_many_arguments)]
fn editor(
    ui: &mut egui::Ui,
    pane: u64,
    v: &mut View,
    d: &mut Document,
    settings: &Settings,
    syntax: &mut Highlighter,
    active: bool,
    modal: bool,
    actions: &mut Vec<Action>,
) {
    if let Some(changes) = d.changes_since(v.observed_revision) {
        for (start, removed, inserted) in changes {
            v.cursor = transform_position(v.cursor, start, removed, inserted);
            v.anchor = v
                .anchor
                .map(|p| transform_position(p, start, removed, inserted));
        }
    }
    v.cursor = v.cursor.min(d.len());
    v.anchor = v.anchor.map(|a| a.min(d.len()));
    let font = FontId::monospace(settings.font_size);
    let row_height = settings.font_size * 1.45;
    let extension = v.language.as_deref().unwrap_or_else(|| {
        d.path
            .as_ref()
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .unwrap_or("txt")
    });
    let highlight = d.rope.len_bytes() <= 10 * 1024 * 1024;
    if highlight {
        syntax.request(d.id, d.revision, extension, d.rope.clone(), settings.dark);
    }
    let id = ui.id().with("editor");
    let rect = ui.available_rect_before_wrap();
    let response = ui.interact(rect, id, Sense::click_and_drag());
    if response.clicked() || response.drag_started() {
        d.end_undo_group();
        v.typing_until = None;
        response.request_focus();
        actions.push(Action::Focus(pane));
    }
    if active && !modal && !ui.ctx().wants_keyboard_input() {
        response.request_focus();
    }
    let focused = response.has_focus() && !modal;
    let events = if focused {
        ui.input(|i| i.events.clone())
    } else {
        vec![]
    };
    for event in events {
        let breaks_typing = matches!(
            &event,
            egui::Event::Copy | egui::Event::Cut | egui::Event::Paste(_)
        ) || matches!(&event,egui::Event::Key{key,pressed:true,modifiers,..} if modifiers.ctrl || matches!(key,Key::ArrowLeft|Key::ArrowRight|Key::ArrowUp|Key::ArrowDown|Key::Home|Key::End|Key::PageUp|Key::PageDown|Key::Backspace|Key::Delete|Key::Enter|Key::Tab|Key::Escape));
        if !settings.vim && breaks_typing {
            d.end_undo_group();
            v.typing_until = None;
        }
        match event {
            egui::Event::Copy => {
                let r = selection(v);
                if !r.is_empty() {
                    ui.ctx().copy_text(d.slice(r));
                }
            }
            egui::Event::Cut => {
                let r = selection(v);
                if !r.is_empty() {
                    ui.ctx().copy_text(d.slice(r.clone()));
                    d.replace(r.clone(), "");
                    v.cursor = r.start;
                    v.anchor = None;
                }
            }
            egui::Event::Paste(text) => {
                if settings.vim && v.vim.mode == VimMode::Normal {
                    v.vim.handle_key("i", d, &mut v.cursor, &mut v.anchor);
                    for ch in text.chars() {
                        v.vim
                            .handle_key(&ch.to_string(), d, &mut v.cursor, &mut v.anchor);
                    }
                    v.vim.handle_key("Escape", d, &mut v.cursor, &mut v.anchor);
                    v.reveal = true;
                } else {
                    insert(v, d, &text);
                }
            }
            egui::Event::Text(text) => {
                if settings.vim {
                    for ch in text.chars() {
                        let key = ch.to_string();
                        if v.vim.mode != VimMode::Insert
                            && matches!(ch, ':' | '/' | '?' | 'n' | 'N')
                        {
                            actions.push(Action::Command(key));
                        } else {
                            v.vim.handle_key(&key, d, &mut v.cursor, &mut v.anchor);
                            v.reveal = true;
                        }
                    }
                } else {
                    if v.typing_until.is_none_or(|until| Instant::now() > until) {
                        d.begin_undo_group();
                    }
                    insert(v, d, &text);
                    v.typing_until = Some(Instant::now() + Duration::from_millis(750));
                }
            }
            egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                if !settings.vim || v.vim.mode == VimMode::Insert {
                    insert(v, d, &text);
                }
            }
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => {
                if modifiers.ctrl && key == Key::A {
                    v.anchor = Some(0);
                    v.cursor = d.len();
                    continue;
                }
                if modifiers.ctrl && key == Key::Z {
                    if modifiers.shift {
                        if let Some(p) = d.redo() {
                            v.cursor = p;
                        }
                    } else if let Some(p) = d.undo() {
                        v.cursor = p;
                    }
                    v.anchor = None;
                    v.reveal = true;
                    continue;
                }
                if modifiers.ctrl && key == Key::Y {
                    if let Some(p) = d.redo() {
                        v.cursor = p;
                    }
                    v.anchor = None;
                    continue;
                }
                if settings.vim {
                    let vk = match key {
                        Key::Escape => Some("Escape"),
                        Key::Backspace => Some("Backspace"),
                        Key::Enter => Some("Enter"),
                        Key::Tab => Some("Tab"),
                        Key::R if modifiers.ctrl => Some("Ctrl+r"),
                        _ => None,
                    };
                    if let Some(k) = vk {
                        let out = v.vim.handle_key(k, d, &mut v.cursor, &mut v.anchor);
                        let _ = &out.message;
                        if out.handled {
                            v.reveal = true;
                            continue;
                        }
                    }
                    if v.vim.mode != VimMode::Insert
                        && matches!(key, Key::Backspace | Key::Delete | Key::Enter | Key::Tab)
                    {
                        continue;
                    }
                }
                let old = v.cursor;
                let line = d.line_of(v.cursor);
                let start = d.line_start(line);
                let text = d.line_text(line);
                let col = v.cursor - start;
                match key {
                    Key::ArrowLeft => {
                        if col > 0 {
                            v.cursor = start + prev_grapheme(&text, col);
                        } else if line > 0 {
                            v.cursor =
                                d.line_start(line - 1) + d.line_text(line - 1).chars().count();
                        }
                    }
                    Key::ArrowRight => {
                        if col < text.chars().count() {
                            v.cursor = start + next_grapheme(&text, col);
                        } else if line + 1 < d.line_count() {
                            v.cursor = d.line_start(line + 1);
                        }
                    }
                    Key::ArrowUp | Key::ArrowDown | Key::PageUp | Key::PageDown => {
                        let delta = if matches!(key, Key::PageUp | Key::PageDown) {
                            (rect.height() / row_height).max(1.0) as usize
                        } else {
                            1
                        };
                        let target = if matches!(key, Key::ArrowUp | Key::PageUp) {
                            line.saturating_sub(delta)
                        } else {
                            (line + delta).min(d.line_count() - 1)
                        };
                        v.cursor =
                            d.line_start(target) + col.min(d.line_text(target).chars().count());
                    }
                    Key::Home => v.cursor = if modifiers.ctrl { 0 } else { start },
                    Key::End => {
                        v.cursor = if modifiers.ctrl {
                            d.len()
                        } else {
                            start + text.chars().count()
                        }
                    }
                    Key::Backspace => {
                        if !selection(v).is_empty() {
                            insert(v, d, "");
                        } else if v.cursor > 0 {
                            let p = if col > 0 {
                                start + prev_grapheme(&text, col)
                            } else {
                                d.line_start(line - 1) + d.line_text(line - 1).chars().count()
                            };
                            d.replace(p..v.cursor, "");
                            v.cursor = p;
                        }
                    }
                    Key::Delete => {
                        if !selection(v).is_empty() {
                            insert(v, d, "");
                        } else if v.cursor < d.len() {
                            let end = if col < text.chars().count() {
                                start + next_grapheme(&text, col)
                            } else {
                                d.line_start(line + 1)
                            };
                            d.replace(v.cursor..end, "");
                        }
                    }
                    Key::Enter => {
                        let indent: String = text
                            .chars()
                            .take_while(|c| *c == ' ' || *c == '\t')
                            .collect();
                        let s = format!("{}{}", d.preferred_newline(), indent);
                        insert(v, d, &s);
                    }
                    Key::Tab => {
                        if modifiers.shift || !selection(v).is_empty() {
                            indent_lines(v, d, settings, modifiers.shift);
                        } else {
                            let s = if settings.insert_spaces {
                                " ".repeat(settings.tab_width - col % settings.tab_width)
                            } else {
                                "\t".into()
                            };
                            insert(v, d, &s);
                        }
                    }
                    Key::Escape => v.anchor = None,
                    _ => {}
                }
                if matches!(
                    key,
                    Key::ArrowLeft
                        | Key::ArrowRight
                        | Key::ArrowUp
                        | Key::ArrowDown
                        | Key::Home
                        | Key::End
                        | Key::PageUp
                        | Key::PageDown
                ) {
                    if modifiers.shift {
                        v.anchor = Some(v.anchor.unwrap_or(old));
                    } else {
                        v.anchor = None;
                    }
                    v.reveal = true;
                }
            }
            _ => {}
        }
    }
    let gutter = 52.0;
    v.observed_revision = d.revision;
    let mut selected = selection(v);
    if settings.vim && v.vim.mode == VimMode::Visual {
        selected.end = d.next_grapheme(selected.end);
    } else if settings.vim && v.vim.mode == VimMode::VisualLine {
        selected.start = d.line_start(d.line_of(selected.start));
        let line = d.line_of(selected.end);
        selected.end = if line + 1 < d.line_count() {
            d.line_start(line + 1)
        } else {
            d.len()
        };
    }
    let cursor_line = d.line_of(v.cursor);
    let mut cursor_rect = None;
    let scroll = egui::ScrollArea::both()
        .id_salt(("document_scroll", d.id))
        .drag_to_scroll(false)
        .auto_shrink([false, false]);
    ui.spacing_mut().item_spacing.y = 0.0;
    scroll.show_rows(ui, row_height, d.line_count(), |ui, rows| {
        let first_row = rows.start;
        if highlight {
            syntax.request_visible(d.id, d.revision, pane, rows.start, rows.end);
        }
        for line in rows {
            let document_line_start = d.line_start(line);
            let segment_start = if line == cursor_line {
                (v.cursor - document_line_start) / 4096 * 4096
            } else {
                0
            };
            let line_start = document_line_start + segment_start;
            let clipped: String = d
                .rope
                .line(line)
                .chars_at(segment_start)
                .take(4096)
                .collect::<String>()
                .trim_end_matches(['\r', '\n'])
                .to_owned();
            let display = clipped.replace('\t', &" ".repeat(settings.tab_width));
            let mut job = syntax.line(d.id, d.revision, line, &clipped, settings.dark);
            // Expanded tabs use plain formatting to preserve exact cursor geometry.
            if clipped.contains('\t') {
                job = egui::text::LayoutJob::simple(
                    display.clone(),
                    font.clone(),
                    ui.visuals().text_color(),
                    f32::INFINITY,
                );
            } else {
                for section in &mut job.sections {
                    section.format.font_id = font.clone();
                }
            }
            job.wrap.max_width = f32::INFINITY;
            let galley = ui.fonts(|f| f.layout_job(job));
            let width = (gutter + galley.size().x + 30.0).max(ui.available_width());
            let (row, _) = ui.allocate_exact_size(Vec2::new(width, row_height), Sense::hover());
            let origin = Pos2::new(row.min.x + gutter, row.min.y);
            if line == cursor_line {
                ui.painter().rect_filled(
                    row,
                    0.0,
                    if settings.dark {
                        Color32::from_rgb(32, 38, 43)
                    } else {
                        Color32::from_rgb(235, 239, 239)
                    },
                );
            }
            ui.painter().text(
                Pos2::new(row.min.x + gutter - 10.0, row.min.y),
                egui::Align2::RIGHT_TOP,
                (line + 1).to_string(),
                font.clone(),
                ui.visuals().weak_text_color(),
            );
            let char_count = clipped.chars().count();
            let a = selected.start.saturating_sub(line_start).min(char_count);
            let b = selected.end.saturating_sub(line_start).min(char_count);
            let visual_col = |col: usize| {
                clipped
                    .chars()
                    .take(col)
                    .map(|c| if c == '\t' { settings.tab_width } else { 1 })
                    .sum::<usize>()
            };
            let x_for = |col: usize| {
                galley
                    .pos_from_cursor(
                        &galley.from_ccursor(egui::text::CCursor::new(visual_col(col))),
                    )
                    .min
                    .x
            };
            if a < b {
                ui.painter().rect_filled(
                    Rect::from_min_max(
                        Pos2::new(origin.x + x_for(a), row.min.y),
                        Pos2::new(origin.x + x_for(b), row.max.y),
                    ),
                    0.0,
                    Color32::from_rgba_unmultiplied(65, 120, 180, 100),
                );
            }
            ui.painter()
                .galley(origin, galley.clone(), ui.visuals().text_color());
            if line == cursor_line {
                let col = v.cursor.saturating_sub(line_start).min(char_count);
                let x = origin.x + x_for(col);
                let caret =
                    Rect::from_min_size(Pos2::new(x, row.min.y), Vec2::new(2.0, row_height));
                cursor_rect = Some(caret);
                if focused {
                    if settings.vim && v.vim.mode != VimMode::Insert {
                        ui.painter().rect_stroke(
                            Rect::from_min_size(
                                caret.min,
                                Vec2::new(settings.font_size * 0.6, row_height),
                            ),
                            0.0,
                            Stroke::new(1.0_f32, ui.visuals().text_color()),
                            egui::StrokeKind::Inside,
                        );
                    } else {
                        ui.painter()
                            .rect_filled(caret, 0.0, ui.visuals().text_color());
                    }
                }
            }
            if (response.clicked() || response.dragged())
                && ui
                    .input(|i| i.pointer.interact_pos())
                    .is_some_and(|p| row.contains(p))
            {
                let pos = ui.input(|i| i.pointer.interact_pos()).unwrap();
                let visual = galley.cursor_from_pos(pos - origin).ccursor.index;
                let mut count = 0;
                let mut col = 0;
                for ch in clipped.chars() {
                    let n = if ch == '\t' { settings.tab_width } else { 1 };
                    if count + n > visual {
                        break;
                    }
                    count += n;
                    col += 1;
                }
                let target = line_start + col;
                if response.dragged() || ui.input(|i| i.modifiers.shift) {
                    v.anchor = Some(v.anchor.unwrap_or(v.cursor));
                } else {
                    v.anchor = None;
                }
                v.cursor = target;
                if response.double_clicked() {
                    let chars = clipped.chars().collect::<Vec<_>>();
                    let mut a = col;
                    let mut b = col;
                    while a > 0 && (chars[a - 1].is_alphanumeric() || chars[a - 1] == '_') {
                        a -= 1;
                    }
                    while b < chars.len() && (chars[b].is_alphanumeric() || chars[b] == '_') {
                        b += 1;
                    }
                    v.anchor = Some(line_start + a);
                    v.cursor = line_start + b;
                }
            }
        }
        if v.reveal {
            let y = ui.max_rect().min.y + (cursor_line as f32 - first_row as f32) * row_height;
            ui.scroll_to_rect(
                cursor_rect.unwrap_or_else(|| {
                    Rect::from_min_size(
                        Pos2::new(ui.max_rect().min.x, y),
                        Vec2::new(1.0, row_height),
                    )
                }),
                None,
            );
            v.reveal = false;
        }
    });
    if focused {
        if let Some(caret) = cursor_rect {
            ui.ctx().output_mut(|out| {
                out.ime = Some(egui::output::IMEOutput {
                    rect,
                    cursor_rect: caret,
                });
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn find_wraps_without_skipping_adjacent_matches_or_splitting_unicode() {
        assert_eq!(find_match("aaaa", "aa", true, 2, false), Some((2, 4)));
        assert_eq!(
            find_match("Élan élan", "élan", false, 0, false),
            Some((0, 5))
        );
        assert_eq!(
            find_match("one two one", "one", true, 8, true),
            Some((0, 3))
        );
        assert_eq!(
            find_match("one two one", "one", true, 11, false),
            Some((0, 3))
        );
    }
    #[test]
    fn indent_selection_and_undo_preserve_crlf() {
        let mut d = Document::new(1);
        d.replace(0..0, "one\r\ntwo\r\n");
        let mut v = View::new(1);
        v.anchor = Some(0);
        v.cursor = d.len();
        indent_lines(&mut v, &mut d, &Settings::default(), false);
        assert_eq!(d.text(), "    one\r\n    two\r\n");
        d.undo();
        assert_eq!(d.text(), "one\r\ntwo\r\n");
    }
    #[test]
    fn split_close_keeps_every_document_accessible() {
        let mut layout = Layout::Leaf(Pane {
            id: 1,
            tabs: vec![View::new(10)],
            active: 0,
        });
        assert!(layout.split(1, 2, true));
        layout.pane_mut(2).unwrap().tabs.push(View::new(20));
        assert_eq!(layout.close_pane(2), Some(1));
        let pane = layout.pane_mut(1).unwrap();
        assert_eq!(
            pane.tabs.iter().map(|v| v.document).collect::<Vec<_>>(),
            vec![10, 20]
        );
    }
    #[test]
    fn editor_accepts_unicode_and_shared_views_track_edits() {
        let ctx = egui::Context::default();
        let mut docs = HashMap::new();
        docs.insert(10, Document::new(10));
        let mut layout = Layout::Leaf(Pane {
            id: 1,
            tabs: vec![View::new(10)],
            active: 0,
        });
        let mut syntax = Highlighter::new();
        let settings = Settings::default();
        let mut frame =
            |events: Vec<egui::Event>, layout: &mut Layout, docs: &mut HashMap<u64, Document>| {
                let input = egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
                    events,
                    ..Default::default()
                };
                let _ = ctx.run(input, |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        draw_layout(
                            ui,
                            layout,
                            docs,
                            &settings,
                            &mut syntax,
                            1,
                            false,
                            &mut vec![],
                        )
                    });
                });
            };
        frame(vec![], &mut layout, &mut docs);
        frame(
            vec![egui::Event::Text("hello 🧵".into())],
            &mut layout,
            &mut docs,
        );
        assert_eq!(docs[&10].text(), "hello 🧵");
        assert_eq!(layout.pane_mut(1).unwrap().tabs[0].cursor, 7);
        let revision = docs[&10].revision;
        layout.split(1, 2, true);
        {
            let view = &mut layout.pane_mut(2).unwrap().tabs[0];
            view.cursor = 7;
            view.observed_revision = revision;
        }
        docs.get_mut(&10).unwrap().replace(0..0, "A");
        frame(vec![], &mut layout, &mut docs);
        assert_eq!(layout.pane_mut(2).unwrap().tabs[0].cursor, 8);
    }
}
