//! drovr fork: one combined sidebar for 2+ machines.
//!
//! Replaces upstream's "machines" + "agents" split with a single list:
//! project headers (pinned first), then "Other" for everything ungrouped, with
//! agents from every machine under them. Three views: detailed (one row per
//! agent), compact (one line per workspace) and structured (workspace headers,
//! one line per agent with a vendor mark and a state-coloured title, styled
//! after herdr-radar; see `radar.rs`). Layout state lives in `projects.rs`.

mod radar;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use super::agent_signal::{self, AgentSignal, InboxFilter, ItemKind};
use super::projects::{self, Presence, ProjectLayout, OTHER};
use super::render::{display_width, put_right_text, put_text, ShellRenderState};
use super::*;
use ratatui::style::Color;

/// Where a sidebar row points; used for click (focus), drag and right-click.
#[derive(Clone, Debug)]
pub(super) struct RowHit {
    pub(super) rect: Rect,
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) workspace_id: String,
    pub(super) pane_id: Option<String>,
}

/// Where a workspace dropped on the sidebar goes: into `section` (a project
/// name, or [`OTHER`]) just before the workspace `before`, or last.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DropTarget {
    pub(super) section: String,
    pub(super) before: Option<String>,
    /// Dropped on the section header itself.
    pub(super) header: bool,
}

/// A band of sidebar rows that resolves to one [`DropTarget`]; `marker` is the
/// row that shows the insertion line (or the header to highlight).
#[derive(Clone, Debug)]
pub(super) struct DropSlot {
    pub(super) rect: Rect,
    pub(super) target: DropTarget,
    pub(super) marker: u16,
}

/// One drawn row as drop resolution sees it: a section header (`key` None)
/// or a row of the workspace `key`.
struct Placed {
    y: u16,
    height: u16,
    section: String,
    key: Option<String>,
}

/// Drop bands for the drawn rows, top to bottom down to `bottom`:
/// - a section header appends to that section;
/// - a workspace's rows, and the blank lines above them, drop before it;
/// - the lower half of a section's last workspace, and the blank lines after
///   it (or after an empty or collapsed header), append to the section.
fn drop_slots(placed: &[Placed], x: u16, width: u16, bottom: u16) -> Vec<DropSlot> {
    // Consecutive rows of one workspace form a block.
    let mut blocks: Vec<(String, Option<String>, u16, u16)> = Vec::new();
    for row in placed {
        let end = row.y.saturating_add(row.height);
        match blocks.last_mut() {
            Some(block) if row.key.is_some() && block.1 == row.key && block.0 == row.section => {
                block.3 = end;
            }
            _ => blocks.push((row.section.clone(), row.key.clone(), row.y, end)),
        }
    }
    let mut slots = Vec::new();
    let mut push = |top: u16, end: u16, section: &str, before: Option<&String>, header, marker| {
        if end > top {
            slots.push(DropSlot {
                rect: Rect::new(x, top, width, end - top),
                target: DropTarget {
                    section: section.to_owned(),
                    before: before.cloned(),
                    header,
                },
                marker,
            });
        }
    };
    for (index, (section, key, top, end)) in blocks.iter().enumerate() {
        let next = blocks.get(index + 1);
        let limit = next.map_or(bottom, |next| next.2).max(*end);
        // The insertion line for "last": the row right after the section.
        let after = (*end).min(bottom.saturating_sub(1));
        let Some(key) = key else {
            push(*top, *end, section, None, true, *top);
            push(*end, limit, section, None, false, after);
            continue;
        };
        let marker = top.saturating_sub(1).max(placed[0].y);
        match next {
            Some((next_section, Some(next_key), next_top, _)) if next_section == section => {
                push(*top, *end, section, Some(key), false, marker);
                let next_marker = next_top.saturating_sub(1);
                push(*end, limit, section, Some(next_key), false, next_marker);
            }
            _ => {
                let split = top + (end - top).div_ceil(2);
                push(*top, split, section, Some(key), false, marker);
                push(split, limit, section, None, false, after);
            }
        }
    }
    slots
}

/// The drop band under `point`, if any (outside the sidebar there is none).
pub(super) fn drop_slot_at(slots: &[DropSlot], point: (u16, u16)) -> Option<&DropSlot> {
    slots.iter().find(|slot| super::contains(slot.rect, point))
}

enum Row {
    Header {
        key: String,
        label: String,
        pinned: bool,
        collapsed: bool,
        presence: Presence,
        count: usize,
        /// Inbox items in the section, and whether one waits on a prompt.
        items: usize,
        waiting: bool,
        /// Today's active time and tokens ("37m · 1.2M"), shown while peeking.
        usage: Option<String>,
    },
    Agent {
        endpoint: usize,
        workspace_id: String,
        pane_id: String,
        presence: Presence,
        focused: bool,
        stale: bool,
        title: String,
        workspace: Option<String>,
        machine: Option<String>,
        number: Option<usize>,
        kept: bool,
        age: Option<String>,
        faded: bool,
        ctx: Option<String>,
        /// Agent id ("claude", "codex"), for the structured view's mark.
        vendor: Option<String>,
        tone: radar::Tone,
        /// The agent's inbox item; the glyph shows on this row only in the
        /// detailed view, which has no workspace rows.
        item: Option<ItemKind>,
        /// While working: the running tool and how long it has run.
        doing: Option<(String, String)>,
    },
    Workspace {
        endpoint: usize,
        workspace_id: String,
        label: String,
        presence: Presence,
        focused: bool,
        stale: bool,
        hidden: bool,
        machine: Option<String>,
        number: Option<usize>,
        age: Option<String>,
        faded: bool,
        /// No agents (structured view dims these).
        empty: bool,
        /// The first of its agents' inbox items in inbox order.
        item: Option<ItemKind>,
    },
}

impl Row {
    fn height(&self) -> u16 {
        match self {
            Row::Agent {
                workspace, machine, ..
            } if workspace.is_some() || machine.is_some() => 2,
            _ => 1,
        }
    }
}

struct AgentInfo {
    pane_id: String,
    presence: Presence,
    focused: bool,
    stale: bool,
    title: String,
    number: Option<usize>,
    kept: bool,
    /// Idle age text ("2h"), only for idle agents with a known timestamp.
    age: Option<String>,
    /// Shown in the "active" filter: working, needs you, kept or recently idle.
    current: bool,
    /// Context size from the usage hook ("581k"), shown while peeking.
    ctx: Option<String>,
    vendor: Option<String>,
    tone: radar::Tone,
    item: Option<ItemKind>,
    doing: Option<(String, String)>,
}

/// The first value that says something.
fn said<'a>(values: impl IntoIterator<Item = Option<&'a str>>) -> Option<&'a str> {
    values
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
}

/// Whether `title` only names the folder `cwd`: the path, `~/…` form, bare
/// folder name, or a shell's `<path>: <job>` (radar's `locationOnly`). `~` is
/// matched by suffix, since a remote machine's home is not known here.
fn location_only(title: &str, cwd: &str) -> bool {
    let names = |text: &str, basename: bool| {
        text == cwd
            || text
                .strip_prefix('~')
                .is_some_and(|tail| tail.starts_with('/') && cwd.ends_with(tail))
            || (basename && cwd.rsplit('/').next() == Some(text))
    };
    !cwd.is_empty()
        && (names(title, true)
            || title
                .split_once(": ")
                .is_some_and(|(head, _)| !head.is_empty() && names(head, false)))
}

/// An agent row's title, first that applies:
/// 1. the session's own name (`drovr_name`, reported by the usage hook);
/// 2. the terminal title, unless it is only the vendor's product name
///    ("Claude Code") or only the pane's folder;
/// 3. the pane's label, its tab's custom name, or its folder's name;
/// 4. the generic title (terminal title, else the agent's name or id).
fn agent_title(
    agent: &crate::protocol::ClientShellAgent,
    pane: Option<&crate::protocol::ClientShellPane>,
    tab: Option<&crate::protocol::ClientShellTab>,
) -> String {
    if let Some(name) = projects::agent_session_name(agent) {
        return name.to_owned();
    }
    let terminal = said([
        agent.terminal_title_stripped.as_deref(),
        agent.title.as_deref(),
        agent.terminal_title.as_deref(),
    ]);
    let product = agent.agent.as_deref().and_then(radar::display_name);
    let vendor_names = [
        agent.display_agent.as_deref(),
        agent.agent.as_deref(),
        product,
    ];
    let cwd = pane
        .and_then(|pane| said([pane.foreground_cwd.as_deref(), pane.cwd.as_deref()]))
        .map(|cwd| cwd.trim_end_matches('/'))
        .unwrap_or_default();
    let topic = terminal.filter(|title| {
        !vendor_names
            .iter()
            .flatten()
            .any(|name| title.eq_ignore_ascii_case(name.trim()))
            && !location_only(title, cwd)
    });
    let place = || {
        said([
            pane.and_then(|pane| pane.label.as_deref()),
            tab.filter(|tab| tab.custom_label)
                .map(|tab| tab.label.as_str()),
            cwd.rsplit('/').next(),
        ])
    };
    topic
        .or_else(place)
        .or(terminal)
        .or_else(|| {
            said([
                agent.display_agent.as_deref(),
                agent.name.as_deref(),
                product,
                agent.agent.as_deref(),
            ])
        })
        .unwrap_or("agent")
        .to_owned()
}

fn worst(presences: impl IntoIterator<Item = Presence>) -> Presence {
    let rank = |presence: Presence| match presence {
        Presence::Blocked => 5,
        Presence::Unread => 4,
        Presence::Done => 3,
        Presence::Working => 2,
        Presence::Idle => 1,
    };
    presences
        .into_iter()
        .max_by_key(|presence| rank(*presence))
        .unwrap_or(Presence::Idle)
}

pub(super) fn presence_icon(
    presence: Presence,
    config: &ClientShellConfig,
) -> (&'static str, Color) {
    use crate::api::schema::AgentStatus;
    let status = match presence {
        Presence::Blocked => AgentStatus::Blocked,
        Presence::Done => AgentStatus::Done,
        Presence::Working => AgentStatus::Working,
        Presence::Idle => AgentStatus::Idle,
        Presence::Unread => return ("●", Color::Yellow),
    };
    (
        status_icon(status, config.status_indicators),
        status_color(status, &config.palette),
    )
}

fn workspace_presence(
    layout: &ProjectLayout,
    endpoint: &ClientShellEndpoint,
    snapshot: &ClientShellSnapshot,
    workspace_id: &str,
) -> Presence {
    worst(
        snapshot
            .agents
            .iter()
            .filter(|agent| agent.workspace_id == workspace_id)
            .map(|agent| {
                layout.presence(
                    &projects::agent_key(endpoint, &agent.pane_id),
                    agent.state_change_seq,
                    agent.agent_status,
                )
            }),
    )
}

