//! drovr fork: Ctrl+click on a Markdown path printed as plain text opens it in
//! the workspace's doc pane. Stock herdr detects only http(s) URLs in plain
//! text, so the client finds the path in the cells it already draws and runs
//! `drovr doc open` on the pane's machine: directly for the local server, and
//! through the `drovr.docs` plugin's `open-link` action (stock
//! `plugin.action.invoke`) for a remote one.

use super::*;
use crate::api::schema::{Method, PluginActionInvokeParams, PluginInvocationContext};
use crate::protocol::SurfaceRect;

/// A Markdown path under the pointer: the path as printed (`~`, relative or
/// absolute; `file://` URLs already turned into paths) and the cells it
/// covers on its row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MdPathHit {
    pub(super) path: String,
    pub(super) start_col: u16,
    pub(super) end_col: u16,
}

fn is_delimiter(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '"' | '\''
                | '`'
                | '<'
                | '>'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '|'
                | ','
                | ';'
                | '='
        )
}

/// The Markdown path under `col` in one row of cell symbols, or None. A wide
/// character's spacer cell (empty or blank after a width-2 symbol) belongs to
/// that character.
pub(super) fn md_path_at(row: &[&str], col: u16) -> Option<MdPathHit> {
    // (byte offset in `text`, first col, last col) per glyph.
    let mut glyphs: Vec<(usize, u16, u16)> = Vec::with_capacity(row.len());
    let mut text = String::new();
    for (index, symbol) in row.iter().enumerate() {
        let index = index as u16;
        if index > 0 && symbol.trim().is_empty() && row[usize::from(index) - 1].width() == 2 {
            if let Some(last) = glyphs.last_mut() {
                last.2 = index;
            }
            continue;
        }
        glyphs.push((text.len(), index, index));
        text.push_str(if symbol.is_empty() { " " } else { symbol });
    }
    let clicked = glyphs
        .iter()
        .find(|(_, first, last)| (*first..=*last).contains(&col))?
        .0;
    if text[clicked..].chars().next().is_none_or(is_delimiter) {
        return None;
    }
    let start = text[..clicked]
        .char_indices()
        .rev()
        .find(|(_, c)| is_delimiter(*c))
        .map_or(0, |(at, c)| at + c.len_utf8());
    let end = text[clicked..]
        .find(is_delimiter)
        .map_or(text.len(), |at| clicked + at);
    let token = &text[start..end];
    let trimmed_start = start + (token.len() - token.trim_start_matches('*').len());
    let token = token
        .trim_start_matches('*')
        .trim_end_matches(['.', ':', '!', '?', '*']);
    let trimmed_end = trimmed_start + token.len();
    if !(trimmed_start..trimmed_end).contains(&clicked) {
        return None;
    }
    let path = markdown_target(token)?;
    let col_of = |byte: usize, last: bool| {
        glyphs
            .iter()
            .rev()
            .find(|(at, _, _)| *at <= byte)
            .map(|(_, first, end)| if last { *end } else { *first })
    };
    Some(MdPathHit {
        path,
        start_col: col_of(trimmed_start, false)?,
        end_col: col_of(trimmed_end - 1, true)?,
    })
}

/// The path a token names when it is a Markdown file: `#anchor` and a
/// `:line` or `:line:col` suffix are dropped, `file://` URLs (no host, or
/// `localhost`) become paths, and any other URL is not a path.
fn markdown_target(token: &str) -> Option<String> {
    let mut path = token.split('#').next().unwrap_or_default();
    for _ in 0..2 {
        if let Some((head, tail)) = path.rsplit_once(':') {
            if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) {
                path = head;
            }
        }
    }
    let path = match path.get(..7) {
        Some(scheme) if scheme.eq_ignore_ascii_case("file://") => {
            let rest = &path[7..];
            let rest = rest.strip_prefix("localhost").unwrap_or(rest);
            if !rest.starts_with('/') {
                return None;
            }
            crate::doc_view::percent_decode(rest)
        }
        _ if path.contains("://") => return None,
        _ => path.to_owned(),
    };
    let lower = path.to_ascii_lowercase();
    let stem = lower
        .strip_suffix(".md")
        .or_else(|| lower.strip_suffix(".markdown"))?;
    (!stem.is_empty() && !stem.ends_with('/')).then_some(path)
}

