//! The per-project board (docs/design/tasks.md, section 4.2): a grouped list
//! under 90 inner columns, four columns `Ready | Working | Blocked | Review`
//! from 90 on, and the project list when no project is shown.

use super::*;

/// Lane index of a status; cancelled cards sit in Done.
pub(super) fn lane_of(status: Status) -> usize {
    Status::LANES
        .iter()
        .position(|lane| *lane == status)
        .unwrap_or(5)
}

/// Lane glyphs of the project list.
const GLYPHS: [&str; 5] = ["·", "▸", "●", "⚠", "◎"];

/// One drawn line of the board.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Row {
    /// The `+ new` or `/` input.
    Input,
    /// The text filter while it is not edited.
    Filter,
    Lane(usize),
    /// Card index in `TasksState.cards`, second line or not.
    Card(usize, bool),
    /// The four column headers.
    Heads,
    /// Row `n` of the columns, second line or not.
    Cols(usize, bool),
}

pub(super) struct Plan {
    pub(super) rows: Vec<Row>,
    /// Selectable cards in board order (indices into `cards`).
    pub(super) order: Vec<usize>,
    pub(super) lanes: [Vec<usize>; 6],
    pub(super) collapsed: [bool; 6],
}

/// Lines and order of the board as it is drawn now.
pub(super) fn plan(state: &TasksState, layout: &ProjectLayout) -> Plan {
    let text = state.text.as_ref().map(|text| text.to_lowercase());
    let mut lanes: [Vec<usize>; 6] = Default::default();
    for (index, card) in state.cards.iter().enumerate() {
        let shown = text.as_ref().is_none_or(|text| {
            card.task.name().to_lowercase().contains(text.as_str())
                || card.task.display_id.to_lowercase().contains(text.as_str())
        });
        if shown {
            lanes[lane_of(card.task.status)].push(index);
        }
    }
    let collapsed: [bool; 6] =
        std::array::from_fn(|lane| state.collapsed(layout, lane, lanes[lane].len()));
    let mut rows = Vec::new();
    let mut order = Vec::new();
    match state.input.as_ref().map(|input| &input.purpose) {
        Some(Purpose::New | Purpose::Filter | Purpose::SendBack(_)) => rows.push(Row::Input),
        _ if state.text.is_some() => rows.push(Row::Filter),
        _ => {}
    }
    let full = |lane: usize, rows: &mut Vec<Row>, order: &mut Vec<usize>| {
        rows.push(Row::Lane(lane));
        if !collapsed[lane] {
            for &card in &lanes[lane] {
                rows.push(Row::Card(card, false));
                rows.push(Row::Card(card, true));
                order.push(card);
            }
        }
    };
    if state.columns {
        full(0, &mut rows, &mut order);
        rows.push(Row::Heads);
        let depth = (1..=4).map(|lane| lanes[lane].len()).max().unwrap_or(0);
        for row in 0..depth {
            rows.push(Row::Cols(row, false));
            rows.push(Row::Cols(row, true));
        }
        for lane in &lanes[1..=4] {
            order.extend(lane);
        }
        full(5, &mut rows, &mut order);
    } else {
        for lane in 0..6 {
            full(lane, &mut rows, &mut order);
        }
    }
    Plan {
        rows,
        order,
        lanes,
        collapsed,
    }
}

/// Display ids in board order.
pub(super) fn order_ids(state: &TasksState, layout: &ProjectLayout) -> Vec<String> {
    plan(state, layout)
        .order
        .iter()
        .map(|&index| state.cards[index].task.display_id.clone())
        .collect()
}

