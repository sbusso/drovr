//! Markdown to styled, pre-wrapped lines for the document viewer.
//!
//! The output keeps a link index on every span so the viewer can highlight
//! the selected link and map mouse clicks back to a link.

use std::collections::HashMap;

use pulldown_cmark::{Alignment, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::state::Palette;

/// Colours used by the viewer.
#[derive(Debug, Clone)]
pub struct Theme {
    pub headings: [Color; 6],
    pub link: Color,
    pub dim: Color,
    pub quote: Color,
    pub code_fg: Color,
    pub code_bg: Color,
    pub inline_code: Color,
    pub checked: Color,
    pub accent: Color,
    pub warn: Color,
    pub match_fg: Color,
    pub match_bg: Color,
}

impl Theme {
    /// Body text keeps the terminal default colour so it stays readable on
    /// both dark and light backgrounds; accents come from the herdr palette.
    pub fn from_palette(p: &Palette) -> Self {
        Self {
            headings: [p.accent, p.mauve, p.blue, p.teal, p.green, p.peach],
            link: p.blue,
            dim: p.overlay0,
            quote: p.overlay1,
            code_fg: p.text,
            code_bg: p.surface0,
            inline_code: p.peach,
            checked: p.green,
            accent: p.accent,
            warn: p.yellow,
            match_fg: Color::Black,
            match_bg: p.yellow,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RSpan {
    pub text: String,
    pub style: Style,
    pub link: Option<usize>,
}

impl RSpan {
    fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
            link: None,
        }
    }
}

pub type RLine = Vec<RSpan>;

#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub url: String,
    /// First rendered line showing the link.
    pub line: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Doc {
    pub lines: Vec<RLine>,
    pub links: Vec<Link>,
    /// Heading slug and its line, in document order.
    pub anchors: Vec<(String, usize)>,
}

impl Doc {
    pub fn anchor_line(&self, fragment: &str) -> Option<usize> {
        let wanted = percent_decode(fragment).to_lowercase();
        let slug = slugify(&wanted);
        self.anchors
            .iter()
            .find(|(name, _)| *name == wanted)
            .or_else(|| self.anchors.iter().find(|(name, _)| *name == slug))
            .map(|(_, line)| *line)
    }
}

pub fn line_text(line: &RLine) -> String {
    line.iter().map(|span| span.text.as_str()).collect()
}

pub fn line_width(line: &[RSpan]) -> usize {
    line.iter().map(|span| span.text.width()).sum()
}

/// GitHub-style heading slug: lowercase, spaces to dashes, punctuation dropped.
pub fn slugify(text: &str) -> String {
    text.trim()
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            '-' | '_' => Some(c),
            c if c.is_alphanumeric() => Some(c),
            _ => None,
        })
        .collect()
}

pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

enum Container {
    Quote,
    List {
        next: Option<u64>,
    },
    Item {
        marker: String,
        style: Style,
        pending: bool,
    },
}

#[derive(Default)]
struct Table {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<Vec<RSpan>>>,
    header_rows: usize,
}

struct Renderer<'t> {
    theme: &'t Theme,
    width: usize,
    doc: Doc,
    slug_counts: HashMap<String, usize>,
    containers: Vec<Container>,
    inline: Vec<RSpan>,
    patches: Vec<Style>,
    link_stack: Vec<usize>,
    need_blank: bool,
    heading: Option<HeadingLevel>,
    code: Option<String>,
    image_alt: Option<String>,
    table: Option<Table>,
    in_html_comment: bool,
}

pub fn render(markdown: &str, width: u16, theme: &Theme) -> Doc {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_FOOTNOTES;
    let mut renderer = Renderer {
        theme,
        width: usize::from(width).max(1),
        doc: Doc::default(),
        slug_counts: HashMap::new(),
        containers: Vec::new(),
        inline: Vec::new(),
        patches: Vec::new(),
        link_stack: Vec::new(),
        need_blank: false,
        heading: None,
        code: None,
        image_alt: None,
        table: None,
        in_html_comment: false,
    };
    for event in Parser::new_ext(markdown, options) {
        renderer.event(event);
    }
    renderer.flush_inline();
    let mut doc = renderer.doc;
    while doc
        .lines
        .last()
        .is_some_and(|line| line_text(line).trim().is_empty())
    {
        doc.lines.pop();
    }
    doc
}

