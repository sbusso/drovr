//! drovr fork: client-local project groups for the federated sidebar.
//! User-facing docs: .github/README.md.
//!
//! Projects group workspaces from any machine (Local, gpu-box, ...) under one header,
//! independent of where the panes live. The layout is purely client-side and
//! lives in `<config_dir>/sidebar.toml`, so the stock server never sees it. It is
//! hand-editable; UI actions (right-click menus, keys) rewrite it.
//!
//! ```toml
//! show_hidden = false
//! hidden = ["gpu-box/scratch"]
//!
//! [[group]]
//! name = "Storefront"
//! pinned = true
//! members = ["gpu-box/Storefront", "local/Storefront"]   # machine/workspace label
//! match = ["storefront"]                                 # auto-assign by label substring
//!
//! [inbox]                      # the inbox panel (inbox.rs)
//! width = 0.4                  # share of the screen
//! stuck_minutes = 10
//! [inbox.stuck_minutes_by_workspace]
//! "mato/migrate-db" = 45
//! ```
//!
//! Kept in its own module behind a process-wide lock so the upstream render and
//! navigation signatures stay untouched (cheap rebases).

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{OnceLock, RwLock},
    time::{Instant, SystemTime},
};

use serde::{Deserialize, Serialize};

use super::ClientShellEndpoint;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub(super) struct ProjectGroup {
    pub(super) name: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) pinned: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) collapsed: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) members: Vec<String>,
    #[serde(default, rename = "match", skip_serializing_if = "Vec::is_empty")]
    pub(super) rules: Vec<String>,
    /// Two-letter tag for the collapsed rail (default: derived from the name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) short: Option<String>,
}

/// Rail tag: explicit `short`, else capitals ("TheCalendar" -> "TC"), else
/// first letters of the first two words ("Outsmartis ops" -> "OO"), else the
/// first two letters ("Infrastructure" -> "In").
pub(super) fn project_tag(name: &str, short: Option<&str>) -> String {
    if let Some(short) = short.filter(|short| !short.trim().is_empty()) {
        return short.trim().chars().take(2).collect();
    }
    let words = name.split_whitespace().collect::<Vec<_>>();
    if words.len() >= 2 {
        return words
            .iter()
            .take(2)
            .filter_map(|word| word.chars().next())
            .flat_map(char::to_uppercase)
            .collect();
    }
    let capitals = name
        .chars()
        .filter(|c| c.is_uppercase())
        .take(2)
        .collect::<String>();
    if capitals.chars().count() == 2 {
        return capitals;
    }
    let mut chars = name.chars();
    let first = chars
        .next()
        .map(|c| c.to_uppercase().collect::<String>())
        .unwrap_or_default();
    let second = chars
        .next()
        .map(|c| c.to_lowercase().collect::<String>())
        .unwrap_or_default();
    format!("{first}{second}")
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub(super) struct ProjectLayout {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) show_hidden: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) hidden: Vec<String>,
    /// Agents marked unread by hand (`machine/pane_id`); cleared when focused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) unread: Vec<String>,
    /// Compact view: one line per workspace instead of one row per agent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) compact: bool,
    /// Structured view: workspace headers with one line per agent (vendor mark,
    /// state-coloured title). Ignored while `compact` is set.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) structured: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) other_collapsed: bool,
    /// Show only agents that are working or need attention.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) active_only: bool,
    /// Agents marked inactive by hand, as `machine/pane@state_change_seq`: the
    /// mark lapses as soon as the agent changes state again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) dismissed: Vec<String>,
    /// Agents pinned to the active view by hand (`machine/pane`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) kept: Vec<String>,
    /// How long an idle agent still counts as active (default 24).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) recent_hours: Option<u64>,
    /// Workspaces dragged to "Other": never auto-matched into a project.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) ungrouped: Vec<String>,
    /// The inbox panel: width, stuck threshold, mutes (`inbox.rs`).
    #[serde(
        default,
        skip_serializing_if = "super::inbox::InboxSettings::is_default"
    )]
    pub(super) inbox: super::inbox::InboxSettings,
    #[serde(default, rename = "group", skip_serializing_if = "Vec::is_empty")]
    pub(super) groups: Vec<ProjectGroup>,
}

struct Store {
    layout: ProjectLayout,
    mtime: Option<SystemTime>,
    checked: Instant,
    last_focused_agent: Option<String>,
}

fn path() -> PathBuf {
    crate::config::config_dir().join("sidebar.toml")
}

/// The layout in `path` and its mtime. A missing file is the default layout;
/// `None` means the file exists but cannot be read or parsed, so it must be
/// neither trusted nor overwritten.
fn load_layout(path: &Path) -> Option<(ProjectLayout, Option<SystemTime>)> {
    let mtime = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok();
    match std::fs::read_to_string(path) {
        Ok(content) => match toml::from_str(&content) {
            Ok(layout) => Some((layout, mtime)),
            Err(err) => {
                tracing::warn!(path = %path.display(), err = %err, "sidebar.toml does not parse; keeping the previous layout and leaving the file alone");
                None
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Some((ProjectLayout::default(), None))
        }
        Err(_) => None,
    }
}

fn read_file() -> Option<(ProjectLayout, Option<SystemTime>)> {
    // Unit tests must never see (or depend on) the developer's real layout.
    if cfg!(test) {
        return Some((ProjectLayout::default(), None));
    }
    load_layout(&path())
}

fn store() -> &'static RwLock<Store> {
    static STORE: OnceLock<RwLock<Store>> = OnceLock::new();
    STORE.get_or_init(|| {
        let (layout, mtime) = read_file().unwrap_or_default();
        RwLock::new(Store {
            layout,
            mtime,
            checked: Instant::now(),
            last_focused_agent: None,
        })
    })
}

/// Current layout; picks up hand edits to sidebar.toml at most once a second.
/// A file that does not parse (a typo, a half-saved edit) is ignored and the
/// previous layout stays.
pub(super) fn layout() -> ProjectLayout {
    let store = store();
    {
        let guard = store.read().unwrap_or_else(|e| e.into_inner());
        if guard.checked.elapsed().as_secs() < 1 {
            return guard.layout.clone();
        }
    }
    let mut guard = store.write().unwrap_or_else(|e| e.into_inner());
    guard.checked = Instant::now();
    let mtime = if cfg!(test) {
        None
    } else {
        std::fs::metadata(path())
            .and_then(|meta| meta.modified())
            .ok()
    };
    if mtime != guard.mtime {
        if let Some((layout, mtime)) = read_file() {
            guard.layout = layout;
            guard.mtime = mtime;
        }
    }
    guard.layout.clone()
}