/// `h` / `l`: the card in the previous or next column (columns layout) or
/// the first card of the previous or next lane (list).
pub(super) fn sideways(state: &TasksState, plan: &Plan, delta: isize) -> Option<String> {
    let id = |index: usize| state.cards[index].task.display_id.clone();
    let Some(selected) = state.selected.as_ref().and_then(|id| {
        state
            .cards
            .iter()
            .position(|card| &card.task.display_id == id)
    }) else {
        return plan.order.first().map(|&index| id(index));
    };
    let lane = lane_of(state.cards[selected].task.status);
    let shown = |lane: usize| !plan.collapsed[lane] && !plan.lanes[lane].is_empty();
    if state.columns && (1..=4).contains(&lane) {
        let row = plan.lanes[lane]
            .iter()
            .position(|&index| index == selected)
            .unwrap_or(0);
        let mut column = lane.checked_add_signed(delta)?;
        while (1..=4).contains(&column) {
            if let Some(&last) = plan.lanes[column].last() {
                return Some(id(plan.lanes[column].get(row).copied().unwrap_or(last)));
            }
            column = column.checked_add_signed(delta)?;
        }
        return None;
    }
    if state.columns {
        return (1..=4)
            .find(|&column| !plan.lanes[column].is_empty())
            .map(|column| id(plan.lanes[column][0]));
    }
    let mut next = lane.checked_add_signed(delta)?;
    while next < 6 {
        if shown(next) {
            return Some(id(plan.lanes[next][0]));
        }
        next = next.checked_add_signed(delta)?;
    }
    None
}

/// What one card line needs from the panel.
struct Ctx<'a> {
    state: &'a TasksState,
    endpoints: &'a [ClientShellEndpoint],
    layout: &'a ProjectLayout,
    palette: &'a Palette,
    /// Buttons show only their glyph (inner width under 50).
    short: bool,
    columns: bool,
}

type Piece = (String, Style);

/// The parts of a card's second line at drop `level` (section 4.2: the
/// `@machine` suffix, the kind word, the outcome mark, then the agent name
/// go; the `●` stays).
fn second_line(ctx: &Ctx, card: &TaskCard, level: u8) -> Vec<Vec<Piece>> {
    let palette = ctx.palette;
    let bg = palette.sidebar_bg;
    let fg = |color| Style::default().fg(color).bg(bg);
    let mut parts: Vec<Vec<Piece>> = Vec::new();
    if level < 2 {
        if let Some(kind) = card.task.kind {
            parts.push(vec![(kind_name(kind).to_owned(), fg(palette.subtext0))]);
        }
    }
    if card.criteria_total > 0 {
        let complete = card.criteria_passed == card.criteria_total;
        let glyph = if card.criteria_passed == 0 {
            "○"
        } else {
            "✓"
        };
        let mut part = vec![(
            format!("{glyph}{}/{}", card.criteria_passed, card.criteria_total),
            fg(if complete {
                palette.green
            } else {
                palette.text
            }),
        )];
        if card.criteria_failed > 0 {
            part.push((format!(" ✗{}", card.criteria_failed), fg(palette.red)));
        }
        parts.push(part);
    }
    if card.open_decision {
        parts.push(vec![(
            "?".into(),
            fg(palette.yellow).add_modifier(Modifier::BOLD),
        )]);
    }
    if level < 3 {
        if let Some(outcome) = card.last_outcome {
            let (mark, color) = match outcome {
                tasks::Outcome::Succeeded => ("✓", palette.green),
                tasks::Outcome::Failed => ("✗", palette.red),
                tasks::Outcome::Stopped => ("s", palette.overlay0),
                tasks::Outcome::NeedsHuman => ("h", palette.yellow),
            };
            parts.push(vec![(mark.into(), fg(color))]);
        }
    }
    if let Some((harness, machine, pane)) = &card.live {
        let color = agent_color(ctx.endpoints, ctx.layout, pane.as_deref(), palette);
        let mut part = vec![("●".to_owned(), fg(color))];
        if level < 4 {
            let name = if level < 1 {
                format!(" {harness}@{machine}")
            } else {
                format!(" {harness}")
            };
            part.push((name, fg(palette.subtext0)));
        }
        parts.push(part);
    }
    parts
}

fn pieces_width(parts: &[Vec<Piece>], sep: &str) -> u16 {
    let text: u16 = parts
        .iter()
        .flatten()
        .map(|(text, _)| display_width(text))
        .sum();
    text + display_width(sep) * parts.len().saturating_sub(1) as u16
}