/// The path `drovr doc open` should get: `~` paths pass through (the pane's
/// machine expands them), absolute paths stay, relative paths are joined to
/// the pane's cwd and normalized. None for a relative path without a cwd or
/// a `~user` path.
pub(super) fn resolve_md_path(path: &str, cwd: Option<&str>) -> Option<String> {
    if path == "~" || path.starts_with("~/") || path.starts_with('/') {
        return Some(path.to_owned());
    }
    if path.starts_with('~') {
        return None;
    }
    let cwd = cwd.filter(|cwd| cwd.starts_with('/'))?;
    let mut parts: Vec<&str> = Vec::new();
    for part in cwd.split('/').chain(path.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    Some(format!("/{}", parts.join("/")))
}

/// Plugin and action that run `drovr doc open` on a remote machine.
const DOCS_PLUGIN_ID: &str = "drovr.docs";
const DOCS_PLUGIN_ACTION: &str = "open-link";
/// `invocation_source` that asks the plugin to focus the doc pane.
pub(super) const DOCS_INVOCATION_SOURCE: &str = "drovr_click";

impl ClientShellState {
    /// The Markdown path at (`col`, `row`) of a pane whose content sits at
    /// `rect` in the pane surface. Cells with an OSC 8 hyperlink are left to
    /// the server's link handling.
    pub(super) fn md_path_in_pane(
        &self,
        rect: SurfaceRect,
        row: u16,
        col: u16,
    ) -> Option<MdPathHit> {
        let surface = self.pane_surface.as_ref()?;
        let frame = &surface.frame;
        let y = usize::from(rect.y) + usize::from(row);
        if row >= rect.height || col >= rect.width || y >= usize::from(frame.height) {
            return None;
        }
        let x0 = usize::from(rect.x);
        let x1 = (x0 + usize::from(rect.width)).min(usize::from(frame.width));
        let line = frame
            .cells
            .get(y * usize::from(frame.width) + x0..y * usize::from(frame.width) + x1)?;
        if line.get(usize::from(col))?.hyperlink.is_some() {
            return None;
        }
        let symbols = line
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect::<Vec<_>>();
        md_path_at(&symbols, col)
    }

    /// Ctrl+click on a pane cell: opens the Markdown path there and returns
    /// true, or returns false so the click goes to the server unchanged.
    pub(super) fn open_md_path_click(
        &mut self,
        pane_id: &str,
        row: u16,
        col: u16,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(rect) = self.pane_surface.as_ref().and_then(|surface| {
            surface
                .panes
                .iter()
                .find(|pane| pane.pane_id == pane_id)
                .map(|pane| pane.inner_rect)
        }) else {
            return false;
        };
        let Some(hit) = self.md_path_in_pane(rect, row, col) else {
            return false;
        };
        let Some(pane) = self
            .snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.panes.iter().find(|pane| pane.pane_id == pane_id))
        else {
            return false;
        };
        let cwd = pane.foreground_cwd.clone().or_else(|| pane.cwd.clone());
        let Some(path) = resolve_md_path(&hit.path, cwd.as_deref()) else {
            return false;
        };
        let workspace_id = pane.workspace_id.clone();
        if self.active_endpoint_id.is_local() {
            outcome.actions.push(ClientShellAction::OpenLocalDocument {
                workspace_id,
                pane_id: pane_id.to_owned(),
                path,
            });
            return true;
        }
        self.push_endpoint_method(
            Method::PluginActionInvoke(PluginActionInvokeParams {
                action_id: DOCS_PLUGIN_ACTION.into(),
                plugin_id: Some(DOCS_PLUGIN_ID.into()),
                context: Some(PluginInvocationContext {
                    workspace_id: Some(workspace_id),
                    workspace_label: None,
                    workspace_cwd: None,
                    worktree: None,
                    tab_id: Some(pane.tab_id.clone()),
                    tab_label: None,
                    focused_pane_id: Some(pane_id.to_owned()),
                    focused_pane_cwd: cwd,
                    focused_pane_agent: None,
                    focused_pane_status: None,
                    selected_text: None,
                    invocation_source: Some(DOCS_INVOCATION_SOURCE.into()),
                    correlation_id: None,
                    clicked_url: Some(path),
                    link_handler_id: None,
                }),
            }),
            outcome,
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(line: &str, col: u16) -> Option<String> {
        let symbols = line.chars().map(|c| c.to_string()).collect::<Vec<_>>();
        let symbols = symbols.iter().map(String::as_str).collect::<Vec<_>>();
        md_path_at(&symbols, col).map(|hit| hit.path)
    }

    fn at(line: &str, needle: &str) -> Option<String> {
        hit(line, line.find(needle).expect("needle") as u16)
    }

    #[test]
    fn finds_markdown_paths_in_plain_text() {
        let line = "Wrote docs/reports/2026-10-02-drovr-status.md to disk";
        assert_eq!(
            at(line, "reports").as_deref(),
            Some("docs/reports/2026-10-02-drovr-status.md")
        );
        assert_eq!(at(line, "Wrote"), None);
        assert_eq!(at(line, " to"), None);
        assert_eq!(
            at("see ./notes/x.md.", "notes").as_deref(),
            Some("./notes/x.md")
        );
        assert_eq!(at("see ./notes/x.md.", "."), Some("./notes/x.md".into()));
        assert_eq!(
            at("open ~/Code/drovr/README.md", "Code").as_deref(),
            Some("~/Code/drovr/README.md")
        );
        assert_eq!(
            at("/abs/path/Report.MARKDOWN!", "abs").as_deref(),
            Some("/abs/path/Report.MARKDOWN")
        );
    }

    #[test]
    fn strips_quotes_brackets_suffixes_and_anchors() {
        for line in [
            "\"docs/plan.md\"",
            "'docs/plan.md'",
            "`docs/plan.md`",
            "(docs/plan.md)",
            "[plan](docs/plan.md)",
            "<docs/plan.md>",
            "docs/plan.md, then",
            "docs/plan.md:12",
            "docs/plan.md:12:4:",
            "docs/plan.md#next-steps",
            "**docs/plan.md**",
        ] {
            assert_eq!(
                at(line, "plan.md").as_deref(),
                Some("docs/plan.md"),
                "{line}"
            );
        }
    }

    #[test]
    fn rejects_other_files_and_web_urls() {
        for line in [
            "docs/plan.md.txt",
            "docs/plan.mdx",
            "https://x/README.md",
            "http://example.com/a.md",
            "ssh://host/a.md",
            "file://otherhost/a.md",
            ".md",
            "docs/.md",
        ] {
            assert_eq!(at(line, "md"), None, "{line}");
        }
    }

    #[test]
    fn turns_file_urls_into_paths() {
        assert_eq!(
            at("file:///tmp/My%20Doc.md#top", "tmp").as_deref(),
            Some("/tmp/My Doc.md")
        );
        assert_eq!(
            at("FILE://localhost/tmp/a.md", "tmp").as_deref(),
            Some("/tmp/a.md")
        );
    }

    #[test]
    fn maps_wide_characters_to_cells() {
        // "界 docs/計画.md": 界 takes cells 0-1, 計 cells 8-9, 画 cells 10-11.
        let row = [
            "界", "", " ", "d", "o", "c", "s", "/", "計", " ", "画", "", ".", "m", "d", " ", "x",
        ];
        let found = md_path_at(&row, 9).expect("spacer cell of a wide char");
        assert_eq!(found.path, "docs/計画.md");
        assert_eq!((found.start_col, found.end_col), (3, 14));
        assert_eq!(md_path_at(&row, 1), None);
        assert_eq!(md_path_at(&row, 16), None);
        assert_eq!(md_path_at(&row, 15), None);
    }

    #[test]
    fn resolves_paths_against_the_pane_cwd() {
        let cwd = Some("/Users/me/Code/drovr");
        assert_eq!(
            resolve_md_path("docs/a.md", cwd).as_deref(),
            Some("/Users/me/Code/drovr/docs/a.md")
        );
        assert_eq!(
            resolve_md_path("./../x/./b.md", cwd).as_deref(),
            Some("/Users/me/Code/x/b.md")
        );
        assert_eq!(resolve_md_path("~/a.md", cwd).as_deref(), Some("~/a.md"));
        assert_eq!(
            resolve_md_path("/abs/a.md", None).as_deref(),
            Some("/abs/a.md")
        );
        assert_eq!(resolve_md_path("a.md", None), None);
        assert_eq!(resolve_md_path("a.md", Some("relative")), None);
        assert_eq!(resolve_md_path("~bob/a.md", cwd), None);
    }
}
