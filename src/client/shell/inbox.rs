//! drovr fork: the inbox panel (docs/design/inbox-pane.md, sections 3 to 7).
//!
//! The client draws the inbox on the right of the screen, like the sidebar:
//! it is not a herdr pane and runs no process on any server. It lists one
//! item per agent pane that needs you, on every machine, built by
//! [`agent_item`] from the same signal the sidebar glyphs use. Dismiss and
//! snooze marks are pane tokens of source `drovr-inbox` on the agent's
//! server, so every client shows the same inbox; mutes, the stuck threshold,
//! grouping and the panel width are client preferences in `sidebar.toml`.
//!
//! Answers (section 9) go through `inbox_answer`: hook decisions for Claude
//! permissions and notes, option keys after screen checks for questions,
//! plan approval and Codex permissions, and `agent.prompt` replies. Notes and
//! replies use the multi-line editor in `inbox_editor`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use serde::{Deserialize, Serialize};

use super::agent_signal::{self, AgentSignal, InboxFilter, ItemKind};
use super::inbox_answer::{self as answer, Action, Choice, Decision};
use super::inbox_editor::{EditorKey, NoteEditor};
use super::projects::{self, ProjectLayout, OTHER};
use super::render::{display_width, put_text};
use super::*;
use crate::protocol::ClientShellAgent;

/// Default panel width, as a share of the screen.
pub(super) const DEFAULT_WIDTH_SHARE: f64 = 0.4;
/// The panel is never narrower than this.
pub(super) const MIN_WIDTH: u16 = 48;
/// Below this many columns left for panes, the panel opens over them.
pub(super) const MIN_PANES_WIDTH: u16 = 32;
/// Below this width an item's chip and age move to a second line.
const NARROW_WIDTH: u16 = 60;
/// Keys that arrive this soon after the panel gains focus are dropped, so a
/// key aimed at an agent pane cannot act on an item.
pub(super) const FOCUS_DROP: Duration = Duration::from_millis(250);
const DEFAULT_STUCK_MINUTES: u64 = 10;
/// Metadata source of the server marks.
const MARK_SOURCE: &str = "drovr-inbox";
/// `drovr_dis`: `<state_change_seq>|<kind>`, the item dismissed.
const DISMISS_TOKEN: &str = "drovr_dis";
/// `drovr_snz`: `<end unix s>|<state_change_seq>|<request id>`.
const SNOOZE_TOKEN: &str = "drovr_snz";
/// A mark sent but not yet seen in a snapshot hides its item locally for at
/// most this long; past it, a mark that never landed shows the item again.
const PENDING_MARK_TTL: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Screen lines kept from `pane.read` for an expanded item.
const SCREEN_LINES: usize = 30;
/// Plan lines shown in an expanded plan item.
const PLAN_LINES: usize = 20;
/// A screen read that confirmed no answer is read again after this long
/// (the dialog may be drawn after the hook published the request).
const SCREEN_RETRY: Duration = Duration::from_secs(2);

/// `[inbox]` in `sidebar.toml`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub(super) struct InboxSettings {
    /// Panel width as a share of the screen (default 0.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) width: Option<f64>,
    /// Minutes without a hook event before a working agent is stuck
    /// (default 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) stuck_minutes: Option<u64>,
    /// Muted workspaces (`machine/id:label`): their done, asks, stuck and
    /// limit items are hidden and raise no toast.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) muted: Vec<String>,
    /// Group items by project.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) grouped: bool,
    /// Stuck threshold per workspace, keyed `machine/workspace` as in
    /// `hidden`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) stuck_minutes_by_workspace: BTreeMap<String, u64>,
}

impl InboxSettings {
    pub(super) fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub(super) fn stuck_secs(&self, workspace_key: &str) -> u64 {
        let minutes = self
            .stuck_minutes_by_workspace
            .iter()
            .find(|(entry, _)| projects::same_workspace(entry, workspace_key))
            .map(|(_, minutes)| *minutes)
            .or(self.stuck_minutes)
            .unwrap_or(DEFAULT_STUCK_MINUTES);
        minutes.max(1) * 60
    }

    pub(super) fn is_muted(&self, workspace_key: &str) -> bool {
        self.muted
            .iter()
            .any(|entry| projects::same_workspace(entry, workspace_key))
    }

    fn toggle_mute(&mut self, workspace_key: &str) {
        if self.is_muted(workspace_key) {
            self.muted
                .retain(|entry| !projects::same_workspace(entry, workspace_key));
        } else {
            self.muted.push(workspace_key.to_owned());
        }
    }

    fn share(&self) -> f64 {
        self.width
            .filter(|share| share.is_finite() && *share > 0.0 && *share < 1.0)
            .unwrap_or(DEFAULT_WIDTH_SHARE)
    }
}

/// Kinds a mute hides.
fn mutable(kind: ItemKind) -> bool {
    matches!(
        kind,
        ItemKind::Finished | ItemKind::Asks | ItemKind::Stuck | ItemKind::Limit
    )
}

/// Where the panel goes: `(panes width, panel rect, over the panes)`. `x` and
/// `width` are the columns right of the sidebar; `cols` is the screen width.
pub(super) fn place(cols: u16, rows: u16, x: u16, width: u16, share: f64) -> (u16, Rect, bool) {
    let wanted = ((f64::from(cols) * share).round() as u16).max(MIN_WIDTH);
    if width >= wanted.saturating_add(MIN_PANES_WIDTH) {
        let panes = width - wanted;
        return (panes, Rect::new(x + panes, 0, wanted, rows), false);
    }
    let panel = wanted.min(width);
    (width, Rect::new(x + width - panel, 0, panel, rows), true)
}

/// The panel share for a left border dragged to `column`.
fn share_at(cols: u16, x: u16, width: u16, column: u16) -> f64 {
    let right = x.saturating_add(width);
    let max = if width >= MIN_WIDTH + MIN_PANES_WIDTH {
        width - MIN_PANES_WIDTH
    } else {
        width
    };
    let panel = right.saturating_sub(column).clamp(MIN_WIDTH.min(max), max);
    let share = f64::from(panel) / f64::from(cols.max(1));
    (share * 100.0).round() / 100.0
}

// ------------------------------------------------------------------ marks

/// A mark this client sent, applied locally until the snapshot carries it.
#[derive(Clone, Debug)]
struct PendingMark {
    endpoint_id: ClientEndpointId,
    pane_id: String,
    token: &'static str,
    value: String,
    at: Instant,
}

fn pending_marks() -> &'static Mutex<Vec<PendingMark>> {
    static MARKS: OnceLock<Mutex<Vec<PendingMark>>> = OnceLock::new();
    MARKS.get_or_init(Default::default)
}

/// A mark token of `agent`: a pending local value wins over the server's.
fn mark_token(
    endpoint: &ClientShellEndpoint,
    agent: &ClientShellAgent,
    token: &str,
) -> Option<String> {
    let mut marks = pending_marks().lock().unwrap_or_else(|e| e.into_inner());
    let server = projects::agent_token(agent, token);
    marks.retain(|mark| {
        mark.at.elapsed() < PENDING_MARK_TTL
            && !(mark.endpoint_id == endpoint.endpoint_id
                && mark.pane_id == agent.pane_id
                && mark.token == token
                && server == Some(mark.value.as_str()))
    });
    marks
        .iter()
        .rev()
        .find(|mark| {
            mark.endpoint_id == endpoint.endpoint_id
                && mark.pane_id == agent.pane_id
                && mark.token == token
        })
        .map(|mark| mark.value.clone())
        .or_else(|| server.map(str::to_owned))
}

/// The pending request id in `drovr_wait`, empty without one.
fn wait_id(agent: &ClientShellAgent) -> &str {
    projects::agent_token(agent, "drovr_wait")
        .and_then(|value| value.split('|').nth(1))
        .unwrap_or_default()
}

/// The kind name in a `drovr_dis` mark.
fn kind_tag(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Permission => "permission",
        ItemKind::Question => "question",
        ItemKind::Plan => "plan",
        ItemKind::Asks => "asks",
        ItemKind::Dialog => "dialog",
        ItemKind::Stuck => "stuck",
        ItemKind::Limit => "limit",
        ItemKind::Exited => "exited",
        ItemKind::Denied => "denied",
        ItemKind::Finished => "done",
    }
}

/// Whether the server marks of `agent` hide an item of `kind` now.
fn marked(
    dismissed: Option<&str>,
    snoozed: Option<&str>,
    kind: ItemKind,
    seq: u64,
    wait: &str,
    now: u64,
) -> bool {
    // A working agent keeps one seq for the whole turn and can show stuck,
    // then limit: the kind keeps a dismissed one from hiding the other. A
    // mark without a kind (older clients) hides any kind of that seq.
    let dismissed = !kind.waiting()
        && dismissed.is_some_and(|value| {
            let (at, tag) = value
                .trim()
                .split_once('|')
                .map_or((value.trim(), None), |(at, tag)| (at, Some(tag)));
            at.parse::<u64>().ok() == Some(seq) && tag.is_none_or(|tag| tag == kind_tag(kind))
        });
    let snoozed = snoozed.is_some_and(|value| {
        let mut fields = value.split('|');
        let end = fields.next().and_then(|end| end.parse::<u64>().ok());
        let at = fields.next().and_then(|at| at.parse::<u64>().ok());
        let request = fields.next().unwrap_or_default();
        end.is_some_and(|end| now < end) && at == Some(seq) && request == wait
    });
    dismissed || snoozed
}

/// The item `agent` makes, and whether marks or a mute hide it. Every rule
/// that decides what the sidebar and the inbox show lives here.
fn classify(
    layout: &ProjectLayout,
    endpoint: &ClientShellEndpoint,
    workspace_key: &str,
    agent: &ClientShellAgent,
    signal: &AgentSignal,
    now: u64,
) -> Option<(ItemKind, bool)> {
    let key = projects::agent_key(endpoint, &agent.pane_id);
    let kind = signal.item(
        agent.agent_status,
        now,
        layout.inbox.stuck_secs(workspace_key),
        layout.is_dismissed(&key, agent.state_change_seq),
    )?;
    let hidden = marked(
        mark_token(endpoint, agent, DISMISS_TOKEN).as_deref(),
        mark_token(endpoint, agent, SNOOZE_TOKEN).as_deref(),
        kind,
        agent.state_change_seq,
        wait_id(agent),
        now,
    ) || (mutable(kind) && layout.inbox.is_muted(workspace_key));
    Some((kind, hidden))
}

/// The workspace key of `agent`'s workspace (`machine/id:label`).
fn agent_workspace_key(endpoint: &ClientShellEndpoint, agent: &ClientShellAgent) -> String {
    endpoint
        .snapshot
        .as_deref()
        .and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == agent.workspace_id)
        })
        .map_or_else(
            || format!("{}/{}", projects::machine_key(endpoint), agent.workspace_id),
            |workspace| projects::workspace_key(endpoint, workspace),
        )
}

/// The inbox item kind `agent` shows in the sidebar and the inbox, after the
/// workspace's stuck threshold, marks and mutes.
pub(super) fn agent_item(
    layout: &ProjectLayout,
    endpoint: &ClientShellEndpoint,
    agent: &ClientShellAgent,
    now: u64,
) -> Option<ItemKind> {
    let signal = AgentSignal::parse(agent);
    let workspace_key = agent_workspace_key(endpoint, agent);
    classify(layout, endpoint, &workspace_key, agent, &signal, now)
        .and_then(|(kind, hidden)| (!hidden).then_some(kind))
}

/// Whether a toast for `pane_id` on `endpoint` is muted (a finished turn in a
/// muted workspace).
pub(super) fn toast_muted(endpoint: &ClientShellEndpoint, pane_id: &str) -> bool {
    let layout = projects::layout();
    if layout.inbox.muted.is_empty() {
        return false;
    }
    endpoint
        .snapshot
        .as_deref()
        .and_then(|snapshot| {
            snapshot
                .agents
                .iter()
                .find(|agent| agent.pane_id == pane_id)
        })
        .is_some_and(|agent| layout.inbox.is_muted(&agent_workspace_key(endpoint, agent)))
}

// ------------------------------------------------------------------ items

/// One agent pane on one machine.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ItemKey {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) pane_id: String,
}

#[derive(Clone, Debug)]
pub(super) struct Item {
    pub(super) key: ItemKey,
    pub(super) kind: ItemKind,
    pub(super) seq: u64,
    wait_id: String,
    pub(super) workspace_id: String,
    tab_id: String,
    pub(super) workspace: String,
    workspace_key: String,
    /// Section key: a project name or [`OTHER`].
    pub(super) project: String,
    /// Position of the section in the sidebar, for grouping.
    project_rank: usize,
    machine: String,
    /// The agent, when it is not claude.
    vendor: Option<String>,
    /// Claude: the drovr-state-hook waits for a decision file.
    decides: bool,
    pub(super) summary: String,
    /// The request text in `drovr_wait` (cut at 80 characters).
    wait_text: String,
    /// Question option labels (`drovr_o1`-`drovr_o4`).
    options: Vec<String>,
    /// Token facts for the expanded detail.
    facts: Vec<String>,
    /// Seconds since the item began; `None` when unknown (sorts as oldest).
    pub(super) age: Option<u64>,
    /// Hidden by a mark; listed only while it is the sticky selection.
    marked: bool,
}

impl Item {
    fn chip(&self) -> String {
        if self.project == OTHER {
            self.machine.to_lowercase()
        } else {
            format!("{}·{}", self.project, self.machine.to_lowercase())
        }
    }
}

fn token<'a>(agent: &'a ClientShellAgent, name: &str) -> Option<&'a str> {
    projects::agent_token(agent, name)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// The one-line text of an item.