impl Renderer<'_> {
    fn event(&mut self, event: Event<'_>) {
        if let Some(code) = self.code.as_mut() {
            if let Event::Text(text) = &event {
                code.push_str(text);
                return;
            }
        }
        if let Some(alt) = self.image_alt.as_mut() {
            match &event {
                Event::Text(text) | Event::Code(text) => {
                    alt.push_str(text);
                    return;
                }
                Event::End(TagEnd::Image) => {}
                _ => return,
            }
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.push_text(&text, self.current_style()),
            Event::Code(text) => {
                let style = self
                    .current_style()
                    .patch(Style::default().fg(self.theme.inline_code));
                self.push_text(&text, style);
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                let style = self
                    .current_style()
                    .patch(Style::default().fg(self.theme.inline_code));
                self.push_text(&text, style);
            }
            Event::SoftBreak => self.push_text(" ", self.current_style()),
            Event::HardBreak => self.push_text("\n", self.current_style()),
            Event::Html(html) => self.html(&html, true),
            Event::InlineHtml(html) => self.html(&html, false),
            Event::FootnoteReference(label) => {
                let style = Style::default().fg(self.theme.dim);
                self.push_text(&format!("[^{label}]"), style);
            }
            Event::Rule => {
                self.begin_block();
                let width = self.avail();
                let style = Style::default().fg(self.theme.dim);
                self.emit_line(vec![RSpan::new("─".repeat(width), style)]);
                self.need_blank = true;
            }
            Event::TaskListMarker(checked) => {
                let (text, style) = if checked {
                    ("☑ ", Style::default().fg(self.theme.checked))
                } else {
                    ("☐ ", Style::default().fg(self.theme.dim))
                };
                if let Some(Container::Item {
                    marker,
                    style: marker_style,
                    pending: true,
                }) = self.containers.last_mut()
                {
                    let pad = marker.width().saturating_sub(text.width());
                    *marker = format!("{}{text}", " ".repeat(pad));
                    *marker_style = style;
                } else {
                    self.push_text(text, style);
                }
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.begin_block(),
            Tag::Heading { level, .. } => {
                self.begin_block();
                self.heading = Some(level);
            }
            Tag::BlockQuote(_) => {
                self.begin_block();
                self.containers.push(Container::Quote);
            }
            Tag::CodeBlock(_) => {
                self.begin_block();
                self.code = Some(String::new());
            }
            Tag::HtmlBlock => self.begin_block(),
            Tag::List(start) => {
                self.begin_block();
                self.containers.push(Container::List { next: start });
            }
            Tag::Item => {
                self.flush_inline();
                let depth = self
                    .containers
                    .iter()
                    .filter(|c| matches!(c, Container::List { .. }))
                    .count();
                let marker = match self.containers.last_mut() {
                    Some(Container::List { next: Some(n) }) => {
                        let marker = format!("{n}. ");
                        *n += 1;
                        marker
                    }
                    _ => match depth {
                        0 | 1 => "• ".to_string(),
                        2 => "◦ ".to_string(),
                        _ => "▪ ".to_string(),
                    },
                };
                self.containers.push(Container::Item {
                    marker,
                    style: Style::default().fg(self.theme.accent),
                    pending: true,
                });
            }
            Tag::FootnoteDefinition(label) => {
                self.begin_block();
                let style = Style::default().fg(self.theme.dim);
                self.push_text(&format!("[^{label}]: "), style);
            }
            Tag::Table(aligns) => {
                self.begin_block();
                self.table = Some(Table {
                    aligns,
                    ..Table::default()
                });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    table.rows.push(Vec::new());
                }
            }
            Tag::TableCell => self.inline.clear(),
            Tag::Emphasis => self
                .patches
                .push(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self
                .patches
                .push(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .patches
                .push(Style::default().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link {
                link_type,
                dest_url,
                ..
            } => {
                let url = if link_type == LinkType::Email && !dest_url.starts_with("mailto:") {
                    format!("mailto:{dest_url}")
                } else {
                    dest_url.to_string()
                };
                self.link_stack.push(self.doc.links.len());
                self.doc.links.push(Link {
                    url,
                    line: usize::MAX,
                });
                self.patches.push(
                    Style::default()
                        .fg(self.theme.link)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
            Tag::Image { .. } => self.image_alt = Some(String::new()),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock => {
                self.flush_inline();
                self.need_blank = true;
            }
            TagEnd::FootnoteDefinition => {
                self.flush_inline();
                self.need_blank = true;
            }
            TagEnd::Heading(level) => {
                let text: String = self.inline.iter().map(|s| s.text.as_str()).collect();
                let base = slugify(&text);
                let count = self.slug_counts.entry(base.clone()).or_insert(0);
                let slug = if *count == 0 {
                    base.clone()
                } else {
                    format!("{base}-{count}")
                };
                *count += 1;
                self.doc.anchors.push((slug, self.doc.lines.len()));
                self.flush_inline();
                let rule = match level {
                    HeadingLevel::H1 => Some("━"),
                    HeadingLevel::H2 => Some("─"),
                    _ => None,
                };
                if let Some(rule) = rule {
                    let style = Style::default().fg(self.heading_color(level));
                    let width = self.avail();
                    self.emit_line(vec![RSpan::new(rule.repeat(width), style)]);
                }
                self.heading = None;
                self.need_blank = true;
            }
            TagEnd::BlockQuote(_) => {
                self.flush_inline();
                self.pop_container(|c| matches!(c, Container::Quote));
                self.need_blank = true;
            }
            TagEnd::CodeBlock => {
                let code = self.code.take().unwrap_or_default();
                self.emit_code(&code);
                self.need_blank = true;
            }
            TagEnd::List(_) => {
                self.flush_inline();
                self.pop_container(|c| matches!(c, Container::List { .. }));
                let nested = self
                    .containers
                    .iter()
                    .any(|c| matches!(c, Container::List { .. }));
                if !nested {
                    self.need_blank = true;
                }
            }
            TagEnd::Item => {
                self.flush_inline();
                if matches!(
                    self.containers.last(),
                    Some(Container::Item { pending: true, .. })
                ) {
                    self.emit_line(Vec::new());
                }
                self.pop_container(|c| matches!(c, Container::Item { .. }));
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.emit_table(table);
                }
                self.need_blank = true;
            }
            TagEnd::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.header_rows = table.rows.len();
                }
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.inline);
                if let Some(row) = self.table.as_mut().and_then(|t| t.rows.last_mut()) {
                    row.push(cell);
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.patches.pop();
            }
            TagEnd::Link => {
                self.patches.pop();
                if let Some(index) = self.link_stack.pop() {
                    let style = Style::default().fg(self.theme.dim);
                    let mut span = RSpan::new(format!("[{}]", index + 1), style);
                    span.link = Some(index);
                    self.inline.push(span);
                }
            }
            TagEnd::Image => {
                let alt = self.image_alt.take().unwrap_or_default();
                let style = Style::default()
                    .fg(self.theme.dim)
                    .add_modifier(Modifier::ITALIC);
                self.push_text(&format!("[image: {alt}]"), style);
            }
            _ => {}
        }
    }

    fn html(&mut self, html: &str, block: bool) {
        let trimmed = html.trim();
        if self.in_html_comment || trimmed.starts_with("<!--") {
            self.in_html_comment = !trimmed.ends_with("-->");
            return;
        }
        if !block && (trimmed.eq_ignore_ascii_case("<br>") || trimmed.eq_ignore_ascii_case("<br/>"))
        {
            self.push_text("\n", self.current_style());
            return;
        }
        let style = Style::default().fg(self.theme.dim);
        let text = if block {
            html.trim_end_matches('\n')
        } else {
            html
        };
        self.push_text(text, style);
        if block {
            self.push_text("\n", style);
        }
    }

    fn heading_color(&self, level: HeadingLevel) -> Color {
        self.theme.headings[level as usize - 1]
    }

    fn current_style(&self) -> Style {
        let base = if let Some(level) = self.heading {
            Style::default()
                .fg(self.heading_color(level))
                .add_modifier(Modifier::BOLD)
        } else if self
            .containers
            .iter()
            .any(|c| matches!(c, Container::Quote))
        {
            Style::default().fg(self.theme.quote)
        } else {
            Style::default()
        };
        self.patches
            .iter()
            .fold(base, |style, patch| style.patch(*patch))
    }

    fn push_text(&mut self, text: &str, style: Style) {
        self.inline.push(RSpan {
            text: text.to_string(),
            style,
            link: self.link_stack.last().copied(),
        });
    }

    fn pop_container(&mut self, matches: impl Fn(&Container) -> bool) {
        if let Some(index) = self.containers.iter().rposition(matches) {
            self.containers.truncate(index);
        }
    }

    fn begin_block(&mut self) {
        self.flush_inline();
        if self.need_blank && !self.doc.lines.is_empty() {
            let prefix = self.blank_prefix();
            self.push_line(prefix);
        }
        self.need_blank = false;
    }

    fn prefix_width(&self) -> usize {
        self.containers
            .iter()
            .map(|c| match c {
                Container::Quote => 2,
                Container::List { .. } => 0,
                Container::Item { marker, .. } => marker.width(),
            })
            .sum()
    }

    fn avail(&self) -> usize {
        self.width.saturating_sub(self.prefix_width()).max(8)
    }

    fn blank_prefix(&self) -> RLine {
        let mut prefix = Vec::new();
        for container in &self.containers {
            match container {
                Container::Quote => prefix.push(self.quote_bar()),
                Container::List { .. } => {}
                Container::Item { marker, .. } => {
                    prefix.push(RSpan::new(" ".repeat(marker.width()), Style::default()))
                }
            }
        }
        prefix
    }

    fn quote_bar(&self) -> RSpan {
        RSpan::new("▎ ", Style::default().fg(self.theme.dim))
    }

    fn line_prefix(&mut self) -> RLine {
        let bar = self.quote_bar();
        let mut prefix = Vec::new();
        for container in &mut self.containers {
            match container {
                Container::Quote => prefix.push(bar.clone()),
                Container::List { .. } => {}
                Container::Item {
                    marker,
                    style,
                    pending,
                } => {
                    if *pending {
                        *pending = false;
                        prefix.push(RSpan::new(marker.clone(), *style));
                    } else {
                        prefix.push(RSpan::new(" ".repeat(marker.width()), Style::default()));
                    }
                }
            }
        }
        prefix
    }

    fn emit_line(&mut self, content: RLine) {
        let mut line = self.line_prefix();
        line.extend(content);
        self.push_line(line);
    }

    fn push_line(&mut self, line: RLine) {
        let index = self.doc.lines.len();
        for span in &line {
            if let Some(link) = span.link.and_then(|i| self.doc.links.get_mut(i)) {
                if link.line == usize::MAX {
                    link.line = index;
                }
            }
        }
        self.doc.lines.push(line);
    }

    fn flush_inline(&mut self) {
        if self.table.is_some() || self.inline.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.inline);
        if spans.iter().all(|s| s.text.trim().is_empty()) {
            return;
        }
        for line in wrap(&spans, self.avail()) {
            self.emit_line(line);
        }
    }

    fn emit_code(&mut self, code: &str) {
        let width = self.avail();
        let style = Style::default()
            .fg(self.theme.code_fg)
            .bg(self.theme.code_bg);
        let inner = width.saturating_sub(2).max(1);
        let code = code.strip_suffix('\n').unwrap_or(code);
        for raw in code.split('\n') {
            let text = raw.replace('\t', "    ");
            for chunk in hard_chunks(&text, inner) {
                let pad = width.saturating_sub(chunk.width() + 1);
                let content = format!(" {chunk}{}", " ".repeat(pad));
                self.emit_line(vec![RSpan::new(content, style)]);
            }
        }
    }

    fn emit_table(&mut self, table: Table) {
        let columns = table
            .rows
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(0)
            .max(table.aligns.len());
        if columns == 0 {
            return;
        }
        let rows: Vec<Vec<Vec<RSpan>>> = table
            .rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|cell| {
                        cell.into_iter()
                            .map(|mut span| {
                                span.text = span.text.replace('\n', " ");
                                span
                            })
                            .collect()
                    })
                    .collect()
            })
            .collect();
        let mut natural = vec![1; columns];
        for row in &rows {
            for (index, cell) in row.iter().enumerate() {
                natural[index] = natural[index].max(line_width(cell));
            }
        }
        let budget = self.avail().saturating_sub(3 * columns + 1);
        let widths = fit_columns(&natural, budget);
        let border = Style::default().fg(self.theme.dim);
        let rule = |left: &str, mid: &str, right: &str| {
            let body: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
            vec![RSpan::new(
                format!("{left}{}{right}", body.join(mid)),
                border,
            )]
        };
        self.emit_line(rule("┌", "┬", "┐"));
        for (row_index, row) in rows.iter().enumerate() {
            let header = row_index < table.header_rows;
            let mut line = vec![RSpan::new("│ ", border)];
            for (col, width) in widths.iter().enumerate() {
                let empty = Vec::new();
                let cell = row.get(col).unwrap_or(&empty);
                let mut spans = truncate(cell, *width);
                if header {
                    for span in &mut spans {
                        span.style = span.style.add_modifier(Modifier::BOLD);
                    }
                }
                let gap = width.saturating_sub(line_width(&spans));
                let (before, after) = match table.aligns.get(col) {
                    Some(Alignment::Right) => (gap, 0),
                    Some(Alignment::Center) => (gap / 2, gap - gap / 2),
                    _ => (0, gap),
                };
                if before > 0 {
                    line.push(RSpan::new(" ".repeat(before), Style::default()));
                }
                line.extend(spans);
                if after > 0 {
                    line.push(RSpan::new(" ".repeat(after), Style::default()));
                }
                let sep = if col + 1 == widths.len() {
                    " │"
                } else {
                    " │ "
                };
                line.push(RSpan::new(sep, border));
            }
            self.emit_line(line);
            if header && row_index + 1 == table.header_rows {
                self.emit_line(rule("├", "┼", "┤"));
            }
        }
        self.emit_line(rule("└", "┴", "┘"));
    }
}