/// Sidebar rows for `layout`. `show_empty` false drops workspaces without
/// agents, except the focused one and new ones (`[ui.sidebar]
/// show_empty_workspaces`; callers apply it to the structured view only, see
/// [`shows_empty`]).
fn build_rows(
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    layout: &ProjectLayout,
    show_empty: bool,
) -> Vec<Row> {
    // Agents in navigation order (the same list prefix+alt+N / prefix+# use).
    let mut agents: HashMap<(usize, String), Vec<AgentInfo>> = HashMap::new();
    let mut next_number = 0usize;
    let now = agent_signal::unix_now();
    for row in super::aggregate_navigation::aggregate_agent_rows(
        endpoints,
        active_endpoint_id,
        crate::config::AgentPanelSortConfig::Spaces,
    ) {
        let endpoint = &endpoints[row.endpoint.endpoint_index];
        let snapshot = endpoint.snapshot.as_deref();
        let pane = snapshot.and_then(|snapshot| {
            snapshot
                .panes
                .iter()
                .find(|pane| pane.pane_id == row.agent.pane_id)
        });
        let tab = snapshot.and_then(|snapshot| {
            snapshot
                .tabs
                .iter()
                .find(|tab| tab.tab_id == row.agent.tab_id)
        });
        let stale = row.endpoint.stale();
        let number = (!stale).then(|| {
            next_number += 1;
            next_number
        });
        let key = projects::agent_key(endpoint, &row.agent.pane_id);
        let presence = layout.presence(&key, row.agent.state_change_seq, row.agent.agent_status);
        let kept = layout.is_kept(&key);
        let idle = (presence == Presence::Idle)
            .then(|| projects::idle_secs(&key))
            .flatten();
        let recent = idle.is_some_and(|secs| secs < layout.recent_secs());
        let unknown = row.agent.agent_status == crate::api::schema::AgentStatus::Unknown;
        let signal = AgentSignal::parse(row.agent);
        // A prompt or finish marked inactive in the sidebar is seen.
        let status = match (presence, row.agent.agent_status) {
            (
                Presence::Idle,
                crate::api::schema::AgentStatus::Blocked | crate::api::schema::AgentStatus::Done,
            ) => crate::api::schema::AgentStatus::Idle,
            (_, status) => status,
        };
        let item = signal.item(status, now, agent_signal::DEFAULT_STUCK_SECS);
        let doing = (presence == Presence::Working)
            .then(|| {
                let secs = signal.doing_secs(now)?;
                Some((signal.doing?, agent_signal::format_elapsed(secs)))
            })
            .flatten();
        agents
            .entry((row.endpoint.endpoint_index, row.agent.workspace_id.clone()))
            .or_default()
            .push(AgentInfo {
                pane_id: row.agent.pane_id.clone(),
                presence,
                kept,
                age: idle.map(projects::format_age),
                current: presence.is_active() || kept || recent,
                ctx: projects::agent_context_tokens(row.agent)
                    .map(|tokens| format!("ctx {}", projects::format_tokens(tokens))),
                focused: row.agent.focused && &endpoint.endpoint_id == active_endpoint_id,
                stale,
                title: agent_title(row.agent, pane, tab),
                number,
                vendor: row.agent.agent.clone(),
                tone: radar::tone(presence, unknown, idle),
                item,
                doing,
            });
    }

    // Groups in display order, then Other.
    let (sections, claimed) = projects::sections(layout, endpoints);
    let mut groups = sections
        .into_iter()
        .map(|section| {
            let group = &layout.groups[section.group];
            (
                group.name.clone(),
                group.name.clone(),
                group.pinned,
                group.collapsed,
                section
                    .members
                    .into_iter()
                    .map(|member| (member.endpoint, member.index, member.hidden))
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<Vec<_>>();
    let mut other = Vec::new();
    for (endpoint_index, endpoint) in endpoints.iter().enumerate() {
        let Some(snapshot) = endpoint.snapshot.as_deref() else {
            continue;
        };
        for (index, workspace) in snapshot.workspaces.iter().enumerate() {
            if !claimed.contains(&(endpoint_index, index)) {
                let hidden = layout.is_hidden(&projects::workspace_key(endpoint, workspace));
                other.push((endpoint_index, index, hidden));
            }
        }
    }
    groups.push((
        OTHER.to_owned(),
        "Other".to_owned(),
        false,
        layout.other_collapsed,
        other,
    ));

    let structured = layout.structured && !layout.compact;
    let mut rows = Vec::new();
    for (key, label, pinned, collapsed, members) in groups {
        let mut body = Vec::new();
        let mut presences = Vec::new();
        let mut count = 0usize;
        let (mut items, mut waiting) = (0usize, false);
        for (endpoint_index, index, hidden) in members {
            if hidden && !layout.show_hidden {
                continue;
            }
            let endpoint = &endpoints[endpoint_index];
            let Some(snapshot) = endpoint.snapshot.as_deref() else {
                continue;
            };
            let Some(workspace) = snapshot.workspaces.get(index) else {
                continue;
            };
            let stale = endpoint.status != ClientEndpointStatus::Online;
            let machine = (!endpoint.endpoint_id.is_local()).then(|| endpoint.label.clone());
            let workspace_agents = agents
                .remove(&(endpoint_index, workspace.workspace_id.clone()))
                .unwrap_or_default();
            let presence = workspace_presence(layout, endpoint, snapshot, &workspace.workspace_id);
            // A brand-new workspace (no agent yet) and the one you're in count as
            // current, so the active view doesn't swallow them.
            let focused_here = workspace.focused && &endpoint.endpoint_id == active_endpoint_id;
            let new_workspace = projects::workspace_age_secs(endpoint, &workspace.workspace_id)
                .is_some_and(|secs| secs < layout.recent_secs());
            let current = presence.is_active()
                || focused_here
                || new_workspace
                || workspace_agents.iter().any(|agent| agent.current);
            if layout.active_only && !current {
                continue;
            }
            if !show_empty && workspace_agents.is_empty() && !focused_here && !new_workspace {
                continue;
            }
            presences.push(presence);
            count += 1;
            let item = workspace_agents.iter().filter_map(|agent| agent.item).min();
            // Like the global badge: only agents you can reach and see.
            for agent in workspace_agents
                .iter()
                .filter(|agent| !agent.stale && !hidden)
            {
                if let Some(item) = agent.item {
                    items += 1;
                    waiting |= item.waiting();
                }
            }
            let focused = workspace.focused && &endpoint.endpoint_id == active_endpoint_id;
            if layout.compact || workspace_agents.is_empty() {
                body.push(Row::Workspace {
                    endpoint: endpoint_index,
                    workspace_id: workspace.workspace_id.clone(),
                    label: workspace.label.clone(),
                    presence,
                    focused,
                    stale,
                    hidden,
                    machine,
                    number: workspace_agents.first().and_then(|agent| agent.number),
                    age: workspace_agents
                        .iter()
                        .all(|agent| agent.age.is_some())
                        .then(|| workspace_agents.first().and_then(|agent| agent.age.clone()))
                        .flatten(),
                    faded: !current,
                    empty: workspace_agents.is_empty(),
                    item,
                });
                continue;
            }
            let show_workspace = !workspace.label.eq_ignore_ascii_case(&label);
            let workspace_agents = workspace_agents
                .into_iter()
                .filter(|agent| !layout.active_only || agent.current)
                .collect::<Vec<_>>();
            if structured && !workspace_agents.is_empty() {
                // Header for the agents below; it carries the highlight only
                // when none of them is the focused pane.
                body.push(Row::Workspace {
                    endpoint: endpoint_index,
                    workspace_id: workspace.workspace_id.clone(),
                    label: workspace.label.clone(),
                    presence,
                    focused: focused && !workspace_agents.iter().any(|agent| agent.focused),
                    stale,
                    hidden,
                    machine: machine.clone(),
                    number: None,
                    age: None,
                    faded: !current,
                    empty: false,
                    item,
                });
            }
            for agent in workspace_agents {
                let (subtitle, machine) = if structured {
                    (None, None)
                } else {
                    (
                        show_workspace.then(|| workspace.label.clone()),
                        machine.clone(),
                    )
                };
                body.push(Row::Agent {
                    endpoint: endpoint_index,
                    workspace_id: workspace.workspace_id.clone(),
                    pane_id: agent.pane_id,
                    presence: agent.presence,
                    focused: agent.focused,
                    stale: agent.stale,
                    title: agent.title,
                    workspace: subtitle,
                    machine,
                    number: agent.number,
                    kept: agent.kept,
                    age: agent.age,
                    faded: !agent.current,
                    ctx: agent.ctx,
                    vendor: agent.vendor,
                    tone: agent.tone,
                    item: agent.item,
                    doing: agent.doing,
                });
            }
        }
        if count == 0 && (layout.active_only || key == OTHER) {
            continue;
        }
        let today = projects::project_usage(layout, (key != OTHER).then_some(key.as_str()), 1);
        let usage = (today[4] > 0).then(|| {
            format!(
                "{} · {}",
                projects::format_minutes(today[4]),
                projects::format_tokens(today[0] + today[1] + today[3])
            )
        });
        rows.push(Row::Header {
            key,
            label,
            pinned,
            collapsed,
            presence: worst(presences),
            count,
            items,
            waiting,
            usage,
        });
        if !collapsed {
            rows.extend(body);
        }
    }
    rows
}

/// Whether the sidebar draws workspaces without agents: always, except in the
/// structured view with `show_empty_workspaces = false`.
fn shows_empty(layout: &ProjectLayout, show_empty_workspaces: bool) -> bool {
    show_empty_workspaces || !layout.structured || layout.compact
}

/// Workspaces in sidebar order, as alt+up/down should walk them: projects then
/// Other, skipping hidden ones and (in the active view) inactive ones.
/// Collapsed projects still count; collapsing is about space, not relevance.
pub(super) fn ordered_workspaces(
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    show_empty_workspaces: bool,
) -> Vec<(ClientEndpointId, String)> {
    let mut layout = projects::layout();
    let show_empty = shows_empty(&layout, show_empty_workspaces);
    layout.compact = true;
    layout.other_collapsed = false;
    for group in &mut layout.groups {
        group.collapsed = false;
    }
    build_rows(endpoints, active_endpoint_id, &layout, show_empty)
        .into_iter()
        .filter_map(|row| match row {
            Row::Workspace {
                endpoint,
                workspace_id,
                stale: false,
                ..
            } => Some((endpoints[endpoint].endpoint_id.clone(), workspace_id)),
            _ => None,
        })
        .collect()
}

pub(super) fn render(
    buffer: &mut Buffer,
    area: Rect,
    config: &ClientShellConfig,
    state: &mut ShellRenderState<'_>,
    hits: &mut ShellHitMap,
) {
    let _ = std::mem::take(state.reveal_navigation_workspace);
    let reveal_focused = std::mem::take(state.reveal_focused_workspace);
    render_panel(
        buffer,
        area,
        config,
        state.endpoints,
        state.active_endpoint_id,
        state.workspace_scroll,
        reveal_focused,
        state.host_appearance,
        config.banner,
        hits,
    );
}

/// The full sidebar into `area` (also used, without the banner, for the peek
/// over a collapsed rail).
#[allow(clippy::too_many_arguments)] // one render pass; a struct would only shuffle these
pub(super) fn render_panel(
    buffer: &mut Buffer,
    area: Rect,
    config: &ClientShellConfig,
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    workspace_scroll: &mut usize,
    reveal_focused: bool,
    host_appearance: Option<crate::terminal_theme::HostAppearance>,
    banner: bool,
    hits: &mut ShellHitMap,
) {
    let palette = &config.palette;
    let ground = radar::ground(palette, host_appearance);
    super::render::render_sidebar_background(buffer, area, palette);
    hits.sidebar_divider = if area.is_empty() {
        Rect::default()
    } else {
        Rect::new(area.right().saturating_sub(1), area.y, 1, area.height)
    };
    hits.sidebar_section_divider = Rect::default();
    if area.height < 3 || area.width < 8 {
        return;
    }
    render_panel_with(
        buffer,
        area,
        config,
        &projects::layout(),
        endpoints,
        active_endpoint_id,
        workspace_scroll,
        reveal_focused,
        ground,
        banner,
        hits,
    );
}

/// The "drovr" wordmark above the toggles, in quarter blocks.
const BANNER: [&str; 2] = ["▛▀▖▛▀▖▞▀▖▌ ▌▛▀▖", "▙▄▘▌▚▖▝▄▘▝▞ ▌▚▖"];
/// The banner needs this many sidebar columns...
const BANNER_MIN_WIDTH: u16 = 18;
/// ...and must leave the rows list at least this many rows.
const BANNER_MIN_BODY: u16 = 10;

/// Versions to try next to the wordmark, widest first: the drovr version
/// (build_info::DROVR_VERSION) without its `.dirty` mark, then without the
/// `+N` commit count, then without the `-N` release suffix. A non-numeric
/// build suffix (`+dev`) has no pixel glyphs and is dropped up front.
fn banner_versions(version: &str) -> Vec<&str> {
    let version = version.strip_suffix(".dirty").unwrap_or(version);
    let release = version.split('+').next().unwrap_or(version);
    let base = release.split('-').next().unwrap_or(release);
    let build = &version[release.len()..];
    let mut versions = Vec::with_capacity(3);
    if build.len() > 1 && build[1..].bytes().all(|b| b.is_ascii_digit()) {
        versions.push(version);
    }
    versions.push(release);
    versions.push(base);
    versions.dedup();
    versions
}

/// 4x4-pixel glyphs (`#` = set) for the version's pixel font. A glyph is
/// as many cells as half its widest row, rounded up: digits and '+' use three
/// pixel columns plus one blank for spacing (2 cells); '.' and '-' are 1 cell.
fn pixel_bitmap(c: char) -> Option<[&'static str; 4]> {
    Some(match c {
        '0' => ["###", "#.#", "#.#", "###"],
        '1' => [".#.", "##.", ".#.", "###"],
        '2' => ["##.", "..#", ".#.", "###"],
        '3' => ["###", ".##", "..#", "###"],
        '4' => ["#.#", "#.#", "###", "..#"],
        '5' => ["###", "##.", "..#", "##."],
        '6' => ["#..", "###", "#.#", "###"],
        '7' => ["###", "..#", ".#.", ".#."],
        '8' => ["###", "###", "#.#", "###"],
        '9' => ["###", "#.#", "###", "..#"],
        '.' => ["", "", "", "#"],
        '-' => ["", "#", "", ""],
        '+' => [".#.", "###", ".#.", ""],
        _ => return None,
    })
}

/// One character of the pixel font as two rows of quadrant blocks.
fn pixel_glyph(c: char) -> Option<[String; 2]> {
    const QUADRANTS: [char; 16] = [
        ' ', '▘', '▝', '▀', '▖', '▌', '▞', '▛', '▗', '▚', '▐', '▜', '▄', '▙', '▟', '█',
    ];
    let bitmap = pixel_bitmap(c)?;
    let cells = bitmap
        .iter()
        .map(|row| row.len())
        .max()
        .unwrap_or(0)
        .div_ceil(2);
    let on = |row: usize, col: usize| bitmap[row].as_bytes().get(col) == Some(&b'#');
    Some([0, 1].map(|half| {
        (0..cells)
            .map(|cell| {
                let (row, col) = (half * 2, cell * 2);
                let bits = usize::from(on(row, col))
                    | usize::from(on(row, col + 1)) << 1
                    | usize::from(on(row + 1, col)) << 2
                    | usize::from(on(row + 1, col + 1)) << 3;
                QUADRANTS[bits]
            })
            .collect()
    }))
}

/// `text` in the pixel font (unknown characters are skipped).
fn pixel_text(text: &str) -> [String; 2] {
    let mut rows = [String::new(), String::new()];
    for glyph in text.chars().filter_map(pixel_glyph) {
        for (row, part) in rows.iter_mut().zip(glyph) {
            row.push_str(&part);
        }
    }
    rows
}

/// Draw the banner when it fits and return the rows it takes (2 + 1 blank):
/// the wordmark in the text colour on the sidebar background, and the version
/// in the pixel font two columns to its right, dimmer. When the sidebar is too
/// narrow the version drops its "+N", then its "-N" suffix, then disappears
/// (see banner_versions). No hit rect.
fn render_banner(buffer: &mut Buffer, area: Rect, inner: Rect, palette: &Palette) -> u16 {
    let rows = BANNER.len() as u16 + 1;
    // Body height is what is left after the banner, toggles, blank and footer.
    if area.width < BANNER_MIN_WIDTH || inner.height.saturating_sub(rows + 3) < BANNER_MIN_BODY {
        return 0;
    }
    let x = inner.x + 1;
    let style = Style::default().fg(palette.text);
    for (offset, line) in BANNER.iter().enumerate() {
        put_text(
            buffer,
            x,
            inner.y + offset as u16,
            inner.width.saturating_sub(1),
            line,
            style,
        );
    }
    let version_x = x + display_width(BANNER[0]) + 2;
    let room = inner.right().saturating_sub(version_x);
    if let Some(version) = banner_versions(crate::build_info::DROVR_VERSION)
        .into_iter()
        .map(pixel_text)
        .find(|version| display_width(&version[0]) <= room)
    {
        for (offset, line) in version.iter().enumerate() {
            put_text(
                buffer,
                version_x,
                inner.y + offset as u16,
                room,
                line.trim_end(),
                Style::default().fg(palette.overlay0),
            );
        }
    }
    rows
}

#[allow(clippy::too_many_arguments)] // one render pass; a struct would only shuffle these
fn render_panel_with(
    buffer: &mut Buffer,
    area: Rect,
    config: &ClientShellConfig,
    layout: &ProjectLayout,
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    workspace_scroll: &mut usize,
    reveal_focused: bool,
    ground: radar::Ground,
    banner: bool,
    hits: &mut ShellHitMap,
) {
    let palette = &config.palette;
    let outer = Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height);
    let banner_rows = if banner {
        render_banner(buffer, area, outer, palette)
    } else {
        0
    };
    // Everything below the banner lays out as if the panel started there.
    let inner = Rect::new(
        outer.x,
        outer.y + banner_rows,
        outer.width,
        outer.height - banner_rows,
    );

    // Header: filter toggle (left) and view toggle (right).
    let filter = if layout.active_only {
        " ◉ active"
    } else {
        " ○ all agents"
    };
    hits.drovr_filter_toggle = Rect::new(inner.x, inner.y, display_width(filter), 1);
    put_text(
        buffer,
        inner.x,
        inner.y,
        inner.width,
        filter,
        Style::default()
            .fg(if layout.active_only {
                palette.accent
            } else {
                palette.overlay0
            })
            .add_modifier(Modifier::BOLD),
    );
    // Attention counter: how many agents need you; click = next one (prefix+u).
    let (needing, blocked) = attention_count(endpoints, layout);
    hits.drovr_attention = Rect::default();
    if needing > 0 {
        let counter = format!(" ● {needing}");
        let x = inner.x + display_width(filter);
        hits.drovr_attention = Rect::new(x, inner.y, display_width(&counter), 1);
        put_text(
            buffer,
            x,
            inner.y,
            inner.width.saturating_sub(display_width(filter)),
            &counter,
            Style::default()
                .fg(if blocked {
                    status_color(crate::api::schema::AgentStatus::Blocked, palette)
                } else {
                    Color::Yellow
                })
                .add_modifier(Modifier::BOLD),
        );
    }
    let view = if layout.compact {
        "compact "
    } else if layout.structured {
        "structured "
    } else {
        "detailed "
    };
    let view_width = display_width(view);
    hits.drovr_view_toggle = Rect::new(
        inner.right().saturating_sub(view_width),
        inner.y,
        view_width,
        1,
    );
    put_right_text(
        buffer,
        inner,
        inner.y,
        view,
        Style::default()
            .fg(palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );

    let rows = build_rows(
        endpoints,
        active_endpoint_id,
        layout,
        shows_empty(layout, config.show_empty_workspaces),
    );
    // One blank, inert line between the toggles and the first section.
    let body = Rect::new(
        inner.x,
        inner.y + 2,
        inner.width,
        inner.height.saturating_sub(3),
    );
    hits.workspace_body = body;
    let row_heights = rows.iter().map(Row::height).collect::<Vec<_>>();
    let gaps = row_gaps(&rows, layout, config.agents.row_gap, config.agent_gap);
    if reveal_focused {
        if let Some(target) = rows.iter().position(|row| match row {
            Row::Agent { focused, .. } | Row::Workspace { focused, .. } => *focused,
            Row::Header { .. } => false,
        }) {
            *workspace_scroll = super::scroll::list_scroll_start_to_reveal(
                &row_heights,
                &gaps,
                body.height,
                *workspace_scroll,
                target,
            );
        }
    }
    let metrics =
        super::scroll::list_scroll_metrics(&row_heights, &gaps, body.height, *workspace_scroll);
    hits.workspace_max_scroll = metrics.max_offset_from_bottom;
    hits.workspace_scroll_metrics = Some(metrics);
    *workspace_scroll = metrics
        .max_offset_from_bottom
        .saturating_sub(metrics.offset_from_bottom);
    let show_scrollbar = metrics.max_offset_from_bottom > 0 && body.width > 1;
    let width = body.width.saturating_sub(u16::from(show_scrollbar));
    let mut section = String::new();
    let sections = rows
        .iter()
        .map(|row| {
            if let Row::Header { key, .. } = row {
                section.clone_from(key);
            }
            section.clone()
        })
        .collect::<Vec<_>>();

    let mut placed = Vec::new();
    let mut y = body.y;
    for (index, row) in rows.iter().enumerate().skip(*workspace_scroll) {
        let height = row_heights[index];
        if y.saturating_add(height) > body.bottom() {
            break;
        }
        let rect = Rect::new(body.x, y, width, height);
        render_row(buffer, rect, row, layout, endpoints, config, ground, hits);
        let key = match row {
            Row::Header { .. } => None,
            Row::Agent {
                endpoint,
                workspace_id,
                ..
            }
            | Row::Workspace {
                endpoint,
                workspace_id,
                ..
            } => row_workspace_key(&endpoints[*endpoint], workspace_id),
        };
        if key.is_some() || matches!(row, Row::Header { .. }) {
            placed.push(Placed {
                y,
                height,
                section: sections[index].clone(),
                key,
            });
        }
        y = y.saturating_add(height).saturating_add(gaps[index]);
    }
    hits.drovr_drops = drop_slots(&placed, body.x, width, body.bottom());
    draw_drag_feedback(buffer, body.x, width, endpoints, config, hits);
    if show_scrollbar {
        let track = Rect::new(body.right().saturating_sub(1), body.y, 1, body.height);
        hits.workspace_scrollbar = track;
        super::scroll::render_list_scrollbar(buffer, track, metrics, palette);
    }

    // Footer: new workspace, menu, collapse.
    let footer_y = inner.bottom().saturating_sub(1);
    if config.mouse_capture {
        let active_label = endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == active_endpoint_id)
            .map_or("Local", |endpoint| endpoint.label.as_str());
        let label = format!(" new · {active_label}");
        hits.new_workspace =
            Rect::new(inner.x, footer_y, display_width(&label).min(inner.width), 1);
        put_text(
            buffer,
            inner.x,
            footer_y,
            inner.width,
            &label,
            Style::default().fg(palette.overlay0),
        );
        hits.global_launcher = Rect::new(inner.right().saturating_sub(8), footer_y, 6, 1);
        put_right_text(
            buffer,
            Rect::new(inner.x, footer_y, inner.width.saturating_sub(2), 1),
            footer_y,
            "menu",
            Style::default().fg(palette.overlay0),
        );
        // Remote machines: latency while peeking; a problem state always.
        let peeking = projects::peeking();
        let machines = endpoints
            .iter()
            .filter(|endpoint| !endpoint.endpoint_id.is_local())
            .map(|endpoint| match endpoint.status {
                ClientEndpointStatus::Online if !peeking => String::new(),
                ClientEndpointStatus::Online => {
                    match crate::client::endpoint::endpoint_rtt_ms(&endpoint.endpoint_id) {
                        Some(rtt) => format!("{} {rtt}ms", endpoint.label),
                        None => endpoint.label.clone(),
                    }
                }
                _ => {
                    let (glyph, text, _) = endpoint_status_presentation(endpoint.status, palette);
                    format!("{} {glyph} {text}", endpoint.label)
                }
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let used = display_width(&label) + 7;
        if !machines.is_empty() && display_width(&machines) + used < inner.width {
            put_right_text(
                buffer,
                Rect::new(inner.x, footer_y, inner.width.saturating_sub(7), 1),
                footer_y,
                &machines,
                Style::default().fg(palette.overlay0),
            );
        }
    }
    hits.sidebar_toggle = Rect::new(area.right().saturating_sub(2), footer_y, 1, 1);
    put_text(
        buffer,
        hits.sidebar_toggle.x,
        footer_y,
        1,
        "«",
        Style::default().fg(palette.overlay0),
    );
}

/// Blank lines after each row. Every view leaves one before a project header;
/// structured also leaves one after each workspace group (before the next
/// workspace header) and `agent_gap` (0 or 1) between a workspace's agents,
/// never two in a row; detailed spaces agents by `row_gap`.
/// Gaps are skipped space, so they get no hit rect.
fn row_gaps(rows: &[Row], layout: &ProjectLayout, row_gap: u16, agent_gap: u16) -> Vec<u16> {
    let structured = layout.structured && !layout.compact;
    rows.iter()
        .enumerate()
        .map(|(index, row)| match (row, rows.get(index + 1)) {
            (_, Some(Row::Header { .. })) => 1,
            (Row::Agent { .. } | Row::Workspace { .. }, Some(Row::Workspace { .. }))
                if structured =>
            {
                1
            }
            // Structured agents follow their own workspace's header or agent.
            (Row::Agent { .. }, Some(Row::Agent { .. })) if structured => agent_gap.min(1),
            (Row::Agent { .. }, Some(Row::Agent { .. }))
                if !layout.compact && !layout.structured =>
            {
                row_gap
            }
            _ => 0,
        })
        .collect()
}

#[allow(clippy::too_many_arguments)] // one render pass; splitting only shuffles args
fn render_row(
    buffer: &mut Buffer,
    rect: Rect,
    row: &Row,
    layout: &ProjectLayout,
    endpoints: &[ClientShellEndpoint],
    config: &ClientShellConfig,
    ground: radar::Ground,
    hits: &mut ShellHitMap,
) {
    let palette = &config.palette;
    let hinting = projects::hinting();
    let peeking = projects::peeking();
    // Right-hand slot: empty normally; the jump number while the jump prompt
    // is open; idle age + number while peeking (prefix+space).
    let right_slot =
        |number: Option<usize>, age: &Option<String>, ctx: &Option<String>| -> (String, Style) {
            if hinting {
                (
                    number
                        .map(|number| format!("{number} "))
                        .unwrap_or_default(),
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD),
                )
            } else if peeking {
                let parts = [
                    age.clone(),
                    ctx.clone(),
                    number.map(|number| format!("#{number}")),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
                (
                    if parts.is_empty() {
                        String::new()
                    } else {
                        format!("{} ", parts.join(" "))
                    },
                    Style::default().fg(palette.overlay0),
                )
            } else {
                (String::new(), Style::default())
            }
        };
    if layout.structured && !layout.compact {
        render_structured_row(
            buffer,
            rect,
            row,
            endpoints,
            config,
            ground,
            &right_slot,
            hits,
        );
        return;
    }
    match row {
        Row::Header {
            key,
            label,
            pinned,
            collapsed,
            presence,
            count,
            items,
            waiting,
            usage,
        } => {
            let marker = if *collapsed { "▸" } else { "▾" };
            let pin = if *pinned { "★ " } else { "" };
            let other = key == OTHER;
            put_text(
                buffer,
                rect.x,
                rect.y,
                rect.width.saturating_sub(6),
                &format!(" {marker} {pin}{label}"),
                Style::default()
                    .fg(if other {
                        palette.overlay0
                    } else {
                        palette.accent
                    })
                    .add_modifier(Modifier::BOLD),
            );
            if peeking {
                if let Some(usage) = usage {
                    put_right_text(
                        buffer,
                        rect,
                        rect.y,
                        &format!("{usage} "),
                        Style::default().fg(palette.overlay0),
                    );
                }
            } else if *items > 0 {
                let text = format!("● {items} ");
                put_right_text(buffer, rect, rect.y, &text, count_style(*waiting, palette));
                let width = display_width(&text);
                hits.drovr_inbox.push((
                    Rect::new(rect.right().saturating_sub(width), rect.y, width, 1),
                    InboxFilter::Project(key.clone()),
                ));
            } else if *collapsed {
                let (icon, color) = presence_icon(*presence, config);
                put_right_text(
                    buffer,
                    rect,
                    rect.y,
                    &format!("{icon} {count} "),
                    Style::default().fg(color),
                );
            }
            hits.projects.push((rect, key.clone()));
        }
        Row::Agent {
            endpoint,
            workspace_id,
            pane_id,
            presence,
            focused,
            stale,
            title,
            workspace,
            machine,
            number,
            kept,
            age,
            faded,
            ctx,
            item,
            doing,
            ..
        } => {
            let endpoint = &endpoints[*endpoint];
            if *focused {
                buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
            }
            let (icon, color) = presence_icon(*presence, config);
            let (slot, slot_style, glyph) =
                signal_slot(right_slot(*number, age, ctx), *item, doing, palette);
            let number_width = display_width(&slot) + u16::from(!slot.is_empty());
            let title = match doing {
                Some((doing, _)) => format!("▸ {doing}"),
                None if *kept => format!("⚑ {title}"),
                None => title.clone(),
            };
            put_text(
                buffer,
                rect.x + 1,
                rect.y,
                1,
                icon,
                Style::default().fg(color),
            );
            put_text(
                buffer,
                rect.x + 3,
                rect.y,
                rect.width.saturating_sub(3 + number_width),
                &title,
                Style::default()
                    .fg(if *focused {
                        palette.text
                    } else {
                        palette.subtext0
                    })
                    .add_modifier(Modifier::BOLD),
            );
            put_right_text(buffer, rect, rect.y, &slot, slot_style);
            if rect.height > 1 {
                let mut x = rect.x + 3;
                let right = rect.right();
                if let Some(workspace) = workspace {
                    put_text(
                        buffer,
                        x,
                        rect.y + 1,
                        right.saturating_sub(x),
                        workspace,
                        Style::default().fg(palette.overlay0),
                    );
                    x = x.saturating_add(display_width(workspace));
                    if machine.is_some() {
                        put_text(
                            buffer,
                            x,
                            rect.y + 1,
                            right.saturating_sub(x),
                            " · ",
                            Style::default().fg(palette.overlay0),
                        );
                        x = x.saturating_add(3);
                    }
                }
                if let Some(machine) = machine {
                    put_text(
                        buffer,
                        x,
                        rect.y + 1,
                        right.saturating_sub(x),
                        machine,
                        Style::default()
                            .fg(palette.subtext0)
                            .add_modifier(Modifier::BOLD),
                    );
                }
            }
            if *stale || (*faded && !*focused) {
                buffer.set_style(rect, Style::default().add_modifier(Modifier::DIM));
            }
            if glyph {
                push_glyph_hit(hits, rect, endpoint, workspace_id);
            }
            hits.endpoint_agents
                .push((rect, endpoint.endpoint_id.clone(), pane_id.clone()));
            hits.drovr_rows.push(RowHit {
                rect,
                endpoint_id: endpoint.endpoint_id.clone(),
                workspace_id: workspace_id.clone(),
                pane_id: Some(pane_id.clone()),
            });
        }
        Row::Workspace {
            endpoint,
            workspace_id,
            label,
            presence,
            focused,
            stale,
            hidden,
            machine,
            number,
            age,
            faded,
            item,
            ..
        } => {
            let endpoint = &endpoints[*endpoint];
            if *focused {
                buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
            }
            let (icon, color) = presence_icon(*presence, config);
            put_text(
                buffer,
                rect.x + 1,
                rect.y,
                1,
                if *hidden { "⊘" } else { icon },
                Style::default().fg(color),
            );
            let (slot, slot_style, glyph) =
                signal_slot(right_slot(*number, age, &None), *item, &None, palette);
            let tag = match machine {
                Some(machine) => format!("{machine} "),
                None => String::new(),
            };
            put_text(
                buffer,
                rect.x + 3,
                rect.y,
                rect.width
                    .saturating_sub(4 + display_width(&tag) + display_width(&slot)),
                label,
                Style::default().fg(if *focused {
                    palette.text
                } else if layout.compact {
                    palette.subtext0
                } else {
                    palette.overlay0
                }),
            );
            put_right_text(buffer, rect, rect.y, &slot, slot_style);
            let tag_rect = Rect::new(
                rect.x,
                rect.y,
                rect.width.saturating_sub(display_width(&slot)),
                1,
            );
            put_right_text(
                buffer,
                tag_rect,
                rect.y,
                &tag,
                Style::default().fg(palette.overlay0),
            );
            if *stale || *hidden || (*faded && !*focused) {
                buffer.set_style(rect, Style::default().add_modifier(Modifier::DIM));
            }
            if glyph {
                push_glyph_hit(hits, rect, endpoint, workspace_id);
            }
            hits.drovr_rows.push(RowHit {
                rect,
                endpoint_id: endpoint.endpoint_id.clone(),
                workspace_id: workspace_id.clone(),
                pane_id: None,
            });
        }
    }
}

fn row_workspace_key(endpoint: &ClientShellEndpoint, workspace_id: &str) -> Option<String> {
    endpoint
        .snapshot
        .as_deref()?
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == workspace_id)
        .map(|workspace| projects::workspace_key(endpoint, workspace))
}

/// While a row is dragged: show the dragged workspace's rows reversed, and
/// where it would land, either the target header highlighted or an accent
/// insertion line (carrying the workspace's name on a blank line).
fn draw_drag_feedback(
    buffer: &mut Buffer,
    x: u16,
    width: u16,
    endpoints: &[ClientShellEndpoint],
    config: &ClientShellConfig,
    hits: &ShellHitMap,
) {
    let Some(press) = projects::press() else {
        return;
    };
    let Some(point) = press.dragging else {
        return;
    };
    let palette = &config.palette;
    for hit in &hits.drovr_rows {
        if hit.endpoint_id == press.endpoint_id && hit.workspace_id == press.workspace_id {
            buffer.set_style(hit.rect, Style::default().add_modifier(Modifier::REVERSED));
        }
    }
    let Some(slot) = drop_slot_at(&hits.drovr_drops, point) else {
        return;
    };
    let line = Rect::new(x, slot.marker, width, 1);
    if slot.target.header {
        buffer.set_style(
            line,
            Style::default()
                .bg(palette.active_row_bg)
                .add_modifier(Modifier::UNDERLINED),
        );
        return;
    }
    let blank =
        (line.x..line.right()).all(|column| buffer[(column, line.y)].symbol().trim().is_empty());
    for column in line.x..line.right() {
        let cell = &mut buffer[(column, line.y)];
        if cell.symbol().trim().is_empty() {
            cell.set_symbol("─");
            cell.set_fg(palette.accent);
        }
    }
    let label = endpoints
        .iter()
        .find(|endpoint| endpoint.endpoint_id == press.endpoint_id)
        .and_then(|endpoint| endpoint.snapshot.as_deref())
        .and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == press.workspace_id)
        })
        .map(|workspace| workspace.label.clone());
    if let (true, Some(label)) = (blank, label) {
        let room = line.width.saturating_sub(4);
        put_text(
            buffer,
            line.x + 2,
            line.y,
            room,
            &radar::fit(&format!(" {label} "), room),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        );
    }
}

