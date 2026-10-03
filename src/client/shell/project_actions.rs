//! drovr fork: menus, clicks, prompts and keys for sidebar projects.
//! The layout model lives in `projects.rs`; this file only turns UI gestures
//! into layout changes (and, for agents on the active machine, stock methods).

use std::borrow::Cow;

use crossterm::event::{MouseButton, MouseEventKind};

use super::projects;
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

/// Recent documents shown in a workspace's "Documents…" submenu.
const DOCUMENTS_MENU_LIMIT: usize = 10;

/// The "Documents…" submenu of a workspace, or None when it has no recent
/// documents. Recent documents are recorded by `drovr doc open` on the machine
/// that hosts the workspace, so only workspaces on this machine have them here.
fn documents_target(
    endpoint_id: &ClientEndpointId,
    workspace_id: &str,
    pane_id: Option<String>,
) -> Option<Box<ClientContextMenuTarget>> {
    if !endpoint_id.is_local() || cfg!(test) {
        return None;
    }
    let store = crate::doc_view::open::load_store(&crate::doc_view::open::store_path());
    let docs = store
        .recent(&crate::doc_view::open::workspace_key(workspace_id))
        .iter()
        .take(DOCUMENTS_MENU_LIMIT)
        .cloned()
        .collect::<Vec<_>>();
    (!docs.is_empty()).then(|| {
        Box::new(ClientContextMenuTarget::Documents {
            workspace_id: workspace_id.to_owned(),
            pane_id,
            docs,
        })
    })
}

/// Runs `drovr doc open` against this machine's server for a workspace (and
/// the agent pane the menu was opened on), in the background.
pub(crate) fn open_local_document(
    workspace_id: String,
    pane_id: Option<String>,
    path: std::path::PathBuf,
) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut command = std::process::Command::new(exe);
    command
        .args(["doc", "open", "--focus"])
        .arg(&path)
        .env("HERDR_WORKSPACE_ID", workspace_id)
        .env(crate::api::SOCKET_PATH_ENV_VAR, crate::api::socket_path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    match pane_id {
        Some(pane_id) => command.env("HERDR_PANE_ID", pane_id),
        None => command.env_remove("HERDR_PANE_ID"),
    };
    std::thread::spawn(move || match command.output() {
        Ok(output) if !output.status.success() => tracing::warn!(
            path = %path.display(),
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "drovr doc open failed"
        ),
        Err(err) => tracing::warn!(err = %err, "cannot run drovr doc open"),
        _ => {}
    });
}

/// Opens `$EDITOR` on `path` in a pane split below `pane_id` on the local
/// server; the pane closes when the editor exits. The path reaches the
/// editor through the pane's environment, never as typed shell text.
pub(crate) fn open_local_editor(pane_id: String, path: std::path::PathBuf) {
    const PATH_ENV: &str = "DROVR_INBOX_NOTE";
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let socket = crate::api::socket_path();
    std::thread::spawn(move || {
        let run = |args: &[&str]| {
            std::process::Command::new(&exe)
                .args(args)
                .env(crate::api::SOCKET_PATH_ENV_VAR, &socket)
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
        };
        let env = format!("{PATH_ENV}={}", path.display());
        let split = run(&[
            "pane",
            "split",
            &pane_id,
            "--direction",
            "down",
            "--focus",
            "--env",
            &env,
        ]);
        let new_pane = split
            .ok()
            .and_then(|output| serde_json::from_slice::<serde_json::Value>(&output.stdout).ok())
            .and_then(|value| {
                value["result"]["pane"]["pane_id"]
                    .as_str()
                    .map(String::from)
            });
        let Some(new_pane) = new_pane else {
            tracing::warn!("cannot open a pane for $EDITOR");
            return;
        };
        // `sh` splits `$EDITOR` into words (`code --wait`); zsh and fish
        // would look for one command named after the whole value.
        let command = format!("sh -c 'exec ${{EDITOR:-vi}} \"$1\"' sh \"${PATH_ENV}\"; exit");
        if let Err(err) = run(&["pane", "run", &new_pane, &command]) {
            tracing::warn!(err = %err, "cannot run $EDITOR");
        }
    });
}

