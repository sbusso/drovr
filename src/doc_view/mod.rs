//! `drovr doc view <path>`: a full-screen Markdown viewer that runs inside a
//! herdr pane, reloads the file when it changes and follows links.

mod image;
pub mod open;
mod render;
pub(crate) use render::percent_decode;

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use render::{line_text, Doc, RLine, Theme};

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const WHEEL_LINES: usize = 3;
/// Metadata source and token other drovr commands use to find doc panes.
pub const METADATA_SOURCE: &str = "drovr";
pub const METADATA_TOKEN: &str = "drovr_doc";
/// Environment variable carrying a file path to the `$EDITOR` pane.
const OPEN_PATH_ENV: &str = "DROVR_OPEN_PATH";

/// Largest document the viewer reads; the rest is cut with a notice.
const MAX_DOC_BYTES: u64 = 4 * 1024 * 1024;
/// Loads slower than this space out reloads of a changing file.
const SLOW_LOAD: Duration = Duration::from_millis(50);
/// URL schemes handed to the OS opener. Anything else (custom app handlers,
/// `smb:`, ...) could launch programs, so it is only shown.
const EXTERNAL_SCHEMES: [&str; 3] = ["http", "https", "mailto"];

/// Short id of the herdr server this process talks to. Pane and workspace
/// ids are counters per server, so files keyed by them include this.
pub fn server_key() -> String {
    // FNV-1a: stable across builds, unlike `DefaultHasher`.
    let socket = crate::api::socket_path();
    let hash = socket
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
        });
    format!("{hash:016x}")
}

/// Control file `drovr doc open` writes to point a viewer pane at a
/// document: a nonce line, then the path.
pub fn control_file_path(pane_id: &str) -> PathBuf {
    crate::config::state_dir()
        .join("drovr")
        .join(format!("doc-pane-{}-{pane_id}.path", server_key()))
}

/// Control file content for `path`. The nonce makes every `doc open` a
/// change, so reopening the shown path still brings it back.
pub fn control_content(path: &Path) -> String {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    format!("{nonce}\n{}\n", path.display())
}

/// The path in control file content: its last non-empty line.
fn control_target(content: &str) -> Option<PathBuf> {
    content
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(PathBuf::from)
}

/// Where a link points, relative to the document that contains it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Anchor(String),
    Doc(PathBuf, Option<String>),
    File(PathBuf),
    External(String),
}

pub fn resolve_link(url: &str, current: &Path) -> Target {
    let url = url.trim();
    let file_url = url
        .get(..5)
        .filter(|scheme| scheme.eq_ignore_ascii_case("file:"))
        .map(|_| {
            let rest = &url[5..];
            // `file:///p` and `file:/p`; `file://localhost/p` keeps `/p`.
            rest.strip_prefix("//localhost")
                .or_else(|| rest.strip_prefix("//"))
                .unwrap_or(rest)
        });
    if file_url.is_none() && has_scheme(url) {
        return Target::External(url.to_string());
    }
    let url = file_url.unwrap_or(url);
    let (path, anchor) = match url.split_once('#') {
        Some((path, anchor)) => (path, Some(anchor.to_string()).filter(|a| !a.is_empty())),
        None => (url, None),
    };
    let path = render::percent_decode(path.split('?').next().unwrap_or(""));
    if path.is_empty() {
        return match anchor {
            Some(anchor) => Target::Anchor(anchor),
            None => Target::Doc(current.to_path_buf(), None),
        };
    }
    let resolved = if let Some(rest) = path.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .unwrap_or_else(|| PathBuf::from(&path))
    } else if Path::new(&path).is_absolute() {
        PathBuf::from(&path)
    } else {
        current.parent().unwrap_or(Path::new("/")).join(&path)
    };
    let resolved = normalize(&resolved);
    if is_markdown(&resolved) {
        Target::Doc(resolved, anchor)
    } else {
        Target::File(resolved)
    }
}

/// Whether the OS opener may receive `url` (see `EXTERNAL_SCHEMES`).
fn is_safe_external(url: &str) -> bool {
    url.split_once(':').is_some_and(|(scheme, _)| {
        EXTERNAL_SCHEMES
            .iter()
            .any(|known| scheme.eq_ignore_ascii_case(known))
    })
}

fn has_scheme(url: &str) -> bool {
    let Some((scheme, _)) = url.split_once(':') else {
        return false;
    };
    // A single letter is a Windows drive, not a scheme.
    scheme.len() > 1
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            ["md", "markdown", "mdown", "mkd"]
                .iter()
                .any(|known| ext.eq_ignore_ascii_case(known))
        })
}

/// Lexically removes `.` and `..` so the back stack and header show clean paths.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Side effects the viewer asks the event loop to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    OpenExternal(String),
    OpenFile(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp {
        modified: meta.modified().ok(),
        len: meta.len(),
    })
}

/// Widest text column. Wider panes centre it, like a reading view.
const MAX_TEXT_WIDTH: u16 = 88;
/// Rows above the body: the header and one blank row.
const BODY_TOP: u16 = 2;

