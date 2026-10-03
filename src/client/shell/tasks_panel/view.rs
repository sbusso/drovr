//! The task view (docs/design/tasks.md, section 4.3): one task inside the
//! panel. The header line and the notes composer stay in place; everything
//! between them scrolls as one column.

use super::*;
use crate::tasks::{Attempt, DecisionState, Entry};

/// A line of the decision card.
#[derive(Clone, Debug, PartialEq)]
enum DecLine {
    Title,
    Summary(String),
    Choice(usize),
    Reply,
}

/// One line of the scrolling column.
#[derive(Clone, Debug, PartialEq)]
enum VLine {
    Waiting(String),
    Title,
    Meta(Vec<(String, Option<Hit>)>),
    Desc(String),
    DescControls {
        foldable: bool,
        empty: bool,
    },
    CritHeader,
    Crit(usize),
    Evidence(String),
    AddCrit,
    /// A decision card line and its frame glyph.
    Dec(DecLine, &'static str),
    Review,
    SendBack,
    Tabs,
    Pinned(usize),
    EntryHead(usize),
    EntryBody(String),
    Events(i64, usize),
    Event(usize),
    Attempt(usize),
    Artifact(usize),
    Empty(&'static str),
    Footer,
}

fn live(detail: &TaskDetail) -> Option<&Attempt> {
    detail
        .attempts
        .iter()
        .find(|attempt| attempt.ended_at.is_none())
}

fn open_decision(detail: &TaskDetail) -> Option<&crate::tasks::Decision> {
    detail
        .decision
        .as_ref()
        .filter(|decision| decision.state == DecisionState::Open)
}

/// `machine/label` of a workspace key (`machine/id:label`).
fn workspace_label(key: &str) -> String {
    match key.split_once('/') {
        Some((machine, rest)) => {
            format!(
                "{machine}/{}",
                rest.split_once(':').map_or(rest, |(_, label)| label)
            )
        }
        None => key.to_owned(),
    }
}

fn cost(detail: &TaskDetail) -> Option<String> {
    let cents: i64 = detail.attempts.iter().filter_map(|a| a.cost_cents).sum();
    (cents > 0).then(|| format!("${}.{:02}", cents / 100, cents % 100))
}

fn input_of(state: &TasksState, matches: impl Fn(&Purpose) -> bool) -> Option<&Input> {
    state.input.as_ref().filter(|input| matches(&input.purpose))
}

/// The scrolling column for `width` columns and a panel `height` lines high.
fn lines(
    state: &TasksState,
    detail: &TaskDetail,
    endpoints: &[ClientShellEndpoint],
    layout: &ProjectLayout,
    width: u16,
    height: u16,
) -> Vec<VLine> {
    let task = &detail.task;
    let now = super::super::agent_signal::unix_now();
    let mut out = Vec::new();
    let waiting = live(detail)
        .and_then(|attempt| attempt.pane_key.as_deref())
        .and_then(|pane| pane_waiting(endpoints, layout, pane))
        .or_else(|| {
            open_decision(detail).map(|decision| {
                format!(
                    "waiting on you: decision · {}",
                    age(&decision.created_at, now)
                )
            })
        });
    if let Some(waiting) = waiting {
        out.push(VLine::Waiting(waiting));
    }
    out.push(VLine::Title);
    // Meta pairs, wrapped.
    let mut pairs: Vec<(String, Option<Hit>)> = vec![
        (
            format!("kind {}", task.kind.map_or("–", kind_name)),
            Some(Hit::Kind),
        ),
        (
            format!("pri {}", priority_name(task.priority)),
            Some(Hit::Priority),
        ),
    ];
    if let Some(key) = &task.workspace_key {
        pairs.push((format!("ws {}", workspace_label(key)), Some(Hit::Workspace)));
    }
    if let Some(cost) = cost(detail) {
        pairs.push((cost, None));
    }
    let mut line: Vec<(String, Option<Hit>)> = Vec::new();
    let mut used = 0;
    for pair in pairs {
        let w = display_width(&pair.0);
        if !line.is_empty() && used + 2 + w > width {
            out.push(VLine::Meta(std::mem::take(&mut line)));
            used = 0;
        }
        used += if line.is_empty() { w } else { w + 2 };
        line.push(pair);
    }
    if !line.is_empty() {
        out.push(VLine::Meta(line));
    }
    // Description, folded to a third of the panel.
    let body = wrap(&task.body, width);
    let fold = usize::from(height / 3).max(2);
    let foldable = body.len() > fold;
    let shown = if foldable && !state.desc_open {
        let mut shown = body[..fold].to_vec();
        if let Some(last) = shown.last_mut() {
            *last = format!(
                "{}…",
                cut(last, width.saturating_sub(1)).trim_end_matches('…')
            );
        }
        shown
    } else {
        body.clone()
    };
    out.extend(shown.into_iter().map(VLine::Desc));
    out.push(VLine::DescControls {
        foldable,
        empty: body.is_empty(),
    });
    // Criteria.
    let addable = matches!(
        task.status,
        Status::Triage | Status::Ready | Status::Working
    );
    if !detail.criteria.is_empty() || addable {
        out.push(VLine::CritHeader);
        for (index, criterion) in detail.criteria.iter().enumerate() {
            out.push(VLine::Crit(index));
            if state.evidence.contains(&criterion.position) {
                let evidence = criterion.evidence.as_deref().unwrap_or("no evidence");
                let wrapped = wrap(evidence, width.saturating_sub(4));
                let more = wrapped.len() > 8;
                for (n, line) in wrapped.into_iter().take(8).enumerate() {
                    let line = if more && n == 7 {
                        format!("{line}…")
                    } else {
                        line
                    };
                    out.push(VLine::Evidence(line));
                }
            }
        }
        if addable {
            out.push(VLine::AddCrit);
        }
    }
    // The open decision.
    if let Some(decision) = open_decision(detail) {
        let mut card = vec![DecLine::Title];
        for line in wrap(&decision.summary, width.saturating_sub(3)) {
            card.push(DecLine::Summary(line));
        }
        card.extend((0..decision.choices.len()).map(DecLine::Choice));
        if decision.allow_text {
            card.push(DecLine::Reply);
        }
        let last = card.len() - 1;
        for (n, line) in card.into_iter().enumerate() {
            let frame = match n {
                0 if last == 0 => " ",
                0 => "╭",
                n if n == last => "╰",
                _ => "│",
            };
            out.push(VLine::Dec(line, frame));
        }
    } else if task.status == Status::Review {
        out.push(VLine::Review);
    }
    if input_of(state, |p| matches!(p, Purpose::SendBack(_))).is_some() {
        out.push(VLine::SendBack);
    }
    // Tabs.
    out.push(VLine::Tabs);
    match state.tab {
        DetailTab::Notes => {
            for (index, entry) in detail.entries.iter().enumerate() {
                if entry.pinned {
                    out.push(VLine::Pinned(index));
                }
            }
            let mut index = 0;
            while index < detail.entries.len() {
                let entry = &detail.entries[index];
                if entry.kind == EntryKind::Event {
                    let run = detail.entries[index..]
                        .iter()
                        .take_while(|e| e.kind == EntryKind::Event)
                        .count();
                    if run >= 2 && !state.events.contains(&entry.id) {
                        out.push(VLine::Events(entry.id, run));
                    } else {
                        if run >= 2 {
                            out.push(VLine::Events(entry.id, run));
                        }
                        out.extend((index..index + run).map(VLine::Event));
                    }
                    index += run;
                    continue;
                }
                out.push(VLine::EntryHead(index));
                for line in wrap(&entry.body, width.saturating_sub(2)) {
                    out.push(VLine::EntryBody(line));
                }
                index += 1;
            }
            if detail.entries.is_empty() {
                out.push(VLine::Empty("No notes yet."));
            }
        }
        DetailTab::Attempts => {
            out.extend((0..detail.attempts.len()).map(VLine::Attempt));
            if detail.attempts.is_empty() {
                out.push(VLine::Empty("Not started yet."));
            }
        }
        DetailTab::Artifacts => {
            out.extend((0..detail.artifacts.len()).map(VLine::Artifact));
            if detail.artifacts.is_empty() {
                out.push(VLine::Empty("No artifacts."));
            }
        }
    }
    if task.status.is_closed() || live(detail).is_none() {
        out.push(VLine::Footer);
    }
    out
}

struct Draw<'a> {
    buffer: &'a mut Buffer,
    palette: &'a Palette,
    hits: Vec<(Rect, Hit)>,
}

impl Draw<'_> {
    fn style(&self, color: ratatui::style::Color) -> Style {
        Style::default().fg(color).bg(self.palette.sidebar_bg)
    }

