//! drovr fork: the Tasks view of the right panel (docs/design/tasks.md,
//! section 4). A sidebar section is a project; the view shows its board
//! (`board.rs`) or one task (`view.rs`) inside the panel the inbox draws.
//!
//! Drawing never queries the database: [`ClientShellState::refresh_tasks`]
//! fills the cache of [`TasksState`] from the store at most every 500 ms
//! (when `PRAGMA data_version` moved, the filter changed, or after a panel
//! write), and [`render`] draws from that cache and the endpoints.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::agent_signal::{InboxFilter, ItemKind};
use super::inbox::PanelView;
use super::inbox_editor::{EditorKey, NoteEditor};
use super::projects::{self, ProjectLayout, OTHER};
use super::render::{display_width, put_text};
use super::*;
use crate::tasks::{
    self, Actor, ArtifactKind, CheckState, EntryKind, Kind, NewTask, OpenDecision, Priority,
    Ruling, Status, StoreError, StoreResult, TaskCard, TaskDetail, TaskFilter, TaskPatch,
    TaskStore,
};

mod board;
mod view;

/// The store is read at most this often.
const REFRESH_EVERY: Duration = Duration::from_millis(500);
/// A refusal or write error stays in the status line this long.
const STATUS_FOR: Duration = Duration::from_secs(4);
/// Closed tasks shown in the Done lane.
const DONE_LIMIT: u32 = 20;
/// Lanes a card can still be started from.
const OPEN_LANES: [Status; 5] = [
    Status::Triage,
    Status::Ready,
    Status::Working,
    Status::Blocked,
    Status::Review,
];

// ------------------------------------------------------------------ menus

#[derive(Debug)]
pub(super) enum TaskMenu {
    /// Right-click a card. The flags pick the items (Start, Focus pane).
    Card {
        startable: bool,
        live: bool,
    },
    Move {
        current: Status,
    },
    Kind,
    Priority,
    Machine {
        machines: Vec<(ClientEndpointId, String)>,
    },
}

/// The items of a task menu.
pub(super) fn task_menu_items(menu: &TaskMenu) -> Vec<ClientContextMenuItem> {
    use ClientContextMenuAction as Action;
    let item = |label: String, action| ClientContextMenuItem {
        label: label.into(),
        action,
    };
    match menu {
        TaskMenu::Card { startable, live } => {
            let mut items = vec![item("Open".into(), Action::TaskOpen)];
            if *startable {
                items.push(item("Start…".into(), Action::TaskStart));
            }
            if *live {
                items.push(item("Focus pane".into(), Action::TaskFocusPane));
            }
            items.push(item("Move…".into(), Action::TaskMoveMenu));
            items.push(item("Copy id".into(), Action::TaskCopyId));
            items
        }
        TaskMenu::Move { current } => Status::LANES
            .into_iter()
            .chain([Status::Cancelled])
            .map(|status| {
                let label = if status == *current {
                    format!("· {}", status.label())
                } else {
                    status.label().to_owned()
                };
                item(label, Action::TaskMove(status))
            })
            .collect(),
        TaskMenu::Kind => KINDS
            .iter()
            .map(|(kind, name)| item((*name).to_owned(), Action::TaskKind(Some(*kind))))
            .chain([item("none".into(), Action::TaskKind(None))])
            .collect(),
        TaskMenu::Priority => PRIORITIES
            .iter()
            .map(|(priority, name)| item((*name).to_owned(), Action::TaskPriority(*priority)))
            .collect(),
        TaskMenu::Machine { machines } => machines
            .iter()
            .enumerate()
            .map(|(index, (_, label))| item(format!("on {label}"), Action::TaskOnMachine(index)))
            .collect(),
    }
}

pub(super) const KINDS: [(Kind, &str); 5] = [
    (Kind::Fix, "fix"),
    (Kind::Feature, "feature"),
    (Kind::Chore, "chore"),
    (Kind::Research, "research"),
    (Kind::Spec, "spec"),
];

pub(super) const PRIORITIES: [(Priority, &str); 4] = [
    (Priority::Urgent, "urgent"),
    (Priority::High, "high"),
    (Priority::Normal, "normal"),
    (Priority::Low, "low"),
];

pub(super) fn kind_name(kind: Kind) -> &'static str {
    KINDS
        .iter()
        .find(|(k, _)| *k == kind)
        .map_or("", |(_, name)| name)
}

pub(super) fn priority_name(priority: Priority) -> &'static str {
    PRIORITIES
        .iter()
        .find(|(p, _)| *p == priority)
        .map_or("", |(_, name)| name)
}

// ------------------------------------------------------------------ state

/// What the panel shows, from the panel filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Scope {
    /// No filter: the project list.
    All,
    Project(String),
    /// A workspace: the `machine/{workspace_id}:` prefix of its key.
    Workspace(String),
}

/// The task project of workspaces in no section.
const OTHER_PROJECT: &str = "Other";