/// Write `content` to `path` through a per-process temporary file, so two
/// clients saving at once never publish each other's half-written file.
fn write_atomic(path: &Path, content: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Read-modify-write of the layout file under an exclusive lock: re-reads the
/// file into `layout` (another client may have changed it), applies `change`,
/// and writes the result back. Returns the new mtime when the file was written.
/// A file that does not parse is never overwritten: the change then applies to
/// `layout` (the in-memory copy) only.
fn persist_change(
    path: &Path,
    layout: &mut ProjectLayout,
    change: impl FnOnce(&mut ProjectLayout),
) -> Option<Option<SystemTime>> {
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    // Best effort: without the lock this is still the single-client behaviour.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(PathBuf::from(lock_path))
        .ok();
    if let Some(lock) = &lock {
        let _ = lock.lock();
    }
    let Some((current, _)) = load_layout(path) else {
        change(layout);
        return None;
    };
    *layout = current;
    let before = layout.clone();
    change(layout);
    if *layout == before {
        return None;
    }
    let content = toml::to_string_pretty(&*layout).ok()?;
    let header = "# herdr (drovr fork) sidebar projects. Hand-editable; see projects.rs.\n";
    write_atomic(path, format!("{header}{content}").as_bytes()).ok()?;
    Some(
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok(),
    )
}

/// Apply a change and persist it. The change is applied to the file as it is
/// now, not to this client's possibly stale copy, so changes made by other
/// drovr clients are kept.
pub(super) fn update(change: impl FnOnce(&mut ProjectLayout)) {
    let store = store();
    let mut guard = store.write().unwrap_or_else(|e| e.into_inner());
    if cfg!(test) {
        change(&mut guard.layout);
        return;
    }
    if let Some(mtime) = persist_change(&path(), &mut guard.layout, change) {
        guard.mtime = mtime;
        guard.checked = Instant::now();
    }
}

pub(super) fn machine_key(endpoint: &ClientShellEndpoint) -> String {
    endpoint.label.to_lowercase()
}

/// Stable identity of a workspace: `machine/id:label`. The id survives renames
/// and tells apart workspaces that share a name; the label is only there so the
/// file stays readable. Older name-only entries (`machine/label`) still match.
pub(super) fn workspace_key(
    endpoint: &ClientShellEndpoint,
    workspace: &crate::protocol::ClientShellWorkspace,
) -> String {
    format!(
        "{}/{}:{}",
        machine_key(endpoint),
        workspace.workspace_id,
        workspace.label
    )
}

fn split_id(rest: &str) -> Option<(&str, &str)> {
    let (id, label) = rest.split_once(':')?;
    let looks_like_id =
        id.len() > 1 && id.starts_with('w') && id[1..].chars().all(|c| c.is_ascii_alphanumeric());
    looks_like_id.then_some((id, label))
}

/// Does a stored entry (new or legacy form) refer to the workspace `key`?
pub(super) fn same_workspace(entry: &str, key: &str) -> bool {
    let (Some((entry_machine, entry_rest)), Some((machine, rest))) =
        (entry.split_once('/'), key.split_once('/'))
    else {
        return entry == key;
    };
    if entry_machine != machine {
        return false;
    }
    match (split_id(entry_rest), split_id(rest)) {
        (Some((entry_id, _)), Some((id, _))) => entry_id == id,
        (None, Some((_, label))) => entry_rest == label,
        _ => entry_rest == rest,
    }
}

pub(super) fn agent_key(endpoint: &ClientShellEndpoint, pane_id: &str) -> String {
    format!("{}/{}", machine_key(endpoint), pane_id)
}

impl ProjectLayout {
    /// Groups in display order (pinned first, otherwise file order) as indices.
    pub(super) fn display_order(&self) -> Vec<usize> {
        let mut order = (0..self.groups.len()).collect::<Vec<_>>();
        order.sort_by_key(|index| !self.groups[*index].pinned);
        order
    }

    pub(super) fn is_hidden(&self, key: &str) -> bool {
        self.hidden.iter().any(|hidden| same_workspace(hidden, key))
    }

    /// Hide or unhide one workspace.
    pub(super) fn toggle_hidden(&mut self, key: &str) {
        if self.is_hidden(key) {
            self.hidden.retain(|hidden| !same_workspace(hidden, key));
        } else {
            self.hidden.push(key.to_owned());
        }
    }

    pub(super) fn is_unread(&self, key: &str) -> bool {
        self.unread.iter().any(|unread| unread == key)
    }

    pub(super) fn explicit_group(&self, key: &str) -> Option<usize> {
        self.groups.iter().position(|group| {
            group
                .members
                .iter()
                .any(|member| same_workspace(member, key))
        })
    }

    /// Group for a workspace: explicit membership wins, then the first rule that
    /// matches the workspace name or any folder its panes run in.
    pub(super) fn group_of(&self, key: &str, label: &str, paths: &[String]) -> Option<usize> {
        if self
            .ungrouped
            .iter()
            .any(|ungrouped| same_workspace(ungrouped, key))
        {
            return None;
        }
        self.explicit_group(key).or_else(|| {
            let label = label.to_lowercase();
            let paths = paths
                .iter()
                .map(|path| path.to_lowercase())
                .collect::<Vec<_>>();
            self.groups.iter().position(|group| {
                group.rules.iter().any(|rule| {
                    let rule = rule.to_lowercase();
                    !rule.is_empty()
                        && (label.contains(&rule) || paths.iter().any(|path| path.contains(&rule)))
                })
            })
        })
    }

    /// Rank inside a group: explicit members first in member order, then matches.
    fn member_rank(&self, group: usize, key: &str) -> usize {
        self.groups[group]
            .members
            .iter()
            .position(|member| same_workspace(member, key))
            .unwrap_or(usize::MAX)
    }

    /// Move a workspace into `group_name`; an empty name moves it to Other.
    pub(super) fn assign(&mut self, key: &str, group_name: &str) {
        for group in &mut self.groups {
            group.members.retain(|member| !same_workspace(member, key));
        }
        self.ungrouped
            .retain(|ungrouped| !same_workspace(ungrouped, key));
        let name = group_name.trim();
        if name.is_empty() {
            self.ungrouped.push(key.to_owned());
            return;
        }
        match self
            .groups
            .iter_mut()
            .find(|group| group.name.eq_ignore_ascii_case(name))
        {
            Some(group) => group.members.push(key.to_owned()),
            None => self.groups.push(ProjectGroup {
                name: name.to_owned(),
                members: vec![key.to_owned()],
                ..ProjectGroup::default()
            }),
        }
    }

    /// Move a workspace into `group_name` (empty = Other) just before the
    /// workspace `before`, or last when `before` is `None`. `resolved` lists
    /// every workspace the group holds right now, in display order. Rule
    /// matches among them become explicit members so the new order sticks;
    /// existing members (offline machines, hidden or filtered workspaces) keep
    /// their place, and only `key` moves.
    pub(super) fn place(
        &mut self,
        key: &str,
        group_name: &str,
        before: Option<&str>,
        resolved: &[String],
    ) {
        // Dropping a workspace just before itself leaves everything as is.
        if before.is_some_and(|before| same_workspace(before, key)) {
            return;
        }
        self.assign(key, group_name);
        let name = group_name.trim();
        let Some(group) = self
            .groups
            .iter_mut()
            .find(|group| !name.is_empty() && group.name.eq_ignore_ascii_case(name))
        else {
            return;
        };
        group.members.retain(|member| !same_workspace(member, key));
        for member in resolved {
            if !same_workspace(member, key)
                && !group
                    .members
                    .iter()
                    .any(|existing| same_workspace(existing, member))
            {
                group.members.push(member.clone());
            }
        }
        let at = before
            .and_then(|before| {
                group
                    .members
                    .iter()
                    .position(|member| same_workspace(member, before))
            })
            .unwrap_or(group.members.len());
        group.members.insert(at, key.to_owned());
    }

    /// Move a workspace `delta` steps within `group`, whose workspaces are
    /// `members_in_view` in display order (see [`ProjectLayout::place`]).
    pub(super) fn move_member(
        &mut self,
        group: usize,
        members_in_view: &[String],
        key: &str,
        delta: isize,
    ) {
        let Some(position) = members_in_view.iter().position(|member| member == key) else {
            return;
        };
        let target = position as isize + delta;
        if target < 0 || target as usize >= members_in_view.len() {
            return;
        }
        let before = if delta < 0 {
            members_in_view.get(target as usize)
        } else {
            members_in_view.get(target as usize + 1)
        };
        let Some(name) = self.groups.get(group).map(|group| group.name.clone()) else {
            return;
        };
        self.place(key, &name, before.map(String::as_str), members_in_view);
    }

    /// Move a group one step in display order (pinned groups stay above others).
    pub(super) fn move_group(&mut self, name: &str, delta: isize) {
        let order = self.display_order();
        let Some(position) = order
            .iter()
            .position(|index| self.groups[*index].name == name)
        else {
            return;
        };
        let target = position as isize + delta;
        if target < 0 || target as usize >= order.len() {
            return;
        }
        let (a, b) = (order[position], order[target as usize]);
        if self.groups[a].pinned != self.groups[b].pinned {
            return;
        }
        self.groups.swap(a, b);
    }

    pub(super) fn group_mut(&mut self, name: &str) -> Option<&mut ProjectGroup> {
        self.groups.iter_mut().find(|group| group.name == name)
    }
}

/// Folders a workspace lives in: its new-pane cwd plus every pane's cwd.
pub(super) fn workspace_paths(
    snapshot: &crate::protocol::ClientShellSnapshot,
    workspace: &crate::protocol::ClientShellWorkspace,
) -> Vec<String> {
    let mut paths = vec![workspace.new_workspace_cwd.clone()];
    for pane in snapshot
        .panes
        .iter()
        .filter(|pane| pane.workspace_id == workspace.workspace_id)
    {
        paths.extend(pane.foreground_cwd.iter().cloned());
        paths.extend(pane.cwd.iter().cloned());
    }
    paths.retain(|path| !path.is_empty());
    paths
}

/// One workspace placed in the federated sidebar.
#[derive(Clone, Debug)]
pub(super) struct PlacedWorkspace {
    pub(super) endpoint: usize,
    pub(super) index: usize,
    pub(super) key: String,
    pub(super) hidden: bool,
}

pub(super) struct ProjectSection {
    pub(super) group: usize,
    pub(super) members: Vec<PlacedWorkspace>,
}

/// Resolve every workspace of every endpoint into project sections; returns the
/// sections in display order plus the set of (endpoint, index) they claimed.
pub(super) fn sections(
    layout: &ProjectLayout,
    endpoints: &[ClientShellEndpoint],
) -> (Vec<ProjectSection>, HashSet<(usize, usize)>) {
    let mut sections = layout
        .display_order()
        .into_iter()
        .map(|group| ProjectSection {
            group,
            members: Vec::new(),
        })
        .collect::<Vec<_>>();
    let mut claimed = HashSet::new();
    for (endpoint_index, endpoint) in endpoints.iter().enumerate() {
        let Some(snapshot) = endpoint.snapshot.as_deref() else {
            continue;
        };
        for (index, workspace) in snapshot.workspaces.iter().enumerate() {
            let key = workspace_key(endpoint, workspace);
            let paths = workspace_paths(snapshot, workspace);
            let Some(group) = layout.group_of(&key, &workspace.label, &paths) else {
                continue;
            };
            claimed.insert((endpoint_index, index));
            let hidden = layout.is_hidden(&key);
            if let Some(section) = sections.iter_mut().find(|section| section.group == group) {
                section.members.push(PlacedWorkspace {
                    endpoint: endpoint_index,
                    index,
                    key,
                    hidden,
                });
            }
        }
    }
    for section in &mut sections {
        section
            .members
            .sort_by_key(|member| layout.member_rank(section.group, &member.key));
    }
    (sections, claimed)
}

/// The group that owns `key` and every workspace it holds, in display order.
pub(super) fn group_members_in_view(
    layout: &ProjectLayout,
    endpoints: &[ClientShellEndpoint],
    key: &str,
) -> Option<(usize, Vec<String>)> {
    let (sections, _) = sections(layout, endpoints);
    sections
        .into_iter()
        .find(|section| section.members.iter().any(|member| member.key == key))
        .map(|section| {
            (
                section.group,
                section
                    .members
                    .into_iter()
                    .map(|member| member.key)
                    .collect(),
            )
        })
}

/// Every workspace the group named `name` holds, in display order.
pub(super) fn resolved_members(
    layout: &ProjectLayout,
    endpoints: &[ClientShellEndpoint],
    name: &str,
) -> Vec<String> {
    let (sections, _) = sections(layout, endpoints);
    sections
        .into_iter()
        .find(|section| layout.groups[section.group].name == name)
        .map(|section| {
            section
                .members
                .into_iter()
                .map(|member| member.key)
                .collect()
        })
        .unwrap_or_default()
}

/// Sort key that puts agents in project order; ungrouped agents keep their
/// original relative order after every project. `None` = hidden, drop it.
pub(super) fn agent_rank(
    layout: &ProjectLayout,
    endpoint: &ClientShellEndpoint,
    workspace: &crate::protocol::ClientShellWorkspace,
    paths: &[String],
) -> Option<(usize, usize)> {
    let workspace_label = workspace.label.as_str();
    let key = workspace_key(endpoint, workspace);
    if layout.is_hidden(&key) && !layout.show_hidden {
        return None;
    }
    let order = layout.display_order();
    Some(match layout.group_of(&key, workspace_label, paths) {
        Some(group) => (
            order.iter().position(|index| *index == group).unwrap_or(0),
            layout.member_rank(group, &key),
        ),
        None => (usize::MAX, 0),
    })
}

/// What the sidebar shows for an agent once manual marks are applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Presence {
    Blocked,
    Unread,
    Done,
    Working,
    Idle,
}