/// Set when a structured render draws a working agent (its spinner).
static SPINNING: AtomicBool = AtomicBool::new(false);

/// Whether the last render drew a spinner, clearing the mark. The client's
/// 100 ms timer repaints while this holds; the repaint sets it again for as
/// long as an agent works, so nothing repaints once none does.
pub(super) fn take_spinning() -> bool {
    SPINNING.swap(false, Ordering::Relaxed)
}

/// Structured workspace headers start one column right of the project
/// header's "▾"; their agents sit two columns further in.
const STRUCTURED_INDENT: u16 = 2;
const STRUCTURED_AGENT_INDENT: u16 = STRUCTURED_INDENT + 2;

type RightSlot<'a> =
    dyn Fn(Option<usize>, &Option<String>, &Option<String>) -> (String, Style) + 'a;

/// Structured view rows: a workspace header (label, remote machine on the
/// right) or one agent line (vendor mark, state mark, state-coloured title).
#[allow(clippy::too_many_arguments)] // one render pass; splitting only shuffles args
fn render_structured_row(
    buffer: &mut Buffer,
    rect: Rect,
    row: &Row,
    endpoints: &[ClientShellEndpoint],
    config: &ClientShellConfig,
    ground: radar::Ground,
    right_slot: &RightSlot<'_>,
    hits: &mut ShellHitMap,
) {
    let palette = &config.palette;
    match row {
        Row::Header {
            key,
            label,
            pinned,
            collapsed,
            items,
            waiting,
            usage,
            ..
        } => {
            let count = render_structured_header(
                buffer,
                rect,
                StructuredHeader {
                    label: &format!(
                        "{} {}{label}",
                        if *collapsed { "▸" } else { "▾" },
                        if *pinned { "★ " } else { "" }
                    ),
                    other: key == OTHER,
                    items: *items,
                    waiting: *waiting,
                    usage: usage.as_deref().filter(|_| projects::peeking()),
                },
                palette,
            );
            if let Some(count) = count {
                hits.drovr_inbox
                    .push((count, InboxFilter::Project(key.clone())));
            }
            hits.projects.push((rect, key.clone()));
        }
        Row::Agent {
            endpoint,
            workspace_id,
            pane_id,
            focused,
            stale,
            title,
            number,
            kept,
            age,
            faded,
            ctx,
            vendor,
            tone,
            doing,
            ..
        } => {
            let endpoint = &endpoints[*endpoint];
            if *focused {
                buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
            }
            // The workspace header above carries the item glyph.
            let (slot, slot_style, _) =
                signal_slot(right_slot(*number, age, ctx), None, doing, palette);
            put_right_text(buffer, rect, rect.y, &slot, slot_style);
            let right = rect
                .right()
                .saturating_sub(display_width(&slot) + u16::from(!slot.is_empty()));
            let mut x = rect.x + STRUCTURED_AGENT_INDENT;
            if let Some((mark, color)) =
                radar::logo(vendor.as_deref(), config.agent_icons, ground, palette)
            {
                put_text(buffer, x, rect.y, 1, &mark, Style::default().fg(color));
                x = x.saturating_add(2);
            }
            let (color, bold) = radar::title_style(*tone, vendor.as_deref(), ground, palette);
            let mut style = Style::default().fg(color);
            if bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            if *tone == radar::Tone::Working {
                SPINNING.store(true, Ordering::Relaxed);
            }
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_millis());
            let text = match doing {
                Some((doing, _)) => format!("▸ {doing}"),
                None => [
                    radar::lead(*tone, now_ms),
                    kept.then_some("⚑"),
                    Some(title.as_str()),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" "),
            };
            let width = right.saturating_sub(x);
            put_text(buffer, x, rect.y, width, &radar::fit(&text, width), style);
            if *stale || (*faded && !*focused) {
                buffer.set_style(rect, Style::default().add_modifier(Modifier::DIM));
            }
            hits.endpoint_agents
                .push((rect, endpoint.endpoint_id.clone(), pane_id.clone()));
            hits.drovr_rows.push(RowHit {
                rect,
                endpoint_id: endpoint.endpoint_id.clone(),
                workspace_id: workspace_id.clone(),
                pane_id: Some(pane_id.clone()),
            });
        }
        Row::Workspace {
            endpoint,
            workspace_id,
            label,
            focused,
            stale,
            hidden,
            machine,
            number,
            age,
            faded,
            empty,
            item,
            ..
        } => {
            let endpoint = &endpoints[*endpoint];
            if *focused {
                buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
            }
            let (slot, slot_style, glyph) =
                signal_slot(right_slot(*number, age, &None), *item, &None, palette);
            put_right_text(buffer, rect, rect.y, &slot, slot_style);
            let tag_rect = Rect::new(
                rect.x,
                rect.y,
                rect.width.saturating_sub(display_width(&slot)),
                1,
            );
            // The label names the agents below; the machine tag gets at most a
            // third of the row so it cannot crowd the label out.
            let tag = machine
                .as_ref()
                .map(|machine| format!("{} ", radar::fit(machine, tag_rect.width / 3)))
                .filter(|tag| tag.len() > 1)
                .unwrap_or_default();
            put_right_text(
                buffer,
                tag_rect,
                rect.y,
                &tag,
                Style::default().fg(palette.overlay0),
            );
            let x = rect.x + STRUCTURED_INDENT;
            let width = tag_rect
                .right()
                .saturating_sub(display_width(&tag) + u16::from(!tag.is_empty()))
                .saturating_sub(x);
            let text = if *hidden {
                format!("⊘ {label}")
            } else {
                label.clone()
            };
            put_text(
                buffer,
                x,
                rect.y,
                width,
                &radar::fit(&text, width),
                Style::default()
                    .fg(if *focused {
                        palette.text
                    } else {
                        radar::subtle(ground, palette)
                    })
                    .add_modifier(Modifier::BOLD),
            );
            if *stale || *hidden || ((*faded || *empty) && !*focused) {
                buffer.set_style(rect, Style::default().add_modifier(Modifier::DIM));
            }
            if glyph {
                push_glyph_hit(hits, rect, endpoint, workspace_id);
            }
            hits.drovr_rows.push(RowHit {
                rect,
                endpoint_id: endpoint.endpoint_id.clone(),
                workspace_id: workspace_id.clone(),
                pane_id: None,
            });
        }
    }
}