/// Read-only usage lines for a project's menu (from the drovr usage hook).
fn usage_items(name: &str) -> Vec<ClientContextMenuItem> {
    let layout = projects::layout();
    let group = (name != projects::OTHER).then_some(name);
    let line = |label: &str, days: i64| {
        let usage = projects::project_usage(&layout, group, days);
        (usage[4] > 0).then(|| {
            item(
                format!(
                    "{label}: {} · {} tokens",
                    projects::format_minutes(usage[4]),
                    projects::format_tokens(usage[0] + usage[1] + usage[3])
                ),
                Action::Info,
            )
        })
    };
    [line("Today", 1), line("Last 7 days", 7)]
        .into_iter()
        .flatten()
        .collect()
}

pub(super) fn project_menu_items(target: &ClientContextMenuTarget) -> Vec<ClientContextMenuItem> {
    match target {
        ClientContextMenuTarget::ProjectWorkspace {
            grouped,
            hidden,
            groups,
            base,
            documents,
            task,
            ..
        } => {
            let mut items = base
                .as_deref()
                .map(super::context_menu::items_for)
                .unwrap_or_default();
            if let Some(task) = task {
                items.push(item(format!("Task {task}"), Action::TaskOpen));
            }
            if documents.is_some() {
                items.push(item("Documents…", Action::Documents));
            }
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
            let usage = usage_items(name);
            if name == projects::OTHER {
                let mut items = vec![
                    item("New agent…", Action::ProjectNewAgent),
                    item("New workspace…", Action::ProjectNewWorkspace),
                    collapse,
                ];
                items.extend(usage);
                return items;
            }
            let mut items = vec![
                item("Tasks", Action::TaskOpen),
                item("New agent here…", Action::ProjectNewAgent),
                item("New workspace here…", Action::ProjectNewWorkspace),
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
            ];
            items.extend(usage);
            items
        }
        ClientContextMenuTarget::NewWorkspacePicker { machines, .. } => machines
            .iter()
            .enumerate()
            .map(|(index, (_, label))| item(format!("on {label}"), Action::NewOnMachine(index)))
            .collect(),
        ClientContextMenuTarget::Documents { docs, .. } => docs
            .iter()
            .enumerate()
            .map(|(index, doc)| item(doc.label(), Action::OpenDocument(index)))
            .collect(),
        ClientContextMenuTarget::Agent {
            unread_key,
            seq,
            status,
            workspace_key,
            hidden,
            active,
            groups,
            grouped,
            documents,
            task,
            ..
        } => {
            let presence = projects::layout().presence(unread_key, *seq, *status);
            let mut items = vec![item("Go to", Action::AgentFocus)];
            if let Some(task) = task {
                items.push(item(format!("Task {task}"), Action::TaskOpen));
            }
            items.push(if presence.needs_attention() {
                item("Mark inactive", Action::AgentMarkInactive)
            } else {
                item("Mark unread", Action::AgentMarkUnread)
            });
            items.push(item(
                if projects::layout().is_kept(unread_key) {
                    "Stop keeping active"
                } else {
                    "Keep active"
                },
                Action::AgentToggleKeep,
            ));
            if *active {
                items.push(item("Rename pane…", Action::AgentRename));
            }
            if documents.is_some() {
                items.push(item("Documents…", Action::Documents));
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
        ClientContextMenuTarget::InboxItem { waiting, muted, .. } => {
            let mut items = vec![item("Jump", Action::AgentFocus)];
            if !waiting {
                items.push(item("Dismiss", Action::InboxDismiss));
            }
            items.push(item(
                "Dismiss all done in project",
                Action::InboxDismissDoneInProject,
            ));
            items.push(item("Snooze 1 h", Action::InboxSnooze));
            items.push(item(
                if *muted {
                    "Unmute workspace"
                } else {
                    "Mute workspace"
                },
                Action::InboxMute,
            ));
            items
        }
        _ => Vec::new(),
    }
}

/// A merged workspace menu whose picked index falls inside the stock part:
/// hand back the stock menu so the upstream activation code runs unchanged.
#[allow(clippy::result_large_err)] // both arms are the same menu, moved not copied
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
    pub(super) fn endpoint_by_id(
        &self,
        endpoint_id: &ClientEndpointId,
    ) -> Option<&ClientShellEndpoint> {
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
            projects::workspace_key(endpoint, workspace),
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
        let task = self.workspace_task(endpoint_id, workspace_id);
        Some(ClientContextMenuTarget::ProjectWorkspace {
            grouped: self.workspace_group(endpoint_id, workspace_id).is_some(),
            hidden: layout.is_hidden(&key),
            groups: Self::group_names(),
            key,
            base,
            documents: documents_target(endpoint_id, workspace_id, None),
            task,
        })
    }

    /// The task linked to a workspace: an open one first, else the newest
    /// closed one (tasks.md 7.4).
    fn workspace_task(&self, endpoint_id: &ClientEndpointId, workspace_id: &str) -> Option<String> {
        let endpoint = self.endpoint_by_id(endpoint_id)?;
        let filter = crate::tasks::TaskFilter {
            workspace_key: Some(format!(
                "{}/{workspace_id}:",
                projects::machine_key(endpoint)
            )),
            ..Default::default()
        };
        let cards = crate::tasks::read_store(|store| store.list(&filter)).ok()?;
        cards
            .iter()
            .find(|card| !card.task.status.is_closed())
            .or_else(|| cards.first())
            .map(|card| card.task.display_id.clone())
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
        let task = crate::tasks::read_store(|store| store.task_for_pane(&unread_key))
            .ok()
            .flatten()
            .map(|task| task.display_id);
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
            documents: documents_target(&endpoint_id, &workspace_id, Some(pane_id.clone())),
            workspace_key,
            active: endpoint_id == self.active_endpoint_id,
            endpoint_id,
            pane_id,
            task,
        })
    }

    pub(super) fn open_menu(&mut self, target: ClientContextMenuTarget, x: u16, y: u16) {
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

    fn row_at(&self, point: (u16, u16)) -> Option<super::drovr_sidebar::RowHit> {
        self.hits
            .drovr_rows
            .iter()
            .find(|hit| super::contains(hit.rect, point))
            .cloned()
    }

    /// Right-click in the drovr sidebar: headers, agent rows, workspace rows.
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
    /// a drag moves the workspace to where it was dropped (see
    /// `drovr_sidebar::drop_slots`): a section header appends to that project
    /// (or Other), a row places it just before that row's workspace. Dropping
    /// outside the sidebar changes nothing.
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
        let Some(target) = super::drovr_sidebar::drop_slot_at(&self.hits.drovr_drops, point)
            .map(|slot| slot.target.clone())
        else {
            return true;
        };
        let Some((key, _, _)) =
            self.workspace_label_and_paths(&press.endpoint_id, &press.workspace_id)
        else {
            return true;
        };
        let name = if target.section == projects::OTHER {
            // Already in Other: moving within it has no order to keep.
            if self
                .workspace_group(&press.endpoint_id, &press.workspace_id)
                .is_none()
            {
                return true;
            }
            String::new()
        } else {
            target.section
        };
        let resolved = projects::resolved_members(&projects::layout(), &self.endpoints, &name);
        projects::update(|layout| {
            layout.place(&key, &name, target.before.as_deref(), &resolved);
            if target.header {
                if let Some(group) = layout.group_mut(&name) {
                    group.collapsed = false;
                }
            }
        });
        true
    }

    /// Drop a row press the release of which never arrived: any mouse event
    /// other than a left drag/up (or a scroll) means the button is up by now.
    pub(super) fn drop_stale_row_press(&mut self, kind: MouseEventKind) -> bool {
        let live = matches!(
            kind,
            MouseEventKind::Drag(MouseButton::Left)
                | MouseEventKind::Up(MouseButton::Left)
                | MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        );
        !live && projects::clear_press()
    }

    /// Left-click on a project header toggles it; the header toggles switch
    /// the view (detailed/compact/structured) and the filter (all/active).
    pub(super) fn handle_project_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        let drovr_sidebar = self.hits.drovr_view_toggle.width > 0;
        if drovr_sidebar && super::contains(self.hits.new_workspace, point) {
            let project = self.focused_project();
            self.open_new_workspace_picker(project, false);
            outcome.repaint = true;
            return true;
        }
        if let Some(key) = self
            .hits
            .drovr_rail
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
            .map(|(_, key)| key.clone())
        {
            if let Some((endpoint_id, target)) = super::drovr_sidebar::project_target(
                &self.endpoints,
                &self.active_endpoint_id,
                &key,
            ) {
                self.focus_or_activate(endpoint_id, target, outcome);
            }
            outcome.repaint = true;
            return true;
        }
        if super::contains(self.hits.drovr_attention, point) {
            self.focus_next_attention_agent(outcome);
            return true;
        }
        let inbox = self
            .hits
            .drovr_inbox
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
            .map(|(_, filter)| filter.clone());
        if inbox.is_some_and(|filter| self.open_inbox(filter, outcome)) {
            return true;
        }
        if super::contains(self.hits.drovr_view_toggle, point) {
            projects::update(projects::ProjectLayout::cycle_view);
        } else if super::contains(self.hits.drovr_filter_toggle, point) {
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

    /// Opens the inbox panel filtered to `filter` (a sidebar glyph or
    /// section count was clicked).
    pub(super) fn open_inbox(
        &mut self,
        filter: super::agent_signal::InboxFilter,
        outcome: &mut ClientShellInput,
    ) -> bool {
        self.open_inbox_filtered(filter, outcome);
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
        (x, y): (u16, u16),
        outcome: &mut ClientShellInput,
    ) {
        match target {
            ClientContextMenuTarget::ProjectWorkspace {
                documents: Some(documents),
                ..
            }
            | ClientContextMenuTarget::Agent {
                documents: Some(documents),
                ..
            } if action == Action::Documents => self.open_menu(*documents, x, y),
            ClientContextMenuTarget::ProjectWorkspace {
                task: Some(task), ..
            }
            | ClientContextMenuTarget::Agent {
                task: Some(task), ..
            } if action == Action::TaskOpen => self.open_task_view(&task, outcome),
            ClientContextMenuTarget::Documents {
                workspace_id,
                pane_id,
                docs,
            } => {
                if let Action::OpenDocument(index) = action {
                    if let Some(doc) = docs.into_iter().nth(index) {
                        open_local_document(workspace_id, pane_id, doc.path);
                    }
                }
            }
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
                    projects::update(|layout| layout.toggle_hidden(&key))
                }
                Action::ProjectMoveUp | Action::ProjectMoveDown => {
                    let layout = projects::layout();
                    let delta = if action == Action::ProjectMoveUp {
                        -1
                    } else {
                        1
                    };
                    if let Some((group, view)) =
                        projects::group_members_in_view(&layout, &self.endpoints, &key)
                    {
                        projects::update(|layout| layout.move_member(group, &view, &key, delta));
                    }
                }
                _ => {}
            },
            ClientContextMenuTarget::NewWorkspacePicker {
                machines,
                project,
                run_agent,
            } => {
                if let Action::NewOnMachine(index) = action {
                    if let Some((endpoint_id, _)) = machines.get(index) {
                        self.prompt_new_workspace(endpoint_id.clone(), project, run_agent);
                    }
                }
            }
            ClientContextMenuTarget::Project { name, .. } => match action {
                Action::TaskOpen => self.open_tasks_panel(name, outcome),
                Action::ProjectNewAgent | Action::ProjectNewWorkspace => self
                    .open_new_workspace_picker(
                        (name != projects::OTHER).then_some(name),
                        action == Action::ProjectNewAgent,
                    ),
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
                Action::AgentToggleKeep => {
                    projects::update(|layout| layout.toggle_kept(&unread_key))
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
                        projects::update(|layout| layout.toggle_hidden(&key))
                    }
                }
                _ => {}
            },
            ClientContextMenuTarget::InboxItem { key, .. } => {
                self.activate_inbox_menu(key, action, outcome);
            }
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
                let mut renamed = false;
                if !text.is_empty() && text != name {
                    projects::update(|layout| {
                        if let Some(group) = layout.group_mut(&name) {
                            group.name = text.clone();
                            renamed = true;
                        }
                    })
                }
                if renamed {
                    self.rename_task_project(&name, &text);
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
                    // Same order as the numbers drawn in the sidebar.
                    let target = super::aggregate_navigation::online_agent_targets(
                        &self.endpoints,
                        &self.active_endpoint_id,
                        crate::config::AgentPanelSortConfig::Spaces,
                    )
                    .into_iter()
                    .nth(number - 1);
                    if let Some(target) = target {
                        self.focus_or_activate(
                            target.endpoint_id,
                            ClientEndpointFocusTarget::Pane(target.pane_id),
                            outcome,
                        );
                    }
                }
            }
            ClientRenameTarget::NewWorkspaceOn {
                endpoint_id,
                project,
                run_agent,
                cwd,
            } => self.create_workspace_on(endpoint_id, project, run_agent, cwd, text, outcome),
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
            .drovr_rows
            .iter()
            .find(|hit| hit.endpoint_id == endpoint_id && hit.workspace_id == workspace_id)
            .map_or((2, 2), |hit| (hit.rect.x + 2, hit.rect.y));
        if let Some(target) = self.workspace_target(&endpoint_id, &workspace_id, x, y) {
            self.open_menu(target, x, y);
        }
    }

    /// True when no longer jump number could start with `n` (e.g. 4 of 25).
    pub(super) fn jump_number_is_final(&self, n: usize) -> bool {
        let total = super::aggregate_navigation::online_agent_targets(
            &self.endpoints,
            &self.active_endpoint_id,
            crate::config::AgentPanelSortConfig::Spaces,
        )
        .len();
        n > 0 && n.saturating_mul(10) > total
    }

    pub(super) fn open_jump_agent_prompt(&mut self) {
        self.prompt("jump to agent #", "", ClientRenameTarget::JumpAgent);
    }

    pub(super) fn toggle_peek(&mut self) {
        projects::toggle_peek();
    }

    pub(super) fn toggle_show_hidden_workspaces(&mut self) {
        projects::update(|layout| layout.show_hidden = !layout.show_hidden);
    }

    /// Machines you can create a workspace on, active one first.
    pub(super) fn online_machines(&self) -> Vec<(ClientEndpointId, String)> {
        let mut machines = self
            .endpoints
            .iter()
            .filter(|endpoint| {
                endpoint.endpoint_id.is_local() || endpoint.status == ClientEndpointStatus::Online
            })
            .map(|endpoint| (endpoint.endpoint_id.clone(), endpoint.label.clone()))
            .collect::<Vec<_>>();
        machines.sort_by_key(|(endpoint_id, _)| endpoint_id != &self.active_endpoint_id);
        machines
    }

    /// Project of the focused workspace on the active machine.
    fn focused_project(&self) -> Option<String> {
        let workspace_id = self.snapshot.as_deref()?.focused_workspace_id.clone()?;
        self.workspace_group(&self.active_endpoint_id, &workspace_id)
    }

    /// prefix+alt+c, the footer "new" button and the project menu land here.
    pub(super) fn open_new_workspace_picker(&mut self, project: Option<String>, run_agent: bool) {
        let machines = self.online_machines();
        if machines.len() <= 1 {
            if let Some((endpoint_id, _)) = machines.into_iter().next() {
                self.prompt_new_workspace(endpoint_id, project, run_agent);
            }
            return;
        }
        let (x, y) = if self.hits.new_workspace.height > 0 {
            (
                self.hits.new_workspace.x + 1,
                self.hits
                    .new_workspace
                    .y
                    .saturating_sub(machines.len() as u16 + 2),
            )
        } else {
            (2, 2)
        };
        self.open_menu(
            ClientContextMenuTarget::NewWorkspacePicker {
                machines,
                project,
                run_agent,
            },
            x,
            y,
        );
    }

    pub(super) fn open_new_workspace_for_focus(&mut self) {
        let project = self.focused_project();
        self.open_new_workspace_picker(project, false);
    }

    fn prompt_new_workspace(
        &mut self,
        endpoint_id: ClientEndpointId,
        project: Option<String>,
        run_agent: bool,
    ) {
        // Start in the project's folder on that machine when it has one there.
        let cwd = project
            .as_deref()
            .and_then(|project| self.project_cwd(&endpoint_id, project));
        let initial = project.clone().unwrap_or_default();
        self.prompt(
            if run_agent {
                "new agent: workspace name"
            } else {
                "new workspace name"
            },
            &initial,
            ClientRenameTarget::NewWorkspaceOn {
                endpoint_id,
                project,
                run_agent,
                cwd,
            },
        );
    }

    /// The project's `new_workspace_cwd` on that machine: the folder of one
    /// of its workspaces there.
    pub(super) fn project_cwd(
        &self,
        endpoint_id: &ClientEndpointId,
        project: &str,
    ) -> Option<String> {
        let layout = projects::layout();
        let (sections, _) = projects::sections(&layout, &self.endpoints);
        sections
            .into_iter()
            .filter(|section| layout.groups[section.group].name == project)
            .flat_map(|section| section.members)
            .find_map(|member| {
                let endpoint = &self.endpoints[member.endpoint];
                (&endpoint.endpoint_id == endpoint_id).then_some(())?;
                let workspace = endpoint.snapshot.as_deref()?.workspaces.get(member.index)?;
                (!workspace.new_workspace_cwd.is_empty())
                    .then(|| workspace.new_workspace_cwd.clone())
            })
    }

    /// Renames the tasks project after its section (tasks.md 2.3). The store
    /// file is not created for this: without tasks there is nothing to rename.
    fn rename_task_project(&mut self, old: &str, new: &str) {
        match crate::tasks::read_store(|store| store.rename_project(old, new)) {
            Ok(()) => {}
            Err(crate::tasks::StoreError::Refused(refusal)) => {
                self.push_task_notice(refusal.message);
            }
            Err(error) => tracing::debug!(%error, "cannot rename the tasks project"),
        }
    }

    /// Send one API request to a specific machine (not just the active one).
    pub(super) fn endpoint_request(
        &mut self,
        endpoint_id: &ClientEndpointId,
        method: crate::api::schema::Method,
    ) -> Option<ClientShellAction> {
        let boot_id = self
            .endpoint_by_id(endpoint_id)?
            .snapshot
            .as_deref()?
            .boot_id
            .clone();
        let id = format!("drovr:{}", self.next_request_id);
        self.next_request_id = self.next_request_id.saturating_add(1);
        Some(ClientShellAction::Endpoint {
            endpoint_id: endpoint_id.clone(),
            boot_id,
            request: Box::new(crate::api::schema::Request { id, method }),
        })
    }

    fn create_workspace_on(
        &mut self,
        endpoint_id: ClientEndpointId,
        project: Option<String>,
        run_agent: bool,
        cwd: Option<String>,
        text: &str,
        outcome: &mut ClientShellInput,
    ) {
        let label = Some(text.trim())
            .filter(|label| !label.is_empty())
            .map(str::to_owned)
            .or_else(|| project.clone())
            .unwrap_or_else(|| "workspace".to_owned());
        let Some(known) = self.request_workspace(
            &endpoint_id,
            project.as_deref(),
            cwd,
            &label,
            Default::default(),
            outcome,
        ) else {
            return;
        };
        projects::set_launch(Some(projects::PendingLaunch {
            endpoint_id,
            label,
            known,
            command: run_agent.then(|| "cc".to_owned()),
            since: std::time::Instant::now(),
        }));
    }

    /// Sends workspace.create (focus true) to the machine, assigns
    /// `machine/label` to `project`, activates the endpoint when it is not
    /// the active one. Returns the workspace ids known before the request.
    /// The active machine gets the request on the client connection; another
    /// one through its API route, since the client connection only carries
    /// requests for the active endpoint.
    pub(super) fn request_workspace(
        &mut self,
        endpoint_id: &ClientEndpointId,
        project: Option<&str>,
        cwd: Option<String>,
        label: &str,
        env: std::collections::HashMap<String, String>,
        outcome: &mut ClientShellInput,
    ) -> Option<std::collections::HashSet<String>> {
        let endpoint = self.endpoint_by_id(endpoint_id)?;
        let machine = projects::machine_key(endpoint);
        let known = endpoint
            .snapshot
            .as_deref()
            .map(|snapshot| {
                snapshot
                    .workspaces
                    .iter()
                    .map(|workspace| workspace.workspace_id.clone())
                    .collect()
            })
            .unwrap_or_default();
        let method = crate::api::schema::Method::WorkspaceCreate(
            crate::api::schema::WorkspaceCreateParams {
                source_workspace_id: None,
                cwd,
                focus: true,
                label: Some(label.to_owned()),
                env,
            },
        );
        if endpoint_id == &self.active_endpoint_id {
            let action = self.endpoint_request(endpoint_id, method)?;
            outcome.actions.push(action);
        } else if !self.send_task_api(&machine, vec![method], outcome) {
            return None;
        }
        if let Some(project) = project {
            let key = format!("{machine}/{label}");
            projects::update(|layout| layout.assign(&key, project));
        }
        if endpoint_id != &self.active_endpoint_id {
            outcome.actions.push(ClientShellAction::ActivateEndpoint {
                endpoint_id: endpoint_id.clone(),
                target: None,
            });
        }
        Some(known)
    }

    /// Periodic drovr work, from the client loop's 100 ms timer.
    pub(crate) fn tick_drovr(&mut self, outcome: &mut ClientShellInput) {
        outcome.actions.extend(self.tick_drovr_launch());
        for (endpoint_id, pane_id) in projects::take_focus_requests() {
            self.focus_or_activate(
                endpoint_id,
                ClientEndpointFocusTarget::Pane(pane_id),
                outcome,
            );
            outcome.repaint = true;
        }
        outcome.repaint |= projects::expire_peek();
        self.tick_inbox(outcome);
        self.tick_tasks(outcome);
        outcome.repaint |= super::drovr_sidebar::take_clock_tick();
    }

    /// Once the workspace created above shows up, type the agent command into
    /// its first pane (and stop waiting after a minute).
    fn tick_drovr_launch(&mut self) -> Vec<ClientShellAction> {
        let Some(launch) = projects::launch() else {
            return Vec::new();
        };
        if launch.since.elapsed().as_secs() > 60 {
            projects::set_launch(None);
            return Vec::new();
        }
        let pane_id = self
            .endpoint_by_id(&launch.endpoint_id)
            .and_then(|endpoint| {
                let snapshot = endpoint.snapshot.as_deref()?;
                let workspace = snapshot.workspaces.iter().find(|workspace| {
                    workspace.label == launch.label
                        && !launch.known.contains(&workspace.workspace_id)
                })?;
                snapshot
                    .panes
                    .iter()
                    .find(|pane| pane.workspace_id == workspace.workspace_id)
                    .map(|pane| pane.pane_id.clone())
            });
        let Some(pane_id) = pane_id else {
            return Vec::new();
        };
        projects::set_launch(None);
        let Some(command) = launch.command else {
            return Vec::new();
        };
        let mut actions = Vec::new();
        let text =
            crate::api::schema::Method::PaneSendText(crate::api::schema::PaneSendTextParams {
                pane_id: pane_id.clone(),
                text: command,
            });
        let enter =
            crate::api::schema::Method::PaneSendKeys(crate::api::schema::PaneSendKeysParams {
                pane_id,
                keys: vec!["Enter".to_owned()],
            });
        actions.extend(self.endpoint_request(&launch.endpoint_id, text));
        actions.extend(self.endpoint_request(&launch.endpoint_id, enter));
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw_input::RawInputEvent;
    use crossterm::event::{KeyCode, KeyModifiers, MouseEvent};

    fn mouse(kind: MouseEventKind) -> RawInputEvent {
        RawInputEvent::Mouse(MouseEvent {
            kind,
            column: 70,
            row: 3,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn press() {
        projects::set_press(Some(projects::RowPress {
            endpoint_id: ClientEndpointId::Local,
            workspace_id: "w1".into(),
            pane_id: None,
            start: (2, 3),
            dragging: Some((2, 6)),
        }));
    }

    #[test]
    fn stale_row_press_is_cleared() {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(
            &crate::config::Config::default(),
        ));
        // A lost mouse-up: the next plain move, a new press (any button) or a
        // focus loss means the button is up, so the old press must not linger.
        let events: [fn() -> RawInputEvent; 5] = [
            || mouse(MouseEventKind::Moved),
            || mouse(MouseEventKind::Down(MouseButton::Left)),
            || mouse(MouseEventKind::Down(MouseButton::Right)),
            || RawInputEvent::OuterFocusLost,
            || {
                RawInputEvent::Key(crate::input::TerminalKey::new(
                    KeyCode::Esc,
                    KeyModifiers::NONE,
                ))
            },
        ];
        for (index, event) in events.iter().enumerate() {
            press();
            state.handle_raw_events(vec![event()]);
            assert!(projects::press().is_none(), "event {index}");
        }
        // A live drag survives drag and scroll events.
        press();
        state.handle_raw_events(vec![
            mouse(MouseEventKind::Drag(MouseButton::Left)),
            mouse(MouseEventKind::ScrollDown),
        ]);
        assert!(projects::press().is_some());
        projects::clear_press();
    }

    fn task_in(project: &str) -> String {
        crate::tasks::with_store(|store| {
            store.create_task(
                &crate::tasks::NewTask {
                    project: project.into(),
                    title: Some("Retry the sync job".into()),
                    ..Default::default()
                },
                &crate::tasks::Actor::Human,
            )
        })
        .expect("task")
        .display_id
    }

    fn group(name: &str) {
        projects::update(|layout| {
            layout.groups.retain(|group| group.name != name);
            layout.groups.push(projects::ProjectGroup {
                name: name.into(),
                ..Default::default()
            });
        });
    }

    #[test]
    fn renaming_a_section_renames_its_tasks_project() {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(
            &crate::config::Config::default(),
        ));
        group("Rename Acme");
        let id = task_in("Rename Acme");
        let mut outcome = ClientShellInput::default();
        state.save_project_prompt(
            ClientRenameTarget::ProjectRename {
                name: "Rename Acme".into(),
            },
            "Rename Acme Labs",
            &mut outcome,
        );
        let project = crate::tasks::with_store(|store| store.task_detail(&id))
            .expect("read")
            .expect("task")
            .project;
        assert_eq!(project.name, "Rename Acme Labs");
        // Onto a name another project row holds: the section is renamed, the
        // tasks stay, and a notice says why.
        task_in("Rename Beta");
        state.save_project_prompt(
            ClientRenameTarget::ProjectRename {
                name: "Rename Acme Labs".into(),
            },
            "Rename Beta",
            &mut outcome,
        );
        assert!(projects::layout()
            .groups
            .iter()
            .any(|group| group.name == "Rename Beta"));
        let notice = state.visible_endpoint_notice.as_ref().expect("notice");
        assert_eq!(notice.body, "a project named Rename Beta already exists");
        let project = crate::tasks::with_store(|store| store.task_detail(&id))
            .expect("read")
            .expect("task")
            .project;
        assert_eq!(project.name, "Rename Acme Labs");
    }

    #[test]
    fn project_and_workspace_menus_open_tasks() {
        let project = project_menu_items(&ClientContextMenuTarget::Project {
            name: "Acme".into(),
            pinned: false,
            collapsed: false,
        });
        assert_eq!(project[0].label, "Tasks");
        assert_eq!(project[0].action, Action::TaskOpen);
        let other = project_menu_items(&ClientContextMenuTarget::Project {
            name: projects::OTHER.into(),
            pinned: false,
            collapsed: false,
        });
        assert!(other.iter().all(|item| item.action != Action::TaskOpen));

        let mut state = ClientShellState::new(ClientShellConfig::from_config(
            &crate::config::Config::default(),
        ));
        state.set_snapshot(Box::new(super::super::tests::snapshot()));
        let target = state
            .workspace_target(&ClientEndpointId::Local, "ws_1", 2, 2)
            .expect("target");
        assert!(project_menu_items(&target)
            .iter()
            .all(|item| item.action != Action::TaskOpen));
        let id = task_in("Menu Acme");
        crate::tasks::with_store(|store| {
            store.link_workspace(&id, Some("local/ws_1:client-shell"))
        })
        .expect("link");
        let target = state
            .workspace_target(&ClientEndpointId::Local, "ws_1", 2, 2)
            .expect("target");
        let items = project_menu_items(&target);
        let task = items
            .iter()
            .find(|item| item.action == Action::TaskOpen)
            .expect("task item");
        assert_eq!(task.label, format!("Task {id}"));
    }
}