impl Presence {
    pub(super) fn needs_attention(self) -> bool {
        matches!(self, Self::Blocked | Self::Unread | Self::Done)
    }

    pub(super) fn is_active(self) -> bool {
        self != Self::Idle
    }
}

impl ProjectLayout {
    fn dismissed_key(key: &str, seq: u64) -> String {
        format!("{key}@{seq}")
    }

    pub(super) fn presence(
        &self,
        key: &str,
        seq: u64,
        status: crate::api::schema::AgentStatus,
    ) -> Presence {
        use crate::api::schema::AgentStatus;
        if self.is_unread(key) {
            return Presence::Unread;
        }
        let dismissed = self.is_dismissed(key, seq);
        match status {
            AgentStatus::Working => Presence::Working,
            AgentStatus::Blocked if !dismissed => Presence::Blocked,
            AgentStatus::Done if !dismissed => Presence::Done,
            _ => Presence::Idle,
        }
    }

    /// The agent was marked inactive in its current state (`seq`).
    pub(super) fn is_dismissed(&self, key: &str, seq: u64) -> bool {
        self.dismissed
            .iter()
            .any(|entry| *entry == Self::dismissed_key(key, seq))
    }

    /// Manual status: unread (needs attention) or inactive (dismissed until the
    /// agent's next state change).
    pub(super) fn mark(&mut self, key: &str, seq: u64, unread: bool) {
        self.unread.retain(|entry| entry != key);
        let dismissed = Self::dismissed_key(key, seq);
        let prefix = format!("{key}@");
        self.dismissed.retain(|entry| !entry.starts_with(&prefix));
        if unread {
            self.unread.push(key.to_owned());
        } else {
            self.dismissed.push(dismissed);
            let excess = self.dismissed.len().saturating_sub(200);
            self.dismissed.drain(..excess);
        }
    }
}