pub(super) fn scope_of(filter: Option<&InboxFilter>, endpoints: &[ClientShellEndpoint]) -> Scope {
    match filter {
        None => Scope::All,
        // The Other section's tasks live in a project named "Other".
        Some(InboxFilter::Project(name)) if name == OTHER => Scope::Project(OTHER_PROJECT.into()),
        Some(InboxFilter::Project(name)) => Scope::Project(name.clone()),
        Some(InboxFilter::Workspace {
            endpoint_id,
            workspace_id,
        }) => {
            let machine = endpoints
                .iter()
                .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
                .map_or_else(|| "local".to_owned(), projects::machine_key);
            Scope::Workspace(format!("{machine}/{workspace_id}:"))
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum DetailTab {
    #[default]
    Notes,
    Attempts,
    Artifacts,
}

impl DetailTab {
    fn next(self) -> Self {
        match self {
            Self::Notes => Self::Attempts,
            Self::Attempts => Self::Artifacts,
            Self::Artifacts => Self::Notes,
        }
    }
}

/// What a click on the Tasks view hits.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Hit {
    Project(String),
    Lane(Status),
    Card(String),
    Button(String),
    FilterClear,
    Back,
    StatusChip,
    Auto,
    Prev,
    Next,
    Pane,
    Waiting,
    Title,
    Kind,
    Priority,
    Workspace,
    More,
    Edit,
    Mark(i64),
    Criterion(i64),
    AddCriterion,
    Choice(usize),
    Reply,
    Accept,
    SendBack,
    TakeOver,
    Tab(DetailTab),
    /// An event run, by its first entry id.
    Events(i64),
    AttemptPane(String),
    Release,
    Artifact(i64),
    Start,
    Archive,
    Composer,
    /// Make a task of the filtered workspace, linked to it.
    Track,
}

#[derive(Clone, Debug, Default)]
pub(super) struct TasksHits {
    /// `+ new` on the header line (drawn by the inbox).
    pub(super) new: Rect,
    pub(super) body: Rect,
    /// Later entries win: buttons are pushed after their card.
    pub(super) items: Vec<(Rect, Hit)>,
}

impl TasksHits {
    fn at(&self, point: (u16, u16)) -> Option<Hit> {
        self.items
            .iter()
            .rev()
            .find(|(rect, _)| super::contains(*rect, point))
            .map(|(_, hit)| hit.clone())
    }
}

/// What a one-line input's text becomes.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Purpose {
    /// `+ new`: `[#kind] [!priority] text` in Triage of the shown project.
    New,
    /// `/`: filters the board as it is typed.
    Filter,
    /// The title; `force` saves without the version check (second Enter).
    Title {
        id: String,
        version: i64,
        force: bool,
    },
    AddCriterion(String),
    /// Free-text ruling of the open decision.
    Reply {
        id: String,
        decision: i64,
    },
    /// The note of a send back (review -> ready), then the move.
    SendBack(String),
    /// The notes composer.
    Composer(String),
}

impl Purpose {
    /// What a text left by a click away is kept under; None for the filter,
    /// which applies as it is typed.
    fn stash_key(&self) -> Option<String> {
        Some(match self {
            Self::New => "new".into(),
            Self::Filter => return None,
            Self::Title { id, .. } => format!("title:{id}"),
            Self::AddCriterion(id) => format!("criterion:{id}"),
            Self::Reply { id, decision } => format!("reply:{id}:{decision}"),
            Self::SendBack(id) => format!("send-back:{id}"),
            Self::Composer(id) => format!("note:{id}"),
        })
    }
}

#[derive(Debug)]
pub(super) struct Input {
    pub(super) purpose: Purpose,
    pub(super) editor: NoteEditor,
}

/// The `$EDITOR` file of a description being edited.
#[derive(Debug)]
struct EditFile {
    id: String,
    path: std::path::PathBuf,
    mtime: Option<std::time::SystemTime>,
    /// The description the edit started from. A save is refused only when
    /// the stored description moved away from it, not on any version bump
    /// (usage copies bump the version of a working task all the time).
    base: String,
    saved: bool,
    /// The last save was refused or failed: the file holds text the store
    /// does not, so `e` reopens it instead of writing over it.
    kept: bool,
}

#[derive(Debug, Default)]
pub(super) struct TasksState {
    /// The shown project or workspace, lane then position.
    cards: Vec<TaskCard>,
    /// The project list: sections in sidebar order with lane counts.
    counts: Vec<(String, [u32; 6])>,
    /// The open task view.
    detail: Option<TaskDetail>,
    /// Open decisions of every project, for the inbox rows.
    pub(super) decisions: Vec<OpenDecision>,
    /// "machine/pane_id" -> display id of the live attempt, for inbox rows.
    pub(super) pane_tasks: HashMap<String, String>,
    data_version: Option<i64>,
    checked: Option<Instant>,
    /// Set after a panel write: `data_version` does not move for this
    /// connection's own commits.
    dirty: bool,
    /// `TooNew` or an open failure, drawn instead of the board.
    error: Option<String>,
    pub(super) hits: TasksHits,
    /// Lane totals of the shown project (Done counts every closed task).
    totals: [u32; 6],
    /// The scope and open task the cache was loaded for.
    loaded: Option<(Scope, Option<String>)>,
    /// Display id of the open task view.
    open: Option<String>,
    /// Board order the view was opened from, for `[` and `]`.
    order: Vec<String>,
    selected: Option<String>,
    /// The project list's selected row.
    selected_project: Option<String>,
    scroll: usize,
    follow: bool,
    view_scroll: usize,
    /// Scroll the view to its decision card on the next draw.
    to_decision: bool,
    /// Lanes expanded by hand this session (`project:lane`).
    expanded: HashSet<String>,
    /// The `/` text filter.
    text: Option<String>,
    pub(super) input: Option<Input>,
    /// Text of inputs closed by a click away or by leaving the view, by
    /// [`Purpose::stash_key`]; reopening the same input restores it.
    stash: HashMap<String, String>,
    status: Option<(String, Instant)>,
    hover: Option<String>,
    tab: DetailTab,
    desc_open: bool,
    /// Criteria (by position) with their evidence unfolded.
    evidence: HashSet<i64>,
    /// Event runs (by first entry id) expanded.
    events: HashSet<i64>,
    edit: Option<EditFile>,
    /// Drawn with the columns layout (inner width 90 or more).
    columns: bool,
    /// Something drawn changed outside a key or click (a reload, an expiry).
    changed: bool,
    /// Actions the panel took, for tests (no real launch or focus).
    #[cfg(test)]
    log: Vec<String>,
}

impl TasksState {
    fn status_line(&mut self, text: impl Into<String>) {
        self.status = Some((text.into(), Instant::now()));
    }

    fn status_text(&self) -> Option<&str> {
        self.status
            .as_ref()
            .filter(|(_, at)| at.elapsed() < STATUS_FOR)
            .map(|(text, _)| text.as_str())
    }

    pub(super) fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    /// The pointer left the panel; true when a hover was drawn.
    pub(super) fn clear_hover(&mut self) -> bool {
        self.hover.take().is_some()
    }

    /// The input is open: keys and pastes go to it.
    pub(super) fn editing(&self) -> bool {
        self.input.is_some()
    }

    fn project(&self) -> Option<&str> {
        match &self.loaded {
            Some((Scope::Project(name), _)) => Some(name),
            _ => None,
        }
    }

    /// The key prefix of collapsed lanes.
    fn lane_key(&self, lane: Status) -> String {
        format!("{}:{}", self.project().unwrap_or("*"), lane.as_str())
    }

    fn card(&self, id: &str) -> Option<&TaskCard> {
        self.cards.iter().find(|card| card.task.display_id == id)
    }

    fn open_input(&mut self, purpose: Purpose, text: &str) {
        let kept = purpose.stash_key().and_then(|key| self.stash.remove(&key));
        self.input = Some(Input {
            editor: NoteEditor::new(kept.as_deref().unwrap_or(text)),
            purpose,
        });
    }

    /// Closes the input without losing its text: only Enter (Ok) and Esc
    /// drop what was typed.
    fn put_input_aside(&mut self) {
        let Some(input) = self.input.take() else {
            return;
        };
        let text = input.editor.text();
        if let Some(key) = input.purpose.stash_key() {
            if !text.trim().is_empty() {
                self.stash.insert(key, text);
            }
        }
    }

    /// Whether `lane` is folded on the board.
    fn collapsed(&self, layout: &ProjectLayout, lane: usize, cards: usize) -> bool {
        let key = self.lane_key(Status::LANES[lane]);
        if self.expanded.contains(&key) {
            return false;
        }
        if layout.tasks.collapsed.contains(&key) {
            return true;
        }
        (lane == 0 || lane == 5) && (self.columns || cards > 3)
    }

    fn close_view(&mut self) {
        self.open = None;
        self.detail = None;
        self.put_input_aside();
        self.to_decision = false;
        self.follow = true;
    }
}

/// Keeps the selected card when it is in `order`, else selects the first.
fn keep_selection(state: &mut TasksState, order: &[String]) {
    if !state.selected.as_ref().is_some_and(|id| order.contains(id)) {
        state.selected = order.first().cloned();
    }
}

/// The action button of a card (section 4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CardButton {
    Start,
    Pane,
    Accept,
}

impl CardButton {
    pub(super) fn of(card: &TaskCard) -> Option<Self> {
        match card.task.status {
            Status::Review => Some(Self::Accept),
            _ if card.live.is_some() => Some(Self::Pane),
            Status::Triage | Status::Ready => Some(Self::Start),
            _ => None,
        }
    }

    pub(super) fn label(self, short: bool) -> &'static str {
        match (self, short) {
            (Self::Start, false) => "▶ start",
            (Self::Pane, false) => "↗ pane",
            (Self::Accept, false) => "✓ accept",
            (Self::Start, true) => "▶",
            (Self::Pane, true) => "↗",
            (Self::Accept, true) => "✓",
        }
    }
}

/// `[#kind] [!priority] text` (`!!` urgent, `!` high).
pub(super) fn parse_new(text: &str) -> (Option<Kind>, Priority, String) {
    let mut kind = None;
    let mut priority = Priority::Normal;
    let mut rest = Vec::new();
    for word in text.split_whitespace() {
        if rest.is_empty() {
            if let Some(name) = word.strip_prefix('#') {
                if let Some((k, _)) = KINDS.iter().find(|(_, n)| n.eq_ignore_ascii_case(name)) {
                    kind = Some(*k);
                    continue;
                }
            }
            match word {
                "!!" => {
                    priority = Priority::Urgent;
                    continue;
                }
                "!" => {
                    priority = Priority::High;
                    continue;
                }
                _ => {}
            }
        }
        rest.push(word);
    }
    (kind, priority, rest.join(" "))
}

/// Unix seconds of an RFC 3339 UTC timestamp written by the store
/// (`2026-10-03T08:15:02Z`).
pub(super) fn unix_of(text: &str) -> Option<u64> {
    let n = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (h, mi, s) = (n(11..13)?, n(14..16)?, n(17..19)?);
    // Days from the civil date (H. Hinnant's algorithm).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((mo + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + mi * 60 + s).ok()
}

/// `14m`, `2h`: the time since `text`.
pub(super) fn age(text: &str, now: u64) -> String {
    unix_of(text).map_or_else(String::new, |at| {
        projects::format_age(now.saturating_sub(at))
    })
}

/// The agent on `pane_key` ("machine/pane_id") and its endpoint.
pub(super) fn find_agent<'a>(
    endpoints: &'a [ClientShellEndpoint],
    pane_key: &str,
) -> Option<(
    &'a ClientShellEndpoint,
    &'a crate::protocol::ClientShellAgent,
)> {
    let (machine, pane_id) = pane_key.split_once('/')?;
    endpoints
        .iter()
        .filter(|endpoint| endpoint.status == ClientEndpointStatus::Online)
        .filter(|endpoint| projects::machine_key(endpoint) == machine)
        .find_map(|endpoint| {
            let agent = endpoint
                .snapshot
                .as_deref()?
                .agents
                .iter()
                .find(|agent| agent.pane_id == pane_id)?;
            Some((endpoint, agent))
        })
}

/// The radar colour of the live agent on `pane_key`, as the sidebar draws
/// it: green working, yellow waiting, blue finished and unseen, `overlay0`
/// idle or not found.
pub(super) fn agent_color(
    endpoints: &[ClientShellEndpoint],
    layout: &ProjectLayout,
    pane_key: Option<&str>,
    palette: &Palette,
) -> ratatui::style::Color {
    let Some((endpoint, agent)) = pane_key.and_then(|key| find_agent(endpoints, key)) else {
        return palette.overlay0;
    };
    let now = super::agent_signal::unix_now();
    if super::inbox::agent_item(layout, endpoint, agent, now).is_some_and(ItemKind::waiting) {
        return palette.yellow;
    }
    match layout.presence(
        &projects::agent_key(endpoint, &agent.pane_id),
        agent.state_change_seq,
        agent.agent_status,
    ) {
        projects::Presence::Working => palette.green,
        projects::Presence::Blocked => palette.yellow,
        projects::Presence::Done | projects::Presence::Unread => palette.blue,
        projects::Presence::Idle => palette.overlay0,
    }
}

