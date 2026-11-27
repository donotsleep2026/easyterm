//! ZMODEM's path chooser and progress display. Every path comes from the daemon.
//! This module never opens, lists or writes the TUI machine's filesystem.

use std::collections::BTreeSet;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, StatefulWidget, Widget},
};
use term_proto::{LocalPathListInfo, ZmodemEventWire};

#[derive(Debug, PartialEq, Eq)]
pub enum Choice {
    Pending,
    Paths(Vec<String>),
    Cancel,
}

pub struct FileChooser {
    save_name: Option<String>,
    directory: Option<LocalPathListInfo>,
    showing_roots: bool,
    directory_cursor: usize,
    cursor: usize,
    list: ListState,
    marked: BTreeSet<String>,
    editing_path: Option<String>,
    save_confirmation: Option<String>,
    request: Option<Option<String>>,
    initial_directory: Option<String>,
    fallback_warning: Option<String>,
    pub loading: bool,
    pub error: Option<String>,
}

impl FileChooser {
    pub fn new(save: bool, name: Option<&str>, initial: Option<String>) -> Self {
        // A remote filename is a suggestion, never a path on the daemon.
        let save_name = save.then(|| name.unwrap_or("download")
            .rsplit(['/', '\\']).next().filter(|n| !n.is_empty() && *n != "." && *n != "..")
            .unwrap_or("download").to_string());
        Self {
            save_name, directory: None, showing_roots: false, directory_cursor: 0, cursor: 0, list: ListState::default(),
            marked: BTreeSet::new(), editing_path: None, save_confirmation: None,
            initial_directory: initial.clone().filter(|path| !path.trim().is_empty() && path != "~"),
            fallback_warning: None, request: Some(initial), loading: true, error: None,
        }
    }

    pub fn take_request(&mut self) -> Option<Option<String>> {
        let request = self.request.take();
        if request.is_some() { self.loading = true; }
        request
    }

    pub fn directory_path(&self) -> Option<&str> { self.directory.as_ref().map(|d| d.path.as_str()) }

    pub fn loaded(&mut self, result: Result<LocalPathListInfo, String>) {
        self.loading = false;
        // Only the first remembered-directory response can trigger recovery.
        // Taking this before the next request prevents repeated home retries.
        let initial_directory = self.initial_directory.take();
        match result {
            Ok(mut directory) => {
                if directory.separator == '\\' {
                    if let Some(name) = &mut self.save_name { *name = windows_filename(name); }
                }
                if self.save_name.is_some() { directory.entries.retain(|entry| entry.is_dir); }
                self.directory = Some(directory);
                self.showing_roots = false;
                self.cursor = 0;
                self.list = ListState::default();
                self.error = None;
            }
            Err(error) => {
                if let Some(path) = initial_directory.filter(|_| self.directory.is_none() && self.request.is_none()) {
                    self.fallback_warning = Some(format!("Could not open {path}: {error}."));
                    self.request = Some(None);
                    self.loading = true;
                }
                self.error = Some(error);
            }
        }
    }

    fn browse(&mut self, path: Option<String>) {
        self.request = Some(path);
        self.loading = true;
        self.error = None;
        self.fallback_warning = None;
    }

    fn row_count(&self) -> usize {
        self.directory.as_ref().map(|d| if self.showing_roots { d.roots.len() }
            else { d.entries.len() + usize::from(d.parent.is_some()) + usize::from(!d.roots.is_empty()) }).unwrap_or(0)
    }

    fn has_roots(&self) -> bool {
        self.directory.as_ref().is_some_and(|directory| !directory.roots.is_empty())
    }

    fn open_roots(&mut self) {
        let Some(directory) = self.directory.as_ref().filter(|directory| !directory.roots.is_empty()) else { return; };
        self.directory_cursor = self.cursor;
        self.cursor = directory.roots.iter().position(|root| directory.path.get(..root.path.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(&root.path))).unwrap_or(0);
        self.showing_roots = true;
        self.list = ListState::default();
        self.error = None;
    }

    fn close_roots(&mut self) {
        self.showing_roots = false;
        self.cursor = self.directory_cursor.min(self.row_count().saturating_sub(1));
        self.list = ListState::default();
        self.error = None;
    }

    fn entry(&self) -> Option<&term_proto::LocalPathEntry> {
        let directory = self.directory.as_ref()?;
        if self.showing_roots { return directory.roots.get(self.cursor); }
        self.cursor.checked_sub(usize::from(directory.parent.is_some()) + usize::from(!directory.roots.is_empty()))
            .and_then(|i| directory.entries.get(i))
    }

    fn toggle(&mut self) {
        let Some(entry) = self.entry().filter(|entry| !entry.is_dir) else { return; };
        let path = entry.path.clone();
        if !self.marked.remove(&path) { self.marked.insert(path); }
    }

