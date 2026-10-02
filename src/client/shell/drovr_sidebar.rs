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

enum Row {
    Header {
        key: String,
        label: String,
        pinned: bool,
        collapsed: bool,
        presence: Presence,
        count: usize,
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
}

fn agent_title(agent: &crate::protocol::ClientShellAgent) -> String {
    [
        agent.terminal_title_stripped.as_deref(),
        agent.title.as_deref(),
        agent.display_agent.as_deref(),
        agent.name.as_deref(),
        agent.agent.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|title| !title.is_empty())
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

fn build_rows(
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    layout: &ProjectLayout,
) -> Vec<Row> {
    // Agents in navigation order (the same list prefix+alt+N / prefix+# use).
    let mut agents: HashMap<(usize, String), Vec<AgentInfo>> = HashMap::new();
    let mut next_number = 0usize;
    for row in super::aggregate_navigation::aggregate_agent_rows(
        endpoints,
        active_endpoint_id,
        crate::config::AgentPanelSortConfig::Spaces,
    ) {
        let endpoint = &endpoints[row.endpoint.endpoint_index];
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
                title: agent_title(row.agent),
                number,
                vendor: row.agent.agent.clone(),
                tone: radar::tone(presence, unknown, idle),
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
            presences.push(presence);
            count += 1;
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
            usage,
        });
        if !collapsed {
            rows.extend(body);
        }
    }
    rows
}

/// Workspaces in sidebar order, as alt+up/down should walk them: projects then
/// Other, skipping hidden ones and (in the active view) inactive ones.
/// Collapsed projects still count; collapsing is about space, not relevance.
pub(super) fn ordered_workspaces(
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
) -> Vec<(ClientEndpointId, String)> {
    let mut layout = projects::layout();
    layout.compact = true;
    layout.other_collapsed = false;
    for group in &mut layout.groups {
        group.collapsed = false;
    }
    build_rows(endpoints, active_endpoint_id, &layout)
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
        hits,
    );
}

/// The full sidebar into `area` (also used for the peek over a collapsed rail).
#[allow(clippy::too_many_arguments)] // one render pass; a struct would only shuffle these
pub(super) fn render_panel(
    buffer: &mut Buffer,
    area: Rect,
    config: &ClientShellConfig,
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    workspace_scroll: &mut usize,
    reveal_focused: bool,
    hits: &mut ShellHitMap,
) {
    let palette = &config.palette;
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
    let inner = Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height);
    let layout = projects::layout();

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
    let (needing, blocked) = attention_count(endpoints, &layout);
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

    let rows = build_rows(endpoints, active_endpoint_id, &layout);
    let body = Rect::new(
        inner.x,
        inner.y + 1,
        inner.width,
        inner.height.saturating_sub(2),
    );
    hits.workspace_body = body;
    let row_heights = rows.iter().map(Row::height).collect::<Vec<_>>();
    let gaps = rows
        .iter()
        .enumerate()
        .map(|(index, row)| match (row, rows.get(index + 1)) {
            (_, Some(Row::Header { .. })) => 1,
            (Row::Agent { .. }, Some(Row::Agent { .. }))
                if !layout.compact && !layout.structured =>
            {
                config.agents.row_gap
            }
            _ => 0,
        })
        .collect::<Vec<_>>();
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
    let drag_point = projects::press().and_then(|press| press.dragging);

    let mut y = body.y;
    for (index, row) in rows.iter().enumerate().skip(*workspace_scroll) {
        let height = row_heights[index];
        if y.saturating_add(height) > body.bottom() {
            break;
        }
        let rect = Rect::new(body.x, y, width, height);
        render_row(
            buffer, rect, row, &layout, endpoints, config, drag_point, hits,
        );
        y = y.saturating_add(height).saturating_add(gaps[index]);
    }
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