/// `waiting on you: permission · 3m` when the live pane waits on an inbox
/// item, with the item's pane; else `None`.
pub(super) fn pane_waiting(
    endpoints: &[ClientShellEndpoint],
    layout: &ProjectLayout,
    pane_key: &str,
) -> Option<String> {
    let (endpoint, agent) = find_agent(endpoints, pane_key)?;
    let now = super::agent_signal::unix_now();
    let kind = super::inbox::agent_item(layout, endpoint, agent, now).filter(|k| k.waiting())?;
    let label = match kind {
        ItemKind::Permission => "permission",
        ItemKind::Question => "question",
        ItemKind::Plan => "plan",
        _ => "dialog",
    };
    let age = projects::idle_secs(&projects::agent_key(endpoint, &agent.pane_id))
        .map(projects::format_age)
        .map_or_else(String::new, |age| format!(" · {age}"));
    Some(format!("waiting on you: {label}{age}"))
}

/// Writes `text` from `x`, clipped at `right`; returns the end column.
pub(super) fn put(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    right: u16,
    text: &str,
    style: Style,
) -> u16 {
    if x >= right {
        return x;
    }
    let width = display_width(text).min(right - x);
    put_text(buffer, x, y, width, text, style);
    x + width
}

/// `text` cut to `width` columns with `…`.
pub(super) fn cut(text: &str, width: u16) -> String {
    if display_width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    for ch in text.chars() {
        let mut next = out.clone();
        next.push(ch);
        if display_width(&next) + 1 > width {
            break;
        }
        out = next;
    }
    out.push('…');
    out
}

/// Words of `text` wrapped to `width` columns (long words are cut).
pub(super) fn wrap(text: &str, width: u16) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_owned()
            } else {
                format!("{line} {word}")
            };
            if display_width(&candidate) <= width {
                line = candidate;
            } else {
                if !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                }
                line = cut(word, width);
            }
        }
        lines.push(line);
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

// ------------------------------------------------------------------ render

/// Draws the Tasks view inside `body` (the panel below the header and chip
/// lines) and stores its hits in `tasks.hits`. Reads only the cache and the
/// endpoints.
pub(super) fn render(
    tasks: &mut TasksState,
    endpoints: &[ClientShellEndpoint],
    filter: Option<&InboxFilter>,
    focused: bool,
    palette: &Palette,
    buffer: &mut Buffer,
    body: Rect,
) {
    let new = tasks.hits.new;
    tasks.hits = TasksHits {
        new,
        body,
        items: Vec::new(),
    };
    if body.width < 8 || body.height == 0 {
        return;
    }
    let layout = projects::layout();
    let scope = scope_of(filter, endpoints);
    let left = body.x + 1;
    let right = body.right().saturating_sub(1);
    tasks.columns = right.saturating_sub(left) >= 90;
    // The status line: the last body line while a message shows.
    let status = tasks.status_text().map(str::to_owned);
    let body = match &status {
        Some(text) => {
            let y = body.bottom() - 1;
            let style = Style::default().fg(palette.yellow).bg(palette.sidebar_bg);
            put(buffer, left, y, right, &cut(text, right - left), style);
            Rect::new(body.x, body.y, body.width, body.height - 1)
        }
        None => body,
    };
    if let Some(error) = &tasks.error {
        for (offset, line) in wrap(error, right - left)
            .iter()
            .enumerate()
            .take(usize::from(body.height))
        {
            let style = Style::default().fg(palette.red).bg(palette.sidebar_bg);
            put(buffer, left, body.y + offset as u16, right, line, style);
        }
        return;
    }
    if tasks.open.is_some() && tasks.detail.is_some() {
        view::draw(tasks, endpoints, &layout, focused, palette, buffer, body);
        return;
    }
    match scope {
        Scope::All => board::draw_projects(tasks, palette, buffer, body),
        Scope::Project(_) | Scope::Workspace(_) => {
            board::draw(tasks, endpoints, &layout, focused, palette, buffer, body)
        }
    }
}

// ------------------------------------------------------------------ shell