fn summary(
    kind: ItemKind,
    agent: &ClientShellAgent,
    signal: &AgentSignal,
    title: &str,
    now: u64,
) -> String {
    let last = token(agent, "drovr_last");
    let text = match kind {
        ItemKind::Permission | ItemKind::Question | ItemKind::Plan => {
            let mut fields = token(agent, "drovr_wait")
                .unwrap_or_default()
                .splitn(4, '|');
            let (_, _, sub, text) = (fields.next(), fields.next(), fields.next(), fields.next());
            let text = text.filter(|text| !text.is_empty()).unwrap_or(match kind {
                ItemKind::Permission => "permission request",
                ItemKind::Question => "question",
                _ => "plan",
            });
            if sub.is_some_and(|sub| !sub.is_empty()) {
                format!("subagent · {text}")
            } else {
                text.to_owned()
            }
        }
        ItemKind::Dialog => format!("dialog · {title}"),
        ItemKind::Stuck => signal.since().map_or_else(
            || "no activity".to_owned(),
            |since| {
                format!(
                    "no activity for {}",
                    agent_signal::format_elapsed(now.saturating_sub(since))
                )
            },
        ),
        ItemKind::Limit => projects::agent_context_tokens(agent).map_or_else(
            || last.unwrap_or("context or rate limit").to_owned(),
            |ctx| format!("context {}", projects::format_tokens(ctx)),
        ),
        ItemKind::Exited => last.map_or_else(
            || "session ended".to_owned(),
            |last| format!("ended · {last}"),
        ),
        ItemKind::Denied => {
            last.map_or_else(|| "denied".to_owned(), |last| format!("denied · {last}"))
        }
        ItemKind::Asks | ItemKind::Finished => last.unwrap_or(title).to_owned(),
    };
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Every inbox item on every online machine, in display order. `keep` stays
/// listed (marked) while a mark hides it, so a second `z` can cycle it.
pub(super) fn collect(
    endpoints: &[ClientShellEndpoint],
    layout: &ProjectLayout,
    now: u64,
    keep: Option<&ItemKey>,
) -> Vec<Item> {
    let (sections, claimed) = projects::sections(layout, endpoints);
    let mut placed = HashMap::new();
    for (rank, section) in sections.iter().enumerate() {
        let name = &layout.groups[section.group].name;
        for member in &section.members {
            placed.insert(
                (member.endpoint, member.index),
                (name.clone(), rank, member.hidden),
            );
        }
    }
    let other_rank = sections.len();
    let mut items = Vec::new();
    for (endpoint_index, endpoint) in endpoints.iter().enumerate() {
        if endpoint.status != ClientEndpointStatus::Online {
            continue;
        }
        let Some(snapshot) = endpoint.snapshot.as_deref() else {
            continue;
        };
        for agent in &snapshot.agents {
            let Some(index) = snapshot
                .workspaces
                .iter()
                .position(|workspace| workspace.workspace_id == agent.workspace_id)
            else {
                continue;
            };
            let workspace = &snapshot.workspaces[index];
            let workspace_key = projects::workspace_key(endpoint, workspace);
            let (project, project_rank, hidden) = placed
                .get(&(endpoint_index, index))
                .cloned()
                .unwrap_or_else(|| {
                    debug_assert!(!claimed.contains(&(endpoint_index, index)));
                    (
                        OTHER.to_owned(),
                        other_rank,
                        layout.is_hidden(&workspace_key),
                    )
                });
            if hidden {
                continue;
            }
            let signal = AgentSignal::parse(agent);
            let Some((kind, marked)) =
                classify(layout, endpoint, &workspace_key, agent, &signal, now)
            else {
                continue;
            };
            let key = ItemKey {
                endpoint_id: endpoint.endpoint_id.clone(),
                pane_id: agent.pane_id.clone(),
            };
            if marked && keep != Some(&key) {
                continue;
            }
            let pane = snapshot
                .panes
                .iter()
                .find(|pane| pane.pane_id == agent.pane_id);
            let tab = snapshot.tabs.iter().find(|tab| tab.tab_id == agent.tab_id);
            let title = super::drovr_sidebar::agent_title(agent, pane, tab);
            let options = if kind == ItemKind::Question {
                (1..=4)
                    .filter_map(|n| token(agent, &format!("drovr_o{n}")).map(str::to_owned))
                    .collect()
            } else {
                Vec::new()
            };
            let mut facts = Vec::new();
            if let Some(diff) = token(agent, "drovr_diff") {
                facts.push(diff.to_owned());
            }
            if let Some(doing) = token(agent, "drovr_doing") {
                facts.push(format!("running {doing}"));
            }
            if kind.waiting() {
                if let Some(last) = token(agent, "drovr_last") {
                    facts.push(format!("last message: {last}"));
                }
            }
            facts.push(title.clone());
            let since_change = projects::idle_secs(&projects::agent_key(endpoint, &agent.pane_id));
            let age = if kind.waiting() || kind == ItemKind::Finished && signal.since().is_none() {
                since_change
            } else {
                signal
                    .since()
                    .map(|since| now.saturating_sub(since))
                    .or(since_change)
            };
            items.push(Item {
                summary: summary(kind, agent, &signal, &title, now),
                key,
                kind,
                seq: agent.state_change_seq,
                wait_id: wait_id(agent).to_owned(),
                workspace_id: workspace.workspace_id.clone(),
                tab_id: agent.tab_id.clone(),
                workspace: workspace.label.clone(),
                workspace_key,
                project: if project == OTHER {
                    OTHER.to_owned()
                } else {
                    project
                },
                project_rank,
                machine: endpoint.label.clone(),
                vendor: agent.agent.clone().filter(|vendor| vendor != "claude"),
                decides: agent.agent.as_deref() == Some("claude"),
                wait_text: token(agent, "drovr_wait")
                    .and_then(|wait| wait.splitn(4, '|').nth(3))
                    .unwrap_or_default()
                    .to_owned(),
                options,
                facts,
                age,
                marked,
            });
        }
    }
    sort(&mut items, layout.inbox.grouped);
    items
}

/// Kind order (section 2), then oldest first; grouped, by project first.
/// An unknown age counts as the oldest.
pub(super) fn sort(items: &mut [Item], grouped: bool) {
    items.sort_by(|a, b| {
        let group = |item: &Item| if grouped { item.project_rank } else { 0 };
        group(a)
            .cmp(&group(b))
            .then(a.kind.cmp(&b.kind))
            .then(b.age.unwrap_or(u64::MAX).cmp(&a.age.unwrap_or(u64::MAX)))
            .then_with(|| a.workspace.cmp(&b.workspace))
            .then_with(|| a.key.pane_id.cmp(&b.key.pane_id))
    });
}

// ------------------------------------------------------------------ state

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum InboxTab {
    #[default]
    Waiting,
    Done,
    All,
}

impl InboxTab {
    fn next(self) -> Self {
        match self {
            Self::Waiting => Self::Done,
            Self::Done => Self::All,
            Self::All => Self::Waiting,
        }
    }

    fn admits(self, kind: ItemKind) -> bool {
        match self {
            Self::Waiting => kind.waiting(),
            Self::Done => !kind.waiting(),
            Self::All => true,
        }
    }
}

/// The item a read or an answer belongs to: a pane, its state and request.
type ItemAt = (ItemKey, u64, String);

fn item_at(item: &Item) -> ItemAt {
    (item.key.clone(), item.seq, item.wait_id.clone())
}

/// Screen text read for a waiting item, for the answer checks
/// that decide which keys it shows and for its detail; shown, never stored.
#[derive(Clone, Debug)]
struct ScreenRead {
    at: ItemAt,
    text: Result<String, String>,
}

/// The plan file of a plan item, read on the agent's machine.
#[derive(Clone, Debug)]
struct PlanRead {
    at: ItemAt,
    /// `None` while the read runs; then `(path, text)` or the error.
    result: Option<Result<(String, String), String>>,
    /// `p`: open it in the doc pane once read.
    open: bool,
}

/// An answer sent and not yet replaced by a new state: its item is hidden.
#[derive(Clone, Debug)]
struct Answering {
    seq: u64,
    wait_id: String,
    /// When the machine confirmed it; the item may show again after
    /// [`PENDING_MARK_TTL`] if the snapshot never changes.
    done_at: Option<Instant>,
}

/// What the editor's text becomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Purpose {
    /// A hook `deny` with the text as its message: `r` on a permission or a
    /// plan, `o` on a question.
    Note,
    /// `agent.prompt`: `r` on a finished, asks or denied item.
    Reply,
}

/// The editor open in an item.
#[derive(Debug)]
struct Compose {
    at: ItemAt,
    purpose: Purpose,
    editor: NoteEditor,
    /// `ctrl+e`: the temporary file `$EDITOR` edits, and its last seen
    /// modification time; the editor reloads it when it changes.
    external: Option<(std::path::PathBuf, Option<std::time::SystemTime>)>,
}

/// What an answer key does.
#[derive(Clone, Debug, PartialEq)]
enum KeyAct {
    Send(Action),
    Compose(Purpose),
    ReadPlan,
}

/// One answer key of an item: `key`, its label, what it does, and whether
/// it grants a lasting permission (a second press confirms it).
#[derive(Clone, Debug, PartialEq)]
struct AnswerKey {
    key: char,
    label: String,
    act: KeyAct,
    grant: bool,
}

#[derive(Clone, Debug, Default)]
pub(super) struct InboxHits {
    area: Rect,
    border: Rect,
    tabs: Vec<(Rect, InboxTab)>,
    group: Rect,
    chip: Rect,
    list: Rect,
    rows: Vec<(Rect, ItemKey)>,
    jumps: Vec<(Rect, ItemKey)>,
    closes: Vec<(Rect, ItemKey)>,
    /// Answer key labels: a click acts as the key.
    keys: Vec<(Rect, ItemKey, char)>,
    /// "more" / "less" labels: a click shows or hides the detail.
    details: Vec<(Rect, ItemKey)>,
    /// Screen width and the columns right of the sidebar, for dragging.
    cols: u16,
    main: (u16, u16),
    /// Drawn over the panes (narrow screen); Esc closes it.
    overlay: bool,
}

#[derive(Debug, Default)]
pub(crate) struct InboxState {
    pub(super) open: bool,
    pub(super) focused: bool,
    focused_at: Option<Instant>,
    pub(super) tab: InboxTab,
    pub(super) filter: Option<InboxFilter>,
    pub(super) selected: Option<ItemKey>,
    /// `space`: the selected item shows its detail.
    detail: bool,
    /// The `(seq, wait id)` the detail was opened for; a new state or
    /// prompt closes it.
    detail_for: Option<(u64, String)>,
    scroll: usize,
    /// A just-snoozed item stays listed until the selection moves.
    sticky: Option<ItemKey>,
    snooze_step: usize,
    dragging: bool,
    /// Width share while the border is dragged (saved on release).
    width: Option<f64>,
    hover: Option<ItemKey>,
    /// The list scrolls to keep the selection in view, until the wheel
    /// scrolls it.
    follow: bool,
    /// Screen reads of the waiting items, so each shows its answers.
    screen: HashMap<ItemKey, ScreenRead>,
    /// The last screen read requested per item, and when.
    screen_asked: HashMap<ItemKey, (ItemAt, Instant)>,
    plan: Option<PlanRead>,
    show_keys: bool,
    hits: InboxHits,
    /// `prefix a` opened the inbox: an answer gives focus back to the panes.
    return_focus: bool,
    /// One answer at a time per pane.
    answering: HashMap<ItemKey, Answering>,
    /// Items whose answer failed a check: their answer keys are removed
    /// until the state or request changes.
    stale: HashMap<ItemKey, (u64, String)>,
    /// A grant key pressed once; the same key again sends it.
    confirm: Option<(ItemAt, char)>,
    compose: Option<Compose>,
    /// Editor text kept per item after `esc` or a failed send.
    drafts: HashMap<ItemKey, String>,
    /// The `$EDITOR` file of a closed editor: `$EDITOR` may still run, so
    /// the file stays and the editor reloads it when it opens again.
    externals: HashMap<ItemKey, (std::path::PathBuf, Option<std::time::SystemTime>)>,
}

impl InboxState {
    /// The panel width share to lay out, or `None` while closed.
    pub(super) fn share(&self, layout: &ProjectLayout) -> Option<f64> {
        self.open
            .then(|| self.width.unwrap_or_else(|| layout.inbox.share()))
    }

    /// The panel is closed: nothing of it is clickable.
    pub(super) fn clear_hits(&mut self) {
        self.hits = InboxHits::default();
    }

    fn focus(&mut self, now: Instant) {
        if !self.focused {
            self.focused = true;
            self.focused_at = Some(now);
        }
    }

    /// The terminal regained focus: keys right after it may be aimed at a
    /// pane, so the drop window starts again.
    pub(super) fn outer_focus_gained(&mut self, now: Instant) {
        if self.focused {
            self.focused_at = Some(now);
        }
    }

    /// Whether `point` is on the open panel.
    pub(super) fn contains(&self, point: (u16, u16)) -> bool {
        self.open && super::contains(self.hits.area, point)
    }

    /// Whether a key at `now` may act: not within [`FOCUS_DROP`] of gaining
    /// focus.
    pub(super) fn accepts_key(&self, now: Instant) -> bool {
        self.focused
            && self
                .focused_at
                .is_none_or(|at| now.saturating_duration_since(at) >= FOCUS_DROP)
    }

    /// An answer for this exact item is in flight or just landed.
    fn answered(&self, item: &Item) -> bool {
        self.answering
            .get(&item.key)
            .is_some_and(|answer| answer.seq == item.seq && answer.wait_id == item.wait_id)
    }

    fn is_stale(&self, item: &Item) -> bool {
        self.stale.get(&item.key) == Some(&(item.seq, item.wait_id.clone()))
    }

    /// The screen read for exactly this item, if it arrived.
    fn screen_of(&self, item: &Item) -> Option<&Result<String, String>> {
        self.screen
            .get(&item.key)
            .filter(|screen| screen.at == item_at(item))
            .map(|screen| &screen.text)
    }

    fn plan_of(&self, item: &Item) -> Option<&PlanRead> {
        self.plan.as_ref().filter(|plan| plan.at == item_at(item))
    }

    fn admits(&self, item: &Item) -> bool {
        if self.answered(item) {
            return false;
        }
        if !self.tab.admits(item.kind) && self.sticky.as_ref() != Some(&item.key) {
            return false;
        }
        match &self.filter {
            None => true,
            Some(InboxFilter::Project(project)) => &item.project == project,
            Some(InboxFilter::Workspace {
                endpoint_id,
                workspace_id,
            }) => &item.key.endpoint_id == endpoint_id && &item.workspace_id == workspace_id,
        }
    }

    fn select(&mut self, key: Option<ItemKey>) {
        self.follow = true;
        if self.selected != key {
            self.selected = key;
            self.detail = false;
            self.detail_for = None;
            self.sticky = None;
            self.snooze_step = 0;
            self.confirm = None;
            if self
                .compose
                .as_ref()
                .is_some_and(|compose| Some(&compose.at.0) != self.selected.as_ref())
            {
                self.close_compose();
            }
        }
    }

    /// Closes the editor, keeping its text as the item's draft and its
    /// `$EDITOR` file for the next time it opens.
    fn close_compose(&mut self) {
        if let Some(compose) = self.compose.take() {
            if let Some(external) = compose.external {
                self.externals.insert(compose.at.0.clone(), external);
            }
            let text = compose.editor.text();
            if text.trim().is_empty() {
                self.drafts.remove(&compose.at.0);
            } else {
                self.drafts.insert(compose.at.0, text);
            }
        }
    }
}