impl ProjectLayout {
    pub(super) fn is_kept(&self, key: &str) -> bool {
        self.kept.iter().any(|kept| kept == key)
    }

    pub(super) fn toggle_kept(&mut self, key: &str) {
        if self.is_kept(key) {
            self.kept.retain(|kept| kept != key);
        } else {
            self.kept.push(key.to_owned());
        }
    }

    /// The view toggle: detailed -> compact -> structured -> detailed.
    pub(super) fn cycle_view(&mut self) {
        (self.compact, self.structured) = match (self.compact, self.structured) {
            (false, false) => (true, false),
            (true, _) => (false, true),
            (false, true) => (false, false),
        };
    }

    pub(super) fn recent_secs(&self) -> u64 {
        self.recent_hours.unwrap_or(24) * 3600
    }
}

/// When each agent last changed state (unix seconds), as observed by this
/// client. herdr sends no timestamps, so drovr records them itself and keeps
/// them in `<state_dir>/drovr-activity.json` so restarts don't reset them.
#[derive(Default, Deserialize, Serialize)]
struct Activity {
    #[serde(default)]
    agents: std::collections::HashMap<String, (u64, u64)>,
    #[serde(skip)]
    dirty: bool,
    #[serde(skip)]
    saved: Option<Instant>,
    #[serde(skip)]
    primed: HashSet<String>,
}

/// A JSON store from `path`; missing means empty. A file that does not parse
/// is moved aside (`<name>.bad-<unix secs>`) rather than overwritten later, so
/// its history can still be recovered by hand.
fn load_json<T: Default + serde::de::DeserializeOwned>(path: &Path) -> T {
    let Ok(content) = std::fs::read_to_string(path) else {
        return T::default();
    };
    match serde_json::from_str(&content) {
        Ok(value) => value,
        Err(err) => {
            let mut aside = path.as_os_str().to_owned();
            aside.push(format!(".bad-{}", unix_now()));
            let aside = PathBuf::from(aside);
            tracing::warn!(path = %path.display(), aside = %aside.display(), err = %err, "drovr store does not parse; moved aside");
            let _ = std::fs::rename(path, aside);
            T::default()
        }
    }
}

impl Activity {
    /// Add entries saved by other clients; on a clash the later change wins.
    fn merge(&mut self, saved: Activity) {
        for (key, (seq, at)) in saved.agents {
            let entry = self.agents.entry(key).or_insert((seq, at));
            if at > entry.1 {
                *entry = (seq, at);
            }
        }
    }
}

fn activity_path() -> PathBuf {
    crate::config::state_dir().join("drovr-activity.json")
}

fn activity() -> &'static std::sync::Mutex<Activity> {
    static ACTIVITY: OnceLock<std::sync::Mutex<Activity>> = OnceLock::new();
    ACTIVITY.get_or_init(|| {
        std::sync::Mutex::new(if cfg!(test) {
            Activity::default()
        } else {
            load_json(&activity_path())
        })
    })
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Record state changes from a fresh endpoint snapshot. Agents seen for the
/// first time get no timestamp (unknown age = treated as old), except ones that
/// are working, which are clearly current.
pub(super) fn observe_activity(endpoint: &ClientShellEndpoint) {
    let Some(snapshot) = endpoint.snapshot.as_deref() else {
        return;
    };
    let now = unix_now();
    let mut store = activity().lock().unwrap_or_else(|e| e.into_inner());
    for agent in &snapshot.agents {
        let key = agent_key(endpoint, &agent.pane_id);
        match store.agents.get(&key) {
            Some((seq, _)) if *seq == agent.state_change_seq => {}
            Some(_) => {
                store.agents.insert(key, (agent.state_change_seq, now));
                store.dirty = true;
            }
            None => {
                let at = if agent.agent_status == crate::api::schema::AgentStatus::Working {
                    now
                } else {
                    0
                };
                store.agents.insert(key, (agent.state_change_seq, at));
                store.dirty = true;
            }
        }
    }
    // Workspaces: remember when each first appeared. On the first snapshot of a
    // machine in this process, unknown ones are old (unknown age); after that,
    // a new id is a workspace that was just created.
    let machine = machine_key(endpoint);
    let primed = store.primed.contains(&machine);
    for workspace in &snapshot.workspaces {
        let key = format!("{machine}/ws:{}", workspace.workspace_id);
        if let std::collections::hash_map::Entry::Vacant(entry) = store.agents.entry(key) {
            entry.insert((0, if primed { now } else { 0 }));
            store.dirty = true;
        }
    }
    store.primed.insert(machine);
    drop(store);
    observe_usage(endpoint, snapshot);
    let mut store = activity().lock().unwrap_or_else(|e| e.into_inner());
    let due = store
        .saved
        .is_none_or(|saved| saved.elapsed().as_secs() >= 10);
    if store.dirty && due && !cfg!(test) {
        let path = activity_path();
        // Keep what other drovr clients recorded since this one loaded.
        store.merge(load_json(&path));
        let month = 30 * 24 * 3600;
        store
            .agents
            .retain(|_, (_, at)| *at == 0 || now.saturating_sub(*at) < month);
        if let Ok(content) = serde_json::to_vec(&*store) {
            if write_atomic(&path, &content).is_ok() {
                store.dirty = false;
                store.saved = Some(Instant::now());
            }
        }
    }
}

