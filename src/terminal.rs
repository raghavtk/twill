//! Embedded pseudoterminal, with parsing and process IO outside the UI thread.
use std::{
    collections::VecDeque,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender, TrySendError},
        Arc, Mutex,
    },
    thread,
};

use anyhow::{Context, Result};
use egui::{Color32, FontId, Key, Modifiers, Pos2, Rect, Sense, Ui, Vec2};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use vt100::{Color, Parser};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::enum_variant_names)]
pub enum Shell {
    PowerShell,
    CommandPrompt,
    Wsl,
}
impl Shell {
    fn label(self) -> &'static str {
        match self {
            Shell::PowerShell => "PowerShell",
            Shell::CommandPrompt => "Command Prompt",
            Shell::Wsl => "WSL",
        }
    }
}

pub struct Terminal {
    parser: Arc<Mutex<Parser>>,
    master: Box<dyn MasterPty + Send>,
    input: SyncSender<Vec<u8>>,
    pending_input: VecDeque<Vec<u8>>,
    selection: Option<((u16, u16), (u16, u16))>,
    dirty: Arc<AtomicBool>,
    shell: Shell,
    cwd: Option<PathBuf>,
    child: Box<dyn Child + Send + Sync>,
    alive: Arc<AtomicBool>,
    rows: u16,
    cols: u16,
    focus_id: Option<egui::Id>,
}