/// Shares `budget` between columns: narrow columns keep their natural width,
/// wide ones split what is left.
pub fn fit_columns(natural: &[usize], budget: usize) -> Vec<usize> {
    if natural.iter().sum::<usize>() <= budget {
        return natural.to_vec();
    }
    let mut order: Vec<usize> = (0..natural.len()).collect();
    order.sort_by_key(|&i| natural[i]);
    let mut widths = vec![0; natural.len()];
    let mut remaining = budget;
    let mut left = natural.len();
    for index in order {
        let share = remaining / left;
        let width = natural[index].min(share).max(1);
        widths[index] = width;
        remaining = remaining.saturating_sub(width);
        left -= 1;
    }
    widths
}

/// Cuts spans to `width` columns, ending with "…" when text was dropped.
pub fn truncate(spans: &[RSpan], width: usize) -> Vec<RSpan> {
    if line_width(spans) <= width {
        return spans.to_vec();
    }
    let mut out = Vec::new();
    let mut used = 0;
    let limit = width.saturating_sub(1);
    'outer: for span in spans {
        let mut text = String::new();
        for c in span.text.chars() {
            let w = c.width().unwrap_or(0);
            if used + w > limit {
                if !text.is_empty() {
                    out.push(RSpan {
                        text,
                        ..span.clone()
                    });
                }
                break 'outer;
            }
            used += w;
            text.push(c);
        }
        if !text.is_empty() {
            out.push(RSpan {
                text,
                ..span.clone()
            });
        }
    }
    if width > 0 {
        let style = spans.last().map(|s| s.style).unwrap_or_default();
        out.push(RSpan::new("…", style));
    }
    out
}