    /// Writes `text` and records `hit` on it; returns the end column.
    fn text(
        &mut self,
        x: u16,
        y: u16,
        right: u16,
        text: &str,
        style: Style,
        hit: Option<Hit>,
    ) -> u16 {
        let end = put(self.buffer, x, y, right, text, style);
        if let Some(hit) = hit {
            if end > x {
                self.hits.push((Rect::new(x, y, end - x, 1), hit));
            }
        }
        end
    }

    fn editor(&mut self, input: &Input, x: u16, y: u16, right: u16) {
        let style = Style::default()
            .fg(self.palette.text)
            .bg(self.palette.surface0);
        let width = right.saturating_sub(x);
        self.buffer.set_style(Rect::new(x, y, width, 1), style);
        input
            .editor
            .render_row(self.buffer, (x, y), width, 0, style);
    }
}

/// The header: `← AC-12  [Review ▾]  auto  ● claude@mato 14m  ↗ pane   ‹ ›`.
/// Parts drop in this order when it does not fit: the age, `@machine`,
/// `‹ ›`, the agent name (the `●` stays), then `auto` becomes `A`.
#[allow(clippy::too_many_arguments)]
fn header(
    d: &mut Draw,
    state: &TasksState,
    detail: &TaskDetail,
    endpoints: &[ClientShellEndpoint],
    layout: &ProjectLayout,
    left: u16,
    right: u16,
    y: u16,
) {
    let palette = d.palette;
    let task = &detail.task;
    let now = super::super::agent_signal::unix_now();
    let attempt = live(detail);
    let pane = attempt.and_then(|a| a.pane_key.clone());
    let accent = d.style(palette.accent).add_modifier(Modifier::BOLD);
    let (auto_long, auto_style) = if task.auto_status {
        ("auto", d.style(palette.accent))
    } else {
        (
            "auto",
            d.style(palette.overlay0)
                .add_modifier(Modifier::CROSSED_OUT),
        )
    };
    let nav = state.order.len() > 1;
    let since = age(&task.status_since, now);
    let build = |level: u8| {
        let mut parts: Vec<(String, Style, Option<Hit>)> = vec![
            ("←".into(), accent, Some(Hit::Back)),
            (task.display_id.clone(), d.style(palette.overlay0), None),
            (
                format!("[{} ▾]", task.status.label()),
                d.style(palette.text).add_modifier(Modifier::BOLD),
                Some(Hit::StatusChip),
            ),
            (
                if level >= 5 {
                    "A".into()
                } else {
                    auto_long.into()
                },
                auto_style,
                Some(Hit::Auto),
            ),
        ];
        if let Some(attempt) = attempt {
            let color = agent_color(endpoints, layout, pane.as_deref(), palette);
            let mut name = String::new();
            if level < 4 {
                name.push_str(&format!(" {}", attempt.harness));
                if level < 2 {
                    name.push_str(&format!("@{}", attempt.machine));
                }
                if level < 1 && !since.is_empty() {
                    name.push_str(&format!(" {since}"));
                }
            }
            parts.push(("●".into(), d.style(color), None));
            if !name.is_empty() {
                // Joined to the dot below (no separator).
                parts.push((name, d.style(palette.subtext0), None));
            }
        }
        if pane.is_some() {
            parts.push(("↗ pane".into(), d.style(palette.accent), Some(Hit::Pane)));
        }
        parts
    };
    let width_of = |parts: &[(String, Style, Option<Hit>)]| -> u16 {
        let mut total = 0;
        for (n, (text, _, _)) in parts.iter().enumerate() {
            let joined = n > 0 && parts[n - 1].0 == "●";
            total += display_width(text)
                + match n {
                    0 => 0,
                    1 => 1,
                    _ if joined => 0,
                    _ => 2,
                };
        }
        total
    };
    let space = right.saturating_sub(left);
    let mut chosen = (build(5), false);
    for level in 0..=5u8 {
        let parts = build(level);
        let with_nav = nav && level < 3;
        let need = width_of(&parts) + if with_nav { 4 } else { 0 };
        if need <= space {
            chosen = (parts, with_nav);
            break;
        }
    }
    let (parts, with_nav) = chosen;
    let nav_right = right;
    let text_right = if with_nav {
        right.saturating_sub(4)
    } else {
        right
    };
    let mut x = left;
    for (n, (text, style, hit)) in parts.into_iter().enumerate() {
        if n == 1 {
            x = put(d.buffer, x, y, text_right, " ", d.style(palette.text));
        } else if n > 0 && !text.starts_with(' ') {
            x = put(d.buffer, x, y, text_right, "  ", d.style(palette.text));
        }
        x = d.text(x, y, text_right, &text, style, hit);
    }
    if with_nav {
        let x = nav_right.saturating_sub(3);
        d.text(x, y, nav_right, "‹", accent, Some(Hit::Prev));
        d.text(x + 2, y, nav_right, "›", accent, Some(Hit::Next));
    }
}