    fn confirm(&mut self) -> Choice {
        // A failed path change must not submit the previous download folder.
        // Browsing successfully (or explicitly returning from Drives) clears it.
        if self.save_name.is_some() && self.error.is_some() { return Choice::Pending; }
        if self.showing_roots && (self.save_name.is_some() || self.marked.is_empty()) {
            self.error = Some("Open a drive with Enter before choosing a destination.".into());
            return Choice::Pending;
        }
        let Some(directory) = &self.directory else { return Choice::Pending; };
        if let Some(name) = &self.save_name {
            let mut path = directory.path.clone();
            if !path.ends_with(['/', '\\']) { path.push(directory.separator); }
            path.push_str(name);
            // Listings may be truncated or stale, and Windows filenames have
            // case/normalization aliases. Always make replacing explicit.
            self.save_confirmation = Some(path);
            return Choice::Pending;
        }
        if self.marked.is_empty() {
            if let Some(entry) = self.entry().filter(|entry| !entry.is_dir) {
                return Choice::Paths(vec![entry.path.clone()]);
            }
            self.error = Some("Select files with Space, then Ctrl-S to send.".into());
            Choice::Pending
        } else {
            Choice::Paths(self.marked.iter().cloned().collect())
        }
    }

    /// Bracketed paste belongs to the modal path input, never the remote PTY.
    pub fn paste(&mut self, text: &str) {
        if let Some(path) = self.editing_path.as_mut() {
            for ch in text.chars().filter(|ch| !ch.is_control()) {
                if path.len() + ch.len_utf8() > 8192 { break; }
                path.push(ch);
            }
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Choice {
        if let Some(path) = self.save_confirmation.take() {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Choice::Paths(vec![path]),
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => Choice::Pending,
                _ => { self.save_confirmation = Some(path); Choice::Pending }
            };
        }
        if let Some(mut path) = self.editing_path.take() {
            match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.browse(Some(path)),
                KeyCode::Backspace => { path.pop(); self.editing_path = Some(path); }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => self.editing_path = Some(String::new()),
                KeyCode::Char(ch) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                    if path.len() < 8192 { path.push(ch); }
                    self.editing_path = Some(path);
                }
                _ => self.editing_path = Some(path),
            }
            return Choice::Pending;
        }
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) { return Choice::Cancel; }
        if key.code == KeyCode::Char('l') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.editing_path = Some(self.directory_path().unwrap_or("").to_string());
            return Choice::Pending;
        }
        if self.loading { return Choice::Pending; }
        if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.showing_roots { self.close_roots(); } else { self.open_roots(); }
            return Choice::Pending;
        }
        match key.code {
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => return self.confirm(),
            KeyCode::Char('j') | KeyCode::Down => self.cursor = (self.cursor + 1).min(self.row_count().saturating_sub(1)),
            KeyCode::Char('k') | KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::PageDown => self.cursor = self.cursor.saturating_add(10).min(self.row_count().saturating_sub(1)),
            KeyCode::PageUp => self.cursor = self.cursor.saturating_sub(10),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.row_count().saturating_sub(1),
            KeyCode::Backspace | KeyCode::Left => {
                if self.showing_roots { self.close_roots(); }
                else if let Some(parent) = self.directory.as_ref().and_then(|d| d.parent.clone()) { self.browse(Some(parent)); }
                else { self.open_roots(); }
            }
            KeyCode::Enter | KeyCode::Right => {
                if self.showing_roots {
                    if let Some(root) = self.entry() { self.browse(Some(root.path.clone())); }
                } else if self.cursor == 0 && self.has_roots() {
                    self.open_roots();
                } else if let Some(entry) = self.entry() {
                    if entry.is_dir { self.browse(Some(entry.path.clone())); }
                    else if key.code == KeyCode::Enter { self.toggle(); }
                } else if self.directory.as_ref().is_some_and(|directory| directory.parent.is_some()) {
                    self.browse(self.directory.as_ref().and_then(|directory| directory.parent.clone()));
                }
            }
            KeyCode::Char(' ') if self.save_name.is_none() => self.toggle(),
            KeyCode::Char('a') if self.save_name.is_none() && !self.showing_roots => {
                if let Some(directory) = &self.directory {
                    let files: Vec<_> = directory.entries.iter().filter(|e| !e.is_dir).map(|e| e.path.clone()).collect();
                    let all = files.iter().all(|path| self.marked.contains(path));
                    for path in files {
                        if all { self.marked.remove(&path); } else { self.marked.insert(path); }
                    }
                }
            }
            KeyCode::Char('r') => self.browse(self.directory_path().map(str::to_string)),
            _ => {}
        }
        Choice::Pending
    }

    pub fn draw(&mut self, buf: &mut Buffer, area: Rect, daemon: &str) {
        let area = Rect::new(area.x + 1.min(area.width), area.y + 1.min(area.height),
            area.width.saturating_sub(2), area.height.saturating_sub(2));
        Clear.render(area, buf);
        let title = if self.save_name.is_some() { " ZMODEM — choose download directory " } else { " ZMODEM — choose files to send " };
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(area);
        block.render(area, buf);
        let header = Rect::new(inner.x, inner.y, inner.width, inner.height.min(3));
        let path = self.editing_path.as_deref().unwrap_or_else(|| if self.showing_roots { "Drives on daemon" }
            else { self.directory_path().unwrap_or("(daemon home)") });
        let mut header_lines = vec![Line::from(format!("daemon {daemon} · files on this daemon")),
            Line::from(Span::styled(format!("{}{}", path, if self.editing_path.is_some() { "_" } else { "" }), Style::default().fg(Color::Cyan)))];
        if let Some(name) = &self.save_name { header_lines.push(Line::from(format!("Save as: {name}"))); }
        else { header_lines.push(Line::from(format!("{} file(s) selected across folders", self.marked.len()))); }
        Paragraph::new(header_lines).render(header, buf);
        let body = Rect::new(inner.x, header.bottom(), inner.width, inner.height.saturating_sub(header.height + 2));
        if let Some(path) = &self.save_confirmation {
            Paragraph::new(vec![Line::from("Confirm destination on daemon:"), Line::from(path.clone())])
                .style(Style::default().fg(Color::Yellow)).wrap(ratatui::widgets::Wrap { trim: false })
                .render(body, buf);
        } else if let Some(directory) = &self.directory {
            let mut rows = Vec::new();
            if !self.showing_roots {
                if !directory.roots.is_empty() {
                    let drives = directory.roots.iter().map(|root| root.name.as_str()).collect::<Vec<_>>().join("  ");
                    rows.push(ListItem::new(Span::styled(format!("    Drives  {drives}  [Ctrl-D]"), Style::default().fg(Color::Cyan))));
                }
                if directory.parent.is_some() { rows.push(ListItem::new("    ../")); }
            }
            for entry in if self.showing_roots { &directory.roots } else { &directory.entries } {
                let marker = if entry.is_dir { "   " } else if self.marked.contains(&entry.path) { "[x]" } else { "[ ]" };
                let suffix = if self.showing_roots { String::new() } else if entry.is_dir { "/".to_string() } else { format!("  {}", bytes(entry.size.unwrap_or(0))) };
                rows.push(ListItem::new(Line::from(Span::styled(format!("{marker} {}{suffix}", entry.name),
                    Style::default().fg(if entry.is_dir { Color::Cyan } else { Color::White })))));
            }
            self.list.select((!rows.is_empty()).then_some(self.cursor));
            StatefulWidget::render(List::new(rows).highlight_symbol("> ")
                .highlight_style(Style::default().bg(Color::Indexed(238)).add_modifier(Modifier::BOLD)), body, buf, &mut self.list);
        }
        if inner.height >= 2 {
            let message = if self.save_confirmation.is_some() { "Saving replaces any existing file.".to_string() }
                else if self.loading {
                    if let Some(warning) = &self.fallback_warning { format!("{warning} Trying daemon home…") }
                    else { "Loading daemon directory…".to_string() }
                }
                else if let Some(error) = &self.error {
                    if let Some(warning) = &self.fallback_warning { format!("{warning} Home failed: {error}") }
                    else { error.clone() }
                }
                else if let Some(warning) = &self.fallback_warning { format!("{warning} Showing daemon home.") }
                else if self.showing_roots { "Choose a drive on this daemon; Enter opens it.".into() }
                else if self.directory.as_ref().is_some_and(|d| d.truncated) { "Directory listing limited to 4096 entries; Ctrl-L opens a path directly.".into() }
                else if self.row_count() == 0 { "This directory is empty.".into() }
                else { String::new() };
            Paragraph::new(message).style(Style::default().fg(Color::Yellow))
                .render(Rect::new(inner.x, inner.bottom().saturating_sub(2), inner.width, 1), buf);
            let hint = if self.save_confirmation.is_some() { "[y] save / replace  [n/Esc] back" }
                else if self.editing_path.is_some() { "[Enter] browse  [Ctrl-U] erase  [Esc] back" }
                else if self.showing_roots {
                    if inner.width < 65 { "Enter drive · Backspace back · Esc cancel" }
                    else { "[Enter] open drive  [Backspace/Ctrl-D] back  [Ctrl-L] path  [Esc] cancel" }
                }
                else if inner.width < 45 {
                    if self.save_name.is_some() { "[Ctrl-S] save here  [Esc] cancel" } else { "Space pick  [Ctrl-S] send  [Esc] cancel" }
                } else if self.has_roots() && inner.width < 65 {
                    if self.save_name.is_some() { "[Ctrl-D] drives  [Ctrl-S] save  [Esc] cancel" }
                    else { "[Ctrl-D] drives  [Ctrl-S] send  [Esc] cancel" }
                } else if self.has_roots() {
                    if self.save_name.is_some() { "Enter folder · Ctrl-D drives · Ctrl-L path · Ctrl-S save · Esc cancel" }
                    else { "Space select · Ctrl-D drives · Ctrl-L path · Ctrl-S send · Esc cancel" }
                } else if inner.width < 90 {
                    if self.save_name.is_some() { "Enter folder · Backspace up · Ctrl-S save · Ctrl-L path · Esc cancel" }
                    else { "Space select · Enter folder · Ctrl-S send · Ctrl-L path · Esc cancel" }
                } else if self.save_name.is_some() { "[Enter] folder  [Backspace] up  [Ctrl-L] path  [Ctrl-S] save here  [Esc] cancel" }
                else { "[Space] select  [a] all  [Enter] folder  [Ctrl-L] path  [Ctrl-S] send  [Esc] cancel" };
            Paragraph::new(hint).style(Style::default().fg(Color::DarkGray))
                .render(Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1), buf);
        }
    }
}

