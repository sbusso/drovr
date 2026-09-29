//! andreconde fork: menus, clicks, prompts and keys for sidebar projects.
//! The layout model lives in `projects.rs`; this file only turns UI gestures
//! into layout changes (and, for agents on the active machine, stock methods).

use std::borrow::Cow;

use super::projects::{self, ProjectLayout};
use super::*;

type Action = ClientContextMenuAction;

fn item(label: impl Into<Cow<'static, str>>, action: Action) -> ClientContextMenuItem {
    ClientContextMenuItem {
        label: label.into(),
        action,
    }
}

fn move_items(items: &mut Vec<ClientContextMenuItem>, groups: &[String], grouped: bool) {
    for (index, group) in groups.iter().enumerate() {
        items.push(item(format!("→ {group}"), Action::ProjectAssignTo(index)));
    }
    if grouped {
        items.push(item("→ Other", Action::ProjectRemove));
    }
    items.push(item("→ New project…", Action::ProjectAssignNew));
}

pub(super) fn project_menu_items(target: &ClientContextMenuTarget) -> Vec<ClientContextMenuItem> {
    match target {
        ClientContextMenuTarget::ProjectWorkspace {
            grouped,
            hidden,
            groups,
            base,
            ..
        } => {
            let mut items = base
                .as_deref()
                .map(super::context_menu::items_for)
                .unwrap_or_default();
            move_items(&mut items, groups, *grouped);
            if *grouped {
                items.push(item("Move up", Action::ProjectMoveUp));
                items.push(item("Move down", Action::ProjectMoveDown));
            }
            items.push(item(
                if *hidden { "Unhide" } else { "Hide" },
                Action::ProjectToggleHidden,
            ));
            items
        }
        ClientContextMenuTarget::Project {
            name,
            pinned,
            collapsed,
            ..
        } => {
            let collapse = item(
                if *collapsed { "Expand" } else { "Collapse" },
                Action::ProjectToggleCollapse,
            );
            if name == projects::OTHER {
                return vec![collapse];
            }
            vec![
                collapse,
                item(
                    if *pinned { "Unpin" } else { "Pin to top" },
                    Action::ProjectTogglePin,
                ),
                item("Move up", Action::ProjectMoveUp),
                item("Move down", Action::ProjectMoveDown),
                item("Rename…", Action::ProjectRename),
                item("Auto-match rules…", Action::ProjectRules),
                item("Delete project", Action::ProjectDelete),
            ]
        }
        ClientContextMenuTarget::Agent {
            unread_key,
            seq,
            status,
            workspace_key,
            hidden,
            active,
            groups,
            grouped,
            ..
        } => {
            let presence = projects::layout().presence(unread_key, *seq, *status);
            let mut items = vec![item("Go to", Action::AgentFocus)];
            items.push(if presence.needs_attention() {
                item("Mark inactive", Action::AgentMarkInactive)
            } else {
                item("Mark unread", Action::AgentMarkUnread)
            });
            if *active {
                items.push(item("Rename pane…", Action::AgentRename));
            }
            if workspace_key.is_some() {
                move_items(&mut items, groups, *grouped);
                items.push(item(
                    if *hidden {
                        "Unhide workspace"
                    } else {
                        "Hide workspace"
                    },
                    Action::ProjectToggleHidden,
                ));
            }
            items
        }
        _ => Vec::new(),
    }
}

/// A merged workspace menu whose picked index falls inside the stock part:
/// hand back the stock menu so the upstream activation code runs unchanged.
pub(super) fn split_project_menu(
    menu: ClientContextMenuOverlay,
    index: usize,
) -> Result<ClientContextMenuOverlay, ClientContextMenuOverlay> {
    match menu.target {
        ClientContextMenuTarget::ProjectWorkspace {
            base: Some(base), ..
        } if index < super::context_menu::items_for(&base).len() => Ok(ClientContextMenuOverlay {
            target: *base,
            x: menu.x,
            y: menu.y,
            highlighted: index,
        }),
        _ => Err(menu),
    }
}

impl ClientShellState {
    fn endpoint_by_id(&self, endpoint_id: &ClientEndpointId) -> Option<&ClientShellEndpoint> {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
    }