/// Draws one line of a card between `frame` (the frame column) and `right`.
#[allow(clippy::too_many_arguments)]
fn draw_card(
    buffer: &mut Buffer,
    ctx: &Ctx,
    index: usize,
    second: bool,
    frame: u16,
    right: u16,
    y: u16,
    hits: &mut Vec<(Rect, Hit)>,
) {
    let palette = ctx.palette;
    let card = &ctx.state.cards[index];
    let id = &card.task.display_id;
    let selected = ctx.state.selected.as_ref() == Some(id);
    // Only the selected card's first line is shaded; the frame marks it.
    let bg = if selected && !second {
        palette.active_row_bg
    } else {
        palette.sidebar_bg
    };
    let row = Rect::new(frame, y, right.saturating_sub(frame), 1);
    buffer.set_style(row, Style::default().bg(bg));
    hits.push((row, Hit::Card(id.clone())));
    if selected {
        let glyph = if second { "╰" } else { "╭" };
        put(
            buffer,
            frame,
            y,
            right,
            glyph,
            Style::default().fg(palette.accent).bg(bg),
        );
    }
    let base = Style::default().fg(palette.text).bg(bg);
    let dim = Style::default().fg(palette.overlay0).bg(bg);
    let x0 = frame + 1;
    if !second {
        let mark = match card.task.priority {
            Priority::Urgent => Some(("!", palette.red)),
            Priority::High => Some(("^", palette.yellow)),
            _ => None,
        };
        let name_right = if mark.is_some() {
            right.saturating_sub(2)
        } else {
            right
        };
        let mut x = put(buffer, x0, y, name_right, id, dim);
        x = put(buffer, x, y, name_right, " ", base);
        let titled = card
            .task
            .title
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty());
        let mut style = if titled { base } else { dim };
        if card.task.status == Status::Cancelled {
            style = dim.add_modifier(Modifier::CROSSED_OUT);
        }
        put(
            buffer,
            x,
            y,
            name_right,
            &cut(card.task.name(), name_right.saturating_sub(x)),
            style,
        );
        if let Some((mark, color)) = mark {
            put(
                buffer,
                right.saturating_sub(1),
                y,
                right,
                mark,
                Style::default().fg(color).bg(bg),
            );
        }
        return;
    }
    let indent = if ctx.columns { 1 } else { 2 };
    let sep = if ctx.columns { " " } else { "  " };
    let start = x0 + indent;
    let button = CardButton::of(card);
    let space = right.saturating_sub(start);
    let attempts = (0..=4u8).map(|level| (level, ctx.short)).chain([(4, true)]);
    let mut chosen = (second_line(ctx, card, 4), button.map(|b| b.label(true)));
    for (level, short) in attempts {
        let parts = second_line(ctx, card, level);
        let label = button.map(|b| b.label(short));
        let need = pieces_width(&parts, sep) + label.map_or(0, |l| display_width(l) + 1);
        if need <= space {
            chosen = (parts, label);
            break;
        }
    }
    let (parts, label) = chosen;
    let label_width = label.map_or(0, display_width);
    let text_right = right.saturating_sub(label_width + u16::from(label.is_some()));
    let mut x = start;
    for (n, part) in parts.iter().enumerate() {
        if n > 0 {
            x = put(buffer, x, y, text_right, sep, base);
        }
        for (text, style) in part {
            x = put(buffer, x, y, text_right, text, style.bg(bg));
        }
    }
    if let Some(label) = label {
        let hover = ctx.state.hover.as_ref() == Some(id);
        let style = if hover {
            Style::default()
                .fg(palette.accent)
                .bg(bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.subtext0).bg(bg)
        };
        let bx = right.saturating_sub(label_width);
        let end = put(buffer, bx, y, right, label, style);
        hits.push((Rect::new(bx, y, end - bx, 1), Hit::Button(id.clone())));
    }
}

