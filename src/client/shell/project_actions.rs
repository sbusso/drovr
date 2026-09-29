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

fn show_hidden_item(show_hidden: bool) -> ClientContextMenuItem {
    item(
        if show_hidden {
            "Conceal hidden workspaces"
        } else {
            "Show hidden workspaces"
        },
        Action::ProjectShowHidden,
    )
}

pub(super) fn project_menu_items(target: &ClientContextMenuTarget) -> Vec<ClientContextMenuItem> {
    match target {
        ClientContextMenuTarget::ProjectWorkspace {
            key,
            explicit,
            grouped,
            hidden,
            show_hidden,
            groups,
            base,
        } => {
            let mut items = base
                .as_deref()
                .map(super::context_menu::items_for)
                .unwrap_or_default();
            let current = key.as_str();
            for (index, group) in groups.iter().enumerate() {
                items.push(item(format!("→ {group}"), Action::ProjectAssignTo(index)));
            }
            items.push(item("→ New project…", Action::ProjectAssignNew));
            if *explicit {
                items.push(item("Remove from project", Action::ProjectRemove));
            }
            if *grouped {
                items.push(item("Move up", Action::ProjectMoveUp));
                items.push(item("Move down", Action::ProjectMoveDown));
            }
            items.push(item(
                if *hidden { "Unhide" } else { "Hide" },
                Action::ProjectToggleHidden,
            ));
            items.push(show_hidden_item(*show_hidden));
            let _ = current;
            items
        }
        ClientContextMenuTarget::Project {
            pinned,
            collapsed,
            show_hidden,
            ..
        } => vec![
            item(
                if *collapsed { "Expand" } else { "Collapse" },
                Action::ProjectToggleCollapse,
            ),
            item(
                if *pinned { "Unpin" } else { "Pin to top" },
                Action::ProjectTogglePin,
            ),
            item("Move up", Action::ProjectMoveUp),
            item("Move down", Action::ProjectMoveDown),
            item("Rename…", Action::ProjectRename),
            item("Auto-match rules…", Action::ProjectRules),
            show_hidden_item(*show_hidden),
            item("Delete project", Action::ProjectDelete),
        ],
        ClientContextMenuTarget::Agent {
            unread_key,
            workspace_key,
            hidden,
            active,
            ..
        } => {
            let unread = projects::layout().is_unread(unread_key);
            let mut items = vec![
                item("Go to", Action::AgentFocus),
                item(
                    if unread { "Mark read" } else { "Mark unread" },
                    Action::AgentToggleUnread,
                ),
            ];
            if *active {
                items.push(item("Rename pane…", Action::AgentRename));
            }
            if workspace_key.is_some() {
                items.push(item("Move workspace to project…", Action::ProjectAssignNew));
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

    fn workspace_key_for(
        &self,
        endpoint_id: &ClientEndpointId,
        workspace_id: &str,
    ) -> Option<String> {
        let endpoint = self.endpoint_by_id(endpoint_id)?;
        let workspace = endpoint
            .snapshot
            .as_deref()?
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == workspace_id)?;
        Some(projects::workspace_key(endpoint, &workspace.label))
    }

    fn workspace_target(
        &mut self,
        endpoint_id: &ClientEndpointId,
        workspace_id: &str,
        x: u16,
        y: u16,
    ) -> Option<ClientContextMenuTarget> {
        let key = self.workspace_key_for(endpoint_id, workspace_id)?;
        let label = key
            .split_once('/')
            .map_or(key.as_str(), |(_, label)| label)
            .to_owned();
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
            explicit: layout.explicit_group(&key).is_some(),
            grouped: layout.group_of(&key, &label).is_some(),
            hidden: layout.is_hidden(&key),
            show_hidden: layout.show_hidden,
            groups: layout
                .display_order()
                .into_iter()
                .map(|index| layout.groups[index].name.clone())
                .collect(),
            key,
            base,
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

    /// Right-click in the federated sidebar: project headers, workspaces on any
    /// machine and agent rows. Returns true when a menu opened.
    pub(super) fn open_project_context_menu_at(
        &mut self,
        point: (u16, u16),
        x: u16,
        y: u16,
    ) -> bool {
        if let Some(name) = self
            .hits
            .projects
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
            .map(|(_, name)| name.clone())
        {
            let layout = projects::layout();
            let Some(group) = layout.groups.iter().find(|group| group.name == name) else {
                return false;
            };
            let target = ClientContextMenuTarget::Project {
                pinned: group.pinned,
                collapsed: group.collapsed,
                show_hidden: layout.show_hidden,
                name,
            };
            self.open_menu(target, x, y);
            return true;
        }
        if self.sidebar_collapsed {
            return false;
        }
        if let Some((endpoint_id, workspace_id)) = self
            .hits
            .workspaces
            .iter()
            .find(|hit| super::contains(hit.rect, point))
            .map(|hit| (hit.endpoint_id.clone(), hit.workspace_id.clone()))
        {
            if let Some(target) = self.workspace_target(&endpoint_id, &workspace_id, x, y) {
                self.open_menu(target, x, y);
                return true;
            }
            return false;
        }
        if let Some((endpoint_id, pane_id)) = self
            .hits
            .endpoint_agents
            .iter()
            .find(|(rect, _, _)| super::contains(*rect, point))
            .map(|(_, endpoint_id, pane_id)| (endpoint_id.clone(), pane_id.clone()))
        {
            let Some(endpoint) = self.endpoint_by_id(&endpoint_id) else {
                return false;
            };
            let workspace_key = endpoint.snapshot.as_deref().and_then(|snapshot| {
                let agent = snapshot
                    .agents
                    .iter()
                    .find(|agent| agent.pane_id == pane_id)?;
                let workspace = snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.workspace_id == agent.workspace_id)?;
                Some(projects::workspace_key(endpoint, &workspace.label))
            });
            let target = ClientContextMenuTarget::Agent {
                unread_key: projects::agent_key(endpoint, &pane_id),
                hidden: workspace_key
                    .as_deref()
                    .is_some_and(|key| projects::layout().is_hidden(key)),
                workspace_key,
                active: endpoint_id == self.active_endpoint_id,
                endpoint_id,
                pane_id,
            };
            self.open_menu(target, x, y);
            return true;
        }
        false
    }

    /// Left-click on a project header toggles it.
    pub(super) fn handle_project_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(name) = self
            .hits
            .projects
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
            .map(|(_, name)| name.clone())
        else {
            return false;
        };
        projects::update(|layout| {
            if let Some(group) = layout.group_mut(&name) {
                group.collapsed = !group.collapsed;
            }
        });
        outcome.repaint = true;
        true
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
                Action::ProjectShowHidden => {
                    projects::update(|layout| layout.show_hidden = !layout.show_hidden)
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
                    if let Some(group) = layout.group_mut(&name) {
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
                Action::ProjectShowHidden => {
                    projects::update(|layout| layout.show_hidden = !layout.show_hidden)
                }
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
                        "auto-match workspace names (comma separated)",
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
                workspace_key,
                ..
            } => match action {
                Action::AgentFocus => {
                    self.focus_or_activate(
                        endpoint_id,
                        ClientEndpointFocusTarget::Pane(pane_id),
                        outcome,
                    );
                }
                Action::AgentToggleUnread => projects::update(|layout| {
                    ProjectLayout::toggle(&mut layout.unread, &unread_key)
                }),
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
            .workspaces
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
