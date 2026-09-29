//! andreconde fork (sheprd): client-local project groups for the federated sidebar.
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
//! ```
//!
//! Kept in its own module behind a process-wide lock so the upstream render and
//! navigation signatures stay untouched (cheap rebases).

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{OnceLock, RwLock},
    time::{Instant, SystemTime},
};

use serde::{Deserialize, Serialize};

use super::{ClientEndpointStatus, ClientShellEndpoint};

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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) other_collapsed: bool,
    /// Show only agents that are working or need attention.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) active_only: bool,
    /// Agents marked inactive by hand, as `machine/pane@state_change_seq`: the
    /// mark lapses as soon as the agent changes state again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) dismissed: Vec<String>,
    /// Workspaces dragged to "Other": never auto-matched into a project.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) ungrouped: Vec<String>,
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

fn read_file() -> (ProjectLayout, Option<SystemTime>) {
    // Unit tests must never see (or depend on) the developer's real layout.
    if cfg!(test) {
        return (ProjectLayout::default(), None);
    }
    let path = path();
    let mtime = std::fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .ok();
    let layout = std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| toml::from_str(&content).ok())
        .unwrap_or_default();
    (layout, mtime)
}

fn store() -> &'static RwLock<Store> {
    static STORE: OnceLock<RwLock<Store>> = OnceLock::new();
    STORE.get_or_init(|| {
        let (layout, mtime) = read_file();
        RwLock::new(Store {
            layout,
            mtime,
            checked: Instant::now(),
            last_focused_agent: None,
        })
    })
}

/// Current layout; picks up hand edits to sidebar.toml at most once a second.
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
        let (layout, mtime) = read_file();
        guard.layout = layout;
        guard.mtime = mtime;
    }
    guard.layout.clone()
}

/// Apply a change and persist it.
pub(super) fn update(change: impl FnOnce(&mut ProjectLayout)) {
    let _ = layout();
    let store = store();
    let mut guard = store.write().unwrap_or_else(|e| e.into_inner());
    let before = guard.layout.clone();
    change(&mut guard.layout);
    if guard.layout == before {
        return;
    }
    if cfg!(test) {
        return;
    }
    let path = path();
    if let Ok(content) = toml::to_string_pretty(&guard.layout) {
        let header =
            "# herdr (andreconde fork) sidebar projects. Hand-editable; see projects.rs.\n";
        let tmp = path.with_extension("toml.tmp");
        if std::fs::write(&tmp, format!("{header}{content}")).is_ok()
            && std::fs::rename(&tmp, &path).is_ok()
        {
            guard.mtime = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .ok();
        }
    }
}

pub(super) fn machine_key(endpoint: &ClientShellEndpoint) -> String {
    endpoint.label.to_lowercase()
}

pub(super) fn workspace_key(endpoint: &ClientShellEndpoint, label: &str) -> String {
    format!("{}/{}", machine_key(endpoint), label)
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
        self.hidden.iter().any(|hidden| hidden == key)
    }

    pub(super) fn is_unread(&self, key: &str) -> bool {
        self.unread.iter().any(|unread| unread == key)
    }

    pub(super) fn explicit_group(&self, key: &str) -> Option<usize> {
        self.groups
            .iter()
            .position(|group| group.members.iter().any(|member| member == key))
    }

    /// Group for a workspace: explicit membership wins, then the first rule that
    /// matches the workspace name or any folder its panes run in.
    pub(super) fn group_of(&self, key: &str, label: &str, paths: &[String]) -> Option<usize> {
        if self.ungrouped.iter().any(|ungrouped| ungrouped == key) {
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
            .position(|member| member == key)
            .unwrap_or(usize::MAX)
    }

    /// Move a workspace into `group_name`; an empty name moves it to Other.
    pub(super) fn assign(&mut self, key: &str, group_name: &str) {
        for group in &mut self.groups {
            group.members.retain(|member| member != key);
        }
        self.ungrouped.retain(|ungrouped| ungrouped != key);
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

    pub(super) fn toggle(list: &mut Vec<String>, key: &str) {
        if let Some(index) = list.iter().position(|item| item == key) {
            list.remove(index);
        } else {
            list.push(key.to_owned());
        }
    }

    /// Move a workspace one step within its group (materialising rule matches
    /// into explicit members so the order sticks).
    pub(super) fn move_member(&mut self, members_in_view: &[String], key: &str, delta: isize) {
        let Some(position) = members_in_view.iter().position(|member| member == key) else {
            return;
        };
        let target = position as isize + delta;
        if target < 0 || target as usize >= members_in_view.len() {
            return;
        }
        let mut members = members_in_view.to_vec();
        members.swap(position, target as usize);
        if let Some(group) = self.explicit_group(key).or_else(|| {
            self.groups.iter().position(|group| {
                members_in_view
                    .iter()
                    .any(|member| group.members.contains(member))
            })
        }) {
            self.groups[group].members = members;
        }
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
            let key = workspace_key(endpoint, &workspace.label);
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

/// Visible member keys of the group that owns `key`, in display order.
pub(super) fn group_members_in_view(
    layout: &ProjectLayout,
    endpoints: &[ClientShellEndpoint],
    key: &str,
) -> Vec<String> {
    let (sections, _) = sections(layout, endpoints);
    sections
        .into_iter()
        .find(|section| section.members.iter().any(|member| member.key == key))
        .map(|section| {
            section
                .members
                .into_iter()
                .map(|member| member.key)
                .collect()
        })
        .unwrap_or_default()
}

/// Worst (most attention-worthy) status among a section's members.
pub(super) fn section_status(
    section: &ProjectSection,
    endpoints: &[ClientShellEndpoint],
) -> crate::api::schema::AgentStatus {
    section
        .members
        .iter()
        .filter_map(|member| {
            let endpoint = &endpoints[member.endpoint];
            if endpoint.status != ClientEndpointStatus::Online {
                return None;
            }
            endpoint
                .snapshot
                .as_deref()?
                .workspaces
                .get(member.index)
                .map(|workspace| workspace.agent_status)
        })
        .max_by_key(|status| super::status_priority(*status))
        .unwrap_or(crate::api::schema::AgentStatus::Unknown)
}

/// Sort key that puts agents in project order; ungrouped agents keep their
/// original relative order after every project. `None` = hidden, drop it.
pub(super) fn agent_rank(
    layout: &ProjectLayout,
    endpoint: &ClientShellEndpoint,
    workspace_label: &str,
    paths: &[String],
) -> Option<(usize, usize)> {
    let key = workspace_key(endpoint, workspace_label);
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
        let dismissed = self
            .dismissed
            .iter()
            .any(|entry| *entry == Self::dismissed_key(key, seq));
        match status {
            AgentStatus::Working => Presence::Working,
            AgentStatus::Blocked if !dismissed => Presence::Blocked,
            AgentStatus::Done if !dismissed => Presence::Done,
            _ => Presence::Idle,
        }
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

/// A press on a sheprd sidebar row, kept until the button comes up so the
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

pub(super) fn drag_to(point: (u16, u16)) -> bool {
    let mut guard = press_store().lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_mut() {
        Some(press) if press.start != point || press.dragging.is_some() => {
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
        layout.move_member(&view, "dev/y", -1);
        assert_eq!(
            layout.groups[0].members,
            vec!["dev/y".to_string(), "local/x".to_string()]
        );
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