/// Left offset and width of the text column in a pane `width` cells wide.
/// `full` uses the whole pane.
fn text_column(width: u16, full: bool) -> (u16, u16) {
    if full {
        return (0, width.max(1));
    }
    let margin = match width {
        40.. => 3,
        20.. => 1,
        _ => 0,
    };
    let text = width.saturating_sub(2 * margin).clamp(1, MAX_TEXT_WIDTH);
    (width.saturating_sub(text) / 2, text)
}

pub struct Viewer {
    path: PathBuf,
    source: Result<String, String>,
    stamp: Option<Stamp>,
    updated: Option<String>,
    theme: Theme,
    images: image::Images,
    /// Text spans the whole pane instead of a centred reading column.
    full_width: bool,
    doc: Doc,
    width: u16,
    height: usize,
    scroll: usize,
    back: Vec<(PathBuf, usize)>,
    selected: Option<usize>,
    search_input: Option<String>,
    query: Option<String>,
    last_match: Option<usize>,
    message: Option<String>,
    pending_anchor: Option<String>,
    control_path: Option<PathBuf>,
    control_seen: Option<String>,
    /// Set whenever the shown path changes, so the loop can report metadata.
    path_changed: bool,
    /// Time the last load took and when it finished; reloads of a changing
    /// file wait a few times that long so a big file cannot hog the loop.
    load_cost: Duration,
    loaded_at: Instant,
}