/// Seconds since the agent last changed state, if known.
pub(super) fn idle_secs(key: &str) -> Option<u64> {
    let store = activity().lock().unwrap_or_else(|e| e.into_inner());
    store
        .agents
        .get(key)
        .and_then(|(_, at)| (*at > 0).then(|| unix_now().saturating_sub(*at)))
}

/// Token/time usage per agent session, reported by the drovr usage hook as
/// pane metadata (`drovr_u_YYYYMMDD`, `drovr_session`) and kept here so it
/// outlives the agent: `<state_dir>/drovr-usage.json`.
#[derive(Clone, Default, Deserialize, Serialize)]
pub(super) struct UsageRecord {
    /// Workspace key at last sighting, plus what project rules match on.
    pub(super) key: String,
    pub(super) label: String,
    #[serde(default)]
    pub(super) paths: Vec<String>,
    /// day -> [input, output, cache_read, cache_write, active_minutes]
    pub(super) days: std::collections::BTreeMap<String, [u64; 5]>,
}

#[derive(Default, Deserialize, Serialize)]
struct UsageStore {
    #[serde(default)]
    sessions: std::collections::HashMap<String, UsageRecord>,
    #[serde(skip)]
    dirty: bool,
    #[serde(skip)]
    saved: Option<Instant>,
}

impl UsageStore {
    /// Add sessions and days saved by other clients. Day totals only grow, so
    /// on a clash the larger value of each field wins.
    fn merge(&mut self, saved: UsageStore) {
        for (session, record) in saved.sessions {
            let Some(mine) = self.sessions.get_mut(&session) else {
                self.sessions.insert(session, record);
                continue;
            };
            for (day, totals) in record.days {
                let slot = mine.days.entry(day).or_default();
                for (value, other) in slot.iter_mut().zip(totals) {
                    *value = (*value).max(other);
                }
            }
        }
    }
}

fn usage_path() -> PathBuf {
    crate::config::state_dir().join("drovr-usage.json")
}

fn usage_store() -> &'static std::sync::Mutex<UsageStore> {
    static USAGE: OnceLock<std::sync::Mutex<UsageStore>> = OnceLock::new();
    USAGE.get_or_init(|| {
        std::sync::Mutex::new(if cfg!(test) {
            UsageStore::default()
        } else {
            load_json(&usage_path())
        })
    })
}

pub(super) fn agent_token<'a>(
    agent: &'a crate::protocol::ClientShellAgent,
    name: &str,
) -> Option<&'a str> {
    agent
        .tokens
        .iter()
        .find(|(token, _)| token == name || token.strip_prefix('$') == Some(name))
        .map(|(_, value)| value.as_str())
}

/// A token from the Claude Code usage hook. Pane tokens outlive the agent, so
/// they only apply while Claude is the pane's agent: a codex started in the
/// same pane must not inherit the last Claude session's name or context.
fn claude_token<'a>(agent: &'a crate::protocol::ClientShellAgent, name: &str) -> Option<&'a str> {
    (agent.agent.as_deref() == Some("claude"))
        .then(|| agent_token(agent, name))
        .flatten()
}

/// Current context size of an agent, from the usage hook.
pub(super) fn agent_context_tokens(agent: &crate::protocol::ClientShellAgent) -> Option<u64> {
    claude_token(agent, "drovr_ctx")?.parse().ok()
}

/// The agent session's own name (Claude's custom or AI title), from the usage hook.
pub(super) fn agent_session_name(agent: &crate::protocol::ClientShellAgent) -> Option<&str> {
    claude_token(agent, "drovr_name")
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

/// State of a Claude workflow, from the workflow hook's `drovr_wf` token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkflowState {
    Running,
    Done,
    Failed,
}

/// A Claude workflow's progress on an agent row: finished phases out of the
/// script's phases, and the Markdown view the hook keeps up to date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkflowCue {
    pub(super) state: WorkflowState,
    pub(super) done: u16,
    pub(super) total: u16,
    /// The current phase ("state hook"), or how the run ended.
    pub(super) phase: Option<String>,
    pub(super) doc: Option<String>,
}

/// The workflow cue from `drovr_wf` ("running 2/6"), `drovr_wf_phase` and
/// `drovr_wf_doc`.
pub(super) fn agent_workflow(agent: &crate::protocol::ClientShellAgent) -> Option<WorkflowCue> {
    let (state, count) = claude_token(agent, "drovr_wf")?.trim().split_once(' ')?;
    let (done, total) = count.split_once('/')?;
    let (done, total): (u16, u16) = (done.parse().ok()?, total.parse().ok()?);
    Some(WorkflowCue {
        state: match state {
            "running" => WorkflowState::Running,
            "done" => WorkflowState::Done,
            "failed" => WorkflowState::Failed,
            _ => return None,
        },
        done: done.min(total),
        total: total.max(1),
        phase: claude_token(agent, "drovr_wf_phase")
            .map(str::trim)
            .filter(|phase| !phase.is_empty())
            .map(str::to_owned),
        doc: claude_token(agent, "drovr_wf_doc")
            .map(str::trim)
            .filter(|doc| doc.starts_with('/'))
            .map(str::to_owned),
    })
}

fn observe_usage(endpoint: &ClientShellEndpoint, snapshot: &crate::protocol::ClientShellSnapshot) {
    let mut store = usage_store().lock().unwrap_or_else(|e| e.into_inner());
    for agent in &snapshot.agents {
        let Some(session) = agent_token(agent, "drovr_session") else {
            continue;
        };
        // One token per day: drovr_u_YYYYMMDD = "in,out,cache_read,cache_write,minutes".
        let days = agent
            .tokens
            .iter()
            .filter_map(|(name, value)| {
                let date = name.trim_start_matches('$').strip_prefix("drovr_u_")?;
                if date.len() != 8 {
                    return None;
                }
                let mut totals = [0u64; 5];
                let mut parts = value.split(',');
                for slot in &mut totals {
                    *slot = parts.next()?.trim().parse().ok()?;
                }
                Some((
                    format!("{}-{}-{}", &date[0..4], &date[4..6], &date[6..8]),
                    totals,
                ))
            })
            .collect::<Vec<_>>();
        if days.is_empty() {
            continue;
        }
        let Some(workspace) = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == agent.workspace_id)
        else {
            continue;
        };
        let record = store
            .sessions
            .entry(format!("{}/{session}", machine_key(endpoint)))
            .or_default();
        let key = workspace_key(endpoint, workspace);
        let mut changed = record.key != key;
        record.key = key;
        record.label = workspace.label.clone();
        record.paths = workspace_paths(snapshot, workspace);
        for (day, totals) in days {
            if record.days.get(&day) != Some(&totals) {
                record.days.insert(day, totals);
                changed = true;
            }
        }
        store.dirty |= changed;
    }
    let due = store
        .saved
        .is_none_or(|saved| saved.elapsed().as_secs() >= 30);
    if store.dirty && due && !cfg!(test) {
        let path = usage_path();
        // Keep what other drovr clients recorded since this one loaded.
        store.merge(load_json(&path));
        if let Ok(content) = serde_json::to_vec(&*store) {
            if write_atomic(&path, &content).is_ok() {
                store.dirty = false;
                store.saved = Some(Instant::now());
            }
        }
    }
}