impl Terminal {
    pub fn spawn(shell: Shell, cwd: Option<&Path>) -> Result<Self> {
        let rows = 24;
        let cols = 80;
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("open terminal")?;
        let executable = match shell {
            Shell::PowerShell => "powershell.exe",
            Shell::CommandPrompt => "cmd.exe",
            Shell::Wsl => "wsl.exe",
        };
        let mut command = CommandBuilder::new(executable);
        if let Some(cwd) = cwd {
            if shell == Shell::Wsl {
                command.arg("--cd");
                command.arg(cwd.as_os_str());
            } else {
                command.cwd(cwd);
            }
        } else if shell == Shell::Wsl {
            command.arg("--cd");
            command.arg("~");
        }
        let child = pair.slave.spawn_command(command).context("launch shell")?;
        drop(pair.slave);
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("open terminal output")?;
        let writer = pair.master.take_writer().context("open terminal input")?;
        let (input, input_rx) = mpsc::sync_channel::<Vec<u8>>(128);
        thread::Builder::new()
            .name("twill-pty-writer".into())
            .spawn(move || {
                let mut writer = writer;
                while let Ok(bytes) = input_rx.recv() {
                    if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                        break;
                    }
                }
            })
            .context("start terminal writer")?;
        let parser = Arc::new(Mutex::new(Parser::new(rows, cols, 2_000)));
        let alive = Arc::new(AtomicBool::new(true));
        let dirty = Arc::new(AtomicBool::new(true));
        let parser_thread = Arc::clone(&parser);
        let alive_thread = Arc::clone(&alive);
        let dirty_thread = Arc::clone(&dirty);
        thread::Builder::new()
            .name("twill-pty-reader".into())
            .spawn(move || {
                let mut buffer = [0u8; 8192];
                while let Ok(count) = reader.read(&mut buffer) {
                    if count == 0 {
                        break;
                    }
                    if let Ok(mut parser) = parser_thread.lock() {
                        parser.process(&buffer[..count]);
                        dirty_thread.store(true, Ordering::Release);
                    }
                }
                alive_thread.store(false, Ordering::Release);
                dirty_thread.store(true, Ordering::Release);
            })
            .context("start terminal reader")?;
        Ok(Self {
            parser,
            master: pair.master,
            input,
            pending_input: VecDeque::new(),
            selection: None,
            dirty,
            shell,
            cwd: cwd.map(Path::to_path_buf),
            child,
            alive,
            rows,
            cols,
            focus_id: None,
        })
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        let available = ui.available_size();
        let size = Vec2::new(available.x.max(80.0), available.y.max(80.0));
        let (full_rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
        self.focus_id = Some(response.id);
        if response.clicked() {
            response.request_focus();
        }
        let painter = ui.painter_at(full_rect);
        painter.rect_filled(full_rect, 0.0, Color32::from_rgb(17, 20, 27));
        let title = format!(
            "{}  {}",
            self.shell.label(),
            self.cwd
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "~".into())
        );
        painter.text(
            full_rect.min + Vec2::new(6.0, 2.0),
            egui::Align2::LEFT_TOP,
            title,
            FontId::monospace(11.0),
            Color32::GRAY,
        );
        let rect = Rect::from_min_max(full_rect.min + Vec2::new(0.0, 18.0), full_rect.max);
        let font = FontId::monospace(14.0);
        let cell_width = painter
            .layout_no_wrap("M".into(), font.clone(), Color32::WHITE)
            .size()
            .x
            .max(1.0);
        let cell_height = 17.0;
        let cols = (rect.width() / cell_width)
            .floor()
            .clamp(1.0, u16::MAX as f32) as u16;
        let rows = (rect.height() / cell_height)
            .floor()
            .clamp(1.0, u16::MAX as f32) as u16;
        if (rows, cols) != (self.rows, self.cols)
            && self
                .master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .is_ok()
        {
            self.rows = rows;
            self.cols = cols;
            if let Ok(mut parser) = self.parser.lock() {
                parser.set_size(rows, cols);
            }
        }
        if response.hovered() {
            let wheel = ui.input(|input| input.smooth_scroll_delta.y);
            if wheel.abs() >= 1.0 {
                if let Ok(mut parser) = self.parser.try_lock() {
                    let current = parser.screen().scrollback();
                    let lines = (wheel.abs() / cell_height).ceil() as usize;
                    parser.set_scrollback(if wheel > 0.0 {
                        current.saturating_add(lines)
                    } else {
                        current.saturating_sub(lines)
                    });
                }
            }
        }
        if response.has_focus() {
            self.handle_input(ui);
        }
        self.flush_input();
        if response.drag_started() {
            if let Some(position) = response.interact_pointer_pos() {
                if rect.contains(position) {
                    let cell = pointer_cell(position, rect, cell_width, cell_height, rows, cols);
                    self.selection = Some((cell, cell));
                }
            }
        }
        if response.dragged() {
            if let (Some((start, _)), Some(position)) =
                (self.selection, response.interact_pointer_pos())
            {
                self.selection = Some((
                    start,
                    pointer_cell(position, rect, cell_width, cell_height, rows, cols),
                ));
            }
        }
        if let Ok(parser) = self.parser.try_lock() {
            let screen = parser.screen();
            for row in 0..rows {
                for col in 0..cols {
                    let Some(cell) = screen.cell(row, col) else {
                        continue;
                    };
                    let x = rect.min.x + col as f32 * cell_width;
                    let y = rect.min.y + row as f32 * cell_height;
                    let background = terminal_color(cell.bgcolor(), Color32::from_rgb(17, 20, 27));
                    if background != Color32::from_rgb(17, 20, 27) {
                        painter.rect_filled(
                            Rect::from_min_size(
                                Pos2::new(x, y),
                                Vec2::new(cell_width, cell_height),
                            ),
                            0.0,
                            background,
                        );
                    }
                    if self
                        .selection
                        .is_some_and(|selection| selected(selection, (row, col)))
                    {
                        painter.rect_filled(
                            Rect::from_min_size(
                                Pos2::new(x, y),
                                Vec2::new(cell_width, cell_height),
                            ),
                            0.0,
                            Color32::from_rgba_unmultiplied(90, 145, 220, 110),
                        );
                    }
                    if !cell.contents().is_empty() {
                        painter.text(
                            Pos2::new(x, y),
                            egui::Align2::LEFT_TOP,
                            cell.contents(),
                            font.clone(),
                            terminal_color(cell.fgcolor(), Color32::LIGHT_GRAY),
                        );
                    }
                }
            }
            if response.has_focus() && !screen.hide_cursor() {
                let (row, col) = screen.cursor_position();
                let cursor = Rect::from_min_size(
                    Pos2::new(
                        rect.min.x + col as f32 * cell_width,
                        rect.min.y + row as f32 * cell_height,
                    ),
                    Vec2::new(cell_width, cell_height),
                );
                painter.rect_stroke(
                    cursor,
                    0.0,
                    egui::Stroke::new(1.0_f32, Color32::WHITE),
                    egui::StrokeKind::Inside,
                );
            }
        }
        if self.dirty.swap(false, Ordering::AcqRel)
            || !self.pending_input.is_empty()
            || self.alive.load(Ordering::Acquire)
        {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn handle_input(&mut self, ui: &mut Ui) {
        let events = ui.input(|input| input.events.clone());
        let copy_requested = events
            .iter()
            .any(|event| matches!(event, egui::Event::Copy));
        let paste_requested = events
            .iter()
            .any(|event| matches!(event, egui::Event::Paste(_)));
        let cut_requested = events.iter().any(|event| matches!(event, egui::Event::Cut));
        let copy_key = events.iter().any(|event| matches!(event, egui::Event::Key { key: Key::C, pressed: true, modifiers, .. } if modifiers.command));
        if (copy_requested || copy_key) && self.selection.is_some() {
            if let Some(text) = self.selected_text() {
                ui.ctx().copy_text(text);
            }
            return;
        }
        for event in events {
            let bytes: Option<Vec<u8>> = match event {
                egui::Event::Copy => Some(vec![3]),
                egui::Event::Cut => Some(vec![24]),
                egui::Event::Paste(text) => {
                    let bracketed = self
                        .parser
                        .try_lock()
                        .map(|parser| parser.screen().bracketed_paste())
                        .unwrap_or(false);
                    if bracketed {
                        Some(
                            [
                                b"\x1b[200~".as_slice(),
                                text.as_bytes(),
                                b"\x1b[201~".as_slice(),
                            ]
                            .concat(),
                        )
                    } else {
                        Some(text.into_bytes())
                    }
                }
                egui::Event::Text(text) => Some(text.into_bytes()),
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    if modifiers.ctrl
                        && ((key == Key::C && copy_requested)
                            || (key == Key::V && paste_requested)
                            || (key == Key::X && cut_requested))
                    {
                        continue;
                    }
                    let application_cursor = self
                        .parser
                        .try_lock()
                        .map(|parser| parser.screen().application_cursor())
                        .unwrap_or(false);
                    key_bytes(key, modifiers, application_cursor)
                }
                _ => None,
            };
            if let Some(bytes) = bytes {
                self.pending_input.push_back(bytes);
            }
        }
    }