/// Keep a remote suggestion a regular Windows filename, including DOS device
/// aliases and NTFS alternate-stream punctuation. The chosen name is shown.
fn windows_filename(name: &str) -> String {
    let sanitized: String = name.chars().map(|ch| if ch.is_control() || "<>:\"/\\|?*".contains(ch) { '_' } else { ch }).collect();
    let mut sanitized = sanitized.trim_end_matches([' ', '.']).to_string();
    if sanitized.is_empty() { return "download".into(); }
    let stem = sanitized.split('.').next().unwrap_or("").trim_end_matches(' ').to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || ["COM", "LPT"].iter().any(|prefix| stem.strip_prefix(prefix).is_some_and(|number|
            matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")));
    if reserved { sanitized.insert(0, '_'); }
    sanitized
}

#[derive(Default)]
pub struct TransferProgress {
    pub active: bool,
    pub label: Option<String>,
    download_name: Option<String>,
}

impl TransferProgress {
    pub fn selected(&mut self, save: bool, paths: &[String]) {
        self.active = true;
        self.download_name = save.then(|| paths.first().map(|path| path.rsplit(['/', '\\']).next().unwrap_or(path).to_string())).flatten();
        self.label = Some(if let Some(name) = &self.download_name { format!("receive {name} · waiting for data") }
            else { format!("send {} file(s) · waiting for data", paths.len()) });
    }

    pub fn event(&mut self, event: &ZmodemEventWire) {
        self.label = Some(match event {
            ZmodemEventWire::UploadDropped { paths, .. } => { self.active = true; format!("sending {} file(s)", paths.len()) }
            ZmodemEventWire::UploadQueued { files, .. } => { self.active = true; format!("{files} file(s) queued for upload") }
            ZmodemEventWire::UploadProgress { file_idx, total_files, name, current, total, .. } => {
                self.active = true;
                format!("send {}/{total_files} {} · {}", file_idx.saturating_add(1), name.rsplit(['/', '\\']).next().unwrap_or(name), progress(*current, *total))
            }
            ZmodemEventWire::DownloadProgress { current, total, .. } => {
                self.active = true;
                format!("receive {} · {}", self.download_name.as_deref().unwrap_or("file"), progress(*current, *total))
            }
            ZmodemEventWire::Cancelled { .. } => { self.active = false; "transfer cancelled; recovering terminal".into() }
            ZmodemEventWire::Ended { .. } => { self.active = false; "transfer ended".into() }
            ZmodemEventWire::UploadDiscarded { files, .. } => { self.active = false; format!("{files} queued file(s) expired before upload") }
        });
    }
}

fn progress(current: u64, total: u64) -> String {
    if total == 0 { format!("{} transferred", bytes(current)) }
    else { format!("{} / {} ({}%)", bytes(current), bytes(total), (u128::from(current) * 100 / u128::from(total)).min(100)) }
}

fn bytes(value: u64) -> String {
    for (unit, scale) in [("GiB", 1u64 << 30), ("MiB", 1u64 << 20), ("KiB", 1u64 << 10)] {
        if value >= scale { return format!("{:.1} {unit}", value as f64 / scale as f64); }
    }
    format!("{value} B")
}

#[cfg(test)]
mod tests {
    use super::*;
    use term_proto::LocalPathEntry;

    fn listing(path: &str, separator: char) -> LocalPathListInfo {
        LocalPathListInfo {
            path: path.into(), parent: None, separator, roots: Vec::new(), truncated: false,
            entries: vec![
                LocalPathEntry { name: "folder".into(), path: format!("{path}{separator}folder"), is_dir: true, size: None },
                LocalPathEntry { name: "中文 file.txt".into(), path: format!("{path}{separator}中文 file.txt"), is_dir: false, size: Some(42) },
            ],
        }
    }

    fn key(code: KeyCode) -> KeyEvent { KeyEvent::new(code, KeyModifiers::NONE) }
    fn confirm() -> KeyEvent { KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL) }

    fn drives() -> KeyEvent { KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL) }

    fn windows_listing(path: &str, parent: Option<&str>) -> LocalPathListInfo {
        let mut directory = listing(path, '\\');
        directory.parent = parent.map(str::to_string);
        directory.roots = ["C:\\", "D:\\", "Z:\\"].into_iter().map(|path| LocalPathEntry {
            name: path.into(), path: path.into(), is_dir: true, size: None,
        }).collect();
        directory
    }

    fn buffer_text(buffer: &Buffer) -> String {
        use unicode_width::UnicodeWidthStr;
        let mut text = String::new();
        for y in buffer.area.top()..buffer.area.bottom() {
            let mut x = buffer.area.left();
            while x < buffer.area.right() {
                let symbol = buffer[(x, y)].symbol();
                text.push_str(symbol);
                x = x.saturating_add(symbol.width().max(1) as u16);
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn multi_selection_survives_daemon_directory_changes() {
        let mut chooser = FileChooser::new(false, None, None);
        assert_eq!(chooser.take_request(), Some(None));
        chooser.loaded(Ok(listing("/daemon", '/')));
        chooser.key(key(KeyCode::Down));
        chooser.key(key(KeyCode::Char(' ')));
        chooser.loaded(Ok(listing("/other", '/')));
        chooser.key(key(KeyCode::Down));
        chooser.key(key(KeyCode::Char(' ')));
        assert_eq!(chooser.key(confirm()), Choice::Paths(vec!["/daemon/中文 file.txt".into(), "/other/中文 file.txt".into()]));
    }

    #[test]
    fn unavailable_remembered_directory_falls_back_once_and_restores_drive_selection() {
        let mut chooser = FileChooser::new(true, Some("file.txt"), Some("Z:\\Downloads".into()));
        assert_eq!(chooser.take_request(), Some(Some("Z:\\Downloads".into())));
        chooser.loaded(Err("drive disconnected".into()));
        assert!(chooser.loading);
        assert_eq!(chooser.take_request(), Some(None), "recover through the daemon's own home directory");
        chooser.loaded(Ok(windows_listing("C:\\Users\\daemon", Some("C:\\Users"))));
        assert_eq!(chooser.directory_path(), Some("C:\\Users\\daemon"));
        assert!(chooser.error.is_none(), "the old error must not block saving in a successfully loaded home");
        assert!(chooser.take_request().is_none());
        let area = Rect::new(0, 0, 120, 16);
        let mut buffer = Buffer::empty(area);
        chooser.draw(&mut buffer, area, "windows-daemon:1234");
        let text = buffer_text(&buffer);
        for expected in ["Z:\\Downloads", "drive disconnected", "Showing daemon home", "Drives", "D:\\"] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        chooser.key(confirm());
        assert_eq!(chooser.save_confirmation.as_deref(), Some("C:\\Users\\daemon\\file.txt"));
        chooser.key(key(KeyCode::Char('n')));
        chooser.key(drives());
        assert!(chooser.showing_roots);
        chooser.key(key(KeyCode::Down));
        chooser.key(key(KeyCode::Enter));
        assert_eq!(chooser.take_request(), Some(Some("D:\\".into())));
        assert!(chooser.fallback_warning.is_none(), "an explicit new destination retires the recovery notice");
    }

    #[test]
    fn a_failed_home_fallback_reports_both_errors_without_a_retry_loop() {
        let mut chooser = FileChooser::new(true, Some("file.txt"), Some("Z:\\Downloads".into()));
        chooser.take_request();
        chooser.loaded(Err("drive disconnected".into()));
        assert_eq!(chooser.take_request(), Some(None));
        chooser.loaded(Err("home access denied".into()));
        assert!(!chooser.loading);
        assert!(chooser.directory_path().is_none());
        for _ in 0..3 { assert!(chooser.take_request().is_none(), "home failure must not enqueue itself again"); }
        assert_eq!(chooser.key(confirm()), Choice::Pending);
        assert!(chooser.save_confirmation.is_none());
        let area = Rect::new(0, 0, 160, 16);
        let mut buffer = Buffer::empty(area);
        chooser.draw(&mut buffer, area, "windows-daemon:1234");
        let text = buffer_text(&buffer);
        for expected in ["Z:\\Downloads", "drive disconnected", "Home failed", "home access denied"] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
    }

    #[test]
    fn an_initial_failure_does_not_replace_a_new_path_the_user_already_entered() {
        let mut chooser = FileChooser::new(false, None, Some("Z:\\Downloads".into()));
        chooser.take_request();
        chooser.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
        chooser.paste("D:\\Uploads");
        chooser.key(key(KeyCode::Enter));
        chooser.loaded(Err("drive disconnected".into()));
        assert_eq!(chooser.take_request(), Some(Some("D:\\Uploads".into())));
        assert!(chooser.fallback_warning.is_none());
        chooser.loaded(Err("typed directory unavailable".into()));
        assert!(chooser.take_request().is_none(), "only the initial remembered path can trigger home recovery");
    }

    #[test]
    fn drives_are_browsable_from_the_top_row_and_keyboard_shortcut() {
        let mut chooser = FileChooser::new(false, None, None);
        chooser.take_request();
        chooser.loaded(Ok(windows_listing("C:\\Downloads", Some("C:\\"))));
        let area = Rect::new(0, 0, 100, 18);
        let mut buffer = Buffer::empty(area);
        chooser.draw(&mut buffer, area, "windows-daemon:1234");
        let text = buffer_text(&buffer);
        for expected in ["Drives", "C:\\", "D:\\", "[Ctrl-D]"] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        chooser.key(key(KeyCode::Enter));
        assert!(chooser.showing_roots);
        assert!(chooser.take_request().is_none(), "showing daemon-provided drives needs no local I/O");
        chooser.key(key(KeyCode::Down));
        chooser.key(key(KeyCode::Enter));
        assert_eq!(chooser.take_request(), Some(Some("D:\\".into())));
        chooser.loaded(Ok(windows_listing("D:\\", None)));
        chooser.key(key(KeyCode::Backspace));
        assert!(chooser.showing_roots, "up from a drive root opens the drive list");
        assert_eq!(chooser.entry().map(|entry| entry.path.as_str()), Some("D:\\"));
        chooser.key(drives());
        assert!(!chooser.showing_roots);
        assert_eq!(chooser.directory_path(), Some("D:\\"));
    }

    #[test]
    fn upload_marks_survive_switching_between_daemon_drives() {
        let mut chooser = FileChooser::new(false, None, None);
        chooser.take_request();
        chooser.loaded(Ok(windows_listing("C:\\Upload", Some("C:\\"))));
        chooser.key(key(KeyCode::End));
        chooser.key(key(KeyCode::Char(' ')));
        chooser.key(drives());
        chooser.key(key(KeyCode::Down));
        chooser.key(key(KeyCode::Enter));
        assert_eq!(chooser.take_request(), Some(Some("D:\\".into())));
        chooser.loaded(Ok(windows_listing("D:\\Upload", Some("D:\\"))));
        chooser.key(key(KeyCode::End));
        chooser.key(key(KeyCode::Char(' ')));
        assert_eq!(chooser.key(confirm()), Choice::Paths(vec![
            "C:\\Upload\\中文 file.txt".into(), "D:\\Upload\\中文 file.txt".into(),
        ]));
    }

    #[test]
    fn download_destination_uses_the_chosen_drive_after_navigation() {
        let mut chooser = FileChooser::new(true, Some("../../report.txt"), None);
        chooser.take_request();
        chooser.loaded(Ok(windows_listing("C:\\Downloads", Some("C:\\"))));
        chooser.key(drives());
        assert_eq!(chooser.key(confirm()), Choice::Pending);
        assert!(chooser.save_confirmation.is_none(), "a drive list cannot silently save in the previous folder");
        chooser.key(key(KeyCode::Down));
        chooser.key(key(KeyCode::Enter));
        assert_eq!(chooser.take_request(), Some(Some("D:\\".into())));
        chooser.loaded(Ok(windows_listing("D:\\", None)));
        assert_eq!(chooser.key(confirm()), Choice::Pending);
        assert_eq!(chooser.key(key(KeyCode::Char('y'))), Choice::Paths(vec!["D:\\report.txt".into()]));
    }

    #[test]
    fn inaccessible_drive_keeps_the_drive_list_and_allows_return_or_another_drive() {
        let mut chooser = FileChooser::new(false, None, None);
        chooser.take_request();
        chooser.loaded(Ok(windows_listing("C:\\Upload", Some("C:\\"))));
        chooser.key(key(KeyCode::End));
        chooser.key(key(KeyCode::Char(' ')));
        chooser.key(drives());
        chooser.key(key(KeyCode::End));
        chooser.key(key(KeyCode::Enter));
        assert_eq!(chooser.take_request(), Some(Some("Z:\\".into())));
        chooser.loaded(Err("Z:\\: access denied".into()));
        assert!(chooser.showing_roots);
        assert_eq!(chooser.directory_path(), Some("C:\\Upload"));
        assert_eq!(chooser.marked.len(), 1);
        let area = Rect::new(0, 0, 80, 18);
        let mut buffer = Buffer::empty(area);
        chooser.draw(&mut buffer, area, "windows-daemon:1234");
        let text = buffer_text(&buffer);
        for expected in ["Drives on daemon", "C:\\", "D:\\", "Z:\\", "access denied"] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        chooser.key(key(KeyCode::Backspace));
        assert!(!chooser.showing_roots);
        chooser.key(drives());
        chooser.key(key(KeyCode::Down));
        chooser.key(key(KeyCode::Enter));
        assert_eq!(chooser.take_request(), Some(Some("D:\\".into())));
    }

    #[test]
    fn unix_and_older_daemon_listings_do_not_invent_drive_letters() {
        let directory: LocalPathListInfo = serde_json::from_value(serde_json::json!({
            "path": "/daemon", "parent": null, "entries": [], "separator": "/", "truncated": false,
        })).unwrap();
        assert!(directory.roots.is_empty());
        let mut chooser = FileChooser::new(false, None, None);
        chooser.take_request();
        chooser.loaded(Ok(directory));
        chooser.key(drives());
        chooser.key(key(KeyCode::Backspace));
        assert!(!chooser.showing_roots);
        assert!(chooser.take_request().is_none());
        let area = Rect::new(0, 0, 80, 16);
        let mut buffer = Buffer::empty(area);
        chooser.draw(&mut buffer, area, "linux-daemon:1234");
        let text = buffer_text(&buffer);
        assert!(!text.contains("Drives") && !text.contains("Ctrl-D"), "{text}");
    }

    #[test]
    fn direct_path_input_remains_available_from_the_drive_list() {
        let mut chooser = FileChooser::new(true, Some("file.txt"), None);
        chooser.take_request();
        chooser.loaded(Ok(windows_listing("C:\\Downloads", Some("C:\\"))));
        chooser.key(drives());
        chooser.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
        chooser.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        chooser.paste("D:\\Downloads\\中文目录");
        chooser.key(key(KeyCode::Enter));
        assert_eq!(chooser.take_request(), Some(Some("D:\\Downloads\\中文目录".into())));
    }

    #[test]
    fn a_failed_typed_download_directory_cannot_confirm_the_previous_folder() {
        let mut chooser = FileChooser::new(true, Some("file.txt"), None);
        chooser.take_request();
        chooser.loaded(Ok(windows_listing("C:\\Downloads", Some("C:\\"))));
        chooser.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
        chooser.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        chooser.paste("D:\\unavailable");
        chooser.key(key(KeyCode::Enter));
        assert_eq!(chooser.take_request(), Some(Some("D:\\unavailable".into())));
        chooser.loaded(Err("D:\\unavailable: access denied".into()));
        assert_eq!(chooser.key(confirm()), Choice::Pending);
        assert!(chooser.save_confirmation.is_none());
        chooser.key(drives());
        chooser.key(key(KeyCode::Backspace));
        assert_eq!(chooser.directory_path(), Some("C:\\Downloads"));
        chooser.key(confirm());
        assert_eq!(chooser.key(key(KeyCode::Char('y'))), Choice::Paths(vec!["C:\\Downloads\\file.txt".into()]));
    }

    #[test]
    fn downloading_uses_daemon_separator_and_a_safe_filename() {
        let mut chooser = FileChooser::new(true, Some("../../中文.txt"), None);
        chooser.loaded(Ok(listing("C:\\Downloads", '\\')));
        assert_eq!(chooser.key(confirm()), Choice::Pending);
        assert_eq!(chooser.key(key(KeyCode::Char('y'))), Choice::Paths(vec!["C:\\Downloads\\中文.txt".into()]));
    }

    #[test]
    fn browsing_and_errors_leave_the_transfer_question_pending() {
        let mut chooser = FileChooser::new(false, None, None);
        chooser.take_request();
        chooser.loaded(Ok(listing("/daemon", '/')));
        assert_eq!(chooser.key(key(KeyCode::Enter)), Choice::Pending);
        assert_eq!(chooser.take_request(), Some(Some("/daemon/folder".into())));
        chooser.loaded(Err("permission denied".into()));
        assert_eq!(chooser.directory_path(), Some("/daemon"));
        assert_eq!(chooser.error.as_deref(), Some("permission denied"));
        assert_eq!(chooser.key(key(KeyCode::Esc)), Choice::Cancel);
    }

    #[test]
    fn pasted_paths_stay_in_the_daemon_path_input() {
        let mut chooser = FileChooser::new(false, None, None);
        chooser.loaded(Ok(listing("/daemon", '/')));
        chooser.paste("must not select or send anything");
        assert!(chooser.marked.is_empty() && chooser.editing_path.is_none());
        chooser.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
        chooser.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        chooser.paste("/daemon/中文 folder\r\n");
        chooser.key(key(KeyCode::Enter));
        assert_eq!(chooser.take_request(), Some(Some("/daemon/中文 folder".into())));
    }

    #[test]
    fn an_existing_download_requires_a_replace_decision() {
        let mut chooser = FileChooser::new(true, Some("中文 file.txt"), None);
        chooser.loaded(Ok(listing("/daemon", '/')));
        assert_eq!(chooser.key(confirm()), Choice::Pending);
        assert_eq!(chooser.key(key(KeyCode::Char('n'))), Choice::Pending);
        assert_eq!(chooser.key(confirm()), Choice::Pending);
        assert_eq!(chooser.key(key(KeyCode::Char('y'))), Choice::Paths(vec!["/daemon/中文 file.txt".into()]));
    }

    #[test]
    fn every_download_confirms_even_with_truncation_or_windows_aliases() {
        let mut chooser = FileChooser::new(true, Some("REPORT.txt. "), None);
        let mut directory = listing("C:\\Downloads", '\\');
        directory.truncated = true;
        directory.entries.clear();
        chooser.loaded(Ok(directory));
        assert_eq!(chooser.key(confirm()), Choice::Pending);
        assert_eq!(chooser.key(key(KeyCode::Char('y'))), Choice::Paths(vec!["C:\\Downloads\\REPORT.txt".into()]));
        for (name, expected) in [("nul.txt", "_nul.txt"), ("a:stream", "a_stream"), ("COM1", "_COM1"), ("LPT¹.txt", "_LPT¹.txt"), (".. ", "download"), ("a\u{7}?.txt", "a__.txt")] {
            assert_eq!(windows_filename(name), expected);
        }
    }

    #[test]
    fn a_narrow_confirmation_shows_the_path_and_replacement_choice() {
        let mut chooser = FileChooser::new(true, Some("file.txt"), None);
        chooser.loaded(Ok(listing("/daemon", '/')));
        chooser.key(confirm());
        let area = Rect::new(0, 0, 44, 12);
        let mut buffer = Buffer::empty(area);
        chooser.draw(&mut buffer, area, "127.0.0.1:13999");
        let text = buffer_text(&buffer);
        for expected in ["/daemon/file.txt", "replaces any existing file", "[y] save / replace", "[n/Esc] back"] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
    }

    #[test]
    fn the_browser_displays_daemon_paths_and_marks_without_local_io() {
        let mut chooser = FileChooser::new(false, None, None);
        chooser.loaded(Ok(listing("/remote-only", '/')));
        chooser.key(key(KeyCode::Down));
        chooser.key(key(KeyCode::Char(' ')));
        let area = Rect::new(0, 0, 100, 18);
        let mut buffer = Buffer::empty(area);
        chooser.draw(&mut buffer, area, "daemon.example:1234");
        let text = buffer_text(&buffer);
        for expected in ["daemon.example:1234", "/remote-only", "[x]", "中文 file.txt", "1 file(s) selected", "[Ctrl-S] send"] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
    }

    #[test]
    fn narrow_browser_keeps_confirmation_and_cancel_visible() {
        let mut chooser = FileChooser::new(false, None, None);
        chooser.loaded(Ok(listing("/daemon", '/')));
        let area = Rect::new(0, 0, 44, 10);
        let mut buffer = Buffer::empty(area);
        chooser.draw(&mut buffer, area, "127.0.0.1:13999");
        let text: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("[Ctrl-S] send"), "{text}");
        assert!(text.contains("[Esc] cancel"), "{text}");
    }

    /// Fixed synthetic data for visual QA, never a user's real directory.
    #[test]
    #[ignore = "writes the requested visual preview artifact"]
    fn file_picker_preview() {
        let mut chooser = FileChooser::new(false, None, None);
        let mut directory = windows_listing("D:\\Work\\EasyTerm\\release", Some("D:\\Work\\EasyTerm"));
        directory.entries = vec![
            ("assets", true, 0), ("docs", true, 0), ("EasyTerm-x64.zip", false, 34_052_192),
            ("release-notes.md", false, 4205), ("配置示例.toml", false, 1842), ("checksums.txt", false, 528),
        ].into_iter().map(|(name, is_dir, size)| term_proto::LocalPathEntry {
            name: name.into(), path: format!("D:\\Work\\EasyTerm\\release\\{name}"), is_dir,
            size: (!is_dir).then_some(size),
        }).collect();
        chooser.loaded(Ok(directory));
        chooser.cursor = 4;
        chooser.key(key(KeyCode::Char(' ')));
        chooser.key(key(KeyCode::Down));
        chooser.key(key(KeyCode::Char(' ')));
        let area = Rect::new(0, 0, 80, 24);
        let mut buffer = Buffer::empty(area);
        chooser.draw(&mut buffer, area, "127.0.0.1:13999");
        let cells: Vec<_> = buffer.content().iter().map(|cell| serde_json::json!({
            "symbol": cell.symbol(), "fg": format!("{:?}", cell.fg), "bg": format!("{:?}", cell.bg),
            "modifiers": format!("{:?}", cell.modifier),
        })).collect();
        let output = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/tui-file-picker-preview.json");
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        std::fs::write(&output, serde_json::to_vec_pretty(&serde_json::json!({"width":80,"height":24,"cells":cells})).unwrap()).unwrap();
        println!("{}", output.display());
    }

    #[test]
    fn transfer_progress_handles_unknown_sizes_large_values_and_cancel() {
        assert!(progress(1024, 0).contains("transferred"));
        assert!(progress(u64::MAX, 1).contains("100%"));
        let mut transfer = TransferProgress::default();
        transfer.event(&ZmodemEventWire::DownloadProgress { sid: 1, current: 42, total: 100 });
        assert!(transfer.active);
        assert!(transfer.label.as_deref().unwrap().contains("42%"));
        transfer.event(&ZmodemEventWire::Cancelled { sid: 1 });
        assert!(!transfer.active);
    }
}