/// Usage of one project (`None` = Other) over the last `days` local days:
/// [input, output, cache_read, cache_write, active_minutes].
pub(super) fn project_usage(layout: &ProjectLayout, group: Option<&str>, days: i64) -> [u64; 5] {
    let since = (chrono_like_today_minus(days - 1)).unwrap_or_default();
    let store = usage_store().lock().unwrap_or_else(|e| e.into_inner());
    let mut total = [0u64; 5];
    for record in store.sessions.values() {
        let owner = layout
            .group_of(&record.key, &record.label, &record.paths)
            .map(|index| layout.groups[index].name.as_str());
        if owner != group {
            continue;
        }
        for totals in record.days.range(since.clone()..).map(|(_, totals)| totals) {
            for (sum, value) in total.iter_mut().zip(totals) {
                *sum += value;
            }
        }
    }
    total
}

/// Local date `n` days ago as YYYY-MM-DD (no chrono dependency: ask `date`
/// once per call is too slow, so compute from the system clock + local offset).
fn chrono_like_today_minus(n: i64) -> Option<String> {
    let offset = local_utc_offset_secs();
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64
        + offset;
    let days = now.div_euclid(86_400) - n;
    Some(civil_from_days(days))
}

fn local_utc_offset_secs() -> i64 {
    static OFFSET: OnceLock<i64> = OnceLock::new();
    *OFFSET.get_or_init(|| {
        std::process::Command::new("date")
            .arg("+%z")
            .output()
            .ok()
            .and_then(|output| {
                let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                let sign = if text.starts_with('-') { -1 } else { 1 };
                let digits = text.trim_start_matches(['+', '-']);
                let hours: i64 = digits.get(0..2)?.parse().ok()?;
                let minutes: i64 = digits.get(2..4)?.parse().ok()?;
                Some(sign * (hours * 3600 + minutes * 60))
            })
            .unwrap_or(0)
    })
}

/// Days since 1970-01-01 -> "YYYY-MM-DD" (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", if m <= 2 { y + 1 } else { y }, m, d)
}

/// "1h05m" / "37m".
pub(super) fn format_minutes(minutes: u64) -> String {
    if minutes >= 60 {
        format!("{}h{:02}m", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}m")
    }
}

/// "1.2M" / "340k" / "900".
pub(super) fn format_tokens(tokens: u64) -> String {
    match tokens {
        0..=999 => tokens.to_string(),
        1_000..=999_999 => format!("{}k", tokens / 1_000),
        _ => format!("{:.1}M", tokens as f64 / 1_000_000.0),
    }
}

/// Seconds since a workspace first appeared, if this client saw it appear.
pub(super) fn workspace_age_secs(
    endpoint: &ClientShellEndpoint,
    workspace_id: &str,
) -> Option<u64> {
    idle_secs(&format!("{}/ws:{}", machine_key(endpoint), workspace_id))
}