/// What a structured section header shows.
struct StructuredHeader<'a> {
    /// Marker, pin and name ("▾ ★ GTM").
    label: &'a str,
    other: bool,
    /// Inbox items in the section, and whether one waits on a prompt.
    items: usize,
    waiting: bool,
    /// Today's usage, shown instead of the count while peeking.
    usage: Option<&'a str>,
}

/// A structured section header: " ▾ GTM ─────────── ● 3 ". The name in the
/// section style, a dim rule, then the count of inbox items (only when there
/// are any; in the blocked colour when one waits on a prompt). Returns the
/// count's rect, a click target that opens the inbox.
fn render_structured_header(
    buffer: &mut Buffer,
    rect: Rect,
    header: StructuredHeader<'_>,
    palette: &Palette,
) -> Option<Rect> {
    let right = match header.usage {
        Some(usage) => Some((usage.to_owned(), Style::default().fg(palette.overlay0))),
        None => (header.items > 0).then(|| {
            (
                format!("● {}", header.items),
                count_style(header.waiting, palette),
            )
        }),
    };
    let right_width = right.as_ref().map_or(0, |(text, _)| display_width(text));
    // One blank column at each edge and around the rule.
    let right_x = rect.right().saturating_sub(right_width + 1);
    let label_x = rect.x + 1;
    let room = right_x.saturating_sub(label_x + 1);
    let label = radar::fit(header.label, room);
    put_text(
        buffer,
        label_x,
        rect.y,
        room,
        &label,
        Style::default()
            .fg(if header.other {
                palette.overlay0
            } else {
                palette.accent
            })
            .add_modifier(Modifier::BOLD),
    );
    let rule_x = label_x + display_width(&label) + 1;
    let rule_width = right_x.saturating_sub(rule_x + u16::from(right_width > 0));
    put_text(
        buffer,
        rule_x,
        rect.y,
        rule_width,
        &"─".repeat(usize::from(rule_width)),
        Style::default().fg(palette.surface_dim),
    );
    let (text, style) = right?;
    put_text(buffer, right_x, rect.y, right_width, &text, style);
    (header.usage.is_none()).then(|| Rect::new(right_x, rect.y, right_width, 1))
}