/// Draws the board of the shown project or workspace into `body`.
pub(super) fn draw(
    state: &mut TasksState,
    endpoints: &[ClientShellEndpoint],
    layout: &ProjectLayout,
    _focused: bool,
    palette: &Palette,
    buffer: &mut Buffer,
    body: Rect,
) {
    let plan = plan(state, layout);
    let ids: Vec<String> = plan
        .order
        .iter()
        .map(|&index| state.cards[index].task.display_id.clone())
        .collect();
    keep_selection(state, &ids);
    let left = body.x + 1;
    let right = body.right().saturating_sub(1);
    let dim = Style::default().fg(palette.overlay0).bg(palette.sidebar_bg);
    if state.cards.is_empty() && matches!(state.loaded, Some((Scope::Workspace(_), _))) {
        put(
            buffer,
            left,
            body.y,
            right,
            "No tasks in this workspace.",
            dim,
        );
        return;
    }
    let height = usize::from(body.height);
    // Keep the selected card in view, unless the wheel moved the board.
    let max_scroll = plan.rows.len().saturating_sub(height);
    let mut scroll = state.scroll.min(max_scroll);
    if state.follow {
        let selected = state.selected.as_ref().and_then(|id| {
            state
                .cards
                .iter()
                .position(|card| &card.task.display_id == id)
        });
        let on = |row: &Row| match (row, selected) {
            (Row::Card(index, _), Some(selected)) => *index == selected,
            (Row::Cols(n, _), Some(selected)) => {
                (1..=4).any(|lane| plan.lanes[lane].get(*n) == Some(&selected))
            }
            _ => false,
        };
        let first = plan.rows.iter().position(on);
        let last = plan.rows.iter().rposition(on);
        if let (Some(first), Some(last)) = (first, last) {
            if first < scroll {
                scroll = first;
            } else if last >= scroll + height {
                scroll = (last + 1).saturating_sub(height).min(first);
            }
        }
    }
    state.scroll = scroll;
    let inner = right.saturating_sub(left);
    let ctx = Ctx {
        state,
        endpoints,
        layout,
        palette,
        short: !state.columns && inner < 50,
        columns: false,
    };
    let column_ctx = Ctx {
        columns: true,
        ..ctx
    };
    // Columns: frame cell plus content, separated by one `│`.
    let span = right.saturating_sub(body.x).saturating_sub(3) / 4;
    let column_x = |column: u16| body.x + column * (span + 1);
    let separator = Style::default()
        .fg(palette.surface_dim)
        .bg(palette.sidebar_bg);
    let mut hits = Vec::new();
    for (offset, row) in plan.rows.iter().skip(scroll).take(height).enumerate() {
        let y = body.y + offset as u16;
        match *row {
            Row::Input => {
                if let Some(input) = &state.input {
                    let prompt = match input.purpose {
                        Purpose::Filter => "/ ",
                        Purpose::SendBack(_) => "note: ",
                        _ => "+ ",
                    };
                    let accent = Style::default().fg(palette.accent).bg(palette.sidebar_bg);
                    let x = put(buffer, left, y, right, prompt, accent);
                    let style = Style::default().fg(palette.text).bg(palette.surface0);
                    let width = right.saturating_sub(x);
                    buffer.set_style(Rect::new(x, y, width, 1), style);
                    input.editor.render_row(buffer, (x, y), width, 0, style);
                }
            }
            Row::Filter => {
                let text = format!("/ {}", state.text.as_deref().unwrap_or_default());
                let x = put(buffer, left, y, right, &text, dim);
                let x = put(buffer, x, y, right, "  ", dim);
                let end = put(buffer, x, y, right, "✕", dim);
                hits.push((Rect::new(x, y, end - x, 1), Hit::FilterClear));
            }
            Row::Lane(lane) => {
                let (rect, hit) = lane_header(state, &plan, lane, palette, buffer, left, right, y);
                hits.push((rect, hit));
            }
            Row::Card(index, second) => {
                draw_card(buffer, &ctx, index, second, body.x, right, y, &mut hits);
            }
            Row::Heads => {
                for column in 0..4u16 {
                    let x = column_x(column);
                    let lane = 1 + usize::from(column);
                    let (rect, hit) =
                        lane_header(state, &plan, lane, palette, buffer, x + 1, x + span, y);
                    hits.push((rect, hit));
                    if column < 3 {
                        put(buffer, x + span, y, x + span + 1, "│", separator);
                    }
                }
            }
            Row::Cols(n, second) => {
                for column in 0..4u16 {
                    let x = column_x(column);
                    if let Some(&index) = plan.lanes[1 + usize::from(column)].get(n) {
                        draw_card(
                            buffer,
                            &column_ctx,
                            index,
                            second,
                            x,
                            x + span,
                            y,
                            &mut hits,
                        );
                    }
                    if column < 3 {
                        put(buffer, x + span, y, x + span + 1, "│", separator);
                    }
                }
            }
        }
    }
    state.hits.items.extend(hits);
}