impl Viewer {
    pub fn new(path: PathBuf, theme: Theme, control_path: Option<PathBuf>) -> Self {
        let control_seen = control_path
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok());
        let mut viewer = Self {
            path,
            source: Err(String::new()),
            stamp: None,
            updated: None,
            theme,
            images: image::Images::default(),
            full_width: false,
            doc: Doc::default(),
            width: 80,
            height: 20,
            scroll: 0,
            back: Vec::new(),
            selected: None,
            search_input: None,
            query: None,
            last_match: None,
            message: None,
            pending_anchor: None,
            control_path,
            control_seen,
            path_changed: true,
            load_cost: Duration::ZERO,
            loaded_at: Instant::now(),
        };
        viewer.load();
        viewer
    }

    fn load(&mut self) {
        let started = Instant::now();
        self.stamp = stamp(&self.path);
        self.source = read_doc(&self.path);
        self.rerender();
        self.load_cost = started.elapsed();
        self.loaded_at = Instant::now();
    }

    fn rerender(&mut self) {
        self.doc = match &self.source {
            Ok(text) => {
                let (images, path) = (&mut self.images, &self.path);
                images.begin();
                let doc = render::render(
                    text,
                    text_column(self.width, self.full_width).1,
                    &self.theme,
                    &mut |url, width| images.place(url, path, width),
                );
                images.finish();
                doc
            }
            Err(err) => self.missing_doc(err),
        };
        if self
            .selected
            .is_some_and(|index| index >= self.doc.links.len())
        {
            self.selected = None;
        }
        if let Some(anchor) = self.pending_anchor.take() {
            if !self.jump_to_anchor(&anchor) && self.source.is_err() {
                self.pending_anchor = Some(anchor);
            }
        }
        self.clamp_scroll();
    }

    fn missing_doc(&self, err: &str) -> Doc {
        let warn = Style::default()
            .fg(self.theme.warn)
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(self.theme.dim);
        let span = |text: String, style| render::RSpan {
            text,
            style,
            link: None,
        };
        Doc {
            lines: vec![
                vec![span("Cannot read this document.".into(), warn)],
                vec![],
                vec![span(self.path.display().to_string(), Style::default())],
                vec![span(err.to_string(), dim)],
                vec![],
                vec![span(
                    "Waiting for it to appear; the view refreshes on its own.".into(),
                    dim,
                )],
            ],
            ..Doc::default()
        }
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.height = usize::from(height.saturating_sub(BODY_TOP + 1)).max(1);
        if width != self.width {
            self.width = width;
            self.rerender();
        } else {
            self.clamp_scroll();
        }
    }

    /// Switches between the reading column and the full pane width, keeping
    /// about the same part of the document in view.
    fn toggle_full_width(&mut self) {
        let (scroll, lines) = (self.scroll, self.doc.lines.len().max(1));
        self.full_width = !self.full_width;
        self.rerender();
        self.scroll_to_line(scroll * self.doc.lines.len() / lines);
        self.message = Some(
            if self.full_width {
                "full width"
            } else {
                "reading width"
            }
            .into(),
        );
    }

    /// Sets the cell size in pixels that images are laid out with.
    pub fn set_cell_size(&mut self, cell: Option<(u32, u32)>) {
        if self.images.set_cell_size(cell) {
            self.rerender();
        }
    }

    /// Graphics commands to write to the terminal before the next frame.
    pub fn take_graphics(&mut self) -> Vec<u8> {
        self.images.take_pending()
    }

    fn max_scroll(&self) -> usize {
        self.doc.lines.len().saturating_sub(self.height)
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.min(self.max_scroll());
    }

    pub fn scroll_by(&mut self, delta: isize) {
        self.scroll = self.scroll.saturating_add_signed(delta);
        self.clamp_scroll();
    }

    fn scroll_to_line(&mut self, line: usize) {
        self.scroll = line;
        self.clamp_scroll();
    }

    /// Reloads when the file's mtime or size changed. Returns true on change.
    pub fn poll_file(&mut self) -> bool {
        let current = stamp(&self.path);
        if current == self.stamp && (current.is_some() || self.source.is_err()) {
            return false;
        }
        // Cheap documents reload at once; costly ones wait four load times.
        if self.load_cost > SLOW_LOAD && self.loaded_at.elapsed() < self.load_cost * 4 {
            return false;
        }
        let scroll = self.scroll;
        self.load();
        self.scroll = scroll;
        self.clamp_scroll();
        self.updated = Some(local_time());
        true
    }

    /// Follows `drovr doc open` requests written to the control file.
    pub fn poll_control(&mut self) -> bool {
        let Some(path) = self.control_path.as_deref() else {
            return false;
        };
        let content = std::fs::read_to_string(path).ok();
        if content == self.control_seen {
            return false;
        }
        self.control_seen = content.clone();
        let Some(target) = content.as_deref().and_then(control_target) else {
            return false;
        };
        if target == self.path {
            self.load();
        } else {
            self.open_doc(target, None);
        }
        true
    }

    pub fn open_doc(&mut self, path: PathBuf, anchor: Option<String>) {
        if path == self.path {
            if let Some(anchor) = anchor {
                self.jump_to_anchor(&anchor);
            }
            return;
        }
        self.back
            .push((std::mem::replace(&mut self.path, path), self.scroll));
        self.after_switch(0, anchor);
    }

    pub fn go_back(&mut self) -> bool {
        let Some((path, scroll)) = self.back.pop() else {
            return false;
        };
        self.path = path;
        self.after_switch(scroll, None);
        true
    }

    fn after_switch(&mut self, scroll: usize, anchor: Option<String>) {
        self.selected = None;
        self.query = None;
        self.last_match = None;
        self.updated = None;
        self.message = None;
        self.pending_anchor = anchor;
        self.scroll = scroll;
        self.path_changed = true;
        self.load();
        self.clamp_scroll();
    }

    fn jump_to_anchor(&mut self, anchor: &str) -> bool {
        match self.doc.anchor_line(anchor) {
            Some(line) => {
                self.scroll_to_line(line);
                true
            }
            None => {
                self.message = Some(format!("no heading #{anchor}"));
                false
            }
        }
    }

    fn select_link(&mut self, forward: bool) {
        let count = self.doc.links.len();
        if count == 0 {
            self.message = Some("no links in this document".into());
            return;
        }
        let next = match self.selected {
            Some(index) if forward => (index + 1) % count,
            Some(index) => (index + count - 1) % count,
            None if forward => self
                .doc
                .links
                .iter()
                .position(|link| link.line >= self.scroll)
                .unwrap_or(0),
            None => self
                .doc
                .links
                .iter()
                .rposition(|link| link.line < self.scroll + self.height)
                .unwrap_or(count - 1),
        };
        self.selected = Some(next);
        let line = self.doc.links[next].line;
        if line < self.scroll || line >= self.scroll + self.height {
            self.scroll_to_line(line.saturating_sub(self.height / 3));
        }
    }

    pub fn follow(&mut self, index: usize) -> Action {
        let Some(link) = self.doc.links.get(index) else {
            return Action::None;
        };
        match resolve_link(&link.url, &self.path) {
            Target::Anchor(anchor) => {
                self.jump_to_anchor(&anchor);
                Action::None
            }
            Target::Doc(path, anchor) => {
                self.open_doc(path, anchor);
                Action::None
            }
            Target::File(path) => Action::OpenFile(path),
            Target::External(url) if is_safe_external(&url) => Action::OpenExternal(url),
            Target::External(url) => {
                self.message = Some(format!("not opening {url}"));
                Action::None
            }
        }
    }

    fn matches(&self) -> Vec<usize> {
        let Some(query) = self.query.as_deref().filter(|q| !q.is_empty()) else {
            return Vec::new();
        };
        let query = query.to_lowercase();
        self.doc
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line_text(line).to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect()
    }

    fn jump_match(&mut self, forward: bool) {
        let matches = self.matches();
        if matches.is_empty() {
            if let Some(query) = &self.query {
                self.message = Some(format!("not found: {query}"));
            }
            return;
        }
        let found = match (self.last_match, forward) {
            (None, _) => matches.iter().find(|&&line| line >= self.scroll),
            (Some(last), true) => matches.iter().find(|&&line| line > last),
            (Some(last), false) => matches.iter().rev().find(|&&line| line < last),
        };
        let line = match found {
            Some(&line) => line,
            None if forward => matches[0],
            None => matches[matches.len() - 1],
        };
        self.last_match = Some(line);
        self.scroll_to_line(line.saturating_sub(2));
        let position = matches.iter().position(|&l| l == line).unwrap_or(0);
        self.message = Some(format!("match {}/{}", position + 1, matches.len()));
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        if let Some(input) = self.search_input.as_mut() {
            match key.code {
                KeyCode::Esc => self.search_input = None,
                KeyCode::Enter => {
                    let query = self.search_input.take().unwrap_or_default();
                    self.query = (!query.is_empty()).then_some(query);
                    self.last_match = None;
                    self.jump_match(true);
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => input.push(c),
                _ => {}
            }
            return Action::None;
        }
        self.message = None;
        let page = self.height.saturating_sub(1).max(1) as isize;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => return Action::Quit,
            KeyCode::Char('d') if ctrl => self.scroll_by(page / 2),
            KeyCode::Char('u') if ctrl => self.scroll_by(-page / 2),
            KeyCode::Char('f') if ctrl => self.scroll_by(page),
            KeyCode::Char('b') if ctrl => self.scroll_by(-page),
            KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
            KeyCode::Char('j') | KeyCode::Down => self.scroll_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_by(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_by(page),
            KeyCode::PageUp => self.scroll_by(-page),
            KeyCode::Char('g') | KeyCode::Home => self.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => self.scroll = self.max_scroll(),
            KeyCode::Tab => self.select_link(true),
            KeyCode::BackTab => self.select_link(false),
            KeyCode::Enter => {
                if let Some(index) = self.selected {
                    return self.follow(index);
                }
            }
            KeyCode::Backspace | KeyCode::Char('b') => {
                if !self.go_back() {
                    self.message = Some("no previous document".into());
                }
            }
            KeyCode::Char('/') => self.search_input = Some(String::new()),
            KeyCode::Char('n') => self.jump_match(true),
            KeyCode::Char('N') => self.jump_match(false),
            KeyCode::Char('w') => self.toggle_full_width(),
            KeyCode::Char('r') => {
                self.load();
                self.updated = Some(local_time());
            }
            _ => {}
        }
        Action::None
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Action {
        match mouse.kind {
            MouseEventKind::ScrollDown => self.scroll_by(WHEEL_LINES as isize),
            MouseEventKind::ScrollUp => self.scroll_by(-(WHEEL_LINES as isize)),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(index) = self.link_at(mouse.column, mouse.row) {
                    self.selected = Some(index);
                    return self.follow(index);
                }
            }
            _ => {}
        }
        Action::None
    }

    /// Link under a screen cell. The body starts at row `BODY_TOP`, inset
    /// by the text column's left margin.
    fn link_at(&self, column: u16, row: u16) -> Option<usize> {
        let row = usize::from(row.checked_sub(BODY_TOP)?);
        let column = column.checked_sub(text_column(self.width, self.full_width).0)?;
        if row >= self.height {
            return None;
        }
        let line = self.doc.lines.get(self.scroll + row)?;
        let mut x = 0;
        for span in line {
            let width = span.text.width();
            if usize::from(column) < x + width {
                return span.link;
            }
            x += width;
        }
        None
    }

    fn header(&self) -> Line<'static> {
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string());
        let dir = self
            .path
            .parent()
            .map(|p| abbreviate_home(p) + "/")
            .unwrap_or_default();
        let dim = Style::default().fg(self.theme.dim);
        let mut spans = vec![
            Span::styled(
                format!(" {name}"),
                Style::default()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {dir}"), dim),
        ];
        if let Some(updated) = &self.updated {
            spans.push(Span::styled(
                format!("  updated {updated}"),
                Style::default().fg(self.theme.checked),
            ));
        }
        if !self.back.is_empty() {
            spans.push(Span::styled(format!("  ← {}", self.back.len()), dim));
        }
        Line::from(spans)
    }

    fn status(&self, width: usize) -> Line<'static> {
        let dim = Style::default().fg(self.theme.dim);
        if let Some(input) = &self.search_input {
            return Line::from(vec![
                Span::styled("/", Style::default().fg(self.theme.accent)),
                Span::raw(input.clone()),
                Span::styled("█", dim),
            ]);
        }
        let left = if let Some(message) = &self.message {
            Span::styled(format!(" {message}"), Style::default().fg(self.theme.warn))
        } else if let Some(link) = self.selected.and_then(|i| self.doc.links.get(i)) {
            Span::styled(
                format!(" → {}", link.url),
                Style::default().fg(self.theme.link),
            )
        } else {
            Span::styled(
                " j/k scroll  tab links  enter open  b back  / search  w width  q quit",
                dim,
            )
        };
        let total = self.doc.lines.len();
        let position = if total <= self.height {
            "all".to_string()
        } else {
            format!("{}%", (self.scroll + self.height).min(total) * 100 / total)
        };
        let gap = width.saturating_sub(left.content.width() + position.width() + 1);
        Line::from(vec![
            left,
            Span::raw(" ".repeat(gap)),
            Span::styled(position, dim),
        ])
    }

    fn styled_line(&self, line: &RLine) -> Line<'static> {
        let query = self
            .query
            .as_deref()
            .filter(|q| !q.is_empty())
            .map(str::to_lowercase);
        let match_style = Style::default()
            .fg(self.theme.match_fg)
            .bg(self.theme.match_bg);
        let mut spans = Vec::new();
        for span in line {
            let mut style = span.style;
            if span.link.is_some() && span.link == self.selected {
                style = style.add_modifier(Modifier::REVERSED);
            }
            match &query {
                Some(query) => {
                    for (text, hit) in split_matches(&span.text, query) {
                        spans.push(Span::styled(
                            text,
                            if hit { style.patch(match_style) } else { style },
                        ));
                    }
                }
                None => spans.push(Span::styled(span.text.clone(), style)),
            }
        }
        Line::from(spans)
    }

    pub fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(
            Paragraph::new(self.header()),
            Rect::new(area.x, area.y, area.width, 1.min(area.height)),
        );
        let body_height = area.height.saturating_sub(BODY_TOP + 1);
        let (left, text_width) = text_column(area.width, self.full_width);
        let lines: Vec<Line> = self
            .doc
            .lines
            .iter()
            .skip(self.scroll)
            .take(usize::from(body_height))
            .map(|line| self.styled_line(line))
            .collect();
        frame.render_widget(
            Paragraph::new(lines),
            Rect::new(
                area.x + left,
                area.y + BODY_TOP.min(area.height),
                text_width.min(area.width),
                body_height,
            ),
        );
        if area.height >= 2 {
            frame.render_widget(
                Paragraph::new(self.status(usize::from(area.width))),
                Rect::new(area.x, area.y + area.height - 1, area.width, 1),
            );
        }
    }
}