/// Inbox count style: the blocked colour when an item waits on a prompt.
fn count_style(waiting: bool, palette: &Palette) -> Style {
    let color = if waiting {
        status_color(crate::api::schema::AgentStatus::Blocked, palette)
    } else {
        Color::Yellow
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

/// A row's right slot. The jump number or peek text wins; else an inbox
/// item's glyph ("! "), else a working agent's elapsed time ("30s "). The
/// flag says the glyph is drawn, so the caller records its click target.
fn signal_slot(
    slot: (String, Style),
    item: Option<ItemKind>,
    doing: &Option<(String, String)>,
    palette: &Palette,
) -> (String, Style, bool) {
    if !slot.0.is_empty() {
        return (slot.0, slot.1, false);
    }
    if let Some(item) = item {
        let color = if item.waiting() {
            status_color(crate::api::schema::AgentStatus::Blocked, palette)
        } else if item == ItemKind::Finished {
            status_color(crate::api::schema::AgentStatus::Done, palette)
        } else {
            Color::Yellow
        };
        let style = Style::default().fg(color).add_modifier(Modifier::BOLD);
        return (format!("{} ", item.glyph()), style, true);
    }
    match doing {
        Some((_, elapsed)) => (
            format!("{elapsed} "),
            Style::default().fg(palette.overlay0),
            false,
        ),
        None => (slot.0, slot.1, false),
    }
}

/// Records the glyph drawn by [`signal_slot`] at the right of `rect` as a
/// click target that opens the inbox on the row's workspace.
fn push_glyph_hit(
    hits: &mut ShellHitMap,
    rect: Rect,
    endpoint: &ClientShellEndpoint,
    workspace_id: &str,
) {
    hits.drovr_inbox.push((
        Rect::new(rect.right().saturating_sub(2), rect.y, 2, 1),
        InboxFilter::Workspace {
            endpoint_id: endpoint.endpoint_id.clone(),
            workspace_id: workspace_id.to_owned(),
        },
    ));
}

/// Agents that need you (blocked, finished-unseen, marked unread) on online
/// machines, skipping hidden workspaces; and whether any is blocked.
fn attention_count(endpoints: &[ClientShellEndpoint], layout: &ProjectLayout) -> (usize, bool) {
    let mut count = 0;
    let mut blocked = false;
    for endpoint in endpoints
        .iter()
        .filter(|endpoint| endpoint.status == ClientEndpointStatus::Online)
    {
        let Some(snapshot) = endpoint.snapshot.as_deref() else {
            continue;
        };
        for agent in &snapshot.agents {
            let hidden = snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == agent.workspace_id)
                .is_some_and(|workspace| {
                    layout.is_hidden(&projects::workspace_key(endpoint, workspace))
                });
            if hidden {
                continue;
            }
            let presence = layout.presence(
                &projects::agent_key(endpoint, &agent.pane_id),
                agent.state_change_seq,
                agent.agent_status,
            );
            if presence.needs_attention() {
                count += 1;
                blocked |= presence == Presence::Blocked;
            }
        }
    }
    (count, blocked)
}

/// One project as the collapsed rail sees it.
struct RailProject {
    key: String,
    tag: String,
    name: String,
    presence: Presence,
    current: bool,
}

/// Projects (and Other) in display order with their worst status, honouring
/// the active filter; `current` marks the project of the focused workspace.
fn rail_projects(
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    layout: &ProjectLayout,
) -> Vec<RailProject> {
    let mut view = layout.clone();
    view.compact = true;
    view.other_collapsed = false;
    for group in &mut view.groups {
        group.collapsed = false;
    }
    let mut projects: Vec<RailProject> = Vec::new();
    for row in build_rows(endpoints, active_endpoint_id, &view, true) {
        match row {
            Row::Header {
                key,
                label,
                presence,
                ..
            } => {
                let short = layout
                    .groups
                    .iter()
                    .find(|group| group.name == key)
                    .and_then(|group| group.short.as_deref());
                projects.push(RailProject {
                    tag: if key == OTHER {
                        "··".to_owned()
                    } else {
                        projects::project_tag(&label, short)
                    },
                    name: label,
                    key,
                    presence,
                    current: false,
                });
            }
            Row::Workspace { focused: true, .. } => {
                if let Some(project) = projects.last_mut() {
                    project.current = true;
                }
            }
            _ => {}
        }
    }
    projects
}

/// drovr fork: the collapsed sidebar as a 3-column project rail:
/// needs-you counter, one row per project (worst status + 2-letter tag), the
/// current project's name written vertically, and the expand toggle.
pub(super) fn render_collapsed(
    buffer: &mut Buffer,
    area: Rect,
    config: &ClientShellConfig,
    state: &mut ShellRenderState<'_>,
    hits: &mut ShellHitMap,
) {
    let palette = &config.palette;
    super::render::render_sidebar_background(buffer, area, palette);
    hits.sidebar_divider = Rect::default();
    if area.height < 4 || area.width < 3 {
        return;
    }
    let width = area.width.saturating_sub(1).max(1);
    let layout = projects::layout();
    let mut y = area.y;

    let (needing, blocked) = attention_count(state.endpoints, &layout);
    if needing > 0 {
        let counter = format!("●{}", needing.min(99));
        hits.drovr_attention = Rect::new(area.x, y, width, 1);
        put_text(
            buffer,
            area.x,
            y,
            width,
            &counter,
            Style::default()
                .fg(if blocked {
                    status_color(crate::api::schema::AgentStatus::Blocked, palette)
                } else {
                    Color::Yellow
                })
                .add_modifier(Modifier::BOLD),
        );
    }
    y += 1;
    put_text(
        buffer,
        area.x,
        y,
        width,
        &"─".repeat(width as usize),
        Style::default().fg(palette.surface_dim),
    );
    y += 1;

    let bottom = area.bottom().saturating_sub(1);
    for project in rail_projects(state.endpoints, state.active_endpoint_id, &layout) {
        if y >= bottom {
            break;
        }
        let rect = Rect::new(area.x, y, width, 1);
        if project.current {
            buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
        }
        let (icon, color) = presence_icon(project.presence, config);
        put_text(buffer, area.x, y, 1, icon, Style::default().fg(color));
        put_text(
            buffer,
            area.x + 1,
            y,
            width.saturating_sub(1),
            &project.tag,
            Style::default()
                .fg(if project.key == OTHER {
                    palette.overlay0
                } else if project.current {
                    palette.text
                } else {
                    palette.subtext0
                })
                .add_modifier(Modifier::BOLD),
        );
        hits.drovr_rail.push((rect, project.key.clone()));
        y += 1;
        if project.current && project.key != OTHER {
            // The project you're in, spelled downwards (up to 8 letters).
            for letter in project.name.chars().filter(|c| !c.is_whitespace()).take(8) {
                if y >= bottom {
                    break;
                }
                put_text(
                    buffer,
                    area.x + 1,
                    y,
                    1,
                    &letter.to_string(),
                    Style::default().fg(palette.accent),
                );
                hits.drovr_rail
                    .push((Rect::new(area.x, y, width, 1), project.key.clone()));
                y += 1;
            }
        }
    }

    hits.sidebar_toggle = Rect::new(area.x + width / 2, bottom, 1, 1);
    put_text(
        buffer,
        hits.sidebar_toggle.x,
        bottom,
        1,
        "»",
        Style::default().fg(palette.overlay0),
    );
}

/// Where clicking a project on the rail goes: its most urgent agent (blocked,
/// unread, finished, working, then any), else its first workspace.
pub(super) fn project_target(
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    key: &str,
) -> Option<(ClientEndpointId, ClientEndpointFocusTarget)> {
    let mut layout = projects::layout();
    layout.compact = false;
    layout.active_only = false;
    layout.other_collapsed = false;
    for group in &mut layout.groups {
        group.collapsed = false;
    }
    let rank = |presence: Presence| match presence {
        Presence::Blocked => 0,
        Presence::Unread => 1,
        Presence::Done => 2,
        Presence::Working => 3,
        Presence::Idle => 4,
    };
    let mut in_project = false;
    let mut best: Option<(u8, ClientEndpointId, ClientEndpointFocusTarget)> = None;
    for row in build_rows(endpoints, active_endpoint_id, &layout, true) {
        match row {
            Row::Header { key: header, .. } => in_project = header == key,
            Row::Agent {
                endpoint,
                pane_id,
                presence,
                stale: false,
                ..
            } if in_project => {
                let candidate = rank(presence);
                if best.as_ref().is_none_or(|(rank, _, _)| candidate < *rank) {
                    best = Some((
                        candidate,
                        endpoints[endpoint].endpoint_id.clone(),
                        ClientEndpointFocusTarget::Pane(pane_id),
                    ));
                }
            }
            Row::Workspace {
                endpoint,
                workspace_id,
                stale: false,
                ..
            } if in_project && best.is_none() => {
                best = Some((
                    5,
                    endpoints[endpoint].endpoint_id.clone(),
                    ClientEndpointFocusTarget::Workspace(workspace_id),
                ));
            }
            _ => {}
        }
    }
    best.map(|(_, endpoint_id, target)| (endpoint_id, target))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::AgentStatus;
    use crate::protocol::{ClientShellAgent, ClientShellPane};

    fn workspace(id: &str, label: &str) -> ClientShellWorkspace {
        let mut workspace = super::super::tests::snapshot().workspaces.remove(0);
        workspace.workspace_id = id.into();
        workspace.label = label.into();
        workspace.new_workspace_cwd = format!("/src/{label}");
        workspace.focused = false;
        workspace
    }

    fn agent(pane: &str, workspace: &str, vendor: &str, title: &str) -> ClientShellAgent {
        ClientShellAgent {
            pane_id: pane.into(),
            workspace_id: workspace.into(),
            tab_id: "tab_1".into(),
            name: None,
            display_agent: None,
            agent: Some(vendor.into()),
            title: Some(title.into()),
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: AgentStatus::Idle,
            state_change_seq: 1,
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: false,
        }
    }

    fn endpoint(
        endpoint_id: ClientEndpointId,
        label: &str,
        workspaces: Vec<ClientShellWorkspace>,
        agents: Vec<ClientShellAgent>,
    ) -> ClientShellEndpoint {
        let mut snapshot = super::super::tests::snapshot();
        snapshot.panes = agents
            .iter()
            .map(|agent| ClientShellPane {
                pane_id: agent.pane_id.clone(),
                workspace_id: agent.workspace_id.clone(),
                tab_id: "tab_1".into(),
                label: None,
                cwd: None,
                foreground_cwd: None,
                focused: false,
                right_click_passthrough: false,
            })
            .collect();
        snapshot.workspaces = workspaces;
        snapshot.agents = agents;
        let mut endpoint = super::super::endpoints::local_endpoint();
        endpoint.endpoint_id = endpoint_id;
        endpoint.label = label.into();
        endpoint.snapshot = Some(Box::new(snapshot));
        endpoint
    }

    /// GTM project: local gtm-rd (2 agents) and Code (1 agent); Other: the
    /// remote machine's turfobet.fr (1 agent) and an empty local scratch.
    fn fixture() -> Vec<ClientShellEndpoint> {
        let remote = ClientEndpointId::Ssh(
            crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef")
                .expect("valid profile id"),
        );
        let mut first = agent("p1", "w1", "claude", "Fix auth flow in gateway");
        first.focused = true;
        vec![
            endpoint(
                ClientEndpointId::Local,
                "Local",
                vec![
                    workspace("w1", "gtm-rd"),
                    workspace("w2", "Code"),
                    workspace("w3", "scratch"),
                ],
                vec![
                    first,
                    agent("p2", "w1", "codex", "Review PR 42"),
                    agent("p3", "w2", "claude", "Herdr mix local and remote"),
                ],
            ),
            endpoint(
                remote,
                "mato",
                vec![workspace("w9", "turfobet.fr")],
                vec![agent(
                    "p9",
                    "w9",
                    "claude",
                    "Claude Code settings permissions",
                )],
            ),
        ]
    }

    fn structured_layout() -> ProjectLayout {
        ProjectLayout {
            structured: true,
            groups: vec![projects::ProjectGroup {
                name: "GTM".into(),
                members: vec!["local/w1:gtm-rd".into(), "local/w2:Code".into()],
                ..projects::ProjectGroup::default()
            }],
            ..ProjectLayout::default()
        }
    }

    /// One line per row: H(eader), W(orkspace) with machine tag, A(gent).
    fn describe(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                Row::Header { label, .. } => format!("H {label}"),
                Row::Workspace { label, machine, .. } => {
                    format!("W {label} {}", machine.as_deref().unwrap_or("-"))
                }
                Row::Agent { title, vendor, .. } => {
                    format!(
                        "A {} {title} h{}",
                        vendor.as_deref().unwrap_or("-"),
                        row.height()
                    )
                }
            })
            .collect()
    }

    #[test]
    fn view_toggle_cycles_through_three_views() {
        let mut layout = ProjectLayout::default();
        layout.cycle_view();
        assert!(layout.compact && !layout.structured);
        layout.cycle_view();
        assert!(!layout.compact && layout.structured);
        layout.cycle_view();
        assert!(!layout.compact && !layout.structured);
    }

    #[test]
    fn structured_rows_have_workspace_headers_and_one_line_per_agent() {
        let endpoints = fixture();
        let rows = build_rows(
            &endpoints,
            &ClientEndpointId::Local,
            &structured_layout(),
            true,
        );
        assert_eq!(
            describe(&rows),
            vec![
                "H GTM",
                "W gtm-rd -",
                "A claude Fix auth flow in gateway h1",
                "A codex Review PR 42 h1",
                "W Code -",
                "A claude Herdr mix local and remote h1",
                "H Other",
                "W scratch -",
                "W turfobet.fr mato",
                "A claude Claude Code settings permissions h1",
            ]
        );
        // The header of the workspace holding the focused agent stays plain.
        assert!(rows
            .iter()
            .all(|row| !matches!(row, Row::Workspace { focused: true, .. })));
    }

    #[test]
    fn active_filter_drops_headers_whose_agents_are_all_filtered() {
        let endpoints = fixture();
        let mut layout = structured_layout();
        layout.active_only = true;
        // Every agent is idle with an unknown age, so none is current; the
        // focused workspace still is, but detailed view would show nothing for
        // it, and neither does structured.
        let mut local = endpoints[0].clone();
        if let Some(snapshot) = local.snapshot.as_deref_mut() {
            snapshot.workspaces[0].focused = true;
        }
        let rows = build_rows(&[local], &ClientEndpointId::Local, &layout, true);
        assert!(describe(&rows)
            .iter()
            .all(|row| !row.starts_with("W gtm-rd")));
    }

    #[test]
    fn structured_hits_match_drawn_rows() {
        let endpoints = fixture();
        let config = ClientShellConfig::from_config(&crate::config::Config::default());
        let area = Rect::new(0, 0, 34, 20);
        let mut buffer = Buffer::empty(area);
        let mut hits = ShellHitMap::default();
        let layout = structured_layout();
        let rows = build_rows(&endpoints, &ClientEndpointId::Local, &layout, true);
        let gaps = row_gaps(&rows, &layout, 0, 0);
        // Same stacking as render_panel: each row, then its blank lines.
        let mut y = 0;
        let mut blanks = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let rect = Rect::new(0, y, area.width, row.height());
            render_row(
                &mut buffer,
                rect,
                row,
                &layout,
                &endpoints,
                &config,
                radar::Ground::Dark,
                &mut hits,
            );
            y += row.height();
            blanks.extend(y..y + gaps[index]);
            y += gaps[index];
        }
        let line = |y: u16| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
        };
        let drawn = hits
            .drovr_rows
            .iter()
            .map(|hit| {
                assert_eq!(hit.rect.height, 1);
                (hit.pane_id.clone(), line(hit.rect.y))
            })
            .collect::<Vec<_>>();
        let find = |pane: Option<&str>, text: &str| {
            drawn
                .iter()
                .any(|(hit, line)| hit.as_deref() == pane && line.contains(text))
        };
        assert!(find(Some("p1"), "Fix auth flow in gateway"));
        assert!(find(Some("p2"), "Review PR 42"));
        assert!(find(Some("p3"), "Herdr mix local and remote"));
        assert!(find(None, "gtm-rd"));
        assert!(find(None, "scratch"));
        // Remote workspace header: machine tag on the right; local ones have none.
        assert!(drawn.iter().any(|(hit, line)| hit.is_none()
            && line.contains("turfobet.fr")
            && line.trim_end().ends_with("mato")));
        assert!(!find(None, "Local"));
        // A long title is cut with an ellipsis inside the row.
        assert!(find(Some("p9"), "…"));
        // Project headers sit on their own lines, between the rows.
        for (rect, _) in &hits.projects {
            assert!(hits.drovr_rows.iter().all(|hit| hit.rect.y != rect.y));
        }
        assert_eq!(hits.drovr_rows.len(), 8);
        assert_eq!(hits.endpoint_agents.len(), 4);
        // Blank lines are drawn empty and no click target covers them.
        assert_eq!(blanks.len(), 3);
        let covers = |rect: Rect, y: u16| rect.y <= y && y < rect.bottom();
        for y in blanks {
            assert!(line(y).trim().is_empty(), "{:?}", line(y));
            assert!(hits.drovr_rows.iter().all(|hit| !covers(hit.rect, y)));
            assert!(hits.projects.iter().all(|(rect, _)| !covers(*rect, y)));
            assert!(hits
                .endpoint_agents
                .iter()
                .all(|(rect, _, _)| !covers(*rect, y)));
        }
    }

    #[test]
    fn structured_agents_sit_two_columns_inside_their_workspace_header() {
        let endpoints = fixture();
        let config = ClientShellConfig::from_config(&crate::config::Config::default());
        let layout = structured_layout();
        let rows = build_rows(&endpoints, &ClientEndpointId::Local, &layout, true);
        let start = |row: &Row| {
            let area = Rect::new(0, 0, 34, 1);
            let mut buffer = Buffer::empty(area);
            render_row(
                &mut buffer,
                area,
                row,
                &layout,
                &endpoints,
                &config,
                radar::Ground::Dark,
                &mut ShellHitMap::default(),
            );
            (0..area.width)
                .find(|x| !buffer[(*x, 0)].symbol().trim().is_empty())
                .expect("row draws something")
        };
        let workspace = rows
            .iter()
            .find(|row| matches!(row, Row::Workspace { .. }))
            .expect("workspace header");
        let agents = rows
            .iter()
            .filter(|row| matches!(row, Row::Agent { .. }))
            .collect::<Vec<_>>();
        assert_eq!(start(workspace), STRUCTURED_INDENT);
        assert!(!agents.is_empty());
        for agent in agents {
            assert_eq!(start(agent), start(workspace) + 2);
        }
    }

    #[test]
    fn structured_gaps_follow_each_workspace_group_once() {
        let endpoints = fixture();
        let layout = structured_layout();
        let rows = build_rows(&endpoints, &ClientEndpointId::Local, &layout, true);
        // H GTM, W gtm-rd, A, A | W Code, A | H Other, W scratch | W turfobet.fr, A
        assert_eq!(
            row_gaps(&rows, &layout, 0, 0),
            vec![0, 0, 0, 1, 0, 1, 0, 1, 0, 0]
        );
        // Detailed and compact views keep only the gap before a header.
        for compact in [false, true] {
            let mut flat = layout.clone();
            flat.structured = false;
            flat.compact = compact;
            let rows = build_rows(&endpoints, &ClientEndpointId::Local, &flat, true);
            let gaps = row_gaps(&rows, &flat, 0, 1);
            for (index, gap) in gaps.iter().enumerate() {
                let before_header = matches!(rows.get(index + 1), Some(Row::Header { .. }));
                assert_eq!(*gap, u16::from(before_header));
            }
        }
    }

    #[test]
    fn collapsed_structured_project_shows_only_its_header() {
        let endpoints = fixture();
        let mut layout = structured_layout();
        layout.groups[0].collapsed = true;
        let rows = build_rows(&endpoints, &ClientEndpointId::Local, &layout, true);
        assert_eq!(
            describe(&rows),
            vec![
                "H GTM",
                "H Other",
                "W scratch -",
                "W turfobet.fr mato",
                "A claude Claude Code settings permissions h1",
            ]
        );
        assert_eq!(row_gaps(&rows, &layout, 0, 0), vec![1, 0, 1, 0, 0]);
    }
    #[test]
    fn redraw_is_requested_only_while_an_agent_works() {
        let config = ClientShellConfig::from_config(&crate::config::Config::default());
        let layout = structured_layout();
        let draw = |endpoints: &[ClientShellEndpoint]| {
            let area = Rect::new(0, 0, 34, 1);
            for row in build_rows(endpoints, &ClientEndpointId::Local, &layout, true)
                .iter()
                .filter(|row| matches!(row, Row::Agent { .. }))
            {
                render_row(
                    &mut Buffer::empty(area),
                    area,
                    row,
                    &layout,
                    endpoints,
                    &config,
                    radar::Ground::Dark,
                    &mut ShellHitMap::default(),
                );
            }
            take_spinning()
        };
        let mut endpoints = fixture();
        assert!(!draw(&endpoints));
        if let Some(snapshot) = endpoints[0].snapshot.as_deref_mut() {
            snapshot.agents[1].agent_status = AgentStatus::Working;
        }
        assert!(draw(&endpoints));
        // Taken once per tick: no new render, no new request.
        assert!(!take_spinning());
    }

    #[test]
    fn structured_header_caps_the_machine_tag_on_narrow_sidebars() {
        let mut endpoints = fixture();
        endpoints[1].label = "gpu-box-staging-eu".into();
        let config = ClientShellConfig::from_config(&crate::config::Config::default());
        let layout = structured_layout();
        let rows = build_rows(&endpoints, &ClientEndpointId::Local, &layout, true);
        let header = rows
            .iter()
            .find(|row| {
                matches!(
                    row,
                    Row::Workspace {
                        machine: Some(_),
                        ..
                    }
                )
            })
            .expect("remote workspace header");
        let area = Rect::new(0, 0, 22, 1);
        let mut buffer = Buffer::empty(area);
        render_row(
            &mut buffer,
            area,
            header,
            &layout,
            &endpoints,
            &config,
            radar::Ground::Dark,
            &mut ShellHitMap::default(),
        );
        let line = (0..area.width)
            .map(|x| buffer[(x, 0)].symbol().to_owned())
            .collect::<String>();
        assert!(line.contains("turfobet.fr"), "{line:?}");
        assert!(line.contains("gpu-bo…"), "{line:?}");
    }

    fn placed(y: u16, section: &str, key: Option<&str>) -> Placed {
        Placed {
            y,
            height: 1,
            section: section.into(),
            key: key.map(Into::into),
        }
    }

    #[test]
    fn drop_targets_cover_headers_rows_gaps_and_other() {
        // 0 H A | 1 W a, 2-3 agents | 4 gap | 5 W b, 6 agent | 7 gap |
        // 8 H Other | 9 W c | 10 W d | 11.. empty body down to 14.
        let rows = vec![
            placed(0, "A", None),
            placed(1, "A", Some("a")),
            placed(2, "A", Some("a")),
            placed(3, "A", Some("a")),
            placed(5, "A", Some("b")),
            placed(6, "A", Some("b")),
            placed(8, OTHER, None),
            placed(9, OTHER, Some("c")),
            placed(10, OTHER, Some("d")),
        ];
        let slots = drop_slots(&rows, 0, 20, 14);
        let at = |y: u16| {
            drop_slot_at(&slots, (3, y)).map(|slot| {
                (
                    slot.target.section.clone(),
                    slot.target.before.clone(),
                    slot.target.header,
                    slot.marker,
                )
            })
        };
        let target = |section: &str, before: Option<&str>, header: bool, marker: u16| {
            Some((section.to_owned(), before.map(Into::into), header, marker))
        };
        // Header: append to that section, highlight the header itself.
        assert_eq!(at(0), target("A", None, true, 0));
        assert_eq!(at(8), target(OTHER, None, true, 8));
        // Any row of a workspace (header or agent): before that workspace.
        for y in 1..=3 {
            assert_eq!(at(y), target("A", Some("a"), false, 0));
        }
        // The blank line between two workspaces: before the lower one.
        assert_eq!(at(4), target("A", Some("b"), false, 4));
        // Last workspace: upper half before it, lower half and the gap after
        // the section append (the line under the section shows the marker).
        assert_eq!(at(5), target("A", Some("b"), false, 4));
        assert_eq!(at(6), target("A", None, false, 7));
        assert_eq!(at(7), target("A", None, false, 7));
        // Other: same rules; a one-row last workspace keeps "before it".
        assert_eq!(at(9), target(OTHER, Some("c"), false, 8));
        assert_eq!(at(10), target(OTHER, Some("d"), false, 9));
        assert_eq!(at(12), target(OTHER, None, false, 11));
        // Outside the body or the sidebar: nothing (the drop is cancelled).
        assert_eq!(at(14), None);
        assert!(drop_slot_at(&slots, (25, 1)).is_none());
        // A collapsed (header-only) section takes drops on its trailing gap.
        let slots = drop_slots(&[placed(0, "A", None), placed(2, "B", None)], 0, 20, 4);
        let section = |y| drop_slot_at(&slots, (0, y)).map(|slot| slot.target.clone());
        assert_eq!(
            section(1),
            Some(DropTarget {
                section: "A".into(),
                before: None,
                header: false
            })
        );
        assert!(section(2).is_some_and(|target| target.header && target.section == "B"));
    }

    #[test]
    fn blank_line_separates_toggles_from_the_first_section_in_every_view() {
        let endpoints = fixture();
        let config = ClientShellConfig::from_config(&crate::config::Config::default());
        for (compact, structured) in [(false, false), (true, false), (false, true)] {
            let mut layout = structured_layout();
            layout.compact = compact;
            layout.structured = structured;
            let area = Rect::new(0, 0, 35, 30);
            let mut buffer = Buffer::empty(area);
            let mut hits = ShellHitMap::default();
            let mut scroll = 0;
            render_panel_with(
                &mut buffer,
                area,
                &config,
                &layout,
                &endpoints,
                &ClientEndpointId::Local,
                &mut scroll,
                true,
                radar::Ground::Dark,
                false,
                &mut hits,
            );
            let view = (compact, structured);
            assert_eq!(hits.drovr_filter_toggle.y, 0, "{view:?}");
            assert_eq!(hits.workspace_body.y, 2, "{view:?}");
            assert_eq!(hits.workspace_body.height, 27, "{view:?}");
            let first = hits.projects.first().map(|(rect, _)| rect.y);
            assert_eq!(first, Some(2), "{view:?}");
            let line = (0..area.width.saturating_sub(1))
                .map(|x| buffer[(x, 1)].symbol().to_owned())
                .collect::<String>();
            assert!(line.trim().is_empty(), "{view:?} {line:?}");
            let covers = |rect: Rect| rect.y <= 1 && 1 < rect.bottom();
            assert!(hits.drovr_rows.iter().all(|hit| !covers(hit.rect)));
            assert!(hits.projects.iter().all(|(rect, _)| !covers(*rect)));
            assert!(hits.drovr_drops.iter().all(|slot| !covers(slot.rect)));
            assert!(drop_slot_at(&hits.drovr_drops, (3, 1)).is_none());
            // Every drawn row still resolves to a drop target below the line.
            for hit in &hits.drovr_rows {
                assert!(
                    drop_slot_at(&hits.drovr_drops, (hit.rect.x + 1, hit.rect.y)).is_some(),
                    "{view:?}"
                );
            }
            assert_eq!(scroll, 0, "{view:?}");
        }
    }

    #[test]
    fn dragging_an_agent_row_targets_its_workspace() {
        let endpoints = fixture();
        let config = ClientShellConfig::from_config(&crate::config::Config::default());
        let layout = structured_layout();
        let area = Rect::new(0, 0, 35, 30);
        let mut buffer = Buffer::empty(area);
        let mut hits = ShellHitMap::default();
        render_panel_with(
            &mut buffer,
            area,
            &config,
            &layout,
            &endpoints,
            &ClientEndpointId::Local,
            &mut 0,
            false,
            radar::Ground::Dark,
            false,
            &mut hits,
        );
        let p2 = hits
            .drovr_rows
            .iter()
            .find(|hit| hit.pane_id.as_deref() == Some("p2"))
            .expect("agent row");
        let target = drop_slot_at(&hits.drovr_drops, (2, p2.rect.y)).expect("drop slot");
        assert_eq!(target.target.section, "GTM");
        assert_eq!(target.target.before.as_deref(), Some("local/w1:gtm-rd"));
        // The last agent row of the section appends to it.
        let p3 = hits
            .drovr_rows
            .iter()
            .find(|hit| hit.pane_id.as_deref() == Some("p3"))
            .expect("agent row");
        let last = drop_slot_at(&hits.drovr_drops, (2, p3.rect.y)).expect("drop slot");
        assert_eq!(
            (last.target.section.as_str(), &last.target.before),
            ("GTM", &None)
        );
        // While dragging gtm-rd onto the blank line under the section, that
        // line shows an accent insertion marker carrying the workspace name.
        let gap = p3.rect.y + 1;
        projects::set_press(Some(projects::RowPress {
            endpoint_id: ClientEndpointId::Local,
            workspace_id: "w1".into(),
            pane_id: None,
            start: (2, 3),
            dragging: Some((2, gap)),
        }));
        let mut buffer = Buffer::empty(area);
        let mut hits = ShellHitMap::default();
        render_panel_with(
            &mut buffer,
            area,
            &config,
            &layout,
            &endpoints,
            &ClientEndpointId::Local,
            &mut 0,
            false,
            radar::Ground::Dark,
            false,
            &mut hits,
        );
        projects::clear_press();
        let line = (0..area.width)
            .map(|x| buffer[(x, gap)].symbol().to_owned())
            .collect::<String>();
        assert!(line.starts_with("── gtm-rd ──"), "{line:?}");
        assert_eq!(buffer[(0, gap)].fg, config.palette.accent);
    }

    fn render_with_banner(area: Rect, scroll: usize) -> (Buffer, ShellHitMap, usize) {
        let endpoints = fixture();
        let config = ClientShellConfig::from_config(&crate::config::Config::default());
        let mut buffer = Buffer::empty(area);
        let mut hits = ShellHitMap::default();
        let mut scroll = scroll;
        render_panel_with(
            &mut buffer,
            area,
            &config,
            &structured_layout(),
            &endpoints,
            &ClientEndpointId::Local,
            &mut scroll,
            false,
            radar::Ground::Dark,
            config.banner,
            &mut hits,
        );
        (buffer, hits, scroll)
    }

    fn line(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol().to_owned())
            .collect()
    }

    #[test]
    fn banner_shows_only_when_it_fits() {
        assert!(crate::config::Config::default().ui.sidebar.banner);
        let shown = |width, height| {
            let (buffer, _, _) = render_with_banner(Rect::new(0, 0, width, height), 0);
            line(&buffer, 0).contains(BANNER[0])
        };
        assert_eq!(pixel_text("0.9.3-1")[0].chars().count(), 11);
        assert!(shown(18, 16));
        assert!(shown(34, 40));
        // Narrower than 18 columns, or fewer than 10 rows left for the list.
        assert!(!shown(17, 40));
        assert!(!shown(34, 15));
    }

    #[test]
    fn banner_draws_letters_and_version_on_the_sidebar_background() {
        let config = ClientShellConfig::from_config(&crate::config::Config::default());
        let palette = &config.palette;
        let (buffer, hits, _) = render_with_banner(Rect::new(0, 0, 34, 40), 0);
        // 34 columns leave 15 cells right of the wordmark.
        let version = banner_versions(crate::build_info::DROVR_VERSION)
            .into_iter()
            .map(pixel_text)
            .find(|version| display_width(&version[0]) <= 15)
            .expect("the release version fits");
        for row in 0..2u16 {
            let expected = format!(
                " {}  {}",
                BANNER[usize::from(row)],
                version[usize::from(row)].trim_end()
            );
            assert_eq!(line(&buffer, row).trim_end_matches(['│', ' ']), expected);
        }
        let letter = &buffer[(1, 0)];
        assert_eq!((letter.fg, letter.bg), (palette.text, palette.sidebar_bg));
        assert!(!letter.modifier.contains(Modifier::REVERSED));
        assert_eq!(buffer[(18, 0)].fg, palette.overlay0);
        assert_eq!(buffer[(0, 0)].bg, palette.sidebar_bg);
        // Too narrow for the version: the letters stay, the version goes.
        let (narrow, _, _) = render_with_banner(Rect::new(0, 0, 19, 40), 0);
        assert_eq!(
            line(&narrow, 0).trim_end_matches(['│', ' ']),
            format!(" {}", BANNER[0])
        );
        // Below the banner: the same sidebar, three rows lower.
        let (_, plain, _) = {
            let endpoints = fixture();
            let mut buffer = Buffer::empty(Rect::new(0, 0, 34, 37));
            let mut hits = ShellHitMap::default();
            let mut scroll = 0;
            render_panel_with(
                &mut buffer,
                Rect::new(0, 0, 34, 37),
                &config,
                &structured_layout(),
                &endpoints,
                &ClientEndpointId::Local,
                &mut scroll,
                false,
                radar::Ground::Dark,
                false,
                &mut hits,
            );
            (buffer, hits, scroll)
        };
        assert_eq!(hits.drovr_filter_toggle.y, 3);
        assert_eq!(hits.drovr_view_toggle.y, 3);
        assert_eq!(hits.workspace_body.y, plain.workspace_body.y + 3);
        assert_eq!(hits.workspace_body.height, plain.workspace_body.height);
        let ys = |hits: &ShellHitMap| {
            hits.drovr_rows
                .iter()
                .map(|hit| hit.rect.y)
                .chain(hits.projects.iter().map(|(rect, _)| rect.y))
                .chain(hits.drovr_drops.iter().map(|slot| slot.rect.y))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ys(&hits),
            ys(&plain).iter().map(|y| y + 3).collect::<Vec<_>>()
        );
        // Nothing on the banner rows is clickable or a drop target.
        for y in 0..3 {
            assert!(ys(&hits).iter().all(|row| *row >= 5), "{y}");
            assert!(drop_slot_at(&hits.drovr_drops, (3, y)).is_none());
        }
        // Scrolling still counts rows of the list only: two rows down skips
        // the GTM header and the gtm-rd workspace header.
        let (_, scrolled, offset) = render_with_banner(Rect::new(0, 0, 34, 16), 2);
        assert_eq!(offset, 2);
        assert_eq!(scrolled.workspace_body, Rect::new(0, 5, 33, 10));
        assert_eq!(
            scrolled
                .drovr_rows
                .first()
                .map(|hit| hit.pane_id.as_deref()),
            Some(Some("p1"))
        );
    }

    #[test]
    fn pixel_digits_are_two_rows_two_cells_and_distinct() {
        let digits = ('0'..='9')
            .map(|digit| pixel_glyph(digit).expect("digit glyph"))
            .collect::<Vec<_>>();
        for glyph in &digits {
            assert_eq!(glyph.len(), 2);
            assert!(glyph.iter().all(|row| row.chars().count() == 2));
        }
        for (a, left) in digits.iter().enumerate() {
            for right in &digits[a + 1..] {
                assert_ne!(left, right);
            }
        }
        for mark in ['.', '-'] {
            let glyph = pixel_glyph(mark).expect("mark glyph");
            assert!(glyph.iter().all(|row| row.chars().count() == 1));
        }
        assert_ne!(pixel_glyph('.'), pixel_glyph('-'));
        assert_eq!(pixel_glyph('+'), Some(["▟▖".to_string(), "▝ ".to_string()]));
        assert_eq!(pixel_text("x"), [String::new(), String::new()]);
    }

    #[test]
    fn banner_versions_drop_suffixes_widest_first() {
        assert_eq!(
            banner_versions("0.9.3-3+1.dirty"),
            ["0.9.3-3+1", "0.9.3-3", "0.9.3"]
        );
        assert_eq!(
            banner_versions("0.9.3-3+12"),
            ["0.9.3-3+12", "0.9.3-3", "0.9.3"]
        );
        assert_eq!(banner_versions("0.9.3-3"), ["0.9.3-3", "0.9.3"]);
        assert_eq!(banner_versions("0.9.3+dev"), ["0.9.3"]);
        assert_eq!(banner_versions("0.9.3"), ["0.9.3"]);
        // Each candidate renders with glyphs only: 15 cells for "0.9.3-3+1".
        assert_eq!(pixel_text("0.9.3-3+1")[0].chars().count(), 15);
        assert_eq!(pixel_text("0.9.3-3+1")[1].chars().count(), 15);
    }

    /// Renders the whole structured panel with the default config adjusted
    /// by `tweak`; returns the buffer and hit map.
    fn render_structured(
        endpoints: &[ClientShellEndpoint],
        layout: &ProjectLayout,
        tweak: impl FnOnce(&mut ClientShellConfig),
    ) -> (Buffer, ShellHitMap) {
        let mut config = ClientShellConfig::from_config(&crate::config::Config::default());
        tweak(&mut config);
        let area = Rect::new(0, 0, 35, 30);
        let mut buffer = Buffer::empty(area);
        let mut hits = ShellHitMap::default();
        render_panel_with(
            &mut buffer,
            area,
            &config,
            layout,
            endpoints,
            &ClientEndpointId::Local,
            &mut 0,
            false,
            radar::Ground::Dark,
            false,
            &mut hits,
        );
        (buffer, hits)
    }

    #[test]
    fn structured_section_header_draws_name_rule_and_inbox_count() {
        let mut endpoints = fixture();
        if let Some(snapshot) = endpoints[0].snapshot.as_deref_mut() {
            snapshot.agents[1].agent_status = AgentStatus::Blocked;
        }
        let config = ClientShellConfig::from_config(&crate::config::Config::default());
        let mut layout = structured_layout();
        let header_line = |layout: &ProjectLayout| {
            let (buffer, hits) = render_structured(&endpoints, layout, |_| {});
            let (rect, key) = hits.projects.first().cloned().expect("GTM header");
            assert_eq!(key, "GTM");
            // The header is still a click and drop target.
            let slot = drop_slot_at(&hits.drovr_drops, (3, rect.y)).expect("drop slot");
            assert!(slot.target.header && slot.target.section == "GTM");
            let text = (rect.x..rect.right())
                .map(|x| buffer[(x, rect.y)].symbol().to_owned())
                .collect::<String>();
            let bullet = (rect.x..rect.right())
                .find(|x| buffer[(*x, rect.y)].symbol() == "●")
                .map(|x| buffer[(x, rect.y)].fg);
            (text, bullet, buffer[(rect.x + 7, rect.y)].fg)
        };
        let (text, bullet, rule) = header_line(&layout);
        assert!(text.starts_with(" ▾ GTM ─"), "{text:?}");
        assert!(text.ends_with("─ ● 1 "), "{text:?}");
        assert_eq!(rule, config.palette.surface_dim);
        assert_eq!(
            bullet,
            Some(status_color(AgentStatus::Blocked, &config.palette))
        );
        // Collapsed: same count.
        layout.groups[0].collapsed = true;
        let (text, _, _) = header_line(&layout);
        assert!(text.starts_with(" ▸ GTM ─"), "{text:?}");
        assert!(text.ends_with("─ ● 1 "), "{text:?}");
        // Nothing in the inbox: no count.
        let rows = build_rows(&fixture(), &ClientEndpointId::Local, &layout, true);
        assert!(matches!(
            rows.first(),
            Some(Row::Header {
                items: 0,
                waiting: false,
                ..
            })
        ));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 1));
        render_structured_header(
            &mut buffer,
            Rect::new(0, 0, 30, 1),
            StructuredHeader {
                label: "▾ GTM",
                other: false,
                items: 0,
                waiting: false,
                usage: None,
            },
            &config.palette,
        );
        assert_eq!(line(&buffer, 0), format!(" ▾ GTM {} ", "─".repeat(22)));
    }

    #[test]
    fn agent_titles_prefer_the_session_name_then_a_real_terminal_title() {
        let pane = |cwd: &str, label: Option<&str>| ClientShellPane {
            pane_id: "p1".into(),
            workspace_id: "w1".into(),
            tab_id: "tab_1".into(),
            label: label.map(Into::into),
            cwd: Some(cwd.into()),
            foreground_cwd: None,
            focused: false,
            right_click_passthrough: false,
        };
        let mut tab = super::super::tests::snapshot()
            .tabs
            .first()
            .cloned()
            .expect("snapshot tab");
        tab.label = "release".into();
        let title = |terminal: &str,
                     tokens: &[(&str, &str)],
                     pane: &ClientShellPane,
                     tab: Option<&crate::protocol::ClientShellTab>| {
            let mut agent = agent("p1", "w1", "claude", "");
            agent.title = None;
            agent.terminal_title_stripped = Some(terminal.into());
            agent.tokens = tokens
                .iter()
                .map(|(key, value)| ((*key).into(), (*value).into()))
                .collect();
            agent_title(&agent, Some(pane), tab)
        };
        let plain = pane("/Users/me/src/mlx-serve", None);
        // 1. The hook's session name wins over anything the terminal says.
        assert_eq!(
            title(
                "Fix the parser",
                &[("drovr_name", "Access downgrade after renewal")],
                &plain,
                None
            ),
            "Access downgrade after renewal"
        );
        // A name left on the pane by an earlier Claude session does not
        // rename another agent running there now.
        let mut codex = agent("p1", "w1", "codex", "");
        codex.title = None;
        codex.terminal_title_stripped = Some("Port the CLI".into());
        codex.tokens = vec![
            ("drovr_name".into(), "Old claude session".into()),
            ("drovr_ctx".into(), "1200".into()),
        ];
        assert_eq!(agent_title(&codex, Some(&plain), None), "Port the CLI");
        assert_eq!(projects::agent_context_tokens(&codex), None);
        // 2. A real terminal title.
        assert_eq!(title("Fix the parser", &[], &plain, None), "Fix the parser");
        // 3. The vendor's product name or the folder alone give way to the
        //    pane's label, its tab's custom name, else its folder's name.
        assert_eq!(title("Claude Code", &[], &plain, None), "mlx-serve");
        assert_eq!(title("~/src/mlx-serve", &[], &plain, None), "mlx-serve");
        assert_eq!(
            title("/Users/me/src/mlx-serve: zsh", &[], &plain, None),
            "mlx-serve"
        );
        assert_eq!(
            title("Claude Code", &[], &pane("/x", Some("api pane")), None),
            "api pane"
        );
        tab.custom_label = true;
        assert_eq!(title("claude code", &[], &plain, Some(&tab)), "release");
        tab.custom_label = false;
        assert_eq!(title("Claude Code", &[], &plain, Some(&tab)), "mlx-serve");
        // A title that only starts with the folder's name is a real title.
        assert_eq!(
            title("mlx-serve: speed up", &[], &plain, None),
            "mlx-serve: speed up"
        );
        // 4. Nothing better: the generic title, else the product name.
        assert_eq!(
            title("Claude Code", &[], &pane("", None), None),
            "Claude Code"
        );
        assert_eq!(title(" ", &[], &pane("", None), None), "Claude Code");
        // The rows use it in every view.
        let mut endpoints = fixture();
        if let Some(snapshot) = endpoints[0].snapshot.as_deref_mut() {
            snapshot.agents[0].tokens = vec![("drovr_name".into(), "Named session".into())];
        }
        for (compact, structured) in [(false, false), (false, true)] {
            let mut layout = structured_layout();
            layout.compact = compact;
            layout.structured = structured;
            let rows = build_rows(&endpoints, &ClientEndpointId::Local, &layout, true);
            assert!(describe(&rows)
                .iter()
                .any(|row| row.starts_with("A claude Named session")));
        }
    }

    #[test]
    fn empty_workspaces_are_dimmed_or_hidden_in_the_structured_view() {
        let mut endpoints = fixture();
        // Code's agent works, so Code is current (not faded).
        if let Some(snapshot) = endpoints[0].snapshot.as_deref_mut() {
            snapshot.agents[2].agent_status = AgentStatus::Working;
        }
        let layout = structured_layout();
        let (buffer, hits) = render_structured(&endpoints, &layout, |_| {});
        let scratch = hits
            .drovr_rows
            .iter()
            .find(|hit| hit.workspace_id == "w3")
            .expect("empty workspace row");
        assert!(line(&buffer, scratch.rect.y).contains("scratch"));
        assert!(buffer[(STRUCTURED_INDENT, scratch.rect.y)]
            .modifier
            .contains(Modifier::DIM));
        // Dimmed for being empty, not only for being inactive.
        let config = ClientShellConfig::from_config(&crate::config::Config::default());
        for mut row in build_rows(&endpoints, &ClientEndpointId::Local, &layout, true) {
            let Row::Workspace { faded, empty, .. } = &mut row else {
                continue;
            };
            *faded = false;
            let empty = *empty;
            let area = Rect::new(0, 0, 34, 1);
            let mut buffer = Buffer::empty(area);
            render_row(
                &mut buffer,
                area,
                &row,
                &layout,
                &endpoints,
                &config,
                radar::Ground::Dark,
                &mut ShellHitMap::default(),
            );
            let dim = buffer[(STRUCTURED_INDENT, 0)]
                .modifier
                .contains(Modifier::DIM);
            assert_eq!(dim, empty);
        }
        // Workspaces with agents stay undimmed.
        let code = hits
            .drovr_rows
            .iter()
            .find(|hit| hit.workspace_id == "w2" && hit.pane_id.is_none())
            .expect("workspace header");
        assert!(!buffer[(STRUCTURED_INDENT, code.rect.y)]
            .modifier
            .contains(Modifier::DIM));
        // show_empty_workspaces = false: not drawn, no click or row target.
        let (buffer, hits) = render_structured(&endpoints, &layout, |config| {
            config.show_empty_workspaces = false;
        });
        assert!(hits.drovr_rows.iter().all(|hit| hit.workspace_id != "w3"));
        assert!((0..30).all(|y| !line(&buffer, y).contains("scratch")));
        let rows = build_rows(&endpoints, &ClientEndpointId::Local, &layout, false);
        assert!(!describe(&rows).contains(&"W scratch -".to_owned()));
        // ...unless it is the focused workspace, which stays reachable.
        let mut focused = endpoints.clone();
        if let Some(snapshot) = focused[0].snapshot.as_deref_mut() {
            for workspace in &mut snapshot.workspaces {
                workspace.focused = workspace.workspace_id == "w3";
            }
        }
        let rows = build_rows(&focused, &ClientEndpointId::Local, &layout, false);
        assert!(describe(&rows).contains(&"W scratch -".to_owned()));
        // Other views keep them.
        let mut detailed = layout.clone();
        detailed.structured = false;
        assert!(!shows_empty(&layout, false));
        assert!(shows_empty(&detailed, false));
        let rows = build_rows(
            &endpoints,
            &ClientEndpointId::Local,
            &detailed,
            shows_empty(&detailed, false),
        );
        assert!(describe(&rows).contains(&"W scratch -".to_owned()));
        let config: crate::config::Config =
            toml::from_str("[ui.sidebar]\nshow_empty_workspaces = false\nagent_gap = 0\n")
                .expect("valid config");
        assert!(!config.ui.sidebar.show_empty_workspaces);
        assert_eq!(config.ui.sidebar.agent_gap, 0);
        let defaults = crate::config::Config::default().ui.sidebar;
        assert!(defaults.show_empty_workspaces);
        assert_eq!(defaults.agent_gap, 1);
    }

    /// The fixture with hook tokens: p1 (claude, gtm-rd) runs a tool, p2
    /// (codex, gtm-rd) waits on a dialog, p3 (claude, Code) finished with a
    /// question.
    fn signal_fixture() -> Vec<ClientShellEndpoint> {
        let mut endpoints = fixture();
        let started = agent_signal::unix_now() - 30;
        let agents = &mut endpoints[0].snapshot.as_mut().expect("snapshot").agents;
        agents[0].agent_status = AgentStatus::Working;
        agents[0].tokens = vec![
            ("drovr_state".into(), format!("working|{started}")),
            ("drovr_doing".into(), "Bash cargo test".into()),
        ];
        agents[1].agent_status = AgentStatus::Blocked;
        agents[2].agent_status = AgentStatus::Done;
        agents[2].tokens = vec![("drovr_state".into(), format!("asks|{started}"))];
        endpoints
    }

    /// The drawn line of the row for `pane` (an agent) or, with `None`, of
    /// the workspace `workspace`'s own row.
    fn row_text(
        buffer: &Buffer,
        hits: &ShellHitMap,
        workspace: &str,
        pane: Option<&str>,
    ) -> String {
        let hit = hits
            .drovr_rows
            .iter()
            .find(|hit| hit.workspace_id == workspace && hit.pane_id.as_deref() == pane)
            .expect("row");
        line(buffer, hit.rect.y).trim_end().to_owned()
    }

    fn workspace_filter(workspace: &str) -> InboxFilter {
        InboxFilter::Workspace {
            endpoint_id: ClientEndpointId::Local,
            workspace_id: workspace.into(),
        }
    }

    #[test]
    fn structured_rows_show_the_running_tool_and_glyphs_on_workspaces() {
        let endpoints = signal_fixture();
        let config = ClientShellConfig::from_config(&crate::config::Config::default());
        let (buffer, hits) = render_structured(&endpoints, &structured_layout(), |_| {});
        // Working: the tool and its time instead of the title.
        let working = row_text(&buffer, &hits, "w1", Some("p1"));
        assert!(working.contains("▸ Bash cargo test"), "{working:?}");
        assert!(!working.contains("Fix auth flow"), "{working:?}");
        assert!(
            working.ends_with("30s") || working.ends_with("31s"),
            "{working:?}"
        );
        // Waiting and done: the glyph on the workspace row only.
        assert!(row_text(&buffer, &hits, "w1", None).ends_with('◆'));
        assert!(row_text(&buffer, &hits, "w2", None).ends_with('?'));
        let waiting = row_text(&buffer, &hits, "w1", Some("p2"));
        assert!(waiting.ends_with("Review PR 42"), "{waiting:?}");
        assert!(row_text(&buffer, &hits, "w2", Some("p3")).ends_with("remote"));
        // One count per section, in the blocked colour while one waits.
        let header = hits.projects[0].0;
        assert!(line(&buffer, header.y).trim_end().ends_with("─ ● 2"));
        let (count, filter) = hits
            .drovr_inbox
            .iter()
            .find(|(rect, _)| rect.y == header.y)
            .expect("count target");
        assert_eq!(filter, &InboxFilter::Project("GTM".into()));
        assert_eq!(
            buffer[(count.x, count.y)].fg,
            status_color(AgentStatus::Blocked, &config.palette)
        );
        assert_eq!(buffer[(count.x, count.y)].symbol(), "●");
        // Each glyph is a click target for its workspace.
        let glyphs = hits
            .drovr_inbox
            .iter()
            .filter(|(rect, _)| rect.y != header.y)
            .collect::<Vec<_>>();
        assert_eq!(glyphs.len(), 2);
        for (rect, filter) in glyphs {
            let InboxFilter::Workspace { workspace_id, .. } = filter else {
                panic!("workspace filter");
            };
            let glyph = buffer[(rect.x, rect.y)].symbol();
            assert_eq!(glyph, if workspace_id == "w1" { "◆" } else { "?" });
        }
    }

    #[test]
    fn detailed_rows_carry_glyphs_and_compact_rows_the_first_item() {
        let endpoints = signal_fixture();
        let detailed = ProjectLayout {
            structured: false,
            ..structured_layout()
        };
        let (buffer, hits) = render_structured(&endpoints, &detailed, |_| {});
        assert!(row_text(&buffer, &hits, "w1", Some("p1")).contains("▸ Bash cargo test"));
        assert!(row_text(&buffer, &hits, "w1", Some("p2")).ends_with('◆'));
        assert!(row_text(&buffer, &hits, "w2", Some("p3")).ends_with('?'));
        let filters = hits
            .drovr_inbox
            .iter()
            .map(|(_, filter)| filter.clone())
            .collect::<Vec<_>>();
        assert!(filters.contains(&InboxFilter::Project("GTM".into())));
        assert!(filters.contains(&workspace_filter("w1")));
        assert!(filters.contains(&workspace_filter("w2")));

        let compact = ProjectLayout {
            compact: true,
            ..structured_layout()
        };
        let (buffer, hits) = render_structured(&endpoints, &compact, |_| {});
        // gtm-rd: the dialog (p2) sorts before nothing (p1 only works).
        assert!(row_text(&buffer, &hits, "w1", None).ends_with('◆'));
        assert!(row_text(&buffer, &hits, "w2", None).ends_with('?'));
        assert!(!row_text(&buffer, &hits, "w9", None).contains('?'));
    }

    #[test]
    fn header_inbox_counts_skip_offline_machines_and_hidden_workspaces() {
        let mut endpoints = fixture();
        for endpoint in &mut endpoints {
            if let Some(snapshot) = endpoint.snapshot.as_deref_mut() {
                for agent in &mut snapshot.agents {
                    agent.agent_status = AgentStatus::Blocked;
                }
            }
        }
        let needs = |endpoints: &[ClientShellEndpoint], layout: &ProjectLayout| {
            build_rows(endpoints, &ClientEndpointId::Local, layout, true)
                .into_iter()
                .filter_map(|row| match row {
                    Row::Header {
                        label,
                        items,
                        waiting,
                        ..
                    } => Some((label, items, waiting)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let mut layout = structured_layout();
        endpoints[1].status = ClientEndpointStatus::Online;
        assert_eq!(
            needs(&endpoints, &layout),
            vec![("GTM".into(), 3, true), ("Other".into(), 1, true)]
        );
        // The remote machine drops offline; Code is hidden but shown.
        endpoints[1].status = ClientEndpointStatus::Reconnecting;
        layout.hidden = vec!["local/w2:Code".into()];
        layout.show_hidden = true;
        assert_eq!(
            needs(&endpoints, &layout),
            vec![("GTM".into(), 2, true), ("Other".into(), 0, false)]
        );
        assert_eq!(attention_count(&endpoints, &layout), (2, true));
    }

    #[test]
    fn agent_gap_spaces_a_workspaces_agents_without_doubling_gaps() {
        let endpoints = fixture();
        let layout = structured_layout();
        let rows = build_rows(&endpoints, &ClientEndpointId::Local, &layout, true);
        // H GTM, W gtm-rd, A p1 _ A p2 | W Code, A | H Other, W scratch | W turfobet.fr, A
        assert_eq!(
            row_gaps(&rows, &layout, 0, 1),
            vec![0, 0, 1, 1, 0, 1, 0, 1, 0, 0]
        );
        // Values above one count as one; no row is followed by two blanks.
        assert_eq!(
            row_gaps(&rows, &layout, 0, 5),
            row_gaps(&rows, &layout, 0, 1)
        );
        // Drawn: the blank between p1 and p2 is empty, not clickable, and
        // drops before their workspace like the rows around it.
        let (buffer, hits) = render_structured(&endpoints, &layout, |config| {
            config.agent_gap = 1;
        });
        let y = |pane: &str| {
            hits.drovr_rows
                .iter()
                .find(|hit| hit.pane_id.as_deref() == Some(pane))
                .map(|hit| hit.rect.y)
                .expect("agent row")
        };
        assert_eq!(y("p2"), y("p1") + 2);
        let blank = y("p1") + 1;
        assert!(line(&buffer, blank).trim_end_matches(['│', ' ']).is_empty());
        let covers = |rect: Rect| rect.y <= blank && blank < rect.bottom();
        assert!(hits.drovr_rows.iter().all(|hit| !covers(hit.rect)));
        assert!(hits
            .endpoint_agents
            .iter()
            .all(|(rect, _, _)| !covers(*rect)));
        let slot = drop_slot_at(&hits.drovr_drops, (3, blank)).expect("drop slot");
        assert_eq!(slot.target.before.as_deref(), Some("local/w1:gtm-rd"));
        // Every hit sits on a row that shows its text.
        for hit in &hits.drovr_rows {
            assert!(!line(&buffer, hit.rect.y).trim().is_empty());
        }
        // agent_gap = 0: agents stack.
        let (_, tight) = render_structured(&endpoints, &layout, |config| {
            config.agent_gap = 0;
        });
        let rows_y = tight
            .drovr_rows
            .iter()
            .filter(|hit| hit.workspace_id == "w1")
            .map(|hit| hit.rect.y)
            .collect::<Vec<_>>();
        assert_eq!(rows_y, vec![rows_y[0], rows_y[0] + 1, rows_y[0] + 2]);
    }
}