/// The text the prompt must show before a key answers `item`: the question,
/// or, for a permission, the command without the tool name. A plan has none
/// (see [`answer::confirm`]).
fn screen_text(item: &Item) -> String {
    match item.kind {
        ItemKind::Permission => item
            .wait_text
            .split_once(' ')
            .map_or(String::new(), |(_, arg)| arg.to_owned()),
        ItemKind::Plan => String::new(),
        _ => item.wait_text.clone(),
    }
}

/// The answer keys `item` offers now (section 2). Keys that send screen keys
/// appear only once a screen read confirmed their label; none appear while
/// an answer failed a check for this state.
fn answer_keys(item: &Item, state: &InboxState) -> Vec<AnswerKey> {
    if state.is_stale(item) {
        return Vec::new();
    }
    let key = |key: char, label: &str, act: KeyAct| AnswerKey {
        key,
        label: label.to_owned(),
        act,
        grant: false,
    };
    let screen = state
        .screen_of(item)
        .and_then(|screen| screen.as_ref().ok());
    let text = screen_text(item);
    let on_screen = |choice: &Choice| {
        screen.is_some_and(|screen| answer::confirm(screen, &text, choice).is_some())
    };
    let send_keys = |choice: Choice| {
        KeyAct::Send(Action::Keys {
            text: text.clone(),
            choice,
        })
    };
    let decide = |decision: Decision| KeyAct::Send(Action::Decide { decision });
    let mut keys = Vec::new();
    match item.kind {
        ItemKind::Permission if item.decides => {
            keys.push(key('y', "yes", decide(Decision::Allow)));
            keys.push(AnswerKey {
                grant: true,
                ..key('a', "always", decide(Decision::Always))
            });
            keys.push(key('n', "no", decide(Decision::Deny(String::new()))));
            keys.push(key('r', "note", KeyAct::Compose(Purpose::Note)));
        }
        ItemKind::Permission => {
            // Codex: its hook decision format is not verified, so keys.
            for (ch, label, choice) in [
                ('y', "yes", Choice::Yes),
                ('a', "always", Choice::Always),
                ('n', "no", Choice::No),
            ] {
                if on_screen(&choice) {
                    keys.push(AnswerKey {
                        grant: choice == Choice::Always,
                        ..key(ch, label, send_keys(choice))
                    });
                }
            }
        }
        ItemKind::Question => {
            for (index, label) in item.options.iter().enumerate().take(4) {
                let choice = Choice::Label(label.clone());
                if on_screen(&choice) {
                    let digit = char::from(b'1' + index as u8);
                    keys.push(AnswerKey {
                        grant: answer::grants(label),
                        ..key(digit, label, send_keys(choice))
                    });
                }
            }
            if item.decides {
                keys.push(key('o', "other", KeyAct::Compose(Purpose::Note)));
            }
        }
        ItemKind::Plan => {
            if on_screen(&Choice::ApprovePlan) {
                keys.push(key('y', "approve", send_keys(Choice::ApprovePlan)));
            }
            if item.decides {
                keys.push(key('r', "keep planning", KeyAct::Compose(Purpose::Note)));
                keys.push(key('p', "read plan", KeyAct::ReadPlan));
            }
        }
        ItemKind::Asks | ItemKind::Finished | ItemKind::Denied => {
            keys.push(key('r', "reply", KeyAct::Compose(Purpose::Reply)));
        }
        ItemKind::Dialog | ItemKind::Stuck | ItemKind::Limit | ItemKind::Exited => {}
    }
    keys
}

/// Keys that need a screen read before they can show.
fn needs_screen(item: &Item) -> bool {
    match item.kind {
        ItemKind::Question | ItemKind::Plan => true,
        ItemKind::Permission => !item.decides,
        _ => false,
    }
}

/// How an inbox request reaches a machine's herdr API.
#[derive(Clone, Debug)]
pub(crate) enum ApiRoute {
    Local,
    Remote(std::sync::Arc<crate::remote::EndpointBridge>),
}

/// What to do with a request's answer.
#[derive(Clone, Debug)]
pub(crate) enum InboxReply {
    Mark {
        machine: String,
        key: ItemKey,
        token: &'static str,
        value: String,
    },
    Screen {
        key: ItemKey,
        seq: u64,
        wait_id: String,
    },
    /// An answer; `draft` is the editor text it sent, kept again when it
    /// fails.
    Answer {
        key: ItemKey,
        seq: u64,
        wait_id: String,
        draft: Option<String>,
    },
    Plan {
        key: ItemKey,
        seq: u64,
        wait_id: String,
    },
}

type LoopEvents = tokio::sync::mpsc::Sender<crate::client::events::ClientLoopEvent>;

/// One inbox request and where its answer goes.
struct InboxJob {
    route: ApiRoute,
    request: Box<crate::api::schema::Request>,
    reply: InboxReply,
    events: Option<LoopEvents>,
}

impl InboxJob {
    fn run(self) {
        let client = match &self.route {
            ApiRoute::Local => Ok(crate::api::client::ApiClient::local()),
            ApiRoute::Remote(bridge) => bridge.api_client(),
        };
        let result = client
            .map_err(|error| error.to_string())
            .and_then(|client| {
                client
                    .request_value_with_timeout(&self.request, REQUEST_TIMEOUT)
                    .map_err(|error| error.to_string())
            })
            .and_then(|value| match value.get("error") {
                Some(error) => Err(error["message"]
                    .as_str()
                    .unwrap_or("request failed")
                    .to_owned()),
                None => Ok(value),
            });
        if let Err(error) = &result {
            tracing::warn!(%error, reply = ?self.reply, "inbox request failed");
        }
        if let Some(events) = self.events {
            let _ = events.blocking_send(crate::client::events::ClientLoopEvent::InboxReply {
                reply: self.reply,
                result,
            });
        }
    }
}

/// Runs an inbox request on a background thread and posts the answer to the
/// client loop. Without `events` (tests) the answer is dropped.
///
/// Marks run one at a time, in the order sent, so a later mark for a token
/// (a second `z`) is never overwritten by an earlier one. Ceiling: a slow
/// machine delays marks for the others by up to [`REQUEST_TIMEOUT`]; the
/// upgrade path is one queue per machine.
pub(crate) fn run_request(
    route: ApiRoute,
    request: Box<crate::api::schema::Request>,
    reply: InboxReply,
    events: Option<LoopEvents>,
) {
    static MARKS: OnceLock<std::sync::mpsc::Sender<InboxJob>> = OnceLock::new();
    let job = InboxJob {
        route,
        request,
        reply,
        events,
    };
    if !matches!(job.reply, InboxReply::Mark { .. }) {
        std::thread::spawn(move || job.run());
        return;
    }
    let queue = MARKS.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::channel::<InboxJob>();
        std::thread::spawn(move || {
            for job in receiver {
                job.run();
            }
        });
        sender
    });
    if let Err(std::sync::mpsc::SendError(job)) = queue.send(job) {
        std::thread::spawn(move || job.run());
    }
}

/// Unix time of 09:00 tomorrow, local time, `secs_of_day` after local
/// midnight. Ceiling: a DST change tonight shifts it by an hour.
fn tomorrow_nine(now: u64, secs_of_day: u64) -> u64 {
    now.saturating_sub(secs_of_day) + 86_400 + 9 * 3600
}

fn local_secs_of_day(now: u64) -> u64 {
    crate::platform::local_datetime().map_or(now % 86_400, |local| {
        u64::from(local.hour()) * 3600 + u64::from(local.minute()) * 60 + u64::from(local.second())
    })
}

/// Snooze end for the `step`-th press: 1 h, 4 h, then tomorrow 09:00.
fn snooze_end(step: usize, now: u64) -> u64 {
    match step % 3 {
        0 => now + 3600,
        1 => now + 4 * 3600,
        _ => tomorrow_nine(now, local_secs_of_day(now)),
    }
}

fn snooze_label(step: usize) -> &'static str {
    match step % 3 {
        0 => "snoozed 1h",
        1 => "snoozed 4h",
        _ => "snoozed until 09:00",
    }
}

// ------------------------------------------------------------------ shell

impl ClientShellState {
    fn inbox_items(&self) -> Vec<Item> {
        collect(
            &self.endpoints,
            &projects::layout(),
            agent_signal::unix_now(),
            self.inbox.sticky.as_ref(),
        )
    }

    fn visible_inbox_items(&self) -> Vec<Item> {
        self.inbox_items()
            .into_iter()
            .filter(|item| self.inbox.admits(item))
            .collect()
    }

    fn relayout_inbox(&mut self, outcome: &mut ClientShellInput) {
        self.invalidate_pane_surface();
        outcome.repaint = true;
        outcome.resize = true;
    }

    fn open_inbox_panel(&mut self, outcome: &mut ClientShellInput) {
        if !self.inbox.open {
            self.inbox.open = true;
            self.inbox.scroll = 0;
            self.relayout_inbox(outcome);
        }
        self.inbox.focus(Instant::now());
        outcome.repaint = true;
    }

    pub(super) fn close_inbox(&mut self, outcome: &mut ClientShellInput) {
        if self.inbox.open {
            self.inbox.open = false;
            self.blur_inbox();
            self.inbox.dragging = false;
            self.inbox.select(None);
            self.relayout_inbox(outcome);
        }
    }

    /// Focus goes back to the panes (the herdr pane focus never moved),
    /// and to copy mode when the focused pane is in it.
    pub(super) fn blur_inbox(&mut self) -> bool {
        self.inbox.return_focus = false;
        let was = std::mem::replace(&mut self.inbox.focused, false);
        if was && self.mode == ClientShellMode::Terminal {
            self.mode = self.copy_or_terminal_mode();
        }
        was
    }

    /// `prefix i`: open and focus; focus when open; close when focused.
    pub(super) fn toggle_inbox(&mut self, outcome: &mut ClientShellInput) {
        if self.inbox.open && self.inbox.focused {
            self.close_inbox(outcome);
        } else {
            self.open_inbox_panel(outcome);
            self.ensure_inbox_selection();
        }
    }

    /// A sidebar glyph or section count: open the inbox filtered to it.
    pub(super) fn open_inbox_filtered(
        &mut self,
        filter: InboxFilter,
        outcome: &mut ClientShellInput,
    ) {
        self.inbox.filter = Some(filter);
        self.inbox.tab = InboxTab::All;
        self.inbox.scroll = 0;
        self.inbox.select(None);
        self.open_inbox_panel(outcome);
        self.ensure_inbox_selection();
    }

    /// `prefix a`: open or focus the inbox on the oldest waiting item.
    pub(super) fn inbox_oldest_waiting(&mut self, outcome: &mut ClientShellInput) {
        self.inbox.filter = None;
        self.inbox.tab = InboxTab::Waiting;
        let oldest = self
            .inbox_items()
            .into_iter()
            .filter(|item| item.kind.waiting())
            .max_by_key(|item| item.age.unwrap_or(u64::MAX))
            .map(|item| item.key);
        self.inbox.select(oldest);
        self.open_inbox_panel(outcome);
        self.ensure_inbox_selection();
        self.inbox.return_focus = true;
    }

    /// Keep the selection on a listed item (the first when it left).
    fn ensure_inbox_selection(&mut self) {
        let items = self.visible_inbox_items();
        if !items
            .iter()
            .any(|item| Some(&item.key) == self.inbox.selected.as_ref())
        {
            self.inbox
                .select(items.first().map(|item| item.key.clone()));
        }
    }

    fn move_inbox_selection(&mut self, delta: isize) {
        let items = self.visible_inbox_items();
        if items.is_empty() {
            return;
        }
        let current = items
            .iter()
            .position(|item| Some(&item.key) == self.inbox.selected.as_ref());
        let next = match current {
            Some(index) => index.saturating_add_signed(delta).min(items.len() - 1),
            None => 0,
        };
        self.inbox.select(Some(items[next].key.clone()));
    }

    fn inbox_jump(&mut self, key: &ItemKey, outcome: &mut ClientShellInput) {
        self.blur_inbox();
        if self.inbox.hits.overlay {
            self.close_inbox(outcome);
        }
        self.focus_or_activate(
            key.endpoint_id.clone(),
            ClientEndpointFocusTarget::Pane(key.pane_id.clone()),
            outcome,
        );
        outcome.repaint = true;
    }

    fn api_route(&self, endpoint_id: &ClientEndpointId) -> Option<ApiRoute> {
        if endpoint_id.is_local() {
            return Some(ApiRoute::Local);
        }
        self.endpoint_by_id(endpoint_id)?
            .bridge
            .clone()
            .map(ApiRoute::Remote)
    }

    /// Sets a server mark on the item's pane and hides it locally until the
    /// snapshot carries the mark.
    fn send_inbox_mark(
        &mut self,
        item: &Item,
        token: &'static str,
        value: String,
        outcome: &mut ClientShellInput,
    ) {
        let Some(route) = self.api_route(&item.key.endpoint_id) else {
            return;
        };
        {
            let mut marks = pending_marks().lock().unwrap_or_else(|e| e.into_inner());
            marks.push(PendingMark {
                endpoint_id: item.key.endpoint_id.clone(),
                pane_id: item.key.pane_id.clone(),
                token,
                value: value.clone(),
                at: Instant::now(),
            });
        }
        let request = crate::api::schema::Request {
            id: format!("drovr:inbox:{}", self.next_request_id),
            method: crate::api::schema::Method::PaneReportMetadata(
                crate::api::schema::PaneReportMetadataParams {
                    pane_id: item.key.pane_id.clone(),
                    source: MARK_SOURCE.into(),
                    agent: None,
                    applies_to_source: None,
                    title: None,
                    display_agent: None,
                    state_labels: HashMap::new(),
                    tokens: HashMap::from([(token.to_owned(), Some(value.clone()))]),
                    clear_title: false,
                    clear_display_agent: false,
                    clear_state_labels: false,
                    seq: None,
                    ttl_ms: None,
                },
            ),
        };
        self.next_request_id = self.next_request_id.saturating_add(1);
        outcome.actions.push(ClientShellAction::InboxRequest {
            route,
            request: Box::new(request),
            reply: InboxReply::Mark {
                machine: item.machine.clone(),
                key: item.key.clone(),
                token,
                value: value.clone(),
            },
        });
        outcome.repaint = true;
    }

    fn dismiss_inbox_item(&mut self, item: &Item, outcome: &mut ClientShellInput) -> bool {
        if item.kind.waiting() {
            return false;
        }
        let value = format!("{}|{}", item.seq, kind_tag(item.kind));
        self.send_inbox_mark(item, DISMISS_TOKEN, value, outcome);
        true
    }