fn hard_chunks(text: &str, width: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width && used > 0 {
            chunks.push(std::mem::take(&mut chunk));
            used = 0;
        }
        used += w;
        chunk.push(c);
    }
    chunks.push(chunk);
    chunks
}

enum Token {
    Word(Vec<RSpan>),
    Space(RSpan),
    Break,
}

fn tokenize(spans: &[RSpan]) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut word: Vec<RSpan> = Vec::new();
    for span in spans {
        let mut piece = String::new();
        for c in span.text.chars() {
            if c == '\n' || c.is_whitespace() {
                if !piece.is_empty() {
                    word.push(RSpan {
                        text: std::mem::take(&mut piece),
                        ..span.clone()
                    });
                }
                if !word.is_empty() {
                    tokens.push(Token::Word(std::mem::take(&mut word)));
                }
                if c == '\n' {
                    tokens.push(Token::Break);
                } else if !matches!(tokens.last(), Some(Token::Space(_))) {
                    tokens.push(Token::Space(RSpan {
                        text: " ".into(),
                        ..span.clone()
                    }));
                }
            } else {
                piece.push(c);
            }
        }
        if !piece.is_empty() {
            word.push(RSpan {
                text: piece,
                ..span.clone()
            });
        }
    }
    if !word.is_empty() {
        tokens.push(Token::Word(word));
    }
    tokens
}