#[allow(clippy::too_many_arguments)] // one render pass; splitting only shuffles args
fn render_row(
    buffer: &mut Buffer,
    rect: Rect,
    row: &Row,
    layout: &ProjectLayout,
    endpoints: &[ClientShellEndpoint],
    config: &ClientShellConfig,
    drag_point: Option<(u16, u16)>,
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
    if layout.structured && !layout.compact && !matches!(row, Row::Header { .. }) {
        render_structured_row(buffer, rect, row, endpoints, config, &right_slot, hits);
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
            usage,
        } => {
            if drag_point.is_some_and(|point| super::contains(rect, point)) {
                buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
            }
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
            ..
        } => {
            let endpoint = &endpoints[*endpoint];
            if *focused {
                buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
            }
            let (icon, color) = presence_icon(*presence, config);
            let (slot, slot_style) = right_slot(*number, age, ctx);
            let number_width = display_width(&slot) + u16::from(!slot.is_empty());
            let title = if *kept {
                format!("⚑ {title}")
            } else {
                title.clone()
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
            let (slot, slot_style) = right_slot(*number, age, &None);
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
            hits.drovr_rows.push(RowHit {
                rect,
                endpoint_id: endpoint.endpoint_id.clone(),
                workspace_id: workspace_id.clone(),
                pane_id: None,
            });
        }
    }
}

/// Structured rows start one column right of the project header's "▾".
const STRUCTURED_INDENT: u16 = 2;

type RightSlot<'a> =
    dyn Fn(Option<usize>, &Option<String>, &Option<String>) -> (String, Style) + 'a;

/// Structured view rows: a workspace header (label, remote machine on the
/// right) or one agent line (vendor mark, state mark, state-coloured title).
fn render_structured_row(
    buffer: &mut Buffer,
    rect: Rect,
    row: &Row,
    endpoints: &[ClientShellEndpoint],
    config: &ClientShellConfig,
    right_slot: &RightSlot<'_>,
    hits: &mut ShellHitMap,
) {
    let palette = &config.palette;
    let light = radar::is_light(palette);
    match row {
        Row::Header { .. } => {}
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
            ..
        } => {
            let endpoint = &endpoints[*endpoint];
            if *focused {
                buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
            }
            let (slot, slot_style) = right_slot(*number, age, ctx);
            put_right_text(buffer, rect, rect.y, &slot, slot_style);
            let right = rect
                .right()
                .saturating_sub(display_width(&slot) + u16::from(!slot.is_empty()));
            let mut x = rect.x + STRUCTURED_INDENT;
            if let Some((mark, color)) = radar::logo(
                vendor.as_deref(),
                config.agent_icons,
                light,
                palette.overlay0,
            ) {
                put_text(buffer, x, rect.y, 1, &mark, Style::default().fg(color));
                x = x.saturating_add(2);
            }
            let (color, bold) = radar::title_style(*tone, vendor.as_deref(), light);
            let mut style = Style::default().fg(color);
            if bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            let text = [
                radar::lead(*tone),
                kept.then_some("⚑"),
                Some(title.as_str()),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
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
            ..
        } => {
            let endpoint = &endpoints[*endpoint];
            if *focused {
                buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
            }
            let (slot, slot_style) = right_slot(*number, age, &None);
            put_right_text(buffer, rect, rect.y, &slot, slot_style);
            let tag = machine
                .as_ref()
                .map(|machine| format!("{machine} "))
                .unwrap_or_default();
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
                        radar::subtle(light)
                    })
                    .add_modifier(Modifier::BOLD),
            );
            if *stale || *hidden || (*faded && !*focused) {
                buffer.set_style(rect, Style::default().add_modifier(Modifier::DIM));
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
    for row in build_rows(endpoints, active_endpoint_id, &view) {
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
    for row in build_rows(endpoints, active_endpoint_id, &layout) {
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
        let rows = build_rows(&endpoints, &ClientEndpointId::Local, &structured_layout());
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
        let rows = build_rows(&[local], &ClientEndpointId::Local, &layout);
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
        // Same stacking as render_panel: one line per row, a gap before headers.
        let mut y = 0;
        for (index, row) in build_rows(&endpoints, &ClientEndpointId::Local, &layout)
            .iter()
            .enumerate()
        {
            if index > 0 && matches!(row, Row::Header { .. }) {
                y += 1;
            }
            let rect = Rect::new(0, y, area.width, row.height());
            render_row(
                &mut buffer,
                rect,
                row,
                &layout,
                &endpoints,
                &config,
                None,
                &mut hits,
            );
            y += row.height();
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
    }
}