/// Draws the open task into `body`.
pub(super) fn draw(
    state: &mut TasksState,
    endpoints: &[ClientShellEndpoint],
    layout: &ProjectLayout,
    _focused: bool,
    palette: &Palette,
    buffer: &mut Buffer,
    body: Rect,
) {
    let Some(detail) = state.detail.clone() else {
        return;
    };
    let left = body.x + 1;
    let right = body.right().saturating_sub(1);
    let width = right.saturating_sub(left);
    let mut d = Draw {
        buffer,
        palette,
        hits: Vec::new(),
    };
    header(
        &mut d, state, &detail, endpoints, layout, left, right, body.y,
    );
    // The composer: the last body line, Notes tab only.
    let composer = (state.tab == DetailTab::Notes && body.height >= 3).then(|| body.bottom() - 1);
    let top = body.y + 1;
    let bottom = composer.unwrap_or(body.bottom());
    let height = usize::from(bottom.saturating_sub(top));
    let lines = lines(state, &detail, endpoints, layout, width, body.height);
    if state.to_decision {
        state.to_decision = false;
        if let Some(index) = lines.iter().position(|l| matches!(l, VLine::Dec(..))) {
            state.view_scroll = index;
        }
    }
    let max_scroll = lines.len().saturating_sub(height);
    state.view_scroll = state.view_scroll.min(max_scroll);
    for (offset, line) in lines
        .iter()
        .skip(state.view_scroll)
        .take(height)
        .enumerate()
    {
        let y = top + offset as u16;
        draw_line(&mut d, state, &detail, endpoints, line, body, y);
    }
    if let Some(y) = composer {
        let prompt = d.style(palette.accent);
        let x = d.text(left, y, right, "> ", prompt, Some(Hit::Composer));
        match input_of(state, |p| matches!(p, Purpose::Composer(_))) {
            Some(input) => d.editor(input, x, y, right),
            None => {
                let dim = d.style(palette.overlay0);
                d.text(x, y, right, "note… (c)", dim, Some(Hit::Composer));
            }
        }
    }
    let hits = std::mem::take(&mut d.hits);
    state.hits.items.extend(hits);
}