/// `Ready 3`, bold when it holds the selection, `▸` when folded.
#[allow(clippy::too_many_arguments)]
fn lane_header(
    state: &TasksState,
    plan: &Plan,
    lane: usize,
    palette: &Palette,
    buffer: &mut Buffer,
    left: u16,
    right: u16,
    y: u16,
) -> (Rect, Hit) {
    let count = if state.text.is_some() {
        plan.lanes[lane].len() as u32
    } else {
        state.totals[lane].max(plan.lanes[lane].len() as u32)
    };
    let holds = plan.lanes[lane]
        .iter()
        .any(|&index| state.selected.as_ref() == Some(&state.cards[index].task.display_id));
    let style = if holds {
        Style::default()
            .fg(palette.text)
            .bg(palette.sidebar_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.subtext0).bg(palette.sidebar_bg)
    };
    let mut text = format!("{} {count}", Status::LANES[lane].label());
    if plan.collapsed[lane] {
        text.push_str(" ▸");
    }
    let end = put(buffer, left, y, right, &text, style);
    (
        Rect::new(left, y, right.saturating_sub(left).max(end - left), 1),
        Hit::Lane(Status::LANES[lane]),
    )
}

/// The project list: one line per section with its lane counts.
pub(super) fn draw_projects(
    state: &mut TasksState,
    palette: &Palette,
    buffer: &mut Buffer,
    body: Rect,
) {
    let left = body.x + 1;
    let right = body.right().saturating_sub(1);
    let dim = Style::default().fg(palette.overlay0).bg(palette.sidebar_bg);
    if state.counts.is_empty() {
        put(
            buffer,
            left,
            body.y,
            right,
            "No sections yet. Add one from the sidebar.",
            dim,
        );
        return;
    }
    let mut hits = Vec::new();
    for (offset, (name, counts)) in state
        .counts
        .iter()
        .enumerate()
        .take(usize::from(body.height))
    {
        let y = body.y + offset as u16;
        let selected = state.selected_project.as_ref() == Some(name);
        let bg = if selected {
            palette.active_row_bg
        } else {
            palette.sidebar_bg
        };
        let row = Rect::new(body.x, y, right.saturating_sub(body.x), 1);
        buffer.set_style(row, Style::default().bg(bg));
        let summary: Vec<String> = counts
            .iter()
            .take(5)
            .zip(GLYPHS)
            .filter(|(count, _)| **count > 0)
            .map(|(count, glyph)| format!("{count} {glyph}"))
            .collect();
        let summary = summary.join(" ");
        let name_right = right.saturating_sub(display_width(&summary) + 2);
        let x = put(
            buffer,
            left,
            y,
            name_right.max(left),
            &cut(name, name_right.saturating_sub(left)),
            Style::default()
                .fg(palette.text)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        );
        put(buffer, x + 2, y, right, &summary, dim.bg(bg));
        hits.push((row, Hit::Project(name.clone())));
    }
    state.hits.items.extend(hits);
}