/// Splits `text` into pieces flagged as search hits (case-insensitive).
/// `query` is already lowercase.
fn split_matches(text: &str, query: &str) -> Vec<(String, bool)> {
    if query.is_empty() {
        return vec![(text.to_string(), false)];
    }
    // Lowercasing can change byte lengths, so map every byte of the
    // lowercase copy back to the start of the char it came from.
    let mut lower = String::with_capacity(text.len());
    let mut origin = Vec::with_capacity(text.len() + 1);
    for (index, ch) in text.char_indices() {
        for lc in ch.to_lowercase() {
            lower.push(lc);
            origin.resize(lower.len(), index);
        }
    }
    origin.push(text.len());
    let mut out = Vec::new();
    let mut start = 0;
    let mut search = 0;
    while let Some(found) = lower[search..].find(query) {
        let lower_begin = search + found;
        let lower_end = lower_begin + query.len();
        search = lower_end;
        let begin = origin[lower_begin].max(start);
        let end = origin[lower_end];
        if end <= begin {
            continue;
        }
        if begin > start {
            out.push((text[start..begin].to_string(), false));
        }
        out.push((text[begin..end].to_string(), true));
        start = end;
    }
    if start < text.len() {
        out.push((text[start..].to_string(), false));
    }
    out
}

/// Reads a document: regular files only (a FIFO or device would block or
/// never end), cut at `MAX_DOC_BYTES` with a notice.
fn read_doc(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let meta = std::fs::metadata(path).map_err(|err| err.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_DOC_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|err| err.to_string())?;
    let cut = bytes.len() as u64 > MAX_DOC_BYTES;
    bytes.truncate(MAX_DOC_BYTES as usize);
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if cut {
        text.push_str(&format!(
            "\n\n---\n\n*Document cut at {} MiB; open it in an editor to see the rest.*\n",
            MAX_DOC_BYTES / (1024 * 1024)
        ));
    }
    Ok(text)
}