fn entry_head(entry: &Entry, detail: &TaskDetail, now: u64) -> String {
    let mut head = format!("{}  {}", entry.author, age(&entry.created_at, now));
    if let Some(attempt) = entry.attempt_id {
        if let Some(index) = detail.attempts.iter().position(|a| a.id == attempt) {
            head.push_str(&format!("  attempt {}", detail.attempts.len() - index));
        }
    }
    head
}

fn draw_line(
    d: &mut Draw,
    state: &TasksState,
    detail: &TaskDetail,
    endpoints: &[ClientShellEndpoint],
    line: &VLine,
    body: Rect,
    y: u16,
) {
    let palette = d.palette;
    let left = body.x + 1;
    let right = body.right().saturating_sub(1);
    let width = right.saturating_sub(left);
    let task = &detail.task;
    let now = super::super::agent_signal::unix_now();
    let base = d.style(palette.text);
    let dim = d.style(palette.overlay0);
    let accent = d.style(palette.accent);
    match line {
        VLine::Waiting(text) => {
            d.text(
                left,
                y,
                right,
                &cut(text, width),
                d.style(palette.yellow),
                Some(Hit::Waiting),
            );
        }
        VLine::Title => match input_of(state, |p| matches!(p, Purpose::Title { .. })) {
            Some(input) => d.editor(input, left, y, right),
            None => {
                let title = task.title.as_deref().filter(|t| !t.trim().is_empty());
                let (text, style) = match title {
                    Some(title) => (title, base.add_modifier(Modifier::BOLD)),
                    None => ("Untitled", dim),
                };
                d.text(left, y, right, &cut(text, width), style, Some(Hit::Title));
            }
        },
        VLine::Meta(pairs) => {
            let mut x = left;
            for (n, (text, hit)) in pairs.iter().enumerate() {
                if n > 0 {
                    x = put(d.buffer, x, y, right, "  ", base);
                }
                let (label, value) = text.split_once(' ').unwrap_or(("", text));
                let start = x;
                if !label.is_empty() {
                    x = put(d.buffer, x, y, right, &format!("{label} "), dim);
                }
                x = put(d.buffer, x, y, right, value, base);
                if let Some(hit) = hit {
                    d.hits
                        .push((Rect::new(start, y, x - start, 1), hit.clone()));
                }
            }
        }
        VLine::Desc(text) => {
            put(d.buffer, left, y, right, text, base);
        }
        VLine::DescControls { foldable, empty } => {
            let mut x = left;
            if *empty {
                x = put(d.buffer, x, y, right, "no description  ", dim);
            }
            x = d.text(x, y, right, "e edit", accent, Some(Hit::Edit));
            if *foldable {
                let label = if state.desc_open {
                    "▴ less"
                } else {
                    "▾ more"
                };
                let at = right.saturating_sub(display_width(label)).max(x + 2);
                d.text(at, y, right, label, accent, Some(Hit::More));
            }
        }
        VLine::CritHeader => {
            let passed = detail
                .criteria
                .iter()
                .filter(|c| c.state == CheckState::Passed)
                .count();
            let text = format!("Criteria {passed}/{}", detail.criteria.len());
            put(
                d.buffer,
                left,
                y,
                right,
                &text,
                d.style(palette.subtext0).add_modifier(Modifier::BOLD),
            );
        }
        VLine::Crit(index) => {
            let criterion = &detail.criteria[*index];
            d.hits.push((
                Rect::new(left, y, width, 1),
                Hit::Criterion(criterion.position),
            ));
            let (mark, color) = match criterion.state {
                CheckState::Passed => ("✓", palette.green),
                CheckState::Failed => ("✗", palette.red),
                CheckState::Open => ("○", palette.overlay0),
            };
            let x = d.text(
                left + 1,
                y,
                right,
                mark,
                d.style(color),
                Some(Hit::Mark(criterion.position)),
            );
            let mut suffix = String::new();
            if criterion.check_cmd.is_some() {
                suffix.push_str("chk");
            }
            if criterion.evidence.is_some() {
                suffix.push_str(if suffix.is_empty() { "e" } else { "   e" });
            }
            let text_right =
                right.saturating_sub(display_width(&suffix) + u16::from(!suffix.is_empty()));
            let style = if criterion.state == CheckState::Passed {
                dim
            } else {
                base
            };
            put(
                d.buffer,
                x + 1,
                y,
                text_right,
                &cut(&criterion.text, text_right.saturating_sub(x + 1)),
                style,
            );
            if !suffix.is_empty() {
                put(
                    d.buffer,
                    right.saturating_sub(display_width(&suffix)),
                    y,
                    right,
                    &suffix,
                    dim,
                );
            }
        }
        VLine::Evidence(text) => {
            put(d.buffer, left + 3, y, right, text, dim);
        }
        VLine::AddCrit => match input_of(state, |p| matches!(p, Purpose::AddCriterion(_))) {
            Some(input) => {
                let x = put(d.buffer, left + 1, y, right, "+ ", accent);
                d.editor(input, x, y, right);
            }
            None => {
                d.text(left + 1, y, right, "+ add", accent, Some(Hit::AddCriterion));
            }
        },
        VLine::Dec(part, frame) => {
            let Some(decision) = open_decision(detail) else {
                return;
            };
            let first = matches!(part, DecLine::Title);
            let bg = if first {
                palette.active_row_bg
            } else {
                palette.sidebar_bg
            };
            if first {
                d.buffer.set_style(
                    Rect::new(body.x, y, right.saturating_sub(body.x), 1),
                    Style::default().bg(bg),
                );
            }
            put(
                d.buffer,
                body.x,
                y,
                right,
                frame,
                Style::default().fg(palette.accent).bg(bg),
            );
            let on = |style: Style| style.bg(bg);
            match part {
                DecLine::Title => {
                    let ago = age(&decision.created_at, now);
                    let x = put(
                        d.buffer,
                        left,
                        y,
                        right,
                        "? ",
                        on(d.style(palette.yellow)).add_modifier(Modifier::BOLD),
                    );
                    let title_right = right.saturating_sub(display_width(&ago) + 1);
                    put(
                        d.buffer,
                        x,
                        y,
                        title_right,
                        &cut(&decision.title, title_right.saturating_sub(x)),
                        on(base).add_modifier(Modifier::BOLD),
                    );
                    put(
                        d.buffer,
                        right.saturating_sub(display_width(&ago)),
                        y,
                        right,
                        &ago,
                        on(dim),
                    );
                }
                DecLine::Summary(text) => {
                    put(d.buffer, left + 2, y, right, text, dim);
                }
                DecLine::Choice(index) => {
                    let choice = &decision.choices[*index];
                    let start = left + 2;
                    let mut x = put(
                        d.buffer,
                        start,
                        y,
                        right,
                        &format!("{}", index + 1),
                        accent.add_modifier(Modifier::BOLD),
                    );
                    x = put(d.buffer, x, y, right, &format!(" {}", choice.label), base);
                    if choice.recommended {
                        x = put(d.buffer, x, y, right, "  (rec)", d.style(palette.green));
                    }
                    if let Some(consequence) = &choice.consequence {
                        put(d.buffer, x, y, right, &format!(" — {consequence}"), dim);
                    }
                    d.hits.push((
                        Rect::new(start, y, right.saturating_sub(start), 1),
                        Hit::Choice(*index),
                    ));
                }
                DecLine::Reply => match input_of(state, |p| matches!(p, Purpose::Reply { .. })) {
                    Some(input) => {
                        let x = put(d.buffer, left + 2, y, right, "r ", accent);
                        d.editor(input, x, y, right);
                    }
                    None => {
                        d.text(left + 2, y, right, "r reply", accent, Some(Hit::Reply));
                    }
                },
            }
        }
        VLine::Review => {
            let mut x = left;
            for (label, hit) in [
                ("[Accept]", Hit::Accept),
                ("[Send back]", Hit::SendBack),
                ("[Take over]", Hit::TakeOver),
            ] {
                x = d.text(x, y, right, label, accent, Some(hit));
                x = put(d.buffer, x, y, right, " ", base);
            }
        }
        VLine::SendBack => {
            if let Some(input) = input_of(state, |p| matches!(p, Purpose::SendBack(_))) {
                let x = put(d.buffer, left, y, right, "note: ", accent);
                d.editor(input, x, y, right);
            }
        }
        VLine::Tabs => {
            let mut x = left;
            for (label, tab) in [
                ("Notes", DetailTab::Notes),
                ("Attempts", DetailTab::Attempts),
                ("Artifacts", DetailTab::Artifacts),
            ] {
                let style = if state.tab == tab {
                    accent.add_modifier(Modifier::BOLD)
                } else {
                    dim
                };
                x = d.text(x, y, right, label, style, Some(Hit::Tab(tab)));
                x = put(d.buffer, x, y, right, "  ", base);
            }
        }
        VLine::Pinned(index) => {
            let entry = &detail.entries[*index];
            let x = put(d.buffer, left, y, right, "▲ ", accent);
            let first = entry.body.lines().next().unwrap_or_default();
            put(
                d.buffer,
                x,
                y,
                right,
                &cut(first, right.saturating_sub(x)),
                base,
            );
        }
        VLine::EntryHead(index) => {
            let head = entry_head(&detail.entries[*index], detail, now);
            put(
                d.buffer,
                left,
                y,
                right,
                &head,
                base.add_modifier(Modifier::BOLD),
            );
        }
        VLine::EntryBody(text) => {
            put(d.buffer, left + 2, y, right, text, base);
        }
        VLine::Events(first, count) => {
            let glyph = if state.events.contains(first) {
                "▾"
            } else {
                "▸"
            };
            d.text(
                left,
                y,
                right,
                &format!("{glyph} {count} events"),
                dim,
                Some(Hit::Events(*first)),
            );
        }
        VLine::Event(index) => {
            let entry = &detail.entries[*index];
            let text = format!("· {}  {}", entry.body, age(&entry.created_at, now));
            put(d.buffer, left, y, right, &cut(&text, width), dim);
        }
        VLine::Attempt(index) => {
            let attempt = &detail.attempts[*index];
            let outcome = attempt.outcome.map_or("open", |outcome| match outcome {
                tasks::Outcome::Succeeded => "succeeded",
                tasks::Outcome::Failed => "failed",
                tasks::Outcome::Stopped => "stopped",
                tasks::Outcome::NeedsHuman => "needs human",
            });
            let started = unix_of(&attempt.started_at);
            let ended = attempt.ended_at.as_deref().and_then(unix_of).unwrap_or(now);
            let took = started.map_or_else(String::new, |s| {
                projects::format_age(ended.saturating_sub(s))
            });
            let mut text = format!(
                "{}  {}  {}  {}  {}",
                attempt.harness,
                attempt.machine,
                outcome,
                age(&attempt.started_at, now),
                took
            );
            if let Some(cents) = attempt.cost_cents {
                text.push_str(&format!("  ${}.{:02}", cents / 100, cents % 100));
            }
            let pane = attempt
                .pane_key
                .as_deref()
                .filter(|key| find_agent(endpoints, key).is_some());
            let action = match (pane, attempt.ended_at.is_none()) {
                (Some(pane), _) => Some(("↗", Hit::AttemptPane(pane.to_owned()))),
                (None, true) => Some(("release", Hit::Release)),
                (None, false) => None,
            };
            let text_right = action.as_ref().map_or(right, |(label, _)| {
                right.saturating_sub(display_width(label) + 1)
            });
            put(
                d.buffer,
                left,
                y,
                text_right,
                &cut(&text, text_right.saturating_sub(left)),
                base,
            );
            if let Some((label, hit)) = action {
                d.text(
                    right.saturating_sub(display_width(label)),
                    y,
                    right,
                    label,
                    accent,
                    Some(hit),
                );
            }
        }
        VLine::Artifact(index) => {
            let artifact = &detail.artifacts[*index];
            let kind = match artifact.kind {
                ArtifactKind::Doc => "doc",
                ArtifactKind::Diff => "diff",
                ArtifactKind::Link => "link",
                ArtifactKind::File => "file",
                ArtifactKind::Report => "report",
            };
            let review = match artifact.review {
                tasks::Review::Unreviewed => "",
                tasks::Review::Accepted => "accepted",
                tasks::Review::Rejected => "rejected",
            };
            let review_right =
                right.saturating_sub(display_width(review) + u16::from(!review.is_empty()));
            let mut x = put(d.buffer, left, y, review_right, &format!("{kind:<6} "), dim);
            x = put(d.buffer, x, y, review_right, &artifact.title, base);
            if let Some(summary) = &artifact.summary {
                put(
                    d.buffer,
                    x,
                    y,
                    review_right,
                    &cut(&format!("  {summary}"), review_right.saturating_sub(x)),
                    dim,
                );
            }
            put(
                d.buffer,
                right.saturating_sub(display_width(review)),
                y,
                right,
                review,
                dim,
            );
            d.hits
                .push((Rect::new(left, y, width, 1), Hit::Artifact(artifact.id)));
        }
        VLine::Empty(text) => {
            put(d.buffer, left, y, right, text, dim);
        }
        VLine::Footer => {
            if task.status.is_closed() {
                d.text(left, y, right, "archive", accent, Some(Hit::Archive));
            } else {
                d.text(left, y, right, "▶ Start on…", accent, Some(Hit::Start));
            }
        }
    }
}