    fn workspace_label_and_paths(
        &self,
        endpoint_id: &ClientEndpointId,
        workspace_id: &str,
    ) -> Option<(String, String, Vec<String>)> {
        let endpoint = self.endpoint_by_id(endpoint_id)?;
        let snapshot = endpoint.snapshot.as_deref()?;
        let workspace = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == workspace_id)?;
        Some((
            projects::workspace_key(endpoint, &workspace.label),
            workspace.label.clone(),
            projects::workspace_paths(snapshot, workspace),
        ))
    }

    /// Project a workspace currently belongs to (explicitly or by rule).
    fn workspace_group(
        &self,
        endpoint_id: &ClientEndpointId,
        workspace_id: &str,
    ) -> Option<String> {
        let (key, label, paths) = self.workspace_label_and_paths(endpoint_id, workspace_id)?;
        let layout = projects::layout();
        layout
            .group_of(&key, &label, &paths)
            .map(|index| layout.groups[index].name.clone())
    }

    fn group_names() -> Vec<String> {
        let layout = projects::layout();
        layout
            .display_order()
            .into_iter()
            .map(|index| layout.groups[index].name.clone())
            .collect()
    }

    fn workspace_target(
        &mut self,
        endpoint_id: &ClientEndpointId,
        workspace_id: &str,
        x: u16,
        y: u16,
    ) -> Option<ClientContextMenuTarget> {
        let (key, _, _) = self.workspace_label_and_paths(endpoint_id, workspace_id)?;
        let base = if endpoint_id == &self.active_endpoint_id {
            self.open_workspace_context_menu(workspace_id.to_owned(), x, y);
            match self.overlay.take() {
                Some(ClientShellOverlay::ContextMenu(menu)) => Some(Box::new(menu.target)),
                other => {
                    self.overlay = other;
                    None
                }
            }
        } else {
            None
        };
        let layout = projects::layout();
        Some(ClientContextMenuTarget::ProjectWorkspace {
            grouped: self.workspace_group(endpoint_id, workspace_id).is_some(),
            hidden: layout.is_hidden(&key),
            groups: Self::group_names(),
            key,
            base,
        })
    }

    fn agent_target(
        &self,
        endpoint_id: ClientEndpointId,
        pane_id: String,
    ) -> Option<ClientContextMenuTarget> {
        let endpoint = self.endpoint_by_id(&endpoint_id)?;
        let agent = endpoint
            .snapshot
            .as_deref()?
            .agents
            .iter()
            .find(|agent| agent.pane_id == pane_id)?;
        let (seq, status, workspace_id) = (
            agent.state_change_seq,
            agent.agent_status,
            agent.workspace_id.clone(),
        );
        let unread_key = projects::agent_key(endpoint, &pane_id);
        let workspace_key = self
            .workspace_label_and_paths(&endpoint_id, &workspace_id)
            .map(|(key, _, _)| key);
        Some(ClientContextMenuTarget::Agent {
            unread_key,
            seq,
            status,
            hidden: workspace_key
                .as_deref()
                .is_some_and(|key| projects::layout().is_hidden(key)),
            grouped: self.workspace_group(&endpoint_id, &workspace_id).is_some(),
            groups: Self::group_names(),
            workspace_key,
            active: endpoint_id == self.active_endpoint_id,
            endpoint_id,
            pane_id,
        })
    }

    fn open_menu(&mut self, target: ClientContextMenuTarget, x: u16, y: u16) {
        self.overlay = Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
            target,
            x,
            y,
            highlighted: 0,
        }));
    }

    fn header_at(&self, point: (u16, u16)) -> Option<String> {
        self.hits
            .projects
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
            .map(|(_, key)| key.clone())
    }

    fn row_at(&self, point: (u16, u16)) -> Option<super::sheprd_sidebar::RowHit> {
        self.hits
            .sheprd_rows
            .iter()
            .find(|hit| super::contains(hit.rect, point))
            .cloned()
    }

    /// Right-click in the sheprd sidebar: headers, agent rows, workspace rows.
    pub(super) fn open_project_context_menu_at(
        &mut self,
        point: (u16, u16),
        x: u16,
        y: u16,
    ) -> bool {
        if let Some(key) = self.header_at(point) {
            let layout = projects::layout();
            let target = if key == projects::OTHER {
                ClientContextMenuTarget::Project {
                    name: key,
                    pinned: false,
                    collapsed: layout.other_collapsed,
                }
            } else {
                let Some(group) = layout.groups.iter().find(|group| group.name == key) else {
                    return false;
                };
                ClientContextMenuTarget::Project {
                    pinned: group.pinned,
                    collapsed: group.collapsed,
                    name: key,
                }
            };
            self.open_menu(target, x, y);
            return true;
        }
        if self.sidebar_collapsed {
            return false;
        }
        let Some(row) = self.row_at(point) else {
            return false;
        };
        let target = match row.pane_id {
            Some(pane_id) => self.agent_target(row.endpoint_id, pane_id),
            None => self.workspace_target(&row.endpoint_id, &row.workspace_id, x, y),
        };
        match target {
            Some(target) => {
                self.open_menu(target, x, y);
                true
            }
            None => false,
        }
    }

    /// Left-button down on a sidebar row: remember it; click vs drag is decided
    /// when the button comes up.
    pub(super) fn begin_row_press(&mut self, point: (u16, u16)) -> bool {
        let Some(row) = self.row_at(point) else {
            return false;
        };
        projects::set_press(Some(projects::RowPress {
            endpoint_id: row.endpoint_id,
            workspace_id: row.workspace_id,
            pane_id: row.pane_id,
            start: point,
            dragging: None,
        }));
        true
    }

    /// Left-button up after a row press. A click focuses the agent/workspace;
    /// a drag onto a header moves the workspace to that project (or Other), and
    /// a drag onto another row moves it into that row's project, just above it.
    pub(super) fn finish_row_press(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(press) = projects::press() else {
            return false;
        };
        projects::set_press(None);
        self.chrome_drag = None;
        self.workspace_press = None;
        outcome.repaint = true;
        if press.dragging.is_none() {
            let target = match press.pane_id {
                Some(pane_id) => ClientEndpointFocusTarget::Pane(pane_id),
                None => ClientEndpointFocusTarget::Workspace(press.workspace_id),
            };
            self.focus_or_activate(press.endpoint_id, target, outcome);
            return true;
        }
        let Some((key, _, _)) =
            self.workspace_label_and_paths(&press.endpoint_id, &press.workspace_id)
        else {
            return true;
        };
        if let Some(header) = self.header_at(point) {
            let name = if header == projects::OTHER {
                String::new()
            } else {
                header
            };
            projects::update(|layout| {
                layout.assign(&key, &name);
                if let Some(group) = layout.group_mut(&name) {
                    group.collapsed = false;
                }
            });
            return true;
        }
        let Some(target) = self.row_at(point) else {
            return true;
        };
        if target.endpoint_id == press.endpoint_id && target.workspace_id == press.workspace_id {
            return true;
        }
        let target_key = self
            .workspace_label_and_paths(&target.endpoint_id, &target.workspace_id)
            .map(|(key, _, _)| key);
        let group = self.workspace_group(&target.endpoint_id, &target.workspace_id);
        let endpoints = &self.endpoints;
        projects::update(|layout| {
            layout.assign(&key, group.as_deref().unwrap_or(""));
            let (Some(group), Some(target_key)) = (group.as_deref(), target_key) else {
                return;
            };
            let mut view = projects::group_members_in_view(layout, endpoints, &key);
            view.retain(|member| member != &key);
            let position = view
                .iter()
                .position(|member| member == &target_key)
                .unwrap_or(view.len());
            view.insert(position, key.clone());
            if let Some(group) = layout.group_mut(group) {
                group.members = view;
            }
        });
        true
    }

    /// Left-click on a project header toggles it; the header toggles switch
    /// the view (detailed/compact) and the filter (all/active).
    pub(super) fn handle_project_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        if super::contains(self.hits.sheprd_view_toggle, point) {
            projects::update(|layout| layout.compact = !layout.compact);
        } else if super::contains(self.hits.sheprd_filter_toggle, point) {
            projects::update(|layout| layout.active_only = !layout.active_only);
            self.workspace_scroll = 0;
        } else if let Some(key) = self.header_at(point) {
            projects::update(|layout| {
                if key == projects::OTHER {
                    layout.other_collapsed = !layout.other_collapsed;
                } else if let Some(group) = layout.group_mut(&key) {
                    group.collapsed = !group.collapsed;
                }
            });
        } else {
            return false;
        }
        outcome.repaint = true;
        true
    }

    /// prefix+u: focus the next agent (in sidebar order) that is blocked,
    /// finished-unseen or marked unread.
    pub(super) fn focus_next_attention_agent(&mut self, outcome: &mut ClientShellInput) {
        let layout = projects::layout();
        let rows = super::aggregate_navigation::aggregate_agent_rows(
            &self.endpoints,
            &self.active_endpoint_id,
            crate::config::AgentPanelSortConfig::Spaces,
        );
        let targets = rows
            .iter()
            .filter(|row| !row.endpoint.stale())
            .map(|row| {
                let endpoint = &self.endpoints[row.endpoint.endpoint_index];
                let presence = layout.presence(
                    &projects::agent_key(endpoint, &row.agent.pane_id),
                    row.agent.state_change_seq,
                    row.agent.agent_status,
                );
                (
                    endpoint.endpoint_id.clone(),
                    row.agent.pane_id.clone(),
                    presence.needs_attention(),
                    row.agent.focused && endpoint.endpoint_id == self.active_endpoint_id,
                )
            })
            .collect::<Vec<_>>();
        let current = targets.iter().position(|(_, _, _, focused)| *focused);
        let start = current.map_or(0, |index| index + 1);
        let next = (0..targets.len())
            .map(|offset| (start + offset) % targets.len().max(1))
            .find(|index| targets[*index].2 && Some(*index) != current);
        match next {
            Some(index) => {
                let (endpoint_id, pane_id, _, _) = targets[index].clone();
                self.focus_or_activate(
                    endpoint_id,
                    ClientEndpointFocusTarget::Pane(pane_id),
                    outcome,
                );
            }
            None => {
                self.receive_endpoint_unavailable("No agent needs you right now".into());
            }
        }
        outcome.repaint = true;
    }

    fn prompt(&mut self, title: &'static str, initial: &str, target: ClientRenameTarget) {
        self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
            title,
            input: TextEditor::new(initial, false),
            target,
        }));
    }

    pub(super) fn activate_project_action(
        &mut self,
        target: ClientContextMenuTarget,
        action: Action,
        outcome: &mut ClientShellInput,
    ) {
        match target {
            ClientContextMenuTarget::ProjectWorkspace { key, groups, .. } => match action {
                Action::ProjectAssignTo(index) => {
                    if let Some(name) = groups.get(index) {
                        projects::update(|layout| layout.assign(&key, name));
                    }
                }
                Action::ProjectAssignNew => self.prompt(
                    "move to project",
                    "",
                    ClientRenameTarget::ProjectAssign { key },
                ),
                Action::ProjectRemove => projects::update(|layout| layout.assign(&key, "")),
                Action::ProjectToggleHidden => {
                    projects::update(|layout| ProjectLayout::toggle(&mut layout.hidden, &key))
                }
                Action::ProjectMoveUp | Action::ProjectMoveDown => {
                    let layout = projects::layout();
                    let view = projects::group_members_in_view(&layout, &self.endpoints, &key);
                    let delta = if action == Action::ProjectMoveUp {
                        -1
                    } else {
                        1
                    };
                    projects::update(|layout| layout.move_member(&view, &key, delta));
                }
                _ => {}
            },
            ClientContextMenuTarget::Project { name, .. } => match action {
                Action::ProjectToggleCollapse => projects::update(|layout| {
                    if name == projects::OTHER {
                        layout.other_collapsed = !layout.other_collapsed;
                    } else if let Some(group) = layout.group_mut(&name) {
                        group.collapsed = !group.collapsed;
                    }
                }),
                Action::ProjectTogglePin => projects::update(|layout| {
                    if let Some(group) = layout.group_mut(&name) {
                        group.pinned = !group.pinned;
                    }
                }),
                Action::ProjectMoveUp => projects::update(|layout| layout.move_group(&name, -1)),
                Action::ProjectMoveDown => projects::update(|layout| layout.move_group(&name, 1)),
                Action::ProjectRename => {
                    let initial = name.clone();
                    self.prompt(
                        "rename project",
                        &initial,
                        ClientRenameTarget::ProjectRename { name },
                    )
                }
                Action::ProjectRules => {
                    let rules = projects::layout()
                        .groups
                        .iter()
                        .find(|group| group.name == name)
                        .map(|group| group.rules.join(", "))
                        .unwrap_or_default();
                    self.prompt(
                        "auto-match workspace names or folders (comma separated)",
                        &rules,
                        ClientRenameTarget::ProjectRules { name },
                    )
                }
                Action::ProjectDelete => {
                    projects::update(|layout| layout.groups.retain(|group| group.name != name))
                }
                _ => {}
            },
            ClientContextMenuTarget::Agent {
                endpoint_id,
                pane_id,
                unread_key,
                seq,
                workspace_key,
                groups,
                ..
            } => match action {
                Action::AgentFocus => {
                    self.focus_or_activate(
                        endpoint_id,
                        ClientEndpointFocusTarget::Pane(pane_id),
                        outcome,
                    );
                }
                Action::AgentMarkUnread => {
                    projects::update(|layout| layout.mark(&unread_key, seq, true))
                }
                Action::AgentMarkInactive => {
                    projects::update(|layout| layout.mark(&unread_key, seq, false))
                }
                Action::AgentRename => {
                    let label = self
                        .snapshot
                        .as_deref()
                        .and_then(|snapshot| {
                            snapshot.panes.iter().find(|pane| pane.pane_id == pane_id)
                        })
                        .and_then(|pane| pane.label.clone())
                        .unwrap_or_default();
                    self.prompt("rename pane", &label, ClientRenameTarget::Pane { pane_id })
                }
                Action::ProjectAssignTo(index) => {
                    if let (Some(key), Some(name)) = (workspace_key, groups.get(index)) {
                        projects::update(|layout| layout.assign(&key, name));
                    }
                }
                Action::ProjectRemove => {
                    if let Some(key) = workspace_key {
                        projects::update(|layout| layout.assign(&key, ""));
                    }
                }
                Action::ProjectAssignNew => {
                    if let Some(key) = workspace_key {
                        self.prompt(
                            "move to project",
                            "",
                            ClientRenameTarget::ProjectAssign { key },
                        )
                    }
                }
                Action::ProjectToggleHidden => {
                    if let Some(key) = workspace_key {
                        projects::update(|layout| ProjectLayout::toggle(&mut layout.hidden, &key))
                    }
                }
                _ => {}
            },
            _ => {}
        }
        outcome.repaint = true;
    }

    /// Answer a fork prompt. Returns the target back when it is a stock one.
    pub(super) fn save_project_prompt(
        &mut self,
        target: ClientRenameTarget,
        text: &str,
        outcome: &mut ClientShellInput,
    ) -> Option<ClientRenameTarget> {
        match target {
            ClientRenameTarget::ProjectAssign { key } => {
                projects::update(|layout| layout.assign(&key, text))
            }
            ClientRenameTarget::ProjectRename { name } => {
                let text = text.trim().to_owned();
                if !text.is_empty() {
                    projects::update(|layout| {
                        if let Some(group) = layout.group_mut(&name) {
                            group.name = text;
                        }
                    })
                }
            }
            ClientRenameTarget::ProjectRules { name } => {
                let rules = text
                    .split(',')
                    .map(str::trim)
                    .filter(|rule| !rule.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                projects::update(|layout| {
                    if let Some(group) = layout.group_mut(&name) {
                        group.rules = rules;
                    }
                })
            }
            ClientRenameTarget::JumpAgent => {
                if let Some(number) = text
                    .trim()
                    .trim_start_matches('#')
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                {
                    self.handle_endpoint_navigation(
                        crate::input::KeybindAction::FocusAgent(number - 1),
                        outcome,
                    );
                }
            }
            other => return Some(other),
        }
        outcome.repaint = true;
        None
    }

    /// Keyboard entry points (bound in keybindings as project_menu / jump_agent
    /// / toggle_hidden_workspaces).
    pub(super) fn open_focused_project_menu(&mut self) {
        let Some(workspace_id) = self
            .snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.focused_workspace_id.clone())
        else {
            return;
        };
        let endpoint_id = self.active_endpoint_id.clone();
        let (x, y) = self
            .hits
            .sheprd_rows
            .iter()
            .find(|hit| hit.endpoint_id == endpoint_id && hit.workspace_id == workspace_id)
            .map_or((2, 2), |hit| (hit.rect.x + 2, hit.rect.y));
        if let Some(target) = self.workspace_target(&endpoint_id, &workspace_id, x, y) {
            self.open_menu(target, x, y);
        }
    }

    pub(super) fn open_jump_agent_prompt(&mut self) {
        self.prompt("jump to agent #", "", ClientRenameTarget::JumpAgent);
    }

    pub(super) fn toggle_show_hidden_workspaces(&mut self) {
        projects::update(|layout| layout.show_hidden = !layout.show_hidden);
    }
}