fn abbreviate_home(path: &Path) -> String {
    let shown = path.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && shown.starts_with(&home) => {
            format!("~{}", &shown[home.len()..])
        }
        _ => shown,
    }
}

fn local_time() -> String {
    crate::platform::local_datetime()
        .map(|t| format!("{:02}:{:02}:{:02}", t.hour(), t.minute(), t.second()))
        .unwrap_or_default()
}

fn herdr_pane_id() -> Option<String> {
    std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|id| !id.trim().is_empty())
}

/// Runs this binary's herdr CLI in the background, output discarded.
fn spawn_cli(args: Vec<String>) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    std::thread::spawn(move || {
        let _ = Command::new(exe)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

fn report_doc(pane_id: &str, path: &Path) {
    spawn_cli(vec![
        "pane".into(),
        "report-metadata".into(),
        pane_id.into(),
        "--source".into(),
        METADATA_SOURCE.into(),
        "--token".into(),
        format!("{METADATA_TOKEN}={}", path.display()),
    ]);
}

fn clear_doc_report(pane_id: &str) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = Command::new(exe)
        .args([
            "pane",
            "report-metadata",
            pane_id,
            "--source",
            METADATA_SOURCE,
            "--clear-token",
            METADATA_TOKEN,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Opens a non-Markdown file in `$EDITOR` in a new herdr pane. Outside herdr
/// the path is only shown: the OS opener would run executables.
fn open_file(path: &Path) -> String {
    let Some(pane_id) = herdr_pane_id() else {
        return format!("file: {}", path.display());
    };
    let Ok(exe) = std::env::current_exe() else {
        return "cannot locate drovr binary".into();
    };
    let editor = std::env::var("EDITOR")
        .ok()
        .filter(|e| !e.trim().is_empty())
        .unwrap_or_else(|| "vi".into());
    let cwd = path.parent().unwrap_or(Path::new("/")).to_path_buf();
    // The path comes from the document, so it reaches the editor through the
    // pane's environment, never as typed shell text.
    let path_env = format!("{OPEN_PATH_ENV}={}", path.display());
    let command = format!("{editor} \"${OPEN_PATH_ENV}\"");
    std::thread::spawn(move || {
        let Ok(output) = Command::new(&exe)
            .args(["pane", "split", &pane_id, "--direction", "right", "--focus"])
            .arg("--cwd")
            .arg(&cwd)
            .arg("--env")
            .arg(&path_env)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        else {
            return;
        };
        let new_pane = serde_json::from_slice::<serde_json::Value>(&output.stdout)
            .ok()
            .and_then(|v| v["result"]["pane"]["pane_id"].as_str().map(String::from));
        if let Some(new_pane) = new_pane {
            let _ = Command::new(&exe)
                .args(["pane", "run", &new_pane, &command])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    });
    format!("opening {} in $EDITOR", path.display())
}

/// Opens an http(s) or mailto URL. The viewer runs where the workspace
/// lives; on a headless server the URL is shown for the user to open.
fn open_external(url: &str) -> String {
    if !crate::platform::can_open_urls() {
        return format!("open on your machine: {url}");
    }
    match crate::platform::open_url(url) {
        Ok(Some(mut child)) => {
            std::thread::spawn(move || child.wait());
            format!("opened {url}")
        }
        Ok(None) => format!("opened {url}"),
        Err(err) => format!("cannot open {url}: {err}"),
    }
}

/// Cell size in pixels, or `None` when the terminal does not report pixels.
fn cell_size() -> Option<(u32, u32)> {
    let size = ratatui::crossterm::terminal::window_size().ok()?;
    if size.columns == 0 || size.rows == 0 {
        return None;
    }
    Some((
        u32::from(size.width) / u32::from(size.columns),
        u32::from(size.height) / u32::from(size.rows),
    ))
}

fn write_graphics(bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    let mut out = io::stdout().lock();
    io::Write::write_all(&mut out, bytes)?;
    io::Write::flush(&mut out)
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
}

/// Runs the viewer on `path`, or, without one, on the path in this pane's
/// control file (how `drovr doc open` starts it, so no path is typed into a
/// shell).
pub fn run_doc_view(path: Option<&Path>) -> io::Result<()> {
    let pane_id = herdr_pane_id();
    let control_path = pane_id.as_deref().map(control_file_path);
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => control_path
            .as_deref()
            .and_then(|control| std::fs::read_to_string(control).ok())
            .as_deref()
            .and_then(control_target)
            .ok_or_else(|| io::Error::other("no document given and no doc open request"))?,
    };
    let path = normalize(&std::path::absolute(path)?);
    let config = crate::config::Config::load().config;
    let palette = crate::app::client_palette_from_config(&config);
    let graphics = config.kitty_graphics_enabled();
    let mut viewer = Viewer::new(path, Theme::from_palette(&palette), control_path);
    if config.ui.doc_full_width {
        viewer.toggle_full_width();
        viewer.message = None;
    }

    enable_raw_mode()?;
    let _guard = TerminalGuard;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous_hook(info);
    }));
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(io::stdout()))?;

    let mut next_poll = Instant::now() + POLL_INTERVAL;
    loop {
        if viewer.path_changed {
            viewer.path_changed = false;
            if let Some(pane_id) = &pane_id {
                report_doc(pane_id, &viewer.path);
            }
        }
        if graphics {
            viewer.set_cell_size(cell_size());
        }
        let size = terminal.size()?;
        viewer.resize(size.width, size.height);
        write_graphics(&viewer.take_graphics())?;
        terminal.draw(|frame| viewer.draw(frame))?;

        let timeout = next_poll.saturating_duration_since(Instant::now());
        if event::poll(timeout)? {
            let action = match event::read()? {
                Event::Key(key) => viewer.handle_key(key),
                Event::Mouse(mouse) => viewer.handle_mouse(mouse),
                _ => Action::None,
            };
            match action {
                Action::None => {}
                Action::Quit => break,
                Action::OpenExternal(url) => viewer.message = Some(open_external(&url)),
                Action::OpenFile(path) => viewer.message = Some(open_file(&path)),
            }
        }
        if Instant::now() >= next_poll {
            viewer.poll_control();
            viewer.poll_file();
            next_poll = Instant::now() + POLL_INTERVAL;
        }
    }
    write_graphics(&viewer.images.clear_all())?;
    drop(terminal);
    if let Some(pane_id) = &pane_id {
        clear_doc_report(pane_id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::Palette;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "drovr-doc-view-{}-{name}-{:?}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, name: &str, text: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn viewer(path: PathBuf, control: Option<PathBuf>) -> Viewer {
        let mut viewer = Viewer::new(path, Theme::from_palette(&Palette::catppuccin()), control);
        viewer.resize(40, 12);
        viewer
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn numbered(count: usize) -> String {
        (0..count).map(|i| format!("line {i}\n\n")).collect()
    }

    #[test]
    fn resolves_relative_absolute_anchor_and_external_links() {
        let current = Path::new("/docs/plans/plan.md");
        assert_eq!(
            resolve_link("../notes.md#next-step", current),
            Target::Doc(PathBuf::from("/docs/notes.md"), Some("next-step".into()))
        );
        assert_eq!(
            resolve_link("./sub/a%20b.MD", current),
            Target::Doc(PathBuf::from("/docs/plans/sub/a b.MD"), None)
        );
        assert_eq!(
            resolve_link("/etc/hosts", current),
            Target::File(PathBuf::from("/etc/hosts"))
        );
        assert_eq!(
            resolve_link("file:///tmp/x.md", current),
            Target::Doc(PathBuf::from("/tmp/x.md"), None)
        );
        assert_eq!(
            resolve_link("#intro", current),
            Target::Anchor("intro".into())
        );
        assert_eq!(
            resolve_link("https://example.com/a.md", current),
            Target::External("https://example.com/a.md".into())
        );
        assert_eq!(
            resolve_link("mailto:a@b.c", current),
            Target::External("mailto:a@b.c".into())
        );
        assert_eq!(
            resolve_link("src/main.rs", current),
            Target::File(PathBuf::from("/docs/plans/src/main.rs"))
        );
        // Any case, one slash or localhost: still a local file.
        for url in [
            "FILE:///tmp/run.command",
            "file:/tmp/run.command",
            "File://localhost/tmp/run.command",
        ] {
            assert_eq!(
                resolve_link(url, current),
                Target::File(PathBuf::from("/tmp/run.command")),
                "{url}"
            );
        }
        assert!(is_safe_external("HTTPS://example.com"));
        assert!(!is_safe_external("x-apple.systempreferences:foo"));
        assert!(!is_safe_external("smb://host/share"));
    }

    #[test]
    fn unsafe_schemes_are_shown_not_opened() {
        let dir = TempDir::new("schemes");
        let main = dir.write("main.md", "[a](smb://host/share) [b](https://x.y)");
        let mut v = viewer(main, None);
        assert_eq!(v.follow(0), Action::None);
        assert_eq!(v.message.as_deref(), Some("not opening smb://host/share"));
        assert_eq!(v.follow(1), Action::OpenExternal("https://x.y".into()));
    }

    #[test]
    fn reads_only_regular_files_and_cuts_big_ones() {
        let dir = TempDir::new("read");
        assert_eq!(read_doc(&dir.0), Err("not a regular file".into()));
        let big = dir.write("big.md", &"x".repeat(MAX_DOC_BYTES as usize + 10));
        let text = read_doc(&big).unwrap();
        assert!(text.contains("Document cut at"));
        assert!(text.len() < MAX_DOC_BYTES as usize + 200);
    }

    #[test]
    fn links_navigate_with_a_back_stack() {
        let dir = TempDir::new("nav");
        let main = dir.write(
            "main.md",
            &format!("[next](next.md#target)\n\n{}", numbered(10)),
        );
        dir.write("next.md", &format!("{}# Target\n\ntext", numbered(20)));
        let mut v = viewer(main.clone(), None);
        v.scroll_by(3);
        v.handle_key(key(KeyCode::Tab));
        assert_eq!(v.selected, Some(0));
        assert_eq!(v.handle_key(key(KeyCode::Enter)), Action::None);
        assert_eq!(v.path, dir.0.join("next.md"));
        let target = v.doc.anchor_line("target").unwrap();
        assert_eq!(v.scroll, target.min(v.max_scroll()));
        assert_eq!(v.back, vec![(main.clone(), 0)]);
        v.handle_key(key(KeyCode::Char('b')));
        assert_eq!(v.path, main);
        assert!(v.back.is_empty());
        v.handle_key(key(KeyCode::Backspace));
        assert_eq!(v.message.as_deref(), Some("no previous document"));
    }

    #[test]
    fn back_restores_scroll() {
        let dir = TempDir::new("back");
        let main = dir.write("main.md", &numbered(30));
        let other = dir.write("other.md", "other");
        let mut v = viewer(main.clone(), None);
        v.scroll_by(15);
        v.open_doc(other.clone(), None);
        assert_eq!(v.scroll, 0);
        assert!(v.go_back());
        assert_eq!((v.path.clone(), v.scroll), (main, 15));
    }

    #[test]
    fn reload_keeps_scroll_and_clamps_it() {
        let dir = TempDir::new("reload");
        let path = dir.write("doc.md", &numbered(30));
        let mut v = viewer(path.clone(), None);
        v.scroll_by(20);
        assert!(!v.poll_file());
        std::fs::write(&path, numbered(31)).unwrap();
        assert!(v.poll_file());
        assert_eq!(v.scroll, 20);
        assert!(v.updated.is_some());
        std::fs::write(&path, numbered(5)).unwrap();
        assert!(v.poll_file());
        assert_eq!(v.scroll, v.max_scroll());
    }

    #[test]
    fn missing_file_shows_message_and_loads_when_created() {
        let dir = TempDir::new("missing");
        let path = dir.0.join("later.md");
        let mut v = viewer(path.clone(), None);
        assert!(line_text(&v.doc.lines[0]).contains("Cannot read"));
        assert!(!v.poll_file());
        std::fs::write(&path, "# Ready").unwrap();
        assert!(v.poll_file());
        assert_eq!(line_text(&v.doc.lines[0]), "Ready");
    }

    #[test]
    fn control_file_switches_document() {
        let dir = TempDir::new("control");
        let first = dir.write("first.md", "first");
        let second = dir.write("second.md", "second");
        let control = dir.write("pane.path", "stale");
        let mut v = viewer(first.clone(), Some(control.clone()));
        assert!(!v.poll_control());
        std::fs::write(&control, format!("{}\n", second.display())).unwrap();
        assert!(v.poll_control());
        assert_eq!(v.path, second);
        assert_eq!(v.back.len(), 1);
        assert_eq!(line_text(&v.doc.lines[0]), "second");

        // Reopening the same path after navigating away brings it back.
        v.go_back();
        assert_eq!(v.path, first);
        std::fs::write(&control, control_content(&second)).unwrap();
        assert!(v.poll_control());
        assert_eq!(v.path, second);
        std::fs::write(&control, control_content(&first)).unwrap();
        assert!(v.poll_control());
        v.go_back();
        assert_eq!(v.path, second);
        std::fs::write(&control, control_content(&second)).unwrap();
        assert!(v.poll_control());
        assert_eq!(control_target("1\n/a.md\n"), Some(PathBuf::from("/a.md")));
    }

    #[test]
    fn search_jumps_between_matches() {
        let dir = TempDir::new("search");
        let path = dir.write(
            "doc.md",
            &format!("{}Needle\n\n{}needle", numbered(10), numbered(10)),
        );
        let mut v = viewer(path, None);
        v.handle_key(key(KeyCode::Char('/')));
        for c in "needle".chars() {
            v.handle_key(key(KeyCode::Char(c)));
        }
        v.handle_key(key(KeyCode::Enter));
        assert_eq!(v.scroll, 20 - 2);
        v.handle_key(key(KeyCode::Char('n')));
        assert_eq!(v.scroll, v.max_scroll());
        v.handle_key(key(KeyCode::Char('N')));
        assert_eq!(v.scroll, 18);
        assert_eq!(
            split_matches("a Needle b", "needle"),
            vec![
                ("a ".to_string(), false),
                ("Needle".to_string(), true),
                (" b".to_string(), false)
            ]
        );
        // 'ẞ' shrinks and 'Ⱥ' grows when lowercased: no panic, right span.
        assert_eq!(
            split_matches("ẞȺ", "ⱥ"),
            vec![("ẞ".to_string(), false), ("Ⱥ".to_string(), true)]
        );
    }

    #[test]
    fn mouse_click_follows_link_under_cursor() {
        let dir = TempDir::new("mouse");
        let main = dir.write("main.md", "go [there](#end)\n\n# End");
        let mut v = viewer(main, None);
        let click = |column| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        // At 80 columns the text starts after a 3-column margin.
        assert_eq!(text_column(80, false), (3, 74));
        assert_eq!(text_column(200, false), (56, 88));
        assert_eq!(text_column(10, false), (0, 10));
        assert_eq!(text_column(200, true), (0, 200));
        assert_eq!(v.link_at(2, 2), None);
        assert_eq!(v.link_at(3, 2), None);
        assert_eq!(v.link_at(7, 1), None);
        assert_eq!(v.link_at(7, 2), Some(0));
        assert_eq!(v.handle_mouse(click(7)), Action::None);
        assert_eq!(v.selected, Some(0));

        // `w` drops the margin, so the link starts at column 3.
        v.handle_key(key(KeyCode::Char('w')));
        assert_eq!(v.link_at(2, 2), None);
        assert_eq!(v.link_at(3, 2), Some(0));
        v.handle_key(key(KeyCode::Char('w')));
        assert_eq!(v.link_at(3, 2), None);
    }
}