/// Greedy word wrap. Words wider than the line are split by character.
pub fn wrap(spans: &[RSpan], width: usize) -> Vec<RLine> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current: RLine = Vec::new();
    let mut used = 0;
    let mut space: Option<RSpan> = None;
    for token in tokenize(spans) {
        match token {
            Token::Break => {
                lines.push(std::mem::take(&mut current));
                used = 0;
                space = None;
            }
            Token::Space(span) => space = Some(span),
            Token::Word(pieces) => {
                let word_width = line_width(&pieces);
                let gap = space.take().filter(|_| used > 0);
                let gap_width = usize::from(gap.is_some());
                if used + gap_width + word_width > width && used > 0 {
                    lines.push(std::mem::take(&mut current));
                    used = 0;
                } else if let Some(gap) = gap {
                    current.push(gap);
                    used += 1;
                }
                if word_width <= width - used {
                    used += word_width;
                    current.extend(pieces);
                    continue;
                }
                for piece in pieces {
                    let mut text = String::new();
                    for c in piece.text.chars() {
                        let w = c.width().unwrap_or(0);
                        if used + w > width && used + text.width() > 0 {
                            if !text.is_empty() {
                                current.push(RSpan {
                                    text: std::mem::take(&mut text),
                                    ..piece.clone()
                                });
                            }
                            lines.push(std::mem::take(&mut current));
                            used = 0;
                        }
                        used += w;
                        text.push(c);
                    }
                    if !text.is_empty() {
                        current.push(RSpan { text, ..piece });
                    }
                }
            }
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::from_palette(&Palette::catppuccin())
    }

    fn texts(doc: &Doc) -> Vec<String> {
        doc.lines.iter().map(line_text).collect()
    }

    fn plain(text: &str) -> Vec<RSpan> {
        vec![RSpan::new(text, Style::default())]
    }

    #[test]
    fn headings_are_bold_coloured_and_h1_h2_get_rules() {
        let t = theme();
        let doc = render("# Title\n\n## Sub\n\n### Third\n\ntext", 20, &t);
        let lines = texts(&doc);
        assert_eq!(lines[0], "Title");
        assert_eq!(lines[1], "━".repeat(20));
        assert_eq!(lines[3], "Sub");
        assert_eq!(lines[4], "─".repeat(20));
        assert_eq!(lines[6], "Third");
        assert_eq!(lines[8], "text");
        let span = &doc.lines[0][0];
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(span.style.fg, Some(t.headings[0]));
        assert_eq!(doc.lines[6][0].style.fg, Some(t.headings[2]));
    }

    #[test]
    fn paragraphs_wrap_to_width_with_inline_styles() {
        let doc = render("one *two* **three** `four` ~~five~~ six", 12, &theme());
        assert_eq!(texts(&doc), vec!["one two", "three four", "five six"]);
        let two = doc.lines[0].iter().find(|s| s.text == "two").unwrap();
        assert!(two.style.add_modifier.contains(Modifier::ITALIC));
        let three = doc.lines[1].iter().find(|s| s.text == "three").unwrap();
        assert!(three.style.add_modifier.contains(Modifier::BOLD));
        let five = doc.lines[2].iter().find(|s| s.text == "five").unwrap();
        assert!(five.style.add_modifier.contains(Modifier::CROSSED_OUT));
        let four = doc.lines[1].iter().find(|s| s.text == "four").unwrap();
        assert_eq!(four.style.fg, Some(theme().inline_code));
    }

    #[test]
    fn wrap_splits_long_words_and_keeps_punctuation_attached() {
        let lines = wrap(&plain("abcdefghij xy"), 4);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts, vec!["abcd", "efgh", "ij", "xy"]);
        let spans = vec![
            RSpan::new("aaa ", Style::default()),
            RSpan::new("bold", Style::default().add_modifier(Modifier::BOLD)),
            RSpan::new(",", Style::default()),
        ];
        let texts: Vec<String> = wrap(&spans, 6).iter().map(line_text).collect();
        assert_eq!(texts, vec!["aaa", "bold,"]);
        let texts: Vec<String> = wrap(&plain("a\nb"), 10).iter().map(line_text).collect();
        assert_eq!(texts, vec!["a", "b"]);
    }

    #[test]
    fn code_blocks_are_padded_with_background() {
        let t = theme();
        let doc = render("```rust\nfn x() {}\n\tlet y;\n```", 16, &t);
        let lines = texts(&doc);
        assert_eq!(lines[0], " fn x() {}      ");
        assert_eq!(lines[1], "     let y;     ");
        assert_eq!(doc.lines[0][0].style.bg, Some(t.code_bg));
    }

    #[test]
    fn block_quotes_get_a_bar_on_every_line() {
        let doc = render("> quoted text here\n>\n> second", 12, &theme());
        assert_eq!(
            texts(&doc),
            vec!["▎ quoted", "▎ text here", "▎ ", "▎ second"]
        );
    }

    #[test]
    fn lists_nest_and_number() {
        let md = "- a\n  - b\n    long text\n- c\n\n3. x\n4. y\n";
        let doc = render(md, 12, &theme());
        assert_eq!(
            texts(&doc),
            vec!["• a", "  ◦ b long", "    text", "• c", "", "3. x", "4. y"]
        );
    }

    #[test]
    fn task_list_markers_replace_bullets() {
        let doc = render("- [ ] todo\n- [x] done", 20, &theme());
        assert_eq!(texts(&doc), vec!["☐ todo", "☑ done"]);
        assert_eq!(doc.lines[1][0].style.fg, Some(theme().checked));
    }

    #[test]
    fn tables_fit_and_truncate() {
        let md = "| a | b |\n|---|--:|\n| 1 | 22 |\n";
        let doc = render(md, 40, &theme());
        assert_eq!(
            texts(&doc),
            vec![
                "┌───┬────┐",
                "│ a │  b │",
                "├───┼────┤",
                "│ 1 │ 22 │",
                "└───┴────┘"
            ]
        );
        let md = "| name | description |\n|---|---|\n| x | a very long description indeed |\n";
        let doc = render(md, 24, &theme());
        let lines = texts(&doc);
        assert!(lines.iter().all(|l| l.width() <= 24), "{lines:?}");
        assert!(lines[3].contains('…'), "{lines:?}");
        assert!(lines[3].starts_with("│ x    │ a very"), "{lines:?}");
    }

    #[test]
    fn fit_columns_keeps_narrow_columns() {
        assert_eq!(fit_columns(&[3, 4], 10), vec![3, 4]);
        assert_eq!(fit_columns(&[5, 50, 50], 40), vec![5, 17, 18]);
    }

    #[test]
    fn rules_images_and_links() {
        let doc = render(
            "see [docs](other.md#part) and ![logo](x.png)\n\n---\n\n<https://example.com>",
            40,
            &theme(),
        );
        let lines = texts(&doc);
        assert_eq!(lines[0], "see docs[1] and [image: logo]");
        assert_eq!(lines[2], "─".repeat(40));
        assert_eq!(lines[4], "https://example.com[2]");
        assert_eq!(
            doc.links,
            vec![
                Link {
                    url: "other.md#part".into(),
                    line: 0
                },
                Link {
                    url: "https://example.com".into(),
                    line: 4
                },
            ]
        );
        let docs = doc.lines[0].iter().find(|s| s.text == "docs").unwrap();
        assert_eq!(docs.link, Some(0));
        assert!(docs.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn anchors_follow_github_slugs() {
        let doc = render(
            "# Hello, World!\n\ntext\n\n## Hello World\n\n## Hello World",
            40,
            &theme(),
        );
        assert_eq!(doc.anchor_line("hello-world"), Some(0));
        assert_eq!(doc.anchor_line("hello-world-1"), Some(5));
        assert_eq!(doc.anchor_line("hello-world-2"), Some(8));
        assert_eq!(doc.anchor_line("Hello%20World"), Some(0));
        assert_eq!(doc.anchor_line("missing"), None);
    }

    #[test]
    fn html_comments_are_hidden() {
        let doc = render("<!-- hidden\nstill -->\n\nshown", 40, &theme());
        assert_eq!(texts(&doc), vec!["shown"]);
    }
}