pub(super) fn format_age(secs: u64) -> String {
    match secs {
        0..=59 => "now".to_owned(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// A workspace drovr just asked a machine to create, waiting to appear so the
/// agent command can be typed into it.
#[derive(Clone, Debug)]
pub(super) struct PendingLaunch {
    pub(super) endpoint_id: super::ClientEndpointId,
    pub(super) label: String,
    pub(super) known: HashSet<String>,
    pub(super) command: Option<String>,
    pub(super) since: Instant,
}

fn launch_store() -> &'static std::sync::Mutex<Option<PendingLaunch>> {
    static LAUNCH: OnceLock<std::sync::Mutex<Option<PendingLaunch>>> = OnceLock::new();
    LAUNCH.get_or_init(Default::default)
}

pub(super) fn set_launch(launch: Option<PendingLaunch>) {
    *launch_store().lock().unwrap_or_else(|e| e.into_inner()) = launch;
}

pub(super) fn launch() -> Option<PendingLaunch> {
    launch_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Peek (prefix+space): reveal idle age, context, numbers and latency for a
/// few seconds instead of showing them all the time.
const PEEK_SECS: u64 = 10;

fn peek_store() -> &'static std::sync::Mutex<Option<Instant>> {
    static PEEK: OnceLock<std::sync::Mutex<Option<Instant>>> = OnceLock::new();
    PEEK.get_or_init(Default::default)
}

pub(super) fn toggle_peek() {
    let mut peek = peek_store().lock().unwrap_or_else(|e| e.into_inner());
    *peek = if peek.is_some() {
        None
    } else {
        Some(Instant::now())
    };
}

pub(super) fn peeking() -> bool {
    peek_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some_and(|since| since.elapsed().as_secs() < PEEK_SECS)
}

/// Ends an expired peek; true when the sidebar needs a repaint.
pub(super) fn expire_peek() -> bool {
    let mut peek = peek_store().lock().unwrap_or_else(|e| e.into_inner());
    if peek.is_some_and(|since| since.elapsed().as_secs() >= PEEK_SECS) {
        *peek = None;
        return true;
    }
    false
}

/// Agents whose desktop notification was clicked, waiting to be focused.
fn focus_queue() -> &'static std::sync::Mutex<Vec<(super::ClientEndpointId, String)>> {
    static QUEUE: OnceLock<std::sync::Mutex<Vec<(super::ClientEndpointId, String)>>> =
        OnceLock::new();
    QUEUE.get_or_init(Default::default)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // clickable notifications are Linux-only
pub(crate) fn request_focus(endpoint_id: super::ClientEndpointId, pane_id: String) {
    focus_queue()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((endpoint_id, pane_id));
}

pub(super) fn take_focus_requests() -> Vec<(super::ClientEndpointId, String)> {
    std::mem::take(&mut *focus_queue().lock().unwrap_or_else(|e| e.into_inner()))
}

static HINTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(super) fn set_hinting(on: bool) {
    HINTING.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub(super) fn hinting() -> bool {
    HINTING.load(std::sync::atomic::Ordering::Relaxed)
}

/// A press on a drovr sidebar row, kept until the button comes up so the
/// same gesture can be a click (focus) or a drag (move to a project).
#[derive(Clone, Debug)]
pub(super) struct RowPress {
    pub(super) endpoint_id: super::ClientEndpointId,
    pub(super) workspace_id: String,
    pub(super) pane_id: Option<String>,
    pub(super) start: (u16, u16),
    pub(super) dragging: Option<(u16, u16)>,
}

fn press_store() -> &'static std::sync::Mutex<Option<RowPress>> {
    static PRESS: OnceLock<std::sync::Mutex<Option<RowPress>>> = OnceLock::new();
    PRESS.get_or_init(Default::default)
}

pub(super) fn set_press(press: Option<RowPress>) {
    *press_store().lock().unwrap_or_else(|e| e.into_inner()) = press;
}

pub(super) fn press() -> Option<RowPress> {
    press_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Drop the row press; true when there was one (the caller repaints).
pub(super) fn clear_press() -> bool {
    press_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .is_some()
}

/// Track the pointer of a pressed row. It becomes a drag once the pointer
/// leaves the pressed row or moves two columns, so a slightly shaky click
/// still focuses.
pub(super) fn drag_to(point: (u16, u16)) -> bool {
    let mut guard = press_store().lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_mut() {
        Some(press)
            if press.dragging.is_some()
                || press.start.1 != point.1
                || press.start.0.abs_diff(point.0) >= 2 =>
        {
            press.dragging = Some(point);
            true
        }
        _ => false,
    }
}

/// Header key used for the catch-all "Other" group.
pub(super) const OTHER: &str = "\u{0}other";

/// Clear the manual unread flag of an agent once it gains focus (on the focus
/// transition only, so marking the focused agent itself still sticks).
pub(super) fn note_focused_agent(key: Option<String>) {
    let store = store();
    let previous = {
        let mut guard = store.write().unwrap_or_else(|e| e.into_inner());
        if guard.last_focused_agent == key {
            return;
        }
        std::mem::replace(&mut guard.last_focused_agent, key.clone())
    };
    let _ = previous;
    if let Some(key) = key {
        if layout().is_unread(&key) {
            update(|layout| layout.unread.retain(|unread| unread != &key));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "drovr-projects-{name}-{}-{}",
            std::process::id(),
            unix_now()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn layout_changes_merge_with_the_file_and_never_overwrite_a_broken_one() {
        let dir = temp_dir("layout");
        let path = dir.join("sidebar.toml");
        // Another client assigned a group since this one last read the file.
        std::fs::write(&path, "[[group]]\nname = \"Billing\"\n").expect("write");
        let mut memory = ProjectLayout::default();
        let mtime = persist_change(&path, &mut memory, |layout| layout.show_hidden = true);
        assert!(mtime.is_some());
        let saved = load_layout(&path).expect("parses").0;
        assert!(saved.show_hidden);
        assert_eq!(saved.groups[0].name, "Billing");
        assert_eq!(memory, saved);
        // A typo: the file is left alone; the change still applies in memory.
        let broken = "show_hidden = tru\n[[group]]\nname = \"Billing\"\n";
        std::fs::write(&path, broken).expect("write");
        assert!(load_layout(&path).is_none());
        let mtime = persist_change(&path, &mut memory, |layout| layout.compact = true);
        assert!(mtime.is_none());
        assert!(memory.compact && memory.groups[0].name == "Billing");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), broken);
        // A missing file is an empty layout, not a broken one.
        assert_eq!(
            load_layout(&dir.join("absent.toml")).map(|(layout, _)| layout),
            Some(ProjectLayout::default())
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn json_stores_merge_other_clients_and_move_broken_files_aside() {
        let dir = temp_dir("json");
        let path = dir.join("drovr-usage.json");
        std::fs::write(&path, "{\"sessions\": {\"bad").expect("write");
        let store: UsageStore = load_json(&path);
        assert!(store.sessions.is_empty());
        assert!(!path.exists());
        let aside = std::fs::read_dir(&dir)
            .expect("list")
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains(".bad-"));
        assert!(aside);

        let record = |days: &[(&str, [u64; 5])]| UsageRecord {
            days: days.iter().map(|(day, t)| ((*day).into(), *t)).collect(),
            ..UsageRecord::default()
        };
        let mut mine = UsageStore::default();
        mine.sessions
            .insert("local/a".into(), record(&[("2026-10-01", [5, 1, 0, 0, 2])]));
        let mut theirs = UsageStore::default();
        theirs.sessions.insert(
            "local/a".into(),
            record(&[("2026-10-01", [3, 4, 0, 0, 1]), ("2026-09-30", [1; 5])]),
        );
        theirs
            .sessions
            .insert("mato/x".into(), record(&[("2026-10-01", [9; 5])]));
        mine.merge(theirs);
        assert_eq!(mine.sessions["local/a"].days["2026-10-01"], [5, 4, 0, 0, 2]);
        assert_eq!(mine.sessions["local/a"].days["2026-09-30"], [1; 5]);
        assert!(mine.sessions.contains_key("mato/x"));

        let mut activity = Activity::default();
        activity.agents.insert("local/p1".into(), (3, 100));
        let mut saved = Activity::default();
        saved.agents.insert("local/p1".into(), (2, 50));
        saved.agents.insert("mato/p9".into(), (7, 70));
        activity.merge(saved);
        assert_eq!(activity.agents["local/p1"], (3, 100));
        assert_eq!(activity.agents["mato/p9"], (7, 70));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn group(name: &str, members: &[&str], rules: &[&str]) -> ProjectGroup {
        ProjectGroup {
            name: name.into(),
            members: members.iter().map(|m| m.to_string()).collect(),
            rules: rules.iter().map(|r| r.to_string()).collect(),
            ..ProjectGroup::default()
        }
    }

    #[test]
    fn explicit_membership_beats_rules() {
        let layout = ProjectLayout {
            groups: vec![
                group("A", &[], &["store"]),
                group("B", &["gpu-box/Storefront"], &[]),
            ],
            ..ProjectLayout::default()
        };
        assert_eq!(
            layout.group_of("gpu-box/Storefront", "Storefront", &[]),
            Some(1)
        );
        assert_eq!(
            layout.group_of("local/Storefront", "Storefront", &[]),
            Some(0)
        );
        assert_eq!(layout.group_of("local/Notes", "Notes", &[]), None);
        assert_eq!(
            layout.group_of("gpu-box/sf", "sf", &["/home/me/code/Storefront".into()]),
            Some(0)
        );
    }

    #[test]
    fn pinned_groups_display_first_and_stay_above() {
        let mut layout = ProjectLayout {
            groups: vec![group("A", &[], &[]), group("B", &[], &[])],
            ..ProjectLayout::default()
        };
        layout.groups[1].pinned = true;
        assert_eq!(layout.display_order(), vec![1, 0]);
        layout.move_group("A", -1);
        assert_eq!(layout.display_order(), vec![1, 0]);
    }

    #[test]
    fn assign_moves_between_groups_and_creates() {
        let mut layout = ProjectLayout {
            groups: vec![group("A", &["local/x"], &[])],
            ..ProjectLayout::default()
        };
        layout.assign("local/x", "b");
        assert!(layout.groups[0].members.is_empty());
        assert_eq!(layout.groups[1].name, "b");
        layout.assign("local/x", "A");
        assert_eq!(layout.groups[0].members, vec!["local/x".to_string()]);
        assert!(layout.groups[1].members.is_empty());
    }

    #[test]
    fn rail_tags() {
        assert_eq!(project_tag("TheCalendar", None), "TC");
        assert_eq!(project_tag("Outsmartis ops", None), "OO");
        assert_eq!(project_tag("Infrastructure", None), "In");
        assert_eq!(project_tag("LF", None), "LF");
        assert_eq!(project_tag("VTM", None), "VT");
        assert_eq!(project_tag("Anything", Some("op")), "op");
    }

    #[test]
    fn civil_dates_and_formatting() {
        assert_eq!(civil_from_days(0), "1970-01-01");
        assert_eq!(civil_from_days(20_727), "2026-10-01");
        assert_eq!(format_minutes(65), "1h05m");
        assert_eq!(format_tokens(1_234_567), "1.2M");
        assert_eq!(format_tokens(340_000), "340k");
    }

    #[test]
    fn manual_marks_override_until_state_changes() {
        use crate::api::schema::AgentStatus;
        let mut layout = ProjectLayout::default();
        assert_eq!(
            layout.presence("dev/p1", 4, AgentStatus::Done),
            Presence::Done
        );
        layout.mark("dev/p1", 4, false);
        assert_eq!(
            layout.presence("dev/p1", 4, AgentStatus::Done),
            Presence::Idle
        );
        assert_eq!(
            layout.presence("dev/p1", 5, AgentStatus::Done),
            Presence::Done
        );
        layout.mark("dev/p1", 5, true);
        assert_eq!(
            layout.presence("dev/p1", 5, AgentStatus::Idle),
            Presence::Unread
        );
        assert!(layout.dismissed.is_empty());
    }

    #[test]
    fn dragging_to_other_beats_rules_and_back() {
        let mut layout = ProjectLayout {
            groups: vec![group("A", &[], &["store"])],
            ..ProjectLayout::default()
        };
        assert_eq!(
            layout.group_of("local/Storefront", "Storefront", &[]),
            Some(0)
        );
        layout.assign("local/Storefront", "");
        assert_eq!(layout.group_of("local/Storefront", "Storefront", &[]), None);
        layout.assign("local/Storefront", "A");
        assert!(layout.ungrouped.is_empty());
        assert_eq!(
            layout.group_of("local/Storefront", "Storefront", &[]),
            Some(0)
        );
    }

    #[test]
    fn move_member_materialises_order() {
        let mut layout = ProjectLayout {
            groups: vec![group("A", &["local/x"], &["y"])],
            ..ProjectLayout::default()
        };
        let view = vec!["local/x".to_string(), "dev/y".to_string()];
        layout.move_member(0, &view, "dev/y", -1);
        assert_eq!(
            layout.groups[0].members,
            vec!["dev/y".to_string(), "local/x".to_string()]
        );
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn reorder_keeps_offline_and_hidden_members() {
        // gpu/w7 is on an offline machine and local/w3 is filtered out, so
        // neither is in the view; both must survive a reorder in place.
        let mut layout = ProjectLayout {
            groups: vec![group(
                "A",
                &["local/w1:a", "gpu/w7:off", "local/w2:b", "local/w3:hid"],
                &[],
            )],
            ..ProjectLayout::default()
        };
        let view = strings(&["local/w1:a", "local/w2:b"]);
        layout.move_member(0, &view, "local/w2:b", -1);
        assert_eq!(
            layout.groups[0].members,
            strings(&["local/w2:b", "local/w1:a", "gpu/w7:off", "local/w3:hid"])
        );
        // Drag-drop from another group: lands before the target, keeps the rest.
        layout.groups.push(group("B", &["dev/w5:c"], &[]));
        layout.place("dev/w5:c", "A", Some("local/w1:a"), &view);
        assert_eq!(
            layout.groups[0].members,
            strings(&[
                "local/w2:b",
                "dev/w5:c",
                "local/w1:a",
                "gpu/w7:off",
                "local/w3:hid"
            ])
        );
        assert!(layout.groups[1].members.is_empty());
        // Dropped last, and dropped just before itself (no change).
        layout.place("local/w2:b", "A", None, &view);
        let last = layout.groups[0].members.clone();
        assert_eq!(last.last().map(String::as_str), Some("local/w2:b"));
        layout.place("local/w1:a", "A", Some("local/w1:a"), &view);
        assert_eq!(layout.groups[0].members, last);
    }

    #[test]
    fn rule_only_project_reorders_into_explicit_members() {
        let mut layout = ProjectLayout {
            groups: vec![group("A", &[], &["store"])],
            ..ProjectLayout::default()
        };
        let view = strings(&["local/w1:store-a", "dev/w2:store-b", "dev/w3:store-c"]);
        layout.move_member(0, &view, "dev/w3:store-c", -1);
        assert_eq!(
            layout.groups[0].members,
            strings(&["local/w1:store-a", "dev/w3:store-c", "dev/w2:store-b"])
        );
        let mut layout = ProjectLayout {
            groups: vec![group("A", &[], &["store"])],
            ..ProjectLayout::default()
        };
        layout.move_member(0, &view, "local/w1:store-a", 1);
        assert_eq!(
            layout.groups[0].members,
            strings(&["dev/w2:store-b", "local/w1:store-a", "dev/w3:store-c"])
        );
        // Appending a newcomer keeps the rule matches above it.
        layout.place("x/w9:new", "A", None, &view);
        assert_eq!(
            layout.groups[0].members.last().map(String::as_str),
            Some("x/w9:new")
        );
        assert_eq!(layout.groups[0].members.len(), 4);
    }

    #[test]
    fn drag_needs_a_small_move_and_clears() {
        set_press(Some(RowPress {
            endpoint_id: super::super::ClientEndpointId::Local,
            workspace_id: "w1".into(),
            pane_id: None,
            start: (5, 5),
            dragging: None,
        }));
        assert!(!drag_to((6, 5)));
        assert!(drag_to((7, 5)));
        assert!(drag_to((6, 5)));
        assert!(clear_press());
        assert!(press().is_none());
        assert!(!clear_press());
    }

    #[test]
    fn layout_round_trips_toml() {
        let layout = ProjectLayout {
            hidden: vec!["dev/old".into()],
            groups: vec![group("A", &["local/x"], &["cal"])],
            ..ProjectLayout::default()
        };
        let text = toml::to_string_pretty(&layout).unwrap();
        assert!(text.contains("[[group]]"));
        assert_eq!(toml::from_str::<ProjectLayout>(&text).unwrap(), layout);
    }
}