    /// Dismisses `item`; when it is selected, the cursor moves to the next
    /// item, worked out before the pending mark hides it from the list.
    fn dismiss_and_advance(&mut self, item: &Item, outcome: &mut ClientShellInput) {
        let next = (self.inbox.selected.as_ref() == Some(&item.key))
            .then(|| self.inbox_neighbour(&item.key));
        if self.dismiss_inbox_item(item, outcome) {
            if let Some(next) = next {
                self.inbox.select(next);
            }
        }
    }

    /// `D`, and the menu's "dismiss all done in project": every done item
    /// that `keep` admits.
    fn dismiss_done(&mut self, keep: impl Fn(&Item) -> bool, outcome: &mut ClientShellInput) {
        for item in self.inbox_items().into_iter().filter(|item| keep(item)) {
            self.dismiss_inbox_item(&item, outcome);
        }
    }

    fn snooze_inbox_item(&mut self, item: &Item, outcome: &mut ClientShellInput) {
        let step = if self.inbox.sticky.as_ref() == Some(&item.key) {
            self.inbox.snooze_step + 1
        } else {
            0
        };
        let end = snooze_end(step, agent_signal::unix_now());
        self.send_inbox_mark(
            item,
            SNOOZE_TOKEN,
            format!("{end}|{}|{}", item.seq, item.wait_id),
            outcome,
        );
        self.inbox.sticky = Some(item.key.clone());
        self.inbox.snooze_step = step;
    }

    fn toggle_inbox_mute(&mut self, item: &Item) {
        let key = item.workspace_key.clone();
        projects::update(|layout| layout.inbox.toggle_mute(&key));
    }

    fn selected_inbox_item(&self) -> Option<Item> {
        let selected = self.inbox.selected.as_ref()?;
        self.visible_inbox_items()
            .into_iter()
            .find(|item| &item.key == selected)
    }

    /// The listed item after `key` (before it when `key` is last).
    fn inbox_neighbour(&self, key: &ItemKey) -> Option<ItemKey> {
        let items = self.visible_inbox_items();
        items
            .iter()
            .position(|item| &item.key == key)
            .and_then(|index| {
                items
                    .get(index + 1)
                    .or_else(|| index.checked_sub(1).and_then(|i| items.get(i)))
            })
            .map(|item| item.key.clone())
    }

    /// `space`: the detail. Its screen text comes from the read the tick
    /// makes on selection ([`Self::tick_inbox`]).
    fn toggle_inbox_detail(&mut self, item: &Item, outcome: &mut ClientShellInput) {
        self.inbox.detail = !self.inbox.detail;
        self.inbox.detail_for = self.inbox.detail.then(|| (item.seq, item.wait_id.clone()));
        outcome.repaint = true;
    }