    fn flush_input(&mut self) {
        while let Some(bytes) = self.pending_input.pop_front() {
            match self.input.try_send(bytes) {
                Ok(()) => {}
                Err(TrySendError::Full(bytes)) => {
                    self.pending_input.push_front(bytes);
                    break;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.pending_input.clear();
                    break;
                }
            }
        }
    }

    fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        let parser = self.parser.try_lock().ok()?;
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        let (start, end) = normalized(selection);
        let mut text = String::new();
        for row in start.0..=end.0.min(rows.saturating_sub(1)) {
            if row != start.0 {
                text.push('\n');
            }
            let first = if row == start.0 { start.1 } else { 0 };
            let last = if row == end.0 {
                end.1
            } else {
                cols.saturating_sub(1)
            };
            let mut line = String::new();
            for col in first..=last.min(cols.saturating_sub(1)) {
                if let Some(cell) = screen.cell(row, col) {
                    if cell.contents().is_empty() {
                        line.push(' ');
                    } else {
                        line.push_str(&cell.contents());
                    }
                }
            }
            text.push_str(line.trim_end());
        }
        Some(text)
    }

    pub fn is_running(&mut self) -> bool {
        if !self.alive.load(Ordering::Acquire) {
            return false;
        }
        matches!(self.child.try_wait(), Ok(None))
    }
    pub fn has_focus(&self, ctx: &egui::Context) -> bool {
        self.focus_id
            .is_some_and(|id| ctx.memory(|m| m.has_focus(id)))
    }

    pub fn shutdown(&mut self) {
        if self.alive.swap(false, Ordering::AcqRel) {
            let _ = self.child.kill();
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn key_bytes(key: Key, modifiers: Modifiers, application_cursor: bool) -> Option<Vec<u8>> {
    if modifiers.ctrl {
        let letter = match key {
            Key::A => Some(1),
            Key::B => Some(2),
            Key::C => Some(3),
            Key::D => Some(4),
            Key::E => Some(5),
            Key::F => Some(6),
            Key::G => Some(7),
            Key::H => Some(8),
            Key::I => Some(9),
            Key::J => Some(10),
            Key::K => Some(11),
            Key::L => Some(12),
            Key::M => Some(13),
            Key::N => Some(14),
            Key::O => Some(15),
            Key::P => Some(16),
            Key::Q => Some(17),
            Key::R => Some(18),
            Key::S => Some(19),
            Key::T => Some(20),
            Key::U => Some(21),
            Key::V => Some(22),
            Key::W => Some(23),
            Key::X => Some(24),
            Key::Y => Some(25),
            Key::Z => Some(26),
            _ => None,
        };
        if let Some(byte) = letter {
            return Some(vec![byte]);
        }
    }
    if key == Key::Tab && modifiers.shift {
        return Some(b"\x1b[Z".to_vec());
    }
    if application_cursor {
        let application: &[u8] = match key {
            Key::ArrowUp => b"\x1bOA",
            Key::ArrowDown => b"\x1bOB",
            Key::ArrowRight => b"\x1bOC",
            Key::ArrowLeft => b"\x1bOD",
            Key::Home => b"\x1bOH",
            Key::End => b"\x1bOF",
            _ => b"",
        };
        if !application.is_empty() {
            return Some(application.to_vec());
        }
    }
    let sequence: &[u8] = match key {
        Key::Enter => b"\r",
        Key::Tab => b"\t",
        Key::Backspace => b"\x7f",
        Key::Escape => b"\x1b",
        Key::ArrowUp => b"\x1b[A",
        Key::ArrowDown => b"\x1b[B",
        Key::ArrowRight => b"\x1b[C",
        Key::ArrowLeft => b"\x1b[D",
        Key::Home => b"\x1b[H",
        Key::End => b"\x1b[F",
        Key::Delete => b"\x1b[3~",
        Key::PageUp => b"\x1b[5~",
        Key::PageDown => b"\x1b[6~",
        _ => return None,
    };
    Some(sequence.to_vec())
}

fn pointer_cell(
    position: Pos2,
    rect: Rect,
    cell_width: f32,
    cell_height: f32,
    rows: u16,
    cols: u16,
) -> (u16, u16) {
    let row = ((position.y - rect.min.y) / cell_height)
        .floor()
        .clamp(0.0, rows.saturating_sub(1) as f32) as u16;
    let col = ((position.x - rect.min.x) / cell_width)
        .floor()
        .clamp(0.0, cols.saturating_sub(1) as f32) as u16;
    (row, col)
}
fn normalized(selection: ((u16, u16), (u16, u16))) -> ((u16, u16), (u16, u16)) {
    if selection.0 <= selection.1 {
        selection
    } else {
        (selection.1, selection.0)
    }
}
fn selected(selection: ((u16, u16), (u16, u16)), cell: (u16, u16)) -> bool {
    let (start, end) = normalized(selection);
    cell >= start && cell <= end
}

fn terminal_color(color: Color, default: Color32) -> Color32 {
    match color {
        Color::Default => default,
        Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
        Color::Idx(index) => {
            const PALETTE: [[u8; 3]; 16] = [
                [0, 0, 0],
                [205, 49, 49],
                [13, 188, 121],
                [229, 229, 16],
                [36, 114, 200],
                [188, 63, 188],
                [17, 168, 205],
                [229, 229, 229],
                [102, 102, 102],
                [241, 76, 76],
                [35, 209, 139],
                [245, 245, 67],
                [59, 142, 234],
                [214, 112, 214],
                [41, 184, 219],
                [255, 255, 255],
            ];
            if index < 16 {
                let [r, g, b] = PALETTE[index as usize];
                Color32::from_rgb(r, g, b)
            } else if index < 232 {
                let n = index - 16;
                let scale = |x: u8| if x == 0 { 0 } else { 55 + 40 * x };
                Color32::from_rgb(scale(n / 36), scale((n / 6) % 6), scale(n % 6))
            } else {
                let gray = 8 + (index - 232) * 10;
                Color32::from_gray(gray)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_c_and_navigation_are_terminal_bytes() {
        assert_eq!(key_bytes(Key::C, Modifiers::CTRL, false), Some(vec![3]));
        assert_eq!(
            key_bytes(Key::ArrowUp, Modifiers::NONE, false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            key_bytes(Key::Tab, Modifiers::SHIFT, false),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            key_bytes(Key::ArrowUp, Modifiers::NONE, true),
            Some(b"\x1bOA".to_vec())
        );
    }

    #[test]
    fn ansi_colors_cover_extended_palette() {
        assert_eq!(
            terminal_color(Color::Idx(196), Color32::BLACK),
            Color32::from_rgb(255, 0, 0)
        );
        assert_eq!(
            terminal_color(Color::Rgb(1, 2, 3), Color32::BLACK),
            Color32::from_rgb(1, 2, 3)
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "ConPTY smoke test requires a Windows console host that answers device-status queries"]
    fn windows_pty_echo_resize_and_cleanup() {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 20,
                cols: 60,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open PTY");
        let mut command = CommandBuilder::new("cmd.exe");
        command.arg("/Q");
        let mut child = pair.slave.spawn_command(command).expect("spawn cmd");
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("reader");
        let mut writer = pair.master.take_writer().expect("writer");
        let (tx, rx) = std::sync::mpsc::channel();
        let reader_thread = std::thread::spawn(move || {
            let mut bytes = [0u8; 4096];
            let mut output = Vec::new();
            while let Ok(count) = reader.read(&mut bytes) {
                if count == 0 {
                    break;
                }
                eprintln!("PTY read: {:?}", String::from_utf8_lossy(&bytes[..count]));
                output.extend_from_slice(&bytes[..count]);
                if String::from_utf8_lossy(&output).contains("TWILL_PTY_SENTINEL") {
                    break;
                }
            }
            let _ = tx.send(output);
        });
        writer
            .write_all(b"echo TWILL_PTY_SENTINEL\r")
            .expect("send shell command");
        writer.flush().expect("flush shell input");
        let received = rx.recv_timeout(std::time::Duration::from_secs(10));
        eprintln!("child status before cleanup: {:?}", child.try_wait());
        pair.master
            .resize(PtySize {
                rows: 25,
                cols: 90,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize PTY");
        let _ = child.kill();
        let _ = child.wait();
        drop(pair.master);
        if received.is_ok() {
            reader_thread.join().expect("reader thread");
        }
        let output = received.expect("PTY output timeout");
        assert!(String::from_utf8_lossy(&output).contains("TWILL_PTY_SENTINEL"));
    }
}