#[cfg(test)]
thread_local! {
    /// Tests: the next panel write fails with `Busy` without reaching the
    /// store.
    static FAIL_NEXT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[derive(Default)]
struct Loaded {
    version: i64,
    cards: Vec<TaskCard>,
    totals: [u32; 6],
    counts: Vec<(String, [u32; 6])>,
    decisions: Vec<OpenDecision>,
    pane_tasks: HashMap<String, String>,
    detail: Option<TaskDetail>,
    /// The one task of a workspace filter, opened directly.
    single: Option<String>,
}

impl ClientShellState {
    fn tasks_log(&mut self, _entry: String) {
        #[cfg(test)]
        self.inbox.tasks.log.push(_entry);
    }

    /// Reloads the cache when the store or the filter changed; see the
    /// module doc. `force` reads now.
    pub(super) fn refresh_tasks(&mut self, force: bool) {
        let scope = scope_of(self.inbox.filter.as_ref(), &self.endpoints);
        let now = Instant::now();
        self.poll_task_edit();
        let state = &mut self.inbox.tasks;
        if state
            .status
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= STATUS_FOR)
        {
            state.status = None;
            state.changed = true;
        }
        let key = (scope.clone(), state.open.clone());
        let moved = state.loaded.as_ref() != Some(&key);
        let urgent = force || state.dirty || moved;
        if !urgent
            && state
                .checked
                .is_some_and(|at| now.saturating_duration_since(at) < REFRESH_EVERY)
        {
            return;
        }
        state.checked = Some(now);
        if !urgent {
            match tasks::read_store(|store| store.data_version()) {
                Ok(version) if Some(version) == state.data_version => return,
                Ok(_) => {}
                Err(error) => {
                    state.error = Some(error.to_string());
                    state.changed = true;
                    return;
                }
            }
        }
        let names: Vec<String> = if scope == Scope::All {
            let layout = projects::layout();
            layout
                .display_order()
                .into_iter()
                .map(|index| layout.groups[index].name.clone())
                .collect()
        } else {
            Vec::new()
        };
        let open = state.open.clone();
        let auto_open = moved && open.is_none() && matches!(scope, Scope::Workspace(_));
        let result =
            tasks::read_store(|store| load(store, &scope, &names, open.as_deref(), auto_open));
        state.dirty = false;
        state.changed = true;
        match result {
            Ok(loaded) => {
                state.error = None;
                state.data_version = Some(loaded.version);
                state.cards = loaded.cards;
                state.totals = loaded.totals;
                state.counts = loaded.counts;
                state.decisions = loaded.decisions;
                state.pane_tasks = loaded.pane_tasks;
                if let Some(single) = loaded.single {
                    state.open = Some(single.clone());
                    state.selected = Some(single);
                    state.view_scroll = 0;
                }
                state.detail = loaded.detail;
                if state.open.is_some() && state.detail.is_none() {
                    state.close_view();
                }
                state.loaded = Some((scope, state.open.clone()));
            }
            Err(error) => state.error = Some(error.to_string()),
        }
    }

    /// One store call for the panel; errors go to the status line (and the
    /// notice for busy). A write marks the cache dirty and reloads it.
    /// `+ track`: a workspace is the task, so make one named after the
    /// filtered workspace, in its section (or Other), linked to it; its
    /// status then follows the workspace's agent.
    fn track_workspace(&mut self) {
        let Some(InboxFilter::Workspace {
            endpoint_id,
            workspace_id,
        }) = self.inbox.filter.clone()
        else {
            return;
        };
        let Some((key, label, project)) = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == endpoint_id)
            .and_then(|endpoint| {
                let snapshot = endpoint.snapshot.as_deref()?;
                let workspace = snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.workspace_id == workspace_id)?;
                let key = projects::workspace_key(endpoint, workspace);
                let paths = projects::workspace_paths(snapshot, workspace);
                let layout = projects::layout();
                // No section, or the Other group's sentinel name: "Other".
                let project = layout
                    .group_of(&key, &workspace.label, &paths)
                    .map(|group| layout.groups[group].name.clone())
                    .filter(|name| name != OTHER)
                    .unwrap_or_else(|| OTHER_PROJECT.to_owned());
                Some((key, workspace.label.clone(), project))
            })
        else {
            return;
        };
        let created = self.task_write(|store| {
            let task = store.create_task(
                &NewTask {
                    project,
                    title: Some(label),
                    ..NewTask::default()
                },
                &Actor::Human,
            )?;
            store.link_workspace(&task.display_id, Some(&key))?;
            Ok(task)
        });
        match created {
            Ok(task) => {
                self.inbox.tasks.selected = Some(task.display_id);
                self.inbox.tasks.follow = true;
                self.refresh_tasks(true);
            }
            Err(error) => self.report_task_error(&error),
        }
    }

    fn task_write<R>(
        &mut self,
        write: impl FnOnce(&TaskStore) -> StoreResult<R>,
    ) -> StoreResult<R> {
        #[cfg(test)]
        if FAIL_NEXT.with(|fail| fail.replace(false)) {
            return Err(StoreError::Busy);
        }
        let result = tasks::with_store(write);
        if result.is_ok() {
            self.inbox.tasks.dirty = true;
            self.refresh_tasks(true);
        }
        result
    }

    fn report_task_error(&mut self, error: &StoreError) {
        self.inbox.tasks.status_line(error.to_string());
        if matches!(error, StoreError::Busy) {
            self.push_task_notice("tasks db busy, try again".into());
        }
    }

    /// A write whose only answer is a status line on failure.
    fn task_apply<R>(&mut self, write: impl FnOnce(&TaskStore) -> StoreResult<R>) -> bool {
        match self.task_write(write) {
            Ok(_) => true,
            Err(error) => {
                self.report_task_error(&error);
                false
            }
        }
    }

    /// The project menu's Tasks item: the board of `project`.
    pub(super) fn open_tasks_panel(&mut self, project: String, outcome: &mut ClientShellInput) {
        self.inbox.filter = Some(InboxFilter::Project(project));
        self.inbox.view = PanelView::Tasks;
        self.inbox.tasks.close_view();
        self.inbox.tasks.scroll = 0;
        self.open_inbox_panel(outcome);
        self.refresh_tasks(true);
    }

    /// Opens the task view of `display_id`, switching project and view.
    pub(super) fn open_task_view(&mut self, display_id: &str, outcome: &mut ClientShellInput) {
        self.open_task_view_at(display_id, false, outcome);
    }

    /// A decision row of the inbox: the task view scrolled to the decision.
    pub(super) fn open_task_decision(&mut self, display_id: &str, outcome: &mut ClientShellInput) {
        self.open_task_view_at(display_id, true, outcome);
    }

    fn open_task_view_at(
        &mut self,
        display_id: &str,
        to_decision: bool,
        outcome: &mut ClientShellInput,
    ) {
        let project = tasks::read_store(|store| {
            Ok(store
                .task_detail(display_id)?
                .map(|detail| detail.project.name))
        });
        let Ok(Some(project)) = project else {
            self.push_task_notice(format!("no task {display_id}"));
            return;
        };
        self.inbox.filter = Some(InboxFilter::Project(project));
        self.inbox.view = PanelView::Tasks;
        self.open_inbox_panel(outcome);
        self.refresh_tasks(true);
        self.open_view(display_id, to_decision);
    }

    /// The task view of `id`, opened from the board.
    fn open_view(&mut self, id: &str, to_decision: bool) {
        let order = board::order_ids(&self.inbox.tasks, &projects::layout());
        let state = &mut self.inbox.tasks;
        state.close_view();
        state.open = Some(id.to_owned());
        state.selected = Some(id.to_owned());
        if !order.is_empty() {
            state.order = order;
        }
        state.view_scroll = 0;
        state.to_decision = to_decision;
        state.tab = DetailTab::Notes;
        state.desc_open = false;
        state.evidence.clear();
        state.events.clear();
        self.finish_task_edit();
        self.refresh_tasks(true);
    }

    /// Back to the board. The loaded key keeps its scope so the reload is
    /// not a move: a workspace filter with one task would reopen it.
    pub(super) fn close_task_view(&mut self) {
        self.finish_task_edit();
        let state = &mut self.inbox.tasks;
        state.close_view();
        if let Some((_, open)) = state.loaded.as_mut() {
            *open = None;
        }
        state.dirty = true;
    }

    /// `]` / `[`: the next or previous task in the order the view was opened
    /// from.
    fn step_task(&mut self, delta: isize) {
        let state = &self.inbox.tasks;
        let Some(open) = state.open.clone() else {
            return;
        };
        let Some(index) = state.order.iter().position(|id| *id == open) else {
            return;
        };
        let next = index
            .checked_add_signed(delta)
            .and_then(|i| state.order.get(i))
            .cloned();
        if let Some(next) = next {
            let order = state.order.clone();
            self.open_view(&next, false);
            self.inbox.tasks.order = order;
        }
    }

    // ---------------------------------------------------------- actions

    fn start_task(&mut self, id: &str, at: (u16, u16), outcome: &mut ClientShellInput) {
        self.tasks_log(format!("start {id}"));
        self.launch_task(id, at, outcome);
    }

    fn focus_task(&mut self, id: &str, outcome: &mut ClientShellInput) {
        let pane = self
            .inbox
            .tasks
            .card(id)
            .and_then(|card| card.live.as_ref())
            .and_then(|live| live.2.clone())
            .or_else(|| {
                let detail = self.inbox.tasks.detail.as_ref()?;
                (detail.task.display_id == id)
                    .then(|| {
                        detail
                            .attempts
                            .iter()
                            .find(|attempt| attempt.ended_at.is_none())?
                            .pane_key
                            .clone()
                    })
                    .flatten()
            });
        let Some(pane) = pane else {
            self.inbox
                .tasks
                .status_line(format!("{id} has no live agent"));
            return;
        };
        self.tasks_log(format!("focus {pane}"));
        if !self.focus_task_pane(&pane, outcome) {
            self.inbox
                .tasks
                .status_line("the pane is gone or its machine is offline");
        }
    }

    fn move_task_to(&mut self, id: &str, to: Status, note: Option<&str>) -> bool {
        let id = id.to_owned();
        let note = note.map(str::to_owned);
        self.task_apply(|store| store.move_task(&id, to, &Actor::Human, note.as_deref()))
    }

    /// A pick from a move menu: review -> ready asks for a note first.
    fn pick_move(&mut self, id: &str, current: Status, to: Status) {
        if to == current {
            return;
        }
        if current == Status::Review && to == Status::Ready {
            self.inbox
                .tasks
                .open_input(Purpose::SendBack(id.to_owned()), "");
            return;
        }
        self.move_task_to(id, to, None);
    }

    fn open_task_menu(&mut self, id: &str, menu: TaskMenu, (x, y): (u16, u16)) {
        self.overlay = Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
            target: ClientContextMenuTarget::Task {
                display_id: id.to_owned(),
                menu,
            },
            x,
            y,
            highlighted: 0,
        }));
    }

    fn task_status(&self, id: &str) -> Option<Status> {
        self.inbox
            .tasks
            .card(id)
            .map(|card| card.task.status)
            .or_else(|| {
                self.inbox
                    .tasks
                    .detail
                    .as_ref()
                    .filter(|detail| detail.task.display_id == id)
                    .map(|detail| detail.task.status)
            })
    }

    fn open_move_menu(&mut self, id: &str, at: (u16, u16)) {
        if let Some(current) = self.task_status(id) {
            self.open_task_menu(id, TaskMenu::Move { current }, at);
        }
    }

    fn open_card_menu(&mut self, id: &str, at: (u16, u16)) {
        let Some(card) = self.inbox.tasks.card(id) else {
            return;
        };
        let menu = TaskMenu::Card {
            startable: card.live.is_none() && OPEN_LANES.contains(&card.task.status),
            live: card.live.as_ref().is_some_and(|live| live.2.is_some()),
        };
        self.open_task_menu(id, menu, at);
    }

    fn copy_task_id(&mut self, id: &str, outcome: &mut ClientShellInput) {
        outcome
            .actions
            .push(ClientShellAction::ClipboardWrite(id.as_bytes().to_vec()));
        self.show_copy_feedback(Instant::now());
    }

    /// A pick from a task menu.
    pub(super) fn activate_task_menu(
        &mut self,
        display_id: String,
        menu: TaskMenu,
        action: ClientContextMenuAction,
        at: (u16, u16),
        outcome: &mut ClientShellInput,
    ) {
        use ClientContextMenuAction as Action;
        let id = display_id.as_str();
        match action {
            Action::TaskOpen => self.open_view(id, false),
            Action::TaskStart => self.start_task(id, at, outcome),
            Action::TaskFocusPane => self.focus_task(id, outcome),
            Action::TaskMoveMenu => self.open_move_menu(id, at),
            Action::TaskCopyId => self.copy_task_id(id, outcome),
            Action::TaskMove(to) => {
                if let TaskMenu::Move { current } = menu {
                    self.pick_move(id, current, to);
                }
            }
            Action::TaskKind(kind) => {
                self.task_apply(|store| {
                    let patch = TaskPatch {
                        kind: Some(kind),
                        ..TaskPatch::default()
                    };
                    store.update_task(id, &patch, &Actor::Human)
                });
            }
            Action::TaskPriority(priority) => {
                self.task_apply(|store| {
                    let patch = TaskPatch {
                        priority: Some(priority),
                        ..TaskPatch::default()
                    };
                    store.update_task(id, &patch, &Actor::Human)
                });
            }
            Action::TaskOnMachine(index) => {
                if let TaskMenu::Machine { machines } = menu {
                    if let Some((endpoint_id, _)) = machines.into_iter().nth(index) {
                        self.launch_task_on(id, endpoint_id, outcome);
                    }
                }
            }
            _ => {}
        }
        outcome.repaint = true;
    }

    /// Rules the open decision of the task view with choice `index`.
    fn rule_choice(&mut self, index: usize, outcome: &mut ClientShellInput) {
        let Some(detail) = self.inbox.tasks.detail.as_ref() else {
            return;
        };
        let Some(decision) = detail
            .decision
            .as_ref()
            .filter(|d| d.state == crate::tasks::DecisionState::Open)
        else {
            return;
        };
        let Some(choice) = decision.choices.get(index) else {
            return;
        };
        let (decision_id, choice_id, label) =
            (decision.id, choice.id.clone(), choice.label.clone());
        let id = detail.task.display_id.clone();
        self.rule(&id, decision_id, Ruling::Choice(choice_id), &label, outcome);
    }

    /// Writes a ruling, relays it when no CLI waits for it, publishes the
    /// reply file for a remote attempt, then moves on to the next task with
    /// an open decision.
    fn rule(
        &mut self,
        id: &str,
        decision_id: i64,
        ruling: Ruling,
        text: &str,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let result = self
            .task_write(|store| store.rule_decision(decision_id, &ruling, "panel", &Actor::Human));
        let decision = match result {
            Ok(decision) => decision,
            Err(error) => {
                self.report_task_error(&error);
                return false;
            }
        };
        self.tasks_log(format!("ruled {id}: {text}"));
        let now = tasks::now_text();
        if decision
            .wait_until
            .as_deref()
            .is_none_or(|until| until <= now.as_str())
        {
            self.relay(id, &format!("Decision on {id}: {text}"), outcome);
        }
        let remote = self
            .inbox
            .tasks
            .detail
            .as_ref()
            .and_then(|detail| detail.attempts.iter().find(|a| a.ended_at.is_none()))
            .is_some_and(|attempt| attempt.machine != "local");
        if remote {
            self.publish_ruling(&decision, outcome);
        }
        let state = &self.inbox.tasks;
        let start = state
            .order
            .iter()
            .position(|other| other == id)
            .unwrap_or(0);
        let next = state
            .order
            .iter()
            .skip(start + 1)
            .chain(state.order.iter().take(start))
            .find(|other| {
                state
                    .card(other)
                    .is_some_and(|card| card.open_decision && *other != id)
            })
            .cloned();
        if let Some(next) = next {
            let order = self.inbox.tasks.order.clone();
            self.open_view(&next, true);
            self.inbox.tasks.order = order;
        }
        true
    }

    /// Relays human text to the live attempt's pane (section 5.4).
    fn relay(&mut self, id: &str, text: &str, outcome: &mut ClientShellInput) {
        let live = self
            .inbox
            .tasks
            .detail
            .as_ref()
            .filter(|detail| detail.task.display_id == id)
            .and_then(|detail| detail.attempts.iter().find(|a| a.ended_at.is_none()))
            .map(|attempt| attempt.machine.clone())
            .or_else(|| {
                self.inbox
                    .tasks
                    .card(id)
                    .and_then(|card| card.live.as_ref())
                    .map(|live| live.1.clone())
            });
        let Some(machine) = live else {
            return;
        };
        self.tasks_log(format!("relay {text}"));
        if !self.relay_to_task(id, text, outcome) {
            self.inbox
                .tasks
                .status_line(format!("saved; {machine} is offline"));
        }
    }

    fn accept_task(&mut self, id: &str) {
        self.move_task_to(id, Status::Done, None);
    }

    fn card_button(&mut self, id: &str, at: (u16, u16), outcome: &mut ClientShellInput) {
        let Some(button) = self.inbox.tasks.card(id).and_then(CardButton::of) else {
            return;
        };
        match button {
            CardButton::Start => self.start_task(id, at, outcome),
            CardButton::Pane => self.focus_task(id, outcome),
            CardButton::Accept => self.accept_task(id),
        }
    }

    /// Focuses the task's workspace (`ws` on the meta line).
    fn focus_task_workspace(&mut self, outcome: &mut ClientShellInput) {
        let Some(key) = self
            .inbox
            .tasks
            .detail
            .as_ref()
            .and_then(|detail| detail.task.workspace_key.clone())
        else {
            return;
        };
        let Some((machine, rest)) = key.split_once('/') else {
            return;
        };
        let workspace_id = rest.split_once(':').map_or(rest, |(id, _)| id).to_owned();
        match self.endpoint_for_machine(machine) {
            Some(endpoint_id) => {
                self.tasks_log(format!("workspace {machine}/{workspace_id}"));
                self.focus_or_activate(
                    endpoint_id,
                    ClientEndpointFocusTarget::Workspace(workspace_id),
                    outcome,
                );
            }
            None => self
                .inbox
                .tasks
                .status_line(format!("{machine} is offline")),
        }
    }

    /// Opens an artifact (section 4.3, Artifacts).
    fn open_artifact(&mut self, artifact_id: i64, outcome: &mut ClientShellInput) {
        let Some(detail) = self.inbox.tasks.detail.as_ref() else {
            return;
        };
        let Some(artifact) = detail
            .artifacts
            .iter()
            .find(|a| a.id == artifact_id)
            .cloned()
        else {
            return;
        };
        let live_pane = detail
            .attempts
            .iter()
            .find(|a| a.ended_at.is_none())
            .and_then(|a| a.pane_key.clone());
        let document = match artifact.kind {
            ArtifactKind::Doc | ArtifactKind::Report => true,
            ArtifactKind::Link => {
                self.tasks_log(format!("url {}", artifact.target));
                outcome
                    .actions
                    .push(ClientShellAction::OpenSafeWebUrl(artifact.target.clone()));
                return;
            }
            ArtifactKind::Diff | ArtifactKind::File => artifact.target.ends_with(".md"),
        };
        if !document {
            self.copy_task_id(&artifact.target, outcome);
            return;
        }
        let machine = artifact.machine.clone().unwrap_or_else(|| "local".into());
        let Some(endpoint_id) = self.endpoint_for_machine(&machine) else {
            self.inbox
                .tasks
                .status_line(format!("{machine} is offline"));
            return;
        };
        let Some(endpoint) = self.endpoint_by_id(&endpoint_id) else {
            return;
        };
        // In the workspace of the live pane when it is on this machine, else
        // the machine's focused workspace and pane.
        let bridge = endpoint.bridge.clone();
        let snapshot = endpoint.snapshot.as_deref();
        let live = live_pane
            .as_deref()
            .and_then(|key| key.strip_prefix(&format!("{machine}/")))
            .and_then(|pane_id| {
                snapshot?
                    .agents
                    .iter()
                    .find(|agent| agent.pane_id == pane_id)
                    .map(|agent| {
                        (
                            agent.workspace_id.clone(),
                            agent.tab_id.clone(),
                            agent.pane_id.clone(),
                        )
                    })
            });
        let place = live.or_else(|| {
            let snapshot = snapshot?;
            let pane_id = snapshot.focused_pane_id.clone()?;
            let pane = snapshot.panes.iter().find(|pane| pane.pane_id == pane_id)?;
            Some((pane.workspace_id.clone(), pane.tab_id.clone(), pane_id))
        });
        let Some((workspace_id, tab_id, pane_id)) = place else {
            self.inbox
                .tasks
                .status_line(format!("{machine} has no pane to open it in"));
            return;
        };
        self.tasks_log(format!("doc {machine} {}", artifact.target));
        if endpoint_id.is_local() {
            outcome.actions.push(ClientShellAction::OpenLocalDocument {
                workspace_id,
                pane_id,
                path: artifact.target,
            });
        } else if let Some(bridge) = bridge {
            outcome.actions.push(ClientShellAction::OpenRemoteDocument {
                bridge,
                doc: crate::remote::RemoteDocOpen {
                    workspace_id,
                    tab_id,
                    pane_id,
                    cwd: None,
                    path: artifact.target,
                },
            });
        } else {
            self.inbox
                .tasks
                .status_line(format!("{machine} is offline"));
        }
    }

    /// `e`: the description in `$EDITOR`, in a split under a local pane.
    fn edit_description(&mut self, outcome: &mut ClientShellInput) {
        let Some(detail) = self.inbox.tasks.detail.as_ref() else {
            return;
        };
        let (id, body) = (detail.task.display_id.clone(), detail.task.body.clone());
        let live_local = detail
            .attempts
            .iter()
            .find(|a| a.ended_at.is_none())
            .and_then(|a| {
                a.pane_key
                    .as_deref()?
                    .strip_prefix("local/")
                    .map(str::to_owned)
            });
        let pane = live_local.or_else(|| {
            self.active_endpoint_id
                .is_local()
                .then(|| self.focused_pane_id())
                .flatten()
        });
        let Some(pane_id) = pane else {
            self.inbox.tasks.status_line("open a local pane to edit");
            return;
        };
        let dir = crate::config::state_dir()
            .join("drovr")
            .join("tasks")
            .join("edit");
        // A file whose last save was refused holds the user's text: reopen
        // it, and let its next save replace the description they were told
        // changed.
        let kept = self
            .inbox
            .tasks
            .edit
            .as_mut()
            .filter(|edit| edit.id == id && edit.kept && edit.path.exists());
        let path = if let Some(edit) = kept {
            edit.base = body;
            edit.mtime = std::fs::metadata(&edit.path)
                .and_then(|m| m.modified())
                .ok();
            let path = edit.path.clone();
            self.inbox.tasks.status_line(format!(
                "your kept text; saving replaces {id}'s description"
            ));
            path
        } else {
            let path = dir.join(format!("{id}.md"));
            let written = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, &body));
            if let Err(error) = written {
                self.inbox.tasks.status_line(format!("not saved: {error}"));
                return;
            }
            let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            self.inbox.tasks.edit = Some(EditFile {
                id,
                path: path.clone(),
                mtime,
                base: body,
                saved: false,
                kept: false,
            });
            path
        };
        outcome
            .actions
            .push(ClientShellAction::OpenLocalEditor { pane_id, path });
        self.blur_inbox();
    }

    /// Saves the `$EDITOR` file when it changed (from the tick).
    fn poll_task_edit(&mut self) {
        let Some(edit) = self.inbox.tasks.edit.as_mut() else {
            return;
        };
        let mtime = std::fs::metadata(&edit.path)
            .and_then(|m| m.modified())
            .ok();
        if mtime.is_none() || mtime == edit.mtime {
            return;
        }
        edit.mtime = mtime;
        let Ok(text) = std::fs::read_to_string(&edit.path) else {
            edit.saved = false;
            return;
        };
        let (id, base, path) = (edit.id.clone(), edit.base.clone(), edit.path.clone());
        let body = text.trim_end_matches('\n').to_owned();
        let result = self.task_write(|store| {
            let current = store
                .task(&id)?
                .ok_or_else(|| StoreError::Invalid(format!("no task {id}")))?;
            if current.body != base {
                return Err(crate::tasks::transitions::stale(&id));
            }
            let patch = TaskPatch {
                body: Some(body),
                expected_version: Some(current.version),
                ..TaskPatch::default()
            };
            store.update_task(&id, &patch, &Actor::Human)
        });
        let state = &mut self.inbox.tasks;
        let saved = result.is_ok();
        if let Some(edit) = state.edit.as_mut() {
            if let Ok(task) = &result {
                edit.base = task.body.clone();
            }
            // Keep the file on a failure: an earlier good save must not let
            // finish_task_edit delete text this save failed to store.
            edit.saved = saved;
            edit.kept = !saved;
        }
        match result {
            Ok(_) => {}
            Err(StoreError::Refused(refusal)) if refusal.code == "stale" => {
                state.status_line(format!(
                    "{id} changed while you edited; your text is in {}; e reopens it",
                    path.display()
                ));
            }
            Err(error) => {
                self.report_task_error(&error);
                let line = format!(
                    "{id} not saved: {error}; your text is in {}; e reopens it",
                    path.display()
                );
                self.inbox.tasks.status_line(line);
            }
        }
        self.inbox.tasks.changed = true;
    }

    /// Leaving the task: a saved `$EDITOR` file is removed. Ceiling: the
    /// panel cannot see the editor pane close, so the file goes when the
    /// view closes after a save, not when the editor exits; a later save in
    /// a still-open editor is not picked up.
    fn finish_task_edit(&mut self) {
        if let Some(edit) = self.inbox.tasks.edit.take() {
            if edit.saved {
                let _ = std::fs::remove_file(&edit.path);
            } else {
                self.inbox.tasks.edit = Some(edit);
            }
        }
    }

    // ---------------------------------------------------------- input

    /// Enter in the one-line input.
    fn submit_task_input(&mut self, outcome: &mut ClientShellInput) {
        let Some(input) = self.inbox.tasks.input.as_ref() else {
            return;
        };
        let text = input.editor.text().trim().to_owned();
        let purpose = input.purpose.clone();
        if text.is_empty() && !matches!(purpose, Purpose::Filter | Purpose::Title { .. }) {
            return;
        }
        let ok = match &purpose {
            Purpose::Filter => {
                self.inbox.tasks.text = (!text.is_empty()).then_some(text);
                true
            }
            Purpose::New => {
                let Some(project) = self.inbox.tasks.project().map(str::to_owned) else {
                    self.inbox.tasks.input = None;
                    return;
                };
                let (kind, priority, title) = parse_new(&text);
                match self.task_write(|store| {
                    store.create_task(
                        &NewTask {
                            project,
                            title: Some(title),
                            kind,
                            priority,
                            ..NewTask::default()
                        },
                        &Actor::Human,
                    )
                }) {
                    Ok(task) => {
                        self.inbox.tasks.selected = Some(task.display_id);
                        self.inbox.tasks.follow = true;
                        true
                    }
                    Err(error) => {
                        self.report_task_error(&error);
                        false
                    }
                }
            }
            Purpose::Title { id, version, force } => {
                let patch = TaskPatch {
                    title: Some((!text.is_empty()).then_some(text)),
                    expected_version: (!force).then_some(*version),
                    ..TaskPatch::default()
                };
                match self.task_write(|store| store.update_task(id, &patch, &Actor::Human)) {
                    Ok(_) => true,
                    Err(StoreError::Refused(refusal)) if refusal.code == "stale" => {
                        self.inbox
                            .tasks
                            .status_line(format!("{id} changed; Enter saves over it"));
                        if let Some(input) = self.inbox.tasks.input.as_mut() {
                            input.purpose = Purpose::Title {
                                id: id.clone(),
                                version: *version,
                                force: true,
                            };
                        }
                        false
                    }
                    Err(error) => {
                        self.report_task_error(&error);
                        false
                    }
                }
            }
            Purpose::AddCriterion(id) => {
                self.task_apply(|store| store.add_criterion(id, &text, &Actor::Human))
            }
            Purpose::Reply { id, decision } => {
                self.rule(id, *decision, Ruling::Text(text.clone()), &text, outcome)
            }
            Purpose::SendBack(id) => {
                let ok = self.move_task_to(id, Status::Ready, Some(&text));
                if ok {
                    self.relay(id, &format!("{id} sent back: {text}"), outcome);
                }
                ok
            }
            Purpose::Composer(id) => {
                let ok = self.task_apply(|store| {
                    store.add_entry(id, EntryKind::Human, &text, &Actor::Human)
                });
                if ok {
                    self.relay(id, &format!("you on {id}: {text}"), outcome);
                    // The composer stays open, empty, for the next note.
                    self.inbox
                        .tasks
                        .open_input(Purpose::Composer(id.clone()), "");
                    return;
                }
                false
            }
        };
        if ok
            && self
                .inbox
                .tasks
                .input
                .as_ref()
                .is_some_and(|input| input.purpose == purpose)
        {
            self.inbox.tasks.input = None;
        }
    }

    fn handle_task_input_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        outcome.repaint = true;
        if code == KeyCode::Enter && modifiers.difference(KeyModifiers::SHIFT).is_empty() {
            self.submit_task_input(outcome);
            return;
        }
        let Some(input) = self.inbox.tasks.input.as_mut() else {
            return;
        };
        match input.editor.handle_key(key) {
            EditorKey::Send => self.submit_task_input(outcome),
            EditorKey::Cancel => {
                if input.purpose == Purpose::Filter {
                    self.inbox.tasks.text = None;
                }
                self.inbox.tasks.input = None;
            }
            EditorKey::Edited => {
                if input.purpose == Purpose::Filter {
                    let text = input.editor.text();
                    self.inbox.tasks.text =
                        (!text.trim().is_empty()).then(|| text.trim().to_owned());
                    self.inbox.tasks.follow = true;
                }
            }
            EditorKey::External | EditorKey::Ignored => {}
        }
    }

    /// Text and pastes while a Tasks input is open; false when none is.
    pub(super) fn insert_task_text(&mut self, text: &str) -> bool {
        let Some(input) = self.inbox.tasks.input.as_mut() else {
            return false;
        };
        // One line: newlines in a paste become spaces.
        input.editor.insert(&text.replace(['\r', '\n'], " "));
        if input.purpose == Purpose::Filter {
            let text = input.editor.text();
            self.inbox.tasks.text = (!text.trim().is_empty()).then(|| text.trim().to_owned());
        }
        true
    }

    // ---------------------------------------------------------- keys

    /// Keys of the Tasks view; false leaves the key to the inbox's generic
    /// keys (Esc blurs, closes an overlay panel).
    pub(super) fn handle_tasks_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if self.inbox.tasks.input.is_some() {
            self.handle_task_input_key(key, outcome);
            return true;
        }
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        if !modifiers.difference(KeyModifiers::SHIFT).is_empty() {
            return false;
        }
        outcome.repaint = true;
        if self.inbox.tasks.open.is_some() && self.inbox.tasks.detail.is_some() {
            return self.view_key(code, outcome);
        }
        match scope_of(self.inbox.filter.as_ref(), &self.endpoints) {
            Scope::All => self.projects_key(code),
            Scope::Project(_) | Scope::Workspace(_) => self.board_key(code, outcome),
        }
    }

    fn board_escape(&mut self, code: KeyCode) -> bool {
        if code != KeyCode::Esc {
            return false;
        }
        if self.inbox.tasks.text.take().is_some() {
            return true;
        }
        if self.inbox.filter.take().is_some() {
            self.inbox.tasks.close_view();
            self.refresh_tasks(true);
            return true;
        }
        false
    }

    fn projects_key(&mut self, code: KeyCode) -> bool {
        let names: Vec<String> = self
            .inbox
            .tasks
            .counts
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        let state = &mut self.inbox.tasks;
        let index = state
            .selected_project
            .as_ref()
            .and_then(|name| names.iter().position(|n| n == name));
        let step = |delta: isize| {
            let next = index.map_or(0, |i| i.saturating_add_signed(delta));
            names.get(next.min(names.len().saturating_sub(1))).cloned()
        };
        match code {
            KeyCode::Char('j') | KeyCode::Down => state.selected_project = step(1),
            KeyCode::Char('k') | KeyCode::Up => state.selected_project = step(-1),
            KeyCode::Enter => {
                if let Some(name) = state
                    .selected_project
                    .clone()
                    .or_else(|| names.first().cloned())
                {
                    self.inbox.filter = Some(InboxFilter::Project(name));
                    self.refresh_tasks(true);
                }
            }
            _ => return false,
        }
        true
    }

    fn board_key(&mut self, code: KeyCode, outcome: &mut ClientShellInput) -> bool {
        let layout = projects::layout();
        let plan = board::plan(&self.inbox.tasks, &layout);
        let order: Vec<String> = plan
            .order
            .iter()
            .map(|&i| self.inbox.tasks.cards[i].task.display_id.clone())
            .collect();
        keep_selection(&mut self.inbox.tasks, &order);
        let selected = self.inbox.tasks.selected.clone();
        let at = self.card_point(selected.as_deref());
        match code {
            KeyCode::Char('j') | KeyCode::Down => self.step_selection(&order, 1),
            KeyCode::Char('k') | KeyCode::Up => self.step_selection(&order, -1),
            KeyCode::Char('h') | KeyCode::Left => {
                let next = board::sideways(&self.inbox.tasks, &plan, -1);
                self.select_card(next);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                let next = board::sideways(&self.inbox.tasks, &plan, 1);
                self.select_card(next);
            }
            KeyCode::Enter => {
                if let Some(id) = selected {
                    self.open_view(&id, false);
                }
            }
            KeyCode::Char('n') => {
                if self.inbox.tasks.project().is_some() {
                    self.inbox.tasks.open_input(Purpose::New, "");
                }
            }
            KeyCode::Char('/') => {
                let text = self.inbox.tasks.text.clone().unwrap_or_default();
                self.inbox.tasks.open_input(Purpose::Filter, &text);
            }
            KeyCode::Char('s') => {
                if let Some(id) = selected {
                    let startable = self.inbox.tasks.card(&id).is_some_and(|card| {
                        card.live.is_none() && OPEN_LANES.contains(&card.task.status)
                    });
                    if startable {
                        self.start_task(&id, at, outcome);
                    }
                }
            }
            KeyCode::Char('p') => {
                if let Some(id) = selected {
                    self.focus_task(&id, outcome);
                }
            }
            KeyCode::Char('a') => {
                if let Some(id) = selected {
                    if self.task_status(&id) == Some(Status::Review) {
                        self.accept_task(&id);
                    }
                }
            }
            KeyCode::Char('m') => {
                if let Some(id) = selected {
                    self.open_move_menu(&id, at);
                }
            }
            KeyCode::Char(' ') => {
                if let Some(status) = selected.as_deref().and_then(|id| self.task_status(id)) {
                    self.toggle_lane(status);
                }
            }
            KeyCode::Esc => return self.board_escape(code),
            _ => return false,
        }
        true
    }

    fn select_card(&mut self, id: Option<String>) {
        if let Some(id) = id {
            self.inbox.tasks.selected = Some(id);
            self.inbox.tasks.follow = true;
        }
    }

    fn step_selection(&mut self, order: &[String], delta: isize) {
        let index = self
            .inbox
            .tasks
            .selected
            .as_ref()
            .and_then(|id| order.iter().position(|other| other == id));
        let next = match index {
            Some(index) => index
                .saturating_add_signed(delta)
                .min(order.len().saturating_sub(1)),
            None => 0,
        };
        self.select_card(order.get(next).cloned());
    }

    /// Where a menu for the card `id` opens: its first line, else the panel.
    fn card_point(&self, id: Option<&str>) -> (u16, u16) {
        let hits = &self.inbox.tasks.hits;
        id.and_then(|id| {
            hits.items
                .iter()
                .find(|(_, hit)| *hit == Hit::Card(id.to_owned()))
                .map(|(rect, _)| (rect.x + 2, rect.y + 1))
        })
        .unwrap_or((hits.body.x + 2, hits.body.y + 1))
    }

    /// Folds or unfolds a lane (cancelled cards sit in Done).
    fn toggle_lane(&mut self, status: Status) {
        let lane = if status == Status::Cancelled {
            Status::Done
        } else {
            status
        };
        let index = Status::LANES.iter().position(|s| *s == lane).unwrap_or(5);
        let layout = projects::layout();
        let plan = board::plan(&self.inbox.tasks, &layout);
        let key = self.inbox.tasks.lane_key(lane);
        if plan.collapsed[index] {
            self.inbox.tasks.expanded.insert(key.clone());
            if layout.tasks.collapsed.contains(&key) {
                projects::update(|layout| layout.tasks.collapsed.retain(|entry| *entry != key));
            }
        } else {
            self.inbox.tasks.expanded.remove(&key);
            projects::update(|layout| {
                if !layout.tasks.collapsed.contains(&key) {
                    layout.tasks.collapsed.push(key);
                }
            });
        }
    }

    fn view_key(&mut self, code: KeyCode, outcome: &mut ClientShellInput) -> bool {
        let Some(detail) = self.inbox.tasks.detail.as_ref() else {
            return false;
        };
        let id = detail.task.display_id.clone();
        let status = detail.task.status;
        let decision = detail
            .decision
            .as_ref()
            .filter(|d| d.state == crate::tasks::DecisionState::Open)
            .map(|d| (d.id, d.allow_text));
        let at = (
            self.inbox.tasks.hits.body.x + 2,
            self.inbox.tasks.hits.body.y + 1,
        );
        match code {
            KeyCode::Esc => self.close_task_view(),
            KeyCode::Char('m') => self.open_move_menu(&id, at),
            KeyCode::Char('e') => self.edit_description(outcome),
            KeyCode::Char(ch @ '1'..='8') => {
                if decision.is_some() {
                    self.rule_choice(usize::from(ch as u8 - b'1'), outcome);
                }
            }
            KeyCode::Char('r') => {
                if let Some((decision, true)) = decision {
                    self.inbox
                        .tasks
                        .open_input(Purpose::Reply { id, decision }, "");
                }
            }
            KeyCode::Char('a') => {
                if status == Status::Review {
                    self.accept_task(&id);
                }
            }
            KeyCode::Char('b') => {
                if status == Status::Review {
                    self.inbox.tasks.open_input(Purpose::SendBack(id), "");
                }
            }
            KeyCode::Char('t') => {
                let state = &mut self.inbox.tasks;
                state.tab = state.tab.next();
            }
            KeyCode::Char('s') => {
                if !status.is_closed() {
                    self.start_task(&id, at, outcome);
                }
            }
            KeyCode::Char('p') => self.focus_task(&id, outcome),
            KeyCode::Char('c') => {
                self.inbox.tasks.tab = DetailTab::Notes;
                self.inbox.tasks.open_input(Purpose::Composer(id), "");
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.inbox.tasks.view_scroll = self.inbox.tasks.view_scroll.saturating_add(1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.inbox.tasks.view_scroll = self.inbox.tasks.view_scroll.saturating_sub(1);
            }
            KeyCode::Char(']') => self.step_task(1),
            KeyCode::Char('[') => self.step_task(-1),
            _ => return false,
        }
        true
    }

    // ---------------------------------------------------------- mouse

    /// Mouse events on the Tasks view; true when consumed.
    pub(super) fn handle_tasks_mouse(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let point = (mouse.column, mouse.row);
        let hits = self.inbox.tasks.hits.clone();
        let on_new = super::contains(hits.new, point);
        if !on_new && !super::contains(hits.body, point) {
            return false;
        }
        let hit = hits.at(point);
        let in_view = self.inbox.tasks.open.is_some() && self.inbox.tasks.detail.is_some();
        match mouse.kind {
            MouseEventKind::Moved => {
                let hover = match &hit {
                    Some(Hit::Card(id) | Hit::Button(id)) => Some(id.clone()),
                    _ => None,
                };
                if self.inbox.tasks.hover != hover {
                    self.inbox.tasks.hover = hover;
                    outcome.repaint = true;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let state = &mut self.inbox.tasks;
                let down = mouse.kind == MouseEventKind::ScrollDown;
                let scroll = if in_view {
                    &mut state.view_scroll
                } else {
                    &mut state.scroll
                };
                *scroll = if down {
                    scroll.saturating_add(2)
                } else {
                    scroll.saturating_sub(2)
                };
                state.follow = false;
                outcome.repaint = true;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                outcome.repaint = true;
                if on_new {
                    if self.inbox.tasks.project().is_some() {
                        // The New input is drawn on the board only.
                        if self.inbox.tasks.open.is_some() {
                            self.close_task_view();
                        }
                        self.inbox.tasks.open_input(Purpose::New, "");
                    }
                    return true;
                }
                if let Some(hit) = hit {
                    // A click elsewhere closes the input; its text comes
                    // back when the same input opens again.
                    if hit != Hit::Composer {
                        self.inbox.tasks.put_input_aside();
                    }
                    self.click_task_hit(hit, point, outcome);
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                if let Some(Hit::Card(id) | Hit::Button(id)) = hit {
                    self.inbox.tasks.selected = Some(id.clone());
                    self.open_card_menu(&id, point);
                    outcome.repaint = true;
                }
            }
            _ => {}
        }
        true
    }

    fn click_task_hit(&mut self, hit: Hit, at: (u16, u16), outcome: &mut ClientShellInput) {
        let open = self.inbox.tasks.open.clone();
        let detail_id = self
            .inbox
            .tasks
            .detail
            .as_ref()
            .map(|detail| detail.task.display_id.clone());
        let id = detail_id.clone().unwrap_or_default();
        match hit {
            Hit::Project(name) => {
                self.inbox.tasks.selected_project = Some(name.clone());
                self.inbox.filter = Some(InboxFilter::Project(name));
                self.refresh_tasks(true);
            }
            Hit::Lane(status) => self.toggle_lane(status),
            Hit::Card(card) => {
                self.inbox.tasks.selected = Some(card.clone());
                self.open_view(&card, false);
            }
            Hit::Button(card) => {
                self.inbox.tasks.selected = Some(card.clone());
                self.card_button(&card, at, outcome);
            }
            Hit::FilterClear => self.inbox.tasks.text = None,
            Hit::Track => self.track_workspace(),
            Hit::Back => self.close_task_view(),
            Hit::StatusChip => self.open_move_menu(&id, at),
            Hit::Auto => {
                let auto = self
                    .inbox
                    .tasks
                    .detail
                    .as_ref()
                    .is_some_and(|detail| detail.task.auto_status);
                self.task_apply(|store| {
                    let patch = TaskPatch {
                        auto_status: Some(!auto),
                        ..TaskPatch::default()
                    };
                    store.update_task(&id, &patch, &Actor::Human)
                });
            }
            Hit::Prev => self.step_task(-1),
            Hit::Next => self.step_task(1),
            Hit::Pane => self.focus_task(&id, outcome),
            Hit::Waiting => self.click_waiting(outcome),
            Hit::Title => {
                if let Some(detail) = self.inbox.tasks.detail.as_ref() {
                    let purpose = Purpose::Title {
                        id: id.clone(),
                        version: detail.task.version,
                        force: false,
                    };
                    let title = detail.task.title.clone().unwrap_or_default();
                    self.inbox.tasks.open_input(purpose, &title);
                }
            }
            Hit::Kind => self.open_task_menu(&id, TaskMenu::Kind, at),
            Hit::Priority => self.open_task_menu(&id, TaskMenu::Priority, at),
            Hit::Workspace => self.focus_task_workspace(outcome),
            Hit::More => self.inbox.tasks.desc_open = !self.inbox.tasks.desc_open,
            Hit::Edit => self.edit_description(outcome),
            Hit::Mark(position) => {
                let state = self.inbox.tasks.detail.as_ref().and_then(|detail| {
                    detail
                        .criteria
                        .iter()
                        .find(|c| c.position == position)
                        .map(|c| c.state)
                });
                if let Some(state) = state {
                    let next = match state {
                        CheckState::Open => CheckState::Passed,
                        CheckState::Passed => CheckState::Failed,
                        CheckState::Failed => CheckState::Open,
                    };
                    self.task_apply(|store| {
                        store.check_criterion(&id, position, next, None, &Actor::Human)
                    });
                }
            }
            Hit::Criterion(position) => {
                let evidence = &mut self.inbox.tasks.evidence;
                if !evidence.remove(&position) {
                    evidence.insert(position);
                }
            }
            Hit::AddCriterion => self.inbox.tasks.open_input(Purpose::AddCriterion(id), ""),
            Hit::Choice(index) => self.rule_choice(index, outcome),
            Hit::Reply => {
                let decision = self
                    .inbox
                    .tasks
                    .detail
                    .as_ref()
                    .and_then(|detail| detail.decision.as_ref())
                    .map(|d| d.id);
                if let Some(decision) = decision {
                    self.inbox
                        .tasks
                        .open_input(Purpose::Reply { id, decision }, "");
                }
            }
            Hit::Accept => self.accept_task(&id),
            Hit::SendBack => self.inbox.tasks.open_input(Purpose::SendBack(id), ""),
            Hit::TakeOver => {
                self.move_task_to(&id, Status::Working, None);
            }
            Hit::Tab(tab) => self.inbox.tasks.tab = tab,
            Hit::Events(first) => {
                let events = &mut self.inbox.tasks.events;
                if !events.remove(&first) {
                    events.insert(first);
                }
            }
            Hit::AttemptPane(pane) => {
                self.tasks_log(format!("focus {pane}"));
                if !self.focus_task_pane(&pane, outcome) {
                    self.inbox
                        .tasks
                        .status_line("the pane is gone or its machine is offline");
                }
            }
            Hit::Release => {
                self.task_apply(|store| store.release(&id, "released by you", &Actor::Human));
            }
            Hit::Artifact(artifact) => self.open_artifact(artifact, outcome),
            Hit::Start => self.start_task(&id, at, outcome),
            Hit::Archive => {
                let archived = self.task_apply(|store| {
                    let patch = TaskPatch {
                        archived: Some(true),
                        ..TaskPatch::default()
                    };
                    store.update_task(&id, &patch, &Actor::Human)
                });
                if archived {
                    self.close_task_view();
                }
            }
            Hit::Composer => {
                if open.is_some()
                    && !matches!(
                        self.inbox.tasks.input.as_ref().map(|input| &input.purpose),
                        Some(Purpose::Composer(_))
                    )
                {
                    self.inbox.tasks.open_input(Purpose::Composer(id), "");
                }
            }
        }
    }

    /// The waiting line: a permission opens the Inbox view on its item; a
    /// decision scrolls to the decision card.
    fn click_waiting(&mut self, outcome: &mut ClientShellInput) {
        let Some(detail) = self.inbox.tasks.detail.as_ref() else {
            return;
        };
        let pane = detail
            .attempts
            .iter()
            .find(|a| a.ended_at.is_none())
            .and_then(|a| a.pane_key.clone());
        let layout = projects::layout();
        let waiting = pane
            .as_deref()
            .filter(|key| pane_waiting(&self.endpoints, &layout, key).is_some())
            .and_then(|key| find_agent(&self.endpoints, key))
            .map(|(endpoint, agent)| super::inbox::ItemKey {
                endpoint_id: endpoint.endpoint_id.clone(),
                pane_id: agent.pane_id.clone(),
            });
        match waiting {
            Some(key) => {
                self.tasks_log(format!("inbox {}", key.pane_id));
                self.inbox.view = PanelView::Inbox;
                self.inbox.filter = None;
                self.show_inbox_item(key);
                outcome.repaint = true;
            }
            None => self.inbox.tasks.to_decision = true,
        }
    }
}

/// Every read of one refresh, in one closure (one deferred read with A's
/// store).
fn load(
    store: &TaskStore,
    scope: &Scope,
    names: &[String],
    open: Option<&str>,
    auto_open: bool,
) -> StoreResult<Loaded> {
    let mut loaded = Loaded {
        version: store.data_version()?,
        ..Loaded::default()
    };
    let shown = |filter: TaskFilter| TaskFilter {
        done_limit: Some(DONE_LIMIT),
        ..filter
    };
    match scope {
        Scope::Project(name) => {
            loaded.cards = store.list(&shown(TaskFilter {
                project: Some(name.clone()),
                ..TaskFilter::default()
            }))?;
            loaded.totals = store.lane_counts(name)?;
        }
        Scope::Workspace(prefix) => {
            loaded.cards = store.list(&shown(TaskFilter {
                workspace_key: Some(prefix.clone()),
                ..TaskFilter::default()
            }))?;
            for card in &loaded.cards {
                loaded.totals[board::lane_of(card.task.status)] += 1;
            }
        }
        Scope::All => {
            for name in names {
                loaded.counts.push((name.clone(), store.lane_counts(name)?));
            }
        }
    }
    loaded.decisions = store.open_decisions(None)?;
    for card in store.list(&TaskFilter {
        statuses: OPEN_LANES.to_vec(),
        ..TaskFilter::default()
    })? {
        if let Some(pane) = card.live.and_then(|live| live.2) {
            loaded.pane_tasks.insert(pane, card.task.display_id);
        }
    }
    let open = match (open, auto_open, loaded.cards.as_slice()) {
        (Some(open), _, _) => Some(open.to_owned()),
        (None, true, [one]) => {
            loaded.single = Some(one.task.display_id.clone());
            loaded.single.clone()
        }
        _ => None,
    };
    if let Some(open) = open {
        loaded.detail = store.task_detail(&open)?;
    }
    Ok(loaded)
}

#[cfg(test)]
mod tests;