    /// Periodic inbox work, from the client loop's 100 ms timer: the screen
    /// read of the selected waiting item (again every [`SCREEN_RETRY`] while
    /// it confirms no answer key), the plan text of an expanded plan, a
    /// requested plan open, `$EDITOR` reloads and answered items expiring.
    pub(crate) fn tick_inbox(&mut self, outcome: &mut ClientShellInput) {
        if !self.inbox.open {
            return;
        }
        let now = Instant::now();
        self.inbox.answering.retain(|_, answer| {
            answer
                .done_at
                .is_none_or(|at| now.saturating_duration_since(at) < PENDING_MARK_TTL)
        });
        self.reload_external_editor(outcome);
        // Every listed waiting item reads its screen, so its answers show
        // without selecting it first.
        let items = self.inbox_items();
        self.inbox
            .screen
            .retain(|key, _| items.iter().any(|item| &item.key == key));
        self.inbox
            .screen_asked
            .retain(|key, _| items.iter().any(|item| &item.key == key));
        let waiting: Vec<&Item> = items
            .iter()
            .filter(|item| {
                matches!(
                    item.kind,
                    ItemKind::Permission | ItemKind::Question | ItemKind::Plan | ItemKind::Dialog
                ) && !item.marked
                    && self.inbox.admits(item)
            })
            .collect();
        for item in waiting {
            let at = item_at(item);
            let due = match self.inbox.screen_asked.get(&item.key) {
                Some((asked, when)) if *asked == at => {
                    needs_screen(item)
                        && !self.inbox.is_stale(item)
                        && self.inbox.screen_of(item).is_some()
                        && now.saturating_duration_since(*when) >= SCREEN_RETRY
                        && !answer_keys(item, &self.inbox)
                            .iter()
                            .any(|key| matches!(key.act, KeyAct::Send(Action::Keys { .. })))
                }
                _ => true,
            };
            if due {
                self.request_screen(item, outcome);
            }
        }
        let Some(item) = self.selected_inbox_item() else {
            return;
        };
        if item.kind == ItemKind::Plan {
            match self
                .inbox
                .plan_of(&item)
                .map(|plan| (plan.open, plan.result.clone()))
            {
                None if self.inbox.detail => self.fetch_plan(&item, false, outcome),
                Some((true, Some(result))) => {
                    if let Some(plan) = self.inbox.plan.as_mut() {
                        plan.open = false;
                    }
                    match result {
                        Ok((path, text)) => self.open_plan(&item, &path, &text, outcome),
                        Err(error) => {
                            outcome.repaint |= self.push_endpoint_notice(
                                ClientEndpointNoticeKind::Rejected,
                                "drovr.inbox.plan",
                                "Plan not read",
                                format!("{}: {error}", item.machine),
                            );
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn request_screen(&mut self, item: &Item, outcome: &mut ClientShellInput) {
        let Some(route) = self.api_route(&item.key.endpoint_id) else {
            return;
        };
        self.inbox
            .screen_asked
            .insert(item.key.clone(), (item_at(item), Instant::now()));
        let request = crate::api::schema::Request {
            id: format!("drovr:inbox:{}", self.next_request_id),
            method: crate::api::schema::Method::PaneRead(answer::screen_read_params(
                &item.key.pane_id,
            )),
        };
        self.next_request_id = self.next_request_id.saturating_add(1);
        outcome.actions.push(ClientShellAction::InboxRequest {
            route,
            request: Box::new(request),
            reply: InboxReply::Screen {
                key: item.key.clone(),
                seq: item.seq,
                wait_id: item.wait_id.clone(),
            },
        });
    }

    fn fetch_plan(&mut self, item: &Item, open: bool, outcome: &mut ClientShellInput) {
        let Some(route) = self.api_route(&item.key.endpoint_id) else {
            return;
        };
        self.inbox.plan = Some(PlanRead {
            at: item_at(item),
            result: None,
            open,
        });
        outcome.actions.push(ClientShellAction::InboxTask {
            route,
            task: answer::Task::FetchPlan {
                pane_id: item.key.pane_id.clone(),
                req: item.wait_id.clone(),
            },
            reply: InboxReply::Plan {
                key: item.key.clone(),
                seq: item.seq,
                wait_id: item.wait_id.clone(),
            },
        });
    }

    /// `p`: the plan in a doc pane, once read.
    fn read_plan(&mut self, item: &Item, outcome: &mut ClientShellInput) {
        match self.inbox.plan_of(item).map(|plan| plan.result.clone()) {
            Some(Some(Ok((path, text)))) => self.open_plan(item, &path, &text, outcome),
            Some(None) => {
                if let Some(plan) = self.inbox.plan.as_mut() {
                    plan.open = true;
                }
            }
            _ => self.fetch_plan(item, true, outcome),
        }
    }

    /// Opens a plan read from `path` on the item's machine. A local agent's
    /// plan opens beside it. A remote agent's plan opens from a local copy in
    /// the local workspace on screen; while the agent's own machine is on
    /// screen, it opens beside the agent there instead (the Ctrl+click
    /// route, which needs drovr or the drovr.docs plugin there), since a
    /// local doc pane would not be visible. With a third machine on screen,
    /// neither would be: a notice asks to switch.
    fn open_plan(&mut self, item: &Item, path: &str, text: &str, outcome: &mut ClientShellInput) {
        if item.key.endpoint_id.is_local() {
            outcome.actions.push(ClientShellAction::OpenLocalDocument {
                workspace_id: item.workspace_id.clone(),
                pane_id: item.key.pane_id.clone(),
                path: path.to_owned(),
            });
            return;
        }
        if self.active_endpoint_id.is_local() {
            let snapshot = self.snapshot.as_deref();
            let workspace = snapshot.and_then(|snapshot| snapshot.focused_workspace_id.clone());
            let pane = snapshot.and_then(|snapshot| snapshot.focused_pane_id.clone());
            let copy =
                write_inbox_file(&format!("plan-{}-{}", item.machine, file_name(path)), text);
            match (workspace, pane, copy) {
                (Some(workspace_id), Some(pane_id), Ok(copy)) => {
                    outcome.actions.push(ClientShellAction::OpenLocalDocument {
                        workspace_id,
                        pane_id,
                        path: copy.to_string_lossy().into_owned(),
                    });
                }
                (_, _, Err(error)) => {
                    outcome.repaint |= self.push_endpoint_notice(
                        ClientEndpointNoticeKind::Rejected,
                        "drovr.inbox.plan",
                        "Plan not saved",
                        error.to_string(),
                    );
                }
                _ => {}
            }
            return;
        }
        if self.active_endpoint_id != item.key.endpoint_id {
            outcome.repaint |= self.push_endpoint_notice(
                ClientEndpointNoticeKind::Rejected,
                "drovr.inbox.plan",
                "Plan not opened",
                format!(
                    "Switch to a workspace on this machine or on {} to read the plan.",
                    item.machine
                ),
            );
            return;
        }
        let Some(bridge) = self
            .endpoint_by_id(&item.key.endpoint_id)
            .and_then(|endpoint| endpoint.bridge.clone())
        else {
            return;
        };
        outcome.actions.push(ClientShellAction::OpenRemoteDocument {
            bridge,
            doc: crate::remote::RemoteDocOpen {
                workspace_id: item.workspace_id.clone(),
                tab_id: item.tab_id.clone(),
                pane_id: item.key.pane_id.clone(),
                cwd: None,
                path: path.to_owned(),
            },
        });
    }

    /// The listed waiting item after `key` (wrapping), else its neighbour:
    /// where the cursor goes after an answer.
    fn next_after_answer(&self, key: &ItemKey) -> Option<ItemKey> {
        let items = self.visible_inbox_items();
        let index = items.iter().position(|item| &item.key == key).unwrap_or(0);
        items
            .iter()
            .skip(index + 1)
            .chain(items.iter().take(index))
            .find(|item| item.kind.waiting() && &item.key != key)
            .map(|item| item.key.clone())
            .or_else(|| self.inbox_neighbour(key))
    }

    /// Sends an answer for the item at `at`; its item hides until the
    /// machine confirms it, and comes back if it fails.
    fn submit_answer(&mut self, at: ItemAt, action: Action, outcome: &mut ClientShellInput) {
        let (key, seq, wait_id) = at;
        if self
            .inbox
            .answering
            .get(&key)
            .is_some_and(|answer| answer.done_at.is_none())
        {
            return;
        }
        let Some(route) = self.api_route(&key.endpoint_id) else {
            return;
        };
        let draft = match &action {
            Action::Decide {
                decision: Decision::Deny(text),
            }
            | Action::Prompt(text)
                if !text.is_empty() =>
            {
                Some(text.clone())
            }
            _ => None,
        };
        let next = self.next_after_answer(&key);
        self.inbox.answering.insert(
            key.clone(),
            Answering {
                seq,
                wait_id: wait_id.clone(),
                done_at: None,
            },
        );
        self.inbox.confirm = None;
        outcome.actions.push(ClientShellAction::InboxTask {
            route,
            task: answer::Task::Answer(answer::Answer {
                pane_id: key.pane_id.clone(),
                seq,
                wait_id: wait_id.clone(),
                action,
            }),
            reply: InboxReply::Answer {
                key,
                seq,
                wait_id,
                draft,
            },
        });
        self.inbox.select(next);
        if self.inbox.return_focus {
            self.blur_inbox();
        }
        outcome.repaint = true;
    }

    /// An answer key (or a click on its label) on `item`; false when `ch`
    /// is not one of its keys. A grant key needs a second press: `pending`
    /// is the press before this one.
    fn inbox_answer_key(
        &mut self,
        item: &Item,
        ch: char,
        pending: Option<(ItemAt, char)>,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(entry) = answer_keys(item, &self.inbox)
            .into_iter()
            .find(|key| key.key == ch)
        else {
            return false;
        };
        let at = item_at(item);
        outcome.repaint = true;
        if entry.grant && pending.as_ref() != Some(&(at.clone(), ch)) {
            self.inbox.confirm = Some((at, ch));
            return true;
        }
        match entry.act {
            KeyAct::Send(action) => self.submit_answer(at, action, outcome),
            KeyAct::Compose(purpose) => {
                self.inbox.close_compose();
                let text = self.inbox.drafts.remove(&at.0).unwrap_or_default();
                let external = self.inbox.externals.remove(&at.0);
                self.inbox.compose = Some(Compose {
                    at,
                    purpose,
                    editor: NoteEditor::new(&text),
                    external,
                });
                // Text written in `$EDITOR` while the editor was closed.
                self.reload_external_editor(outcome);
            }
            KeyAct::ReadPlan => self.read_plan(item, outcome),
        }
        true
    }

    /// Keys while the editor is open: they all go to it.
    fn handle_compose_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        let Some(compose) = self.inbox.compose.as_mut() else {
            return;
        };
        outcome.repaint = true;
        match compose.editor.handle_key(key) {
            EditorKey::Edited | EditorKey::Ignored => {}
            EditorKey::Cancel => self.inbox.close_compose(),
            EditorKey::External => self.open_external_editor(outcome),
            EditorKey::Send => {
                let text = compose.editor.text().trim().to_owned();
                if text.is_empty() {
                    return;
                }
                if compose.purpose == Purpose::Note && text.len() > answer::NOTE_MAX_BYTES {
                    self.push_endpoint_notice(
                        ClientEndpointNoticeKind::Rejected,
                        "drovr.inbox.note",
                        "Note too long",
                        format!("A note is at most {} bytes.", answer::NOTE_MAX_BYTES),
                    );
                    return;
                }
                let Some(compose) = self.inbox.compose.take() else {
                    return;
                };
                if let Some((path, _)) = &compose.external {
                    let _ = std::fs::remove_file(path);
                }
                self.inbox.drafts.remove(&compose.at.0);
                let action = match compose.purpose {
                    Purpose::Note => Action::Decide {
                        decision: Decision::Deny(text),
                    },
                    Purpose::Reply => Action::Prompt(text),
                };
                self.submit_answer(compose.at, action, outcome);
            }
        }
    }

    /// Text and pastes while the editor is open; false when it is not.
    pub(super) fn insert_inbox_text(&mut self, text: &str) -> bool {
        if !(self.inbox.open && self.inbox.focused) {
            return false;
        }
        match self.inbox.compose.as_mut() {
            Some(compose) => {
                compose.editor.insert(text);
                true
            }
            None => false,
        }
    }

    /// `ctrl+e`: the draft in `$EDITOR`, in a pane split from the focused
    /// pane of the local workspace on screen. The editor reloads the file
    /// whenever it changes. Ceiling: a remote workspace on screen has no
    /// local pane to split, so `ctrl+e` asks for a local one.
    fn open_external_editor(&mut self, outcome: &mut ClientShellInput) {
        if !self.active_endpoint_id.is_local() {
            outcome.repaint |= self.push_endpoint_notice(
                ClientEndpointNoticeKind::Rejected,
                "drovr.inbox.editor",
                "$EDITOR runs on this machine",
                "Switch to a workspace on this machine to edit the note in $EDITOR.",
            );
            return;
        }
        let Some(pane_id) = self.focused_pane_id() else {
            return;
        };
        let Some(compose) = self.inbox.compose.as_mut() else {
            return;
        };
        let name = format!("note-{}.md", file_name(&compose.at.0.pane_id));
        match write_inbox_file(&name, &compose.editor.text()) {
            Ok(path) => {
                let modified = std::fs::metadata(&path)
                    .and_then(|meta| meta.modified())
                    .ok();
                compose.external = Some((path.clone(), modified));
                outcome
                    .actions
                    .push(ClientShellAction::OpenLocalEditor { pane_id, path });
                self.blur_inbox();
            }
            Err(error) => {
                outcome.repaint |= self.push_endpoint_notice(
                    ClientEndpointNoticeKind::Rejected,
                    "drovr.inbox.editor",
                    "Note not saved",
                    error.to_string(),
                );
            }
        }
    }

    fn reload_external_editor(&mut self, outcome: &mut ClientShellInput) {
        let Some(compose) = self.inbox.compose.as_mut() else {
            return;
        };
        let Some((path, seen)) = compose.external.as_mut() else {
            return;
        };
        let modified = std::fs::metadata(&*path)
            .and_then(|meta| meta.modified())
            .ok();
        if modified.is_some() && modified != *seen {
            *seen = modified;
            if let Ok(text) = std::fs::read_to_string(&*path) {
                compose.editor.set_text(text.trim_end_matches('\n'));
                outcome.repaint = true;
            }
        }
    }

    /// An inbox request answered; true when the frame changed.
    pub(crate) fn receive_inbox_reply(
        &mut self,
        reply: InboxReply,
        result: Result<serde_json::Value, String>,
    ) -> bool {
        match reply {
            InboxReply::Mark {
                machine,
                key,
                token,
                value,
            } => match result {
                Ok(_) => false,
                Err(error) => {
                    // The failed mark lapses and its item comes back; newer
                    // marks for the pane stay.
                    pending_marks()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .retain(|mark| {
                            mark.endpoint_id != key.endpoint_id
                                || mark.pane_id != key.pane_id
                                || mark.token != token
                                || mark.value != value
                        });
                    self.push_endpoint_notice(
                        ClientEndpointNoticeKind::Rejected,
                        "drovr.inbox.mark",
                        "Inbox mark not saved",
                        format!("{machine}: {error}"),
                    )
                }
            },
            InboxReply::Screen { key, seq, wait_id } => {
                let at = (key.clone(), seq, wait_id);
                if self.inbox.screen_asked.get(&key).map(|(asked, _)| asked) != Some(&at) {
                    return false;
                }
                let text = result.map(|value| {
                    value["result"]["read"]["text"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned()
                });
                // The read repeats every SCREEN_RETRY; only a change repaints.
                let changed = self
                    .inbox
                    .screen
                    .get(&key)
                    .is_none_or(|screen| screen.at != at || screen.text != text);
                self.inbox.screen.insert(key, ScreenRead { at, text });
                changed
            }
            InboxReply::Answer {
                key,
                seq,
                wait_id,
                draft,
            } => match result {
                Ok(_) => {
                    if let Some(answer) = self.inbox.answering.get_mut(&key) {
                        answer.done_at = Some(Instant::now());
                    }
                    false
                }
                Err(error) => {
                    // The item comes back, with the text that was sent.
                    self.inbox.answering.remove(&key);
                    if let Some(draft) = draft {
                        self.inbox.drafts.insert(key.clone(), draft);
                    }
                    if error == answer::CHANGED {
                        self.inbox.stale.insert(key, (seq, wait_id));
                        true
                    } else if error == answer::UNKNOWN {
                        // The hook may have answered: no keys for this
                        // request again.
                        self.inbox.stale.insert(key, (seq, wait_id));
                        self.push_endpoint_notice(
                            ClientEndpointNoticeKind::Rejected,
                            "drovr.inbox.answer",
                            "Answer unknown",
                            error,
                        );
                        true
                    } else {
                        self.push_endpoint_notice(
                            ClientEndpointNoticeKind::Rejected,
                            "drovr.inbox.answer",
                            "Answer not sent",
                            error,
                        );
                        true
                    }
                }
            },
            InboxReply::Plan { key, seq, wait_id } => {
                let at = (key, seq, wait_id);
                let Some(plan) = self.inbox.plan.as_mut().filter(|plan| plan.at == at) else {
                    return false;
                };
                plan.result = Some(result.map(|value| {
                    (
                        value["path"].as_str().unwrap_or_default().to_owned(),
                        value["text"].as_str().unwrap_or_default().to_owned(),
                    )
                }));
                true
            }
        }
    }

    /// Keys while the inbox has focus. Returns false for keys it leaves to
    /// the shell (none today: every key stays in the inbox).
    pub(super) fn handle_inbox_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        if !self.inbox.accepts_key(Instant::now()) {
            return;
        }
        if self.inbox.compose.is_some() {
            self.handle_compose_key(key, outcome);
            return;
        }
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        let plain = modifiers.difference(KeyModifiers::SHIFT).is_empty();
        if !plain {
            return;
        }
        outcome.repaint = true;
        // Any key but the second press of a grant key cancels it.
        let pending = self.inbox.confirm.take();
        let selected = self.selected_inbox_item();
        match code {
            KeyCode::Char('j') | KeyCode::Down => self.move_inbox_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_inbox_selection(-1),
            KeyCode::Home => self.inbox.select(
                self.visible_inbox_items()
                    .first()
                    .map(|item| item.key.clone()),
            ),
            KeyCode::End => self.inbox.select(
                self.visible_inbox_items()
                    .last()
                    .map(|item| item.key.clone()),
            ),
            KeyCode::Tab => {
                self.inbox.tab = self.inbox.tab.next();
                self.inbox.scroll = 0;
                self.inbox.sticky = None;
                self.ensure_inbox_selection();
            }
            KeyCode::Char('g') => {
                projects::update(|layout| layout.inbox.grouped = !layout.inbox.grouped);
            }
            KeyCode::Char('?') => self.inbox.show_keys = !self.inbox.show_keys,
            KeyCode::Esc => {
                if self.inbox.hits.overlay {
                    self.close_inbox(outcome);
                } else if self.inbox.filter.take().is_some() {
                    self.ensure_inbox_selection();
                } else {
                    self.blur_inbox();
                }
            }
            KeyCode::Char('D') => {
                let tab = self.inbox.tab;
                let filter = self.inbox.filter.clone();
                let state = InboxState {
                    tab,
                    filter,
                    ..InboxState::default()
                };
                self.dismiss_done(|item| !item.kind.waiting() && state.admits(item), outcome);
                self.ensure_inbox_selection();
            }
            _ => {
                let Some(item) = selected else {
                    return;
                };
                if let KeyCode::Char(ch) = code {
                    if self.inbox_answer_key(&item, ch, pending, outcome) {
                        return;
                    }
                }
                match code {
                    KeyCode::Enter => self.inbox_jump(&item.key, outcome),
                    KeyCode::Char(' ' | 'l') => self.toggle_inbox_detail(&item, outcome),
                    KeyCode::Char('d') => self.dismiss_and_advance(&item, outcome),
                    KeyCode::Char('z') => self.snooze_inbox_item(&item, outcome),
                    KeyCode::Char('m') => self.toggle_inbox_mute(&item),
                    _ => {}
                }
            }
        }
    }

    /// Mouse events on the inbox panel; true when consumed.
    pub(super) fn handle_inbox_mouse(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !self.inbox.open || self.overlay.is_some() {
            return false;
        }
        let point = (mouse.column, mouse.row);
        let hits = self.inbox.hits.clone();
        if self.inbox.dragging {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    let share = share_at(hits.cols, hits.main.0, hits.main.1, mouse.column);
                    if self.inbox.width != Some(share) {
                        self.inbox.width = Some(share);
                        self.relayout_inbox(outcome);
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    self.inbox.dragging = false;
                    if let Some(share) = self.inbox.width.take() {
                        projects::update(|layout| layout.inbox.width = Some(share));
                    }
                    self.relayout_inbox(outcome);
                }
                _ => {}
            }
            return true;
        }
        if !super::contains(hits.area, point) {
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) && self.blur_inbox() {
                outcome.repaint = true;
            }
            if self.inbox.hover.take().is_some() {
                outcome.repaint = true;
            }
            return false;
        }
        let row = hits
            .rows
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
            .map(|(_, key)| key.clone());
        match mouse.kind {
            MouseEventKind::Moved => {
                if self.inbox.hover != row {
                    self.inbox.hover = row;
                    outcome.repaint = true;
                }
            }
            MouseEventKind::ScrollUp => {
                self.inbox.follow = false;
                self.inbox.scroll = self.inbox.scroll.saturating_sub(2);
                outcome.repaint = true;
            }
            MouseEventKind::ScrollDown => {
                self.inbox.follow = false;
                self.inbox.scroll = self.inbox.scroll.saturating_add(2);
                outcome.repaint = true;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.inbox.focus(Instant::now());
                outcome.repaint = true;
                if super::contains(hits.border, point) {
                    self.inbox.dragging = true;
                    self.inbox.width = self.inbox.share(&projects::layout());
                } else if let Some((_, tab)) = hits
                    .tabs
                    .iter()
                    .find(|(rect, _)| super::contains(*rect, point))
                {
                    self.inbox.tab = *tab;
                    self.inbox.scroll = 0;
                    self.ensure_inbox_selection();
                } else if super::contains(hits.group, point) {
                    projects::update(|layout| layout.inbox.grouped = !layout.inbox.grouped);
                } else if super::contains(hits.chip, point) {
                    self.inbox.filter = None;
                    self.ensure_inbox_selection();
                } else if let Some((_, key, ch)) = hits
                    .keys
                    .iter()
                    .find(|(rect, _, _)| super::contains(*rect, point))
                {
                    // A click on an answer label: the same checks as the key.
                    let pending = self.inbox.confirm.take();
                    if let Some(item) = self.inbox_items().into_iter().find(|item| &item.key == key)
                    {
                        self.inbox_answer_key(&item, *ch, pending, outcome);
                    }
                } else if let Some((_, key)) = hits
                    .closes
                    .iter()
                    .find(|(rect, _)| super::contains(*rect, point))
                {
                    if let Some(item) = self.inbox_items().into_iter().find(|item| &item.key == key)
                    {
                        self.dismiss_and_advance(&item, outcome);
                    }
                } else if let Some((_, key)) = hits
                    .jumps
                    .iter()
                    .find(|(rect, _)| super::contains(*rect, point))
                {
                    let key = key.clone();
                    self.inbox_jump(&key, outcome);
                } else if let Some((_, key)) = hits
                    .details
                    .iter()
                    .find(|(rect, _)| super::contains(*rect, point))
                {
                    self.inbox.select(Some(key.clone()));
                    if let Some(item) = self.selected_inbox_item() {
                        self.toggle_inbox_detail(&item, outcome);
                    }
                } else if let Some(key) = row {
                    // A row click only selects: the detail opens from its
                    // "more" label, so a slightly missed answer click does
                    // not unfold the agent's screen.
                    self.inbox.select(Some(key));
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                if let Some(key) = row {
                    self.inbox.select(Some(key.clone()));
                    let item = self.inbox_items().into_iter().find(|item| item.key == key);
                    if let Some(item) = item {
                        self.overlay =
                            Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
                                target: ClientContextMenuTarget::InboxItem {
                                    key,
                                    waiting: item.kind.waiting(),
                                    muted: projects::layout().inbox.is_muted(&item.workspace_key),
                                },
                                x: mouse.column,
                                y: mouse.row,
                                highlighted: 0,
                            }));
                    }
                }
                outcome.repaint = true;
            }
            _ => {}
        }
        true
    }

    /// A pick from an inbox row's right-click menu.
    pub(super) fn activate_inbox_menu(
        &mut self,
        key: ItemKey,
        action: ClientContextMenuAction,
        outcome: &mut ClientShellInput,
    ) {
        let Some(item) = self.inbox_items().into_iter().find(|item| item.key == key) else {
            return;
        };
        match action {
            ClientContextMenuAction::AgentFocus => self.inbox_jump(&key, outcome),
            ClientContextMenuAction::InboxDismiss => self.dismiss_and_advance(&item, outcome),
            ClientContextMenuAction::InboxDismissDoneInProject => {
                let project = item.project.clone();
                self.dismiss_done(
                    |other| !other.kind.waiting() && other.project == project,
                    outcome,
                );
                self.ensure_inbox_selection();
            }
            ClientContextMenuAction::InboxSnooze => self.snooze_inbox_item(&item, outcome),
            ClientContextMenuAction::InboxMute => self.toggle_inbox_mute(&item),
            _ => {}
        }
        outcome.repaint = true;
    }
}

/// The last part of `path`, with characters other than letters, digits,
/// `.`, `-` and `_` replaced: a safe file name.
fn file_name(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Writes `text` to `<state dir>/drovr-inbox/<name>` (a plan copy or a note
/// for `$EDITOR`) and returns the path.
fn write_inbox_file(name: &str, text: &str) -> std::io::Result<std::path::PathBuf> {
    let dir = crate::config::state_dir().join("drovr-inbox");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(file_name(name));
    std::fs::write(&path, text)?;
    Ok(path)
}

/// Draws the panel into `buffer` (the whole screen) at `area`. A free
/// function over the inbox state, so composition can call it while it holds
/// the active snapshot.
pub(super) fn render(
    inbox: &mut InboxState,
    endpoints: &[ClientShellEndpoint],
    palette: &Palette,
    buffer: &mut Buffer,
    area: Rect,
    overlay: bool,
    cols: u16,
    main: (u16, u16),
) {
    let layout = projects::layout();
    let items = collect(
        endpoints,
        &layout,
        agent_signal::unix_now(),
        inbox.sticky.as_ref(),
    );
    let visible: Vec<Item> = items
        .iter()
        .filter(|item| inbox.admits(item))
        .cloned()
        .collect();
    if !visible
        .iter()
        .any(|item| Some(&item.key) == inbox.selected.as_ref())
    {
        inbox.select(visible.first().map(|item| item.key.clone()));
    }
    // A new state or prompt on the expanded item closes its detail, so it
    // never shows another prompt's screen or waits on a read never sent.
    if inbox.detail
        && visible.iter().any(|item| {
            Some(&item.key) == inbox.selected.as_ref()
                && inbox.detail_for.as_ref() != Some(&(item.seq, item.wait_id.clone()))
        })
    {
        inbox.detail = false;
        inbox.detail_for = None;
    }
    let waiting = items
        .iter()
        .filter(|item| item.kind.waiting() && !item.marked && !inbox.answered(item))
        .count();
    let done = items
        .iter()
        .filter(|item| !item.kind.waiting() && !item.marked && !inbox.answered(item))
        .count();
    let view = View {
        items: &visible,
        waiting,
        done,
        state: inbox,
        palette,
        snooze: inbox
            .sticky
            .as_ref()
            .map(|key| (key.clone(), snooze_label(inbox.snooze_step))),
        muted: &layout.inbox.muted,
        grouped: layout.inbox.grouped,
    };
    let (hits, scroll) = draw(buffer, area, &view);
    inbox.scroll = scroll;
    inbox.hits = InboxHits {
        cols,
        main,
        overlay,
        ..hits
    };
}

// ------------------------------------------------------------------ drawing

struct View<'a> {
    items: &'a [Item],
    waiting: usize,
    done: usize,
    state: &'a InboxState,
    palette: &'a Palette,
    snooze: Option<(ItemKey, &'static str)>,
    muted: &'a [String],
    grouped: bool,
}

/// One drawn line of the list.
#[derive(Clone, Debug, PartialEq)]
enum Line {
    Group(String),
    Main(usize),
    Meta(usize),
    /// A question's options: one row per confirmed option (the option's
    /// place among them), or one note row (`None`) while none is confirmed.
    Options(usize, Option<usize>),
    /// A row of the open editor.
    Editor(usize, usize),
    Detail(usize, String),
    Actions(usize),
}

fn item_color(kind: ItemKind, palette: &Palette) -> ratatui::style::Color {
    if kind.waiting() {
        super::status_color(crate::api::schema::AgentStatus::Blocked, palette)
    } else if kind == ItemKind::Finished {
        super::status_color(crate::api::schema::AgentStatus::Done, palette)
    } else {
        palette.yellow
    }
}

fn lines_for(view: &View, narrow: bool) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut group = None;
    for (index, item) in view.items.iter().enumerate() {
        if view.grouped && group.as_ref() != Some(&item.project) {
            group = Some(item.project.clone());
            lines.push(Line::Group(if item.project == OTHER {
                "Other".to_owned()
            } else {
                item.project.clone()
            }));
        }
        lines.push(Line::Main(index));
        if narrow {
            lines.push(Line::Meta(index));
        }
        let selected = view.state.selected.as_ref() == Some(&item.key);
        // A waiting item always shows its answers: one click answers it.
        let answers = selected || item.kind.waiting();
        if answers && item.kind == ItemKind::Question {
            {
                let options = answer_keys(item, view.state)
                    .iter()
                    .filter(|key| key.key.is_ascii_digit())
                    .count();
                if options == 0 || view.state.is_stale(item) {
                    lines.push(Line::Options(index, None));
                }
                for option in 0..options {
                    lines.push(Line::Options(index, Some(option)));
                }
            }
        }
        if selected {
            if let Some(compose) = view
                .state
                .compose
                .as_ref()
                .filter(|compose| compose.at.0 == item.key)
            {
                for row in 0..compose.editor.rows() {
                    lines.push(Line::Editor(index, row));
                }
                lines.push(Line::Detail(
                    index,
                    "ctrl+s send  enter new line  ctrl+e $EDITOR  esc keep draft".into(),
                ));
            }
            if view.state.detail {
                for fact in &item.facts {
                    lines.push(Line::Detail(index, fact.clone()));
                }
                let plan = (item.kind == ItemKind::Plan)
                    .then(|| view.state.plan_of(item))
                    .flatten();
                match (plan, view.state.screen_of(item)) {
                    (Some(plan), _) => match &plan.result {
                        Some(Ok((_, text))) => {
                            for line in text.lines().take(PLAN_LINES) {
                                lines.push(Line::Detail(index, line.trim_end().to_owned()));
                            }
                        }
                        Some(Err(error)) => {
                            lines.push(Line::Detail(index, format!("plan not read: {error}")))
                        }
                        None => lines.push(Line::Detail(index, "reading the plan…".into())),
                    },
                    (None, Some(Ok(text))) => {
                        let screen = text.lines().map(str::trim_end).collect::<Vec<_>>();
                        let end = screen
                            .iter()
                            .rposition(|line| !line.is_empty())
                            .map_or(0, |i| i + 1);
                        for line in &screen[end.saturating_sub(SCREEN_LINES)..end] {
                            lines.push(Line::Detail(index, (*line).to_owned()));
                        }
                    }
                    (None, Some(Err(error))) => {
                        lines.push(Line::Detail(index, format!("screen not read: {error}")))
                    }
                    (None, None)
                        if matches!(
                            item.kind,
                            ItemKind::Permission
                                | ItemKind::Question
                                | ItemKind::Plan
                                | ItemKind::Dialog
                        ) =>
                    {
                        lines.push(Line::Detail(index, "reading the screen…".into()))
                    }
                    (None, None) => {}
                }
            }
        }
        if answers {
            lines.push(Line::Actions(index));
        }
    }
    lines
}

/// Draws the panel; returns its hit map and the list scroll used.
fn draw(buffer: &mut Buffer, area: Rect, view: &View) -> (InboxHits, usize) {
    let palette = view.palette;
    let state = view.state;
    let mut hits = InboxHits {
        area,
        ..InboxHits::default()
    };
    if area.width < 4 || area.height < 3 {
        return (hits, 0);
    }
    let base = Style::default().fg(palette.text).bg(palette.sidebar_bg);
    let dim = Style::default().fg(palette.overlay0).bg(palette.sidebar_bg);
    let accent = Style::default()
        .fg(palette.accent)
        .bg(palette.sidebar_bg)
        .add_modifier(Modifier::BOLD);
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.reset();
                cell.set_symbol(" ").set_style(base);
            }
        }
    }
    // Left border: the resize handle; accent while the inbox has focus.
    let border_style = if state.focused {
        accent
    } else {
        Style::default()
            .fg(palette.surface_dim)
            .bg(palette.sidebar_bg)
    };
    for y in area.y..area.bottom() {
        if let Some(cell) = buffer.cell_mut((area.x, y)) {
            cell.set_symbol("│").set_style(border_style);
        }
    }
    hits.border = Rect::new(area.x, area.y, 1, area.height);
    let left = area.x + 2;
    let right = area.right().saturating_sub(1);
    let narrow = area.width < NARROW_WIDTH;

    // Header: title and counts, tabs, grouping.
    let mut y = area.y;
    let mut x = put(buffer, left, y, right, "Inbox", accent);
    let counts = format!("  {} waiting · {} done", view.waiting, view.done);
    put(buffer, x, y, right, &counts, dim);
    let group_label = if narrow { "≡" } else { "≡ group" };
    let group_x = right.saturating_sub(display_width(group_label));
    put(
        buffer,
        group_x,
        y,
        right,
        group_label,
        if view.grouped { accent } else { dim },
    );
    hits.group = Rect::new(group_x, y, display_width(group_label), 1);
    let tabs = [
        (InboxTab::Waiting, "Waiting", "W"),
        (InboxTab::Done, "Done", "D"),
        (InboxTab::All, "All", "A"),
    ];
    let tab_text = |(tab, long, short): &(InboxTab, &str, &str)| {
        let label = if narrow { *short } else { *long };
        if *tab == state.tab {
            format!("[{label}]")
        } else {
            format!(" {label} ")
        }
    };
    let tabs_width: u16 = tabs.iter().map(|tab| display_width(&tab_text(tab))).sum();
    x = group_x.saturating_sub(tabs_width + 2).max(left);
    for tab in &tabs {
        let text = tab_text(tab);
        let style = if tab.0 == state.tab { accent } else { dim };
        let end = put(buffer, x, y, group_x, &text, style);
        hits.tabs.push((Rect::new(x, y, end - x, 1), tab.0));
        x = end;
    }
    y += 1;

    // Filter chip from a sidebar click.
    if let Some(filter) = &state.filter {
        let label = match filter {
            InboxFilter::Project(project) if project == OTHER => "Other".to_owned(),
            InboxFilter::Project(project) => project.clone(),
            InboxFilter::Workspace { workspace_id, .. } => view
                .items
                .iter()
                .find(|item| &item.workspace_id == workspace_id)
                .map_or_else(|| workspace_id.clone(), |item| item.workspace.clone()),
        };
        let text = format!(" {label} ✕ ");
        let chip = Style::default().fg(palette.text).bg(palette.surface0);
        let end = put(buffer, left, y, right, &text, chip);
        hits.chip = Rect::new(left, y, end - left, 1);
        y += 1;
    }

    // Footer: keys for the selected item.
    let selected = view
        .items
        .iter()
        .find(|item| state.selected.as_ref() == Some(&item.key));
    let mut footer: Vec<String> = if state.compose.is_some() {
        vec!["ctrl+s send".into(), "esc keep draft".into()]
    } else {
        selected
            .map(|item| {
                answer_keys(item, state)
                    .into_iter()
                    .filter(|key| !key.key.is_ascii_digit())
                    .map(|key| format!("{} {}", key.key, key.label))
                    .collect()
            })
            .unwrap_or_default()
    };
    if state.compose.is_none() {
        footer.extend(["enter jump".into(), "space more".into()]);
        if selected.is_some_and(|item| !item.kind.waiting()) {
            footer.push("d dismiss".into());
        }
        footer.extend(["z snooze".into(), "m mute".into(), "? keys".into()]);
    }
    let mut footers = vec![footer.join("  ")];
    if state.show_keys {
        footers.insert(
            0,
            "j/k move  tab filter  g group  D dismiss done  esc back".to_owned(),
        );
        footers.insert(
            1,
            "1-4 option  y yes  a always  n no  r reply or note  o other  p plan".to_owned(),
        );
    }
    let footer_top = area.bottom().saturating_sub(footers.len() as u16);
    for (offset, text) in footers.iter().enumerate() {
        put(buffer, left, footer_top + offset as u16, right, text, dim);
    }

    // The list.
    let list = Rect::new(area.x + 1, y, area.width - 1, footer_top.saturating_sub(y));
    hits.list = list;
    if view.items.is_empty() {
        let text = match state.tab {
            InboxTab::Waiting => "Nothing is waiting on you.",
            InboxTab::Done => "Nothing finished to review.",
            InboxTab::All => "Nothing needs you.",
        };
        if list.height > 0 {
            put(buffer, left, list.y, right, text, dim);
        }
        return (hits, 0);
    }
    let lines = lines_for(view, narrow);
    let height = usize::from(list.height);
    let max_scroll = lines.len().saturating_sub(height);
    let mut scroll = state.scroll.min(max_scroll);
    // Keep the selected item's lines in view, unless the wheel moved them.
    if let Some(index) = view
        .items
        .iter()
        .position(|item| state.follow && state.selected.as_ref() == Some(&item.key))
    {
        let first = lines.iter().position(|line| line_item(line) == Some(index));
        let last = lines
            .iter()
            .rposition(|line| line_item(line) == Some(index));
        if let (Some(first), Some(last)) = (first, last) {
            if first < scroll {
                scroll = first;
            } else if last >= scroll + height {
                scroll = (last + 1).saturating_sub(height).min(first);
            }
        }
    }
    for (offset, line) in lines.iter().skip(scroll).take(height).enumerate() {
        let y = list.y + offset as u16;
        draw_line(
            buffer, view, line, &lines, left, right, narrow, y, &mut hits,
        );
    }
    (hits, scroll)
}

fn line_item(line: &Line) -> Option<usize> {
    match line {
        Line::Group(_) => None,
        Line::Main(index)
        | Line::Meta(index)
        | Line::Options(index, _)
        | Line::Editor(index, _)
        | Line::Actions(index)
        | Line::Detail(index, _) => Some(*index),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_line(
    buffer: &mut Buffer,
    view: &View,
    line: &Line,
    lines: &[Line],
    left: u16,
    right: u16,
    narrow: bool,
    y: u16,
    hits: &mut InboxHits,
) {
    let palette = view.palette;
    let bg = |selected: bool| {
        if selected {
            palette.active_row_bg
        } else {
            palette.sidebar_bg
        }
    };
    let Some(index) = line_item(line) else {
        if let Line::Group(name) = line {
            let style = Style::default()
                .fg(palette.subtext0)
                .bg(palette.sidebar_bg)
                .add_modifier(Modifier::BOLD);
            put(buffer, left, y, right, name, style);
        }
        return;
    };
    let item = &view.items[index];
    let selected = view.state.selected.as_ref() == Some(&item.key);
    // Only the selected item's first line is shaded; the frame marks the
    // rest, so an unfolded item is not one large block of colour.
    let row_bg = bg(selected && matches!(line, Line::Main(_)));
    let base = Style::default().fg(palette.text).bg(row_bg);
    let dim = Style::default().fg(palette.overlay0).bg(row_bg);
    let row = Rect::new(left.saturating_sub(1), y, right.saturating_sub(left) + 2, 1);
    buffer.set_style(row, Style::default().bg(row_bg));
    hits.rows.push((row, item.key.clone()));
    // Frame: ╭ │ ╰ around the selected item's lines.
    if selected {
        let first = lines
            .iter()
            .position(|other| line_item(other) == Some(index));
        let last = lines
            .iter()
            .rposition(|other| line_item(other) == Some(index));
        let here = lines.iter().position(|other| other == line);
        let frame = if first == last {
            " "
        } else if here == first {
            "╭"
        } else if here == last {
            "╰"
        } else {
            "│"
        };
        put(
            buffer,
            left.saturating_sub(1),
            y,
            right,
            frame,
            Style::default().fg(palette.accent).bg(row_bg),
        );
    }
    let text = left + 1;
    let snooze = view
        .snooze
        .as_ref()
        .filter(|(key, _)| key == &item.key)
        .map(|(_, label)| *label);
    let muted = view
        .muted
        .iter()
        .any(|entry| projects::same_workspace(entry, &item.workspace_key));
    let chip_and_age = || {
        let mut right_text = item.chip();
        if muted {
            right_text = format!("muted · {right_text}");
        }
        if let Some(label) = snooze {
            right_text = format!("{label} · {right_text}");
        }
        let age = item.age.map(projects::format_age).unwrap_or_default();
        format!("{right_text} {age:>3}")
    };
    match line {
        Line::Main(_) => {
            let glyph = Style::default()
                .fg(item_color(item.kind, palette))
                .bg(row_bg)
                .add_modifier(Modifier::BOLD);
            let mut x = put(buffer, text, y, right, item.kind.glyph(), glyph);
            x = put(buffer, x, y, right, " ", base);
            let name_start = x;
            let name_style = if item.marked {
                dim
            } else {
                base.add_modifier(Modifier::BOLD)
            };
            x = put(buffer, x, y, right, &item.workspace, name_style);
            hits.jumps.push((
                Rect::new(name_start, y, x - name_start, 1),
                item.key.clone(),
            ));
            if let Some(vendor) = &item.vendor {
                x = put(buffer, x, y, right, &format!(" {vendor}"), dim);
            }
            let close = view.state.hover.as_ref() == Some(&item.key) && !item.kind.waiting();
            // Narrow rows carry the chip and age on their Meta line.
            let right_text = if !narrow {
                chip_and_age()
            } else {
                String::new()
            };
            let right_text = if close {
                format!("{right_text} ✕")
            } else {
                right_text
            };
            let right_width = display_width(&right_text);
            let summary_end = right.saturating_sub(right_width + 1);
            put(
                buffer,
                x + 2,
                y,
                summary_end.max(x + 2),
                &item.summary,
                if item.marked { dim } else { base },
            );
            if right_width > 0 {
                put(
                    buffer,
                    right - right_width.min(right - text),
                    y,
                    right,
                    &right_text,
                    dim,
                );
            }
            if close {
                hits.closes
                    .push((Rect::new(right - 1, y, 1, 1), item.key.clone()));
            }
        }
        Line::Meta(_) => {
            put(buffer, text + 2, y, right, &chip_and_age(), dim);
        }
        Line::Options(_, slot) => {
            // Options appear only once the screen shows them (section 9),
            // one per row so long labels keep their key and click target.
            let keys = answer_keys(item, view.state);
            let option =
                slot.and_then(|slot| keys.iter().filter(|key| key.key.is_ascii_digit()).nth(slot));
            if let Some(option) = option {
                put_key(buffer, text + 2, y, right, option, view, item, row_bg, hits);
            } else if view.state.is_stale(item) {
                let changed = Style::default().fg(palette.yellow).bg(row_bg);
                put(buffer, text + 2, y, right, answer::CHANGED, changed);
            } else {
                // Unconfirmed is not changed: the dialog may not be drawn
                // yet, or its labels differ from the hook's.
                let note = match view.state.screen_of(item) {
                    None => "reading the screen…".to_owned(),
                    Some(Err(error)) => format!("screen not read: {error}"),
                    Some(Ok(_)) => "options not confirmed on screen yet".to_owned(),
                };
                put(buffer, text + 2, y, right, &note, dim);
            }
        }
        Line::Editor(_, row) => {
            if let Some(compose) = view.state.compose.as_ref() {
                let style = Style::default().fg(palette.text).bg(palette.surface0);
                let x = text + 2;
                let width = right.saturating_sub(x);
                buffer.set_style(Rect::new(x, y, width, 1), style);
                compose
                    .editor
                    .render_row(buffer, (x, y), width, *row, style);
            }
        }
        Line::Detail(_, detail) => {
            put(buffer, text + 2, y, right, detail, dim);
        }
        Line::Actions(_) => {
            let more = if view.state.detail {
                "space less"
            } else {
                "space more"
            };
            let mut x = text + 2;
            let confirm = view
                .state
                .confirm
                .as_ref()
                .filter(|(at, _)| *at == item_at(item))
                .map(|(_, ch)| *ch);
            let keys = answer_keys(item, view.state);
            if item.kind.waiting() && view.state.is_stale(item) {
                let changed = Style::default().fg(palette.yellow).bg(row_bg);
                x = put(buffer, x, y, right, answer::CHANGED, changed);
                x = put(buffer, x, y, right, "  ", dim);
            } else if let Some(key) = confirm.and_then(|ch| keys.iter().find(|key| key.key == ch)) {
                let style = Style::default()
                    .fg(palette.accent)
                    .bg(row_bg)
                    .add_modifier(Modifier::BOLD);
                let text = format!("press {} again: {}", key.key, key.label);
                let start = x;
                x = put(buffer, x, y, right, &text, style);
                // A click on the prompt is the second press.
                if x > start {
                    hits.keys
                        .push((Rect::new(start, y, x - start, 1), item.key.clone(), key.key));
                }
                x = put(buffer, x, y, right, "  ", dim);
            } else if view.state.compose.is_none() {
                for key in keys.iter().filter(|key| !key.key.is_ascii_digit()) {
                    x = put_key(buffer, x, y, right, key, view, item, row_bg, hits);
                    x = put(buffer, x, y, right, "  ", dim);
                }
            }
            let x = if selected {
                let x = put(buffer, x, y, right, "enter jump  ", dim);
                let start = x;
                let x = put(buffer, x, y, right, more, dim);
                if x > start {
                    hits.details
                        .push((Rect::new(start, y, x - start, 1), item.key.clone()));
                }
                put(buffer, x, y, right, "  ", dim)
            } else {
                x
            };
            let end = put(
                buffer,
                x,
                y,
                right,
                "↗",
                Style::default().fg(palette.accent).bg(row_bg),
            );
            hits.jumps
                .push((Rect::new(x, y, end - x, 1), item.key.clone()));
        }
        Line::Group(_) => {}
    }
}

/// Draws an answer key as `<key> <label>`, the key in the accent colour,
/// and records it as a click target; returns the end column.
#[allow(clippy::too_many_arguments)]
fn put_key(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    right: u16,
    key: &AnswerKey,
    view: &View,
    item: &Item,
    bg: ratatui::style::Color,
    hits: &mut InboxHits,
) -> u16 {
    let palette = view.palette;
    let accent = Style::default()
        .fg(palette.accent)
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    let base = Style::default().fg(palette.text).bg(bg);
    let start = x;
    let x = put(buffer, x, y, right, &key.key.to_string(), accent);
    let end = put(buffer, x, y, right, &format!(" {}", key.label), base);
    if end > start {
        hits.keys.push((
            Rect::new(start, y, end - start, 1),
            item.key.clone(),
            key.key,
        ));
    }
    end
}

/// Writes `text` from `x`, clipped at `right`; returns the end column.
fn put(buffer: &mut Buffer, x: u16, y: u16, right: u16, text: &str, style: Style) -> u16 {
    if x >= right {
        return x;
    }
    let width = display_width(text).min(right - x);
    put_text(buffer, x, y, width, text, style);
    x + width
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::AgentStatus;

    const NOW: u64 = 1_800_000_000;

    fn agent(pane: &str, status: AgentStatus, tokens: &[(&str, String)]) -> ClientShellAgent {
        ClientShellAgent {
            pane_id: pane.into(),
            workspace_id: "w1".into(),
            tab_id: "tab_1".into(),
            name: None,
            display_agent: None,
            agent: Some("claude".into()),
            title: Some(format!("title {pane}")),
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: status,
            state_change_seq: 7,
            state_labels: Vec::new(),
            tokens: tokens
                .iter()
                .map(|(name, value)| ((*name).to_owned(), value.clone()))
                .collect(),
            focused: false,
        }
    }

    fn endpoint(agents: Vec<ClientShellAgent>) -> ClientShellEndpoint {
        let mut snapshot = super::super::tests::snapshot();
        let mut workspace = snapshot.workspaces[0].clone();
        workspace.workspace_id = "w1".into();
        workspace.label = "migrate-db".into();
        snapshot.workspaces = vec![workspace];
        snapshot.panes.clear();
        snapshot.agents = agents;
        let mut endpoint = super::super::endpoints::local_endpoint();
        endpoint.label = "mato".into();
        endpoint.snapshot = Some(Box::new(snapshot));
        endpoint
    }

    fn item(kind: ItemKind, age: Option<u64>, project: usize, pane: &str) -> Item {
        Item {
            key: ItemKey {
                endpoint_id: ClientEndpointId::Local,
                pane_id: pane.into(),
            },
            kind,
            seq: 1,
            wait_id: String::new(),
            workspace_id: "w1".into(),
            tab_id: "tab_1".into(),
            workspace: pane.into(),
            workspace_key: format!("local/w1:{pane}"),
            project: format!("p{project}"),
            project_rank: project,
            machine: "Local".into(),
            vendor: None,
            decides: true,
            summary: String::new(),
            wait_text: String::new(),
            options: Vec::new(),
            facts: Vec::new(),
            age,
            marked: false,
        }
    }

    #[test]
    fn items_sort_by_kind_then_oldest_first() {
        let mut items = vec![
            item(ItemKind::Finished, Some(5000), 0, "done-old"),
            item(ItemKind::Asks, Some(60), 1, "asks"),
            item(ItemKind::Permission, Some(30), 1, "perm-new"),
            item(ItemKind::Stuck, Some(900), 0, "stuck"),
            item(ItemKind::Permission, Some(300), 0, "perm-old"),
            item(ItemKind::Permission, None, 1, "perm-unknown"),
            item(ItemKind::Dialog, Some(10), 0, "dialog"),
        ];
        sort(&mut items, false);
        let order = items
            .iter()
            .map(|item| item.workspace.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            [
                "perm-unknown",
                "perm-old",
                "perm-new",
                "asks",
                "dialog",
                "stuck",
                "done-old"
            ]
        );
        // Grouped: by project (sidebar order), the same order inside.
        sort(&mut items, true);
        let order = items
            .iter()
            .map(|item| item.workspace.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            [
                "perm-old",
                "dialog",
                "stuck",
                "done-old",
                "perm-unknown",
                "perm-new",
                "asks"
            ]
        );
    }

    #[test]
    fn keys_within_the_focus_drop_window_are_dropped() {
        let mut state = InboxState::default();
        let start = Instant::now();
        assert!(!state.accepts_key(start), "not focused");
        state.focus(start);
        assert!(!state.accepts_key(start));
        assert!(!state.accepts_key(start + FOCUS_DROP - Duration::from_millis(1)));
        assert!(state.accepts_key(start + FOCUS_DROP));
        // Focusing again while focused keeps the first time.
        state.focus(start + FOCUS_DROP);
        assert!(state.accepts_key(start + FOCUS_DROP));
        state.focused = false;
        state.focus(start + Duration::from_secs(5));
        assert!(!state.accepts_key(start + Duration::from_secs(5) + Duration::from_millis(100)));
        // The terminal regaining focus starts the window again.
        let back = start + Duration::from_secs(9);
        state.outer_focus_gained(back);
        assert!(!state.accepts_key(back + Duration::from_millis(100)));
        assert!(state.accepts_key(back + FOCUS_DROP));
    }

    #[test]
    fn the_panel_shrinks_the_panes_or_opens_over_them_when_narrow() {
        // 200 columns, 30-column sidebar: 40% is 80 columns beside 90 of panes.
        assert_eq!(
            place(200, 50, 30, 170, 0.4),
            (90, Rect::new(120, 0, 80, 50), false)
        );
        // The share never goes below 48 columns.
        assert_eq!(
            place(100, 40, 0, 100, 0.2),
            (52, Rect::new(52, 0, 48, 40), false)
        );
        // Fewer than 32 columns left for panes: over the panes, panes keep
        // their width.
        assert_eq!(
            place(100, 40, 30, 70, 0.4),
            (70, Rect::new(52, 0, 48, 40), true)
        );
        // Narrower than the panel: the panel takes the whole main area.
        assert_eq!(
            place(60, 20, 20, 40, 0.4),
            (40, Rect::new(20, 0, 40, 20), true)
        );
        // Dragging the border: clamped to 48 columns and to 32 for panes.
        assert_eq!(share_at(200, 30, 170, 120), 0.4);
        assert_eq!(share_at(200, 30, 170, 190), 0.24);
        assert_eq!(share_at(200, 30, 170, 40), 0.69);
    }

    #[test]
    fn options_get_a_row_each_and_a_grant_prompt_is_clickable() {
        let mut question = item(ItemKind::Question, Some(5), 0, "q");
        question.wait_id = "cd34ef56".into();
        question.wait_text = "Which layout?".into();
        question.options = vec![
            "Keep the current single-pane layout (Recommended)".into(),
            "Two side-by-side panes".into(),
            "Three panes".into(),
        ];
        let mut permission = item(ItemKind::Permission, Some(9), 0, "p");
        permission.wait_id = "ab12cd34".into();
        let screen = "Which layout?\n❯ 1. Keep the current single-pane layout (Recommended)\n  2. Two side-by-side panes\n  3. Three panes\n";
        let palette = Palette::catppuccin();
        let draw_with = |items: &[Item], state: &InboxState| {
            let view = View {
                items,
                waiting: items.len(),
                done: 0,
                state,
                palette: &palette,
                snooze: None,
                muted: &[],
                grouped: false,
            };
            let mut buffer = Buffer::empty(Rect::new(0, 0, 48, 20));
            draw(&mut buffer, Rect::new(0, 0, 48, 20), &view).0
        };

        // Before a read confirms them, no option row: a neutral note.
        let mut state = InboxState {
            open: true,
            selected: Some(question.key.clone()),
            ..InboxState::default()
        };
        let items = [question.clone()];
        let digit = |(_, _, ch): &&(Rect, ItemKey, char)| ch.is_ascii_digit();
        assert!(!draw_with(&items, &state).keys.iter().any(|key| digit(&key)));
        state.screen.insert(
            question.key.clone(),
            ScreenRead {
                at: item_at(&question),
                text: Ok(screen.into()),
            },
        );
        let hits = draw_with(&items, &state);
        let digits: Vec<_> = hits
            .keys
            .iter()
            .filter(digit)
            .map(|(rect, _, ch)| (rect.y, *ch))
            .collect();
        assert_eq!(digits.len(), 3, "{digits:?}");
        assert!(digits.windows(2).all(|pair| pair[0].0 < pair[1].0));
        // Unselected, a waiting item still shows its answers: one click
        // answers it.
        state.selected = None;
        let hits = draw_with(&items, &state);
        assert_eq!(hits.keys.iter().filter(digit).count(), 3);
        assert!(hits.details.is_empty(), "hints only on the selected item");

        // `a` pressed once on a permission: the prompt is the second click.
        let items = [permission.clone()];
        let state = InboxState {
            open: true,
            selected: Some(permission.key.clone()),
            confirm: Some((item_at(&permission), 'a')),
            ..InboxState::default()
        };
        let hits = draw_with(&items, &state);
        assert!(hits.keys.iter().any(|(_, _, ch)| *ch == 'a'));
    }

    #[test]
    fn narrow_panels_move_the_chip_to_a_second_line() {
        let items = vec![item(ItemKind::Finished, Some(120), 0, "pane-layout")];
        let state = InboxState {
            open: true,
            ..InboxState::default()
        };
        let palette = Palette::catppuccin();
        let view = View {
            items: &items,
            waiting: 0,
            done: 1,
            state: &state,
            palette: &palette,
            snooze: None,
            muted: &[],
            grouped: false,
        };
        let render = |width: u16| {
            let mut buffer = Buffer::empty(Rect::new(0, 0, width, 10));
            draw(&mut buffer, Rect::new(0, 0, width, 10), &view);
            (0..10)
                .map(|y| {
                    (0..width)
                        .map(|x| buffer[(x, y)].symbol().to_owned())
                        .collect::<String>()
                        .trim_end()
                        .to_owned()
                })
                .collect::<Vec<_>>()
        };
        let wide = render(70);
        assert!(wide[0].contains("[Waiting] Done  All"), "{:?}", wide[0]);
        assert!(
            wide[1].contains("pane-layout") && wide[1].ends_with("p0·local  2m"),
            "{:?}",
            wide[1]
        );
        // 60 columns is not narrow: the chip stays on the main line.
        let edge = render(60);
        assert!(edge[1].ends_with("p0·local  2m"), "{:?}", edge[1]);
        let narrow = render(50);
        assert!(narrow[0].contains("[W] D  A"), "{:?}", narrow[0]);
        assert!(
            narrow[1].contains("pane-layout") && !narrow[1].contains("p0·local"),
            "{:?}",
            narrow[1]
        );
        assert!(narrow[2].contains("p0·local  2m"), "{:?}", narrow[2]);
    }

    #[test]
    fn marks_mutes_and_the_workspace_threshold_decide_the_item() {
        let quiet = format!("working|{}", NOW - 20 * 60);
        let mut layout = ProjectLayout::default();
        let stuck = agent("p1", AgentStatus::Working, &[("drovr_state", quiet)]);
        let endpoint = endpoint(vec![stuck.clone()]);
        assert_eq!(
            agent_item(&layout, &endpoint, &stuck, NOW),
            Some(ItemKind::Stuck)
        );
        // A 45-minute threshold for this workspace (legacy `machine/label` key).
        layout
            .inbox
            .stuck_minutes_by_workspace
            .insert("mato/migrate-db".into(), 45);
        assert_eq!(agent_item(&layout, &endpoint, &stuck, NOW), None);
        layout.inbox.stuck_minutes_by_workspace.clear();
        layout.inbox.stuck_minutes = Some(30);
        assert_eq!(agent_item(&layout, &endpoint, &stuck, NOW), None);
        layout.inbox.stuck_minutes = None;
        // Muted: stuck hides, a waiting item still shows.
        layout.inbox.muted.push("mato/w1:migrate-db".into());
        assert_eq!(agent_item(&layout, &endpoint, &stuck, NOW), None);
        let blocked = agent("p2", AgentStatus::Blocked, &[]);
        assert_eq!(
            agent_item(&layout, &endpoint, &blocked, NOW),
            Some(ItemKind::Dialog)
        );
        layout.inbox.muted.clear();

        // Dismissed at this seq: hidden until the state changes; never a
        // waiting item.
        let done = agent("p3", AgentStatus::Done, &[("drovr_dis", "7".into())]);
        assert_eq!(agent_item(&layout, &endpoint, &done, NOW), None);
        let mut next = done.clone();
        next.state_change_seq = 8;
        assert_eq!(
            agent_item(&layout, &endpoint, &next, NOW),
            Some(ItemKind::Finished)
        );
        // A dismissed kind hides only that kind at the seq.
        assert!(marked(Some("7|limit"), None, ItemKind::Limit, 7, "", NOW));
        assert!(!marked(Some("7|limit"), None, ItemKind::Stuck, 7, "", NOW));
        assert!(marked(Some("7"), None, ItemKind::Stuck, 7, "", NOW));
        let waiting = agent("p4", AgentStatus::Blocked, &[("drovr_dis", "7".into())]);
        assert_eq!(
            agent_item(&layout, &endpoint, &waiting, NOW),
            Some(ItemKind::Dialog)
        );

        // Snoozed: until the end, while the state and the request stay.
        let wait = ("drovr_wait", "permission|ab12cd34||git push".to_owned());
        let snoozed = |value: String| {
            agent(
                "p5",
                AgentStatus::Blocked,
                &[wait.clone(), ("drovr_snz", value)],
            )
        };
        let hidden = snoozed(format!("{}|7|ab12cd34", NOW + 60));
        assert_eq!(agent_item(&layout, &endpoint, &hidden, NOW), None);
        let over = snoozed(format!("{}|7|ab12cd34", NOW));
        assert_eq!(
            agent_item(&layout, &endpoint, &over, NOW),
            Some(ItemKind::Permission)
        );
        let new_prompt = snoozed(format!("{}|7|ffff0000", NOW + 60));
        assert_eq!(
            agent_item(&layout, &endpoint, &new_prompt, NOW),
            Some(ItemKind::Permission)
        );
    }

    #[test]
    fn prefix_i_shrinks_the_panes_and_keeps_keys_from_them() {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
        state.set_snapshot(Box::new(super::super::tests::snapshot()));
        let closed = state.surface_size(200, 50);
        state.handle_input_bytes(&[0x02]);
        let open = state.handle_input_bytes(b"i");
        assert!(state.inbox.open && state.inbox.focused && open.resize);
        let beside = state.surface_size(200, 50);
        assert_eq!(beside.rows, closed.rows);
        assert_eq!(closed.cols - beside.cols, 80, "40% of 200 columns");
        // A key in the focus-drop window is dropped, not sent to the pane.
        let typed = state.handle_input_bytes(b"x");
        assert!(typed.requests.is_empty());
        assert!(state.inbox.open);
        // Narrow screen: the panel opens over the panes, which keep their
        // width.
        assert_eq!(state.surface_size(100, 40), {
            state.inbox.open = false;
            let size = state.surface_size(100, 40);
            state.inbox.open = true;
            size
        });
        // Focused: prefix i closes it and the panes get the width back.
        state.handle_input_bytes(&[0x02]);
        state.handle_input_bytes(b"i");
        assert!(!state.inbox.open && !state.inbox.focused);
        assert_eq!(state.surface_size(200, 50), closed);
        // Open but not focused: prefix i focuses it.
        state.handle_input_bytes(&[0x02]);
        state.handle_input_bytes(b"i");
        state.inbox.focused = false;
        state.handle_input_bytes(&[0x02]);
        state.handle_input_bytes(b"i");
        assert!(state.inbox.open && state.inbox.focused);
    }

    #[test]
    fn the_focused_inbox_takes_keys_from_a_pane_in_copy_mode() {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
        state.set_snapshot(Box::new(super::super::tests::snapshot()));
        let pane_id = state.focused_pane_id().expect("focused pane");
        state.mode = ClientShellMode::Copy;
        state.copy_mode = Some(ClientCopyModeState {
            pane_id,
            content_revision: 0,
            geometry: (80, 24),
            alternate_screen_active: false,
            cursor: crate::api::schema::PaneTextPoint { row: 0, col: 0 },
            offset_from_bottom: 0,
            max_offset_from_bottom: 0,
            entry_offset_from_bottom: 0,
            selection: None,
            search_prompt: None,
            search_query: String::new(),
            search_direction: None,
            search_matches: Vec::new(),
            search_total: 0,
            search_current: None,
            search_current_global: None,
            search_generation: 0,
            copy_after_search: false,
        });
        state.handle_input_bytes(&[0x02]);
        state.handle_input_bytes(b"i");
        assert!(state.inbox.open && state.inbox.focused);
        state.inbox.focused_at = None;
        // Esc acts on the inbox (blurs it), not on copy mode, which resumes.
        state.handle_input_bytes(&[0x1b]);
        assert!(!state.inbox.focused);
        assert!(state.copy_mode.is_some());
        assert_eq!(state.mode, ClientShellMode::Copy);
    }

    fn press(
        state: &mut ClientShellState,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        state.handle_inbox_key(
            &crate::input::TerminalKey::new(code, modifiers),
            &mut outcome,
        );
        outcome
    }

    fn tasks(outcome: &ClientShellInput) -> Vec<answer::Answer> {
        outcome
            .actions
            .iter()
            .filter_map(|action| match action {
                ClientShellAction::InboxTask {
                    task: answer::Task::Answer(answer),
                    ..
                } => Some(answer.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn answer_keys_confirm_grants_wait_for_the_screen_and_keep_drafts() {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
        let mut snapshot = super::super::tests::snapshot();
        let in_ws = |mut agent: ClientShellAgent| {
            agent.workspace_id = "ws_1".into();
            agent
        };
        snapshot.agents = vec![
            in_ws(agent(
                "pane_1",
                AgentStatus::Blocked,
                &[("drovr_wait", "permission|ab12cd34||Bash git push".into())],
            )),
            in_ws(agent(
                "pane_2",
                AgentStatus::Blocked,
                &[
                    (
                        "drovr_wait",
                        "question|cd34ef56||Which layout for the inbox?".into(),
                    ),
                    ("drovr_o1", "one pane".into()),
                    ("drovr_o2", "two panes".into()),
                ],
            )),
        ];
        state.set_snapshot(Box::new(snapshot));
        state.inbox.open = true;
        state.inbox.focused = true;
        state.ensure_inbox_selection();
        let selected =
            |state: &ClientShellState| state.inbox.selected.clone().map(|key| key.pane_id);
        assert_eq!(selected(&state).as_deref(), Some("pane_1"));

        // `a` grants a lasting permission: the first press only asks.
        assert!(tasks(&press(&mut state, KeyCode::Char('a'), KeyModifiers::NONE)).is_empty());
        assert!(state.inbox.confirm.is_some());
        let sent = tasks(&press(&mut state, KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(
            sent,
            [answer::Answer {
                pane_id: "pane_1".into(),
                seq: 7,
                wait_id: "ab12cd34".into(),
                action: Action::Decide {
                    decision: Decision::Always
                },
            }]
        );
        // The answered item hides and the cursor moves to the next waiting
        // item; focus stays in the inbox.
        assert_eq!(selected(&state).as_deref(), Some("pane_2"));
        assert!(state.inbox.focused);
        assert!(state
            .visible_inbox_items()
            .iter()
            .all(|item| item.key.pane_id != "pane_1"));

        // Question options wait for the screen read the tick asks for.
        assert!(tasks(&press(&mut state, KeyCode::Char('1'), KeyModifiers::NONE)).is_empty());
        let mut outcome = ClientShellInput::default();
        state.tick_inbox(&mut outcome);
        let Some(ClientShellAction::InboxRequest { reply, .. }) = outcome.actions.first() else {
            panic!("expected a screen read");
        };
        let screen =
            "Which layout for the inbox?\n❯ 1. two panes\n  2. one pane\n  3. Type something.\n";
        assert!(state.receive_inbox_reply(
            reply.clone(),
            Ok(serde_json::json!({ "result": { "read": { "text": screen } } }))
        ));
        // Option 1 is "one pane", which the screen numbers 2: the check
        // carries the label, and the machine sends the on-screen number.
        let sent = tasks(&press(&mut state, KeyCode::Char('1'), KeyModifiers::NONE));
        assert_eq!(
            sent[0].action,
            Action::Keys {
                text: "Which layout for the inbox?".into(),
                choice: Choice::Label("one pane".into()),
            }
        );
        // A transport failure brings the item back with its keys.
        let reply = InboxReply::Answer {
            key: state
                .inbox_items()
                .into_iter()
                .find(|item| item.key.pane_id == "pane_2")
                .expect("question item")
                .key,
            seq: 7,
            wait_id: "cd34ef56".into(),
            draft: None,
        };
        state.receive_inbox_reply(reply.clone(), Err("connection refused".into()));
        state.ensure_inbox_selection();
        assert_eq!(selected(&state).as_deref(), Some("pane_2"));

        // `o`: a note in the editor; enter adds a line, ctrl+s sends it as a
        // hook decision.
        press(&mut state, KeyCode::Char('o'), KeyModifiers::NONE);
        assert!(state.inbox.compose.is_some());
        for ch in "a grid".chars() {
            press(&mut state, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
        assert!(state.insert_inbox_text("please"));
        let sent = tasks(&press(
            &mut state,
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(
            sent[0].action,
            Action::Decide {
                decision: Decision::Deny("a grid\nplease".into())
            }
        );
        // The prompt changed in the terminal: the item comes back without
        // answer keys, and the note is kept as its draft.
        let InboxReply::Answer { key, .. } = &reply else {
            unreachable!()
        };
        state.receive_inbox_reply(
            InboxReply::Answer {
                key: key.clone(),
                seq: 7,
                wait_id: "cd34ef56".into(),
                draft: Some("a grid\nplease".into()),
            },
            Err(answer::CHANGED.into()),
        );
        let item = state
            .visible_inbox_items()
            .into_iter()
            .find(|item| &item.key == key)
            .expect("item is back");
        assert!(answer_keys(&item, &state.inbox).is_empty());
        assert_eq!(
            state.inbox.drafts.get(key).map(String::as_str),
            Some("a grid\nplease")
        );
    }

    #[test]
    fn settings_round_trip_through_sidebar_toml() {
        let text = r#"
[inbox]
stuck_minutes = 10
width = 0.35
muted = ["mato/w2:api-tests"]

[inbox.stuck_minutes_by_workspace]
"mato/migrate-db" = 45
"#;
        let layout: ProjectLayout = toml::from_str(text).expect("inbox settings");
        assert_eq!(layout.inbox.stuck_secs("mato/w7:migrate-db"), 45 * 60);
        assert_eq!(layout.inbox.stuck_secs("local/w7:migrate-db"), 10 * 60);
        assert_eq!(layout.inbox.share(), 0.35);
        assert!(layout.inbox.is_muted("mato/w2:api-tests"));
        let encoded = toml::to_string_pretty(&layout).expect("encode");
        assert_eq!(
            toml::from_str::<ProjectLayout>(&encoded).expect("decode"),
            layout
        );
        let empty = toml::to_string_pretty(&ProjectLayout::default()).expect("encode");
        assert!(!empty.contains("inbox"), "{empty}");
        // 22:00 local: tomorrow 09:00 is 11 hours away.
        assert_eq!(tomorrow_nine(NOW, 22 * 3600), NOW + 11 * 3600);
        assert_eq!(snooze_end(0, NOW), NOW + 3600);
        assert_eq!(snooze_end(1, NOW), NOW + 4 * 3600);
    }
}
