//! drovr fork: start-task and the panel's links to agent panes
//! (docs/design/tasks.md, sections 5.1 and 5.4).
//!
//! A start picks a machine, writes the task's context file there, creates a
//! workspace in the project's section with `DROVR_TASK*` in its environment,
//! types the agent command into its root pane, opens the attempt, and sends
//! the first prompt once the agent shows (or after 5 s). Launches are kept in
//! `TaskRuntime.launches`, several at once, each matched by its own label and
//! the workspace ids known before its request.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use super::projects;
use super::task_ingest::{task_files, FileScript};
use super::tasks_panel::TaskMenu;
use super::*;
use crate::api::schema::{AgentPromptParams, Method, PaneSendKeysParams, PaneSendTextParams};
use crate::tasks::{self, Actor, CheckState, Decision, EntryKind, NewAttempt, TaskDetail};

/// A workspace that has not appeared after this long is given up.
const APPEAR_WITHIN: Duration = Duration::from_secs(60);
/// The probe and the context file must be done within this.
const PREPARE_WITHIN: Duration = Duration::from_secs(90);
/// The first prompt goes out this long after typing the command at the
/// latest, even when no agent was detected.
const PROMPT_AFTER: Duration = Duration::from_secs(5);
/// The command typed into the root pane (the user's Claude alias).
const AGENT_COMMAND: &str = "cc";
const HARNESS: &str = "claude";
/// Workspace labels are cut to this many characters.
const LABEL_MAX: usize = 40;

/// Where a launch is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LaunchStage {
    /// Waiting for the machine's `drovr task proto` answer.
    Probe,
    /// The context file job is queued or running.
    Context,
    /// Ready to send workspace.create.
    Create,
    /// Waiting for the workspace and its root pane.
    Wait,
    /// The command was typed; the first prompt is next.
    Prompt,
}

#[derive(Debug)]
pub(super) struct TaskLaunch {
    pub(super) id: u64,
    pub(super) display_id: String,
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) machine: String,
    pub(super) project: String,
    pub(super) name: String,
    pub(super) label: String,
    pub(super) known: HashSet<String>,
    pub(super) since: Instant,
    /// Set once the workspace and its root pane are found.
    pub(super) pane: Option<(
        String, /*workspace key*/
        String, /*pane key*/
        String, /*pane id*/
    )>,
    pub(super) typed_at: Option<Instant>,
    pub(super) stage: LaunchStage,
    /// The context file's absolute path on the machine.
    pub(super) context_path: Option<String>,
}

/// `{display_id} {name}`, cut to [`LABEL_MAX`] characters.
pub(super) fn launch_label(display_id: &str, name: &str) -> String {
    format!("{display_id} {name}")
        .chars()
        .take(LABEL_MAX)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

/// The label of a decision's ruling: the choice label, or the free text.
pub(super) fn ruling_label(decision: &Decision) -> String {
    match (&decision.ruling_choice, &decision.ruling_text) {
        (Some(choice), _) => decision
            .choices
            .iter()
            .find(|c| &c.id == choice)
            .map_or_else(|| choice.clone(), |c| c.label.clone()),
        (None, Some(text)) => text.clone(),
        (None, None) => String::new(),
    }
}

/// A ruling is relayed into the pane only while no `decide --wait` CLI
/// polls for it: `wait_until` unset or past (section 2.5).
pub(super) fn relay_due(decision: &Decision, now: &str) -> bool {
    decision
        .wait_until
        .as_deref()
        .is_none_or(|until| until <= now)
}

/// What `decide --wait` prints for a ruling (section 6.1).
fn ruling_line(decision: &Decision) -> String {
    match (&decision.ruling_choice, &decision.ruling_text) {
        (Some(choice), _) => format!("ruled {choice}: {}", ruling_label(decision)),
        (None, Some(text)) => format!("ruled text: {text}"),
        (None, None) => "ruled".into(),
    }
}

/// Section 5.4's relay of a ruling (the panel formats its own relays the
/// same way).
pub(super) fn ruling_relay(id: &str, label: &str) -> String {
    format!("Decision on {id}: {label}")
}

/// A multi-line text as one Markdown list item.
fn list_item(text: &str) -> String {
    text.trim().lines().collect::<Vec<_>>().join("\n  ")
}

/// The context file of a task (section 5.1, step 3); sections without
/// content are left out.
pub(super) fn context_text(detail: &TaskDetail) -> String {
    let task = &detail.task;
    let id = &task.display_id;
    let mut text = format!("# {id} {}\n", task.name());
    let mut meta = vec![format!("Status: {}", task.status.as_str())];
    if let Some(kind) = task.kind {
        meta.push(format!("Kind: {}", super::tasks_panel::kind_name(kind)));
    }
    meta.push(format!(
        "Priority: {}",
        super::tasks_panel::priority_name(task.priority)
    ));
    meta.push(format!("Project: {}", detail.project.name));
    text.push_str(&meta.join(" · "));
    text.push('\n');
    if !task.body.trim().is_empty() {
        text.push_str(&format!("\n## Task\n{}\n", task.body.trim()));
    }
    if !detail.criteria.is_empty() {
        text.push_str("\n## Acceptance criteria\n");
        for criterion in &detail.criteria {
            let mark = if criterion.state == CheckState::Passed {
                "x"
            } else {
                " "
            };
            text.push_str(&format!(
                "- [{mark}] {}. {}",
                criterion.position,
                list_item(&criterion.text)
            ));
            if criterion.state == CheckState::Failed {
                text.push_str(" (failed)");
            }
            if let Some(cmd) = &criterion.check_cmd {
                text.push_str(&format!("   (check: `{cmd}`)"));
            }
            text.push('\n');
        }
    }
    let pinned: Vec<_> = detail.entries.iter().filter(|e| e.pinned).collect();
    if !pinned.is_empty() {
        text.push_str("\n## Pinned notes\n");
        for entry in pinned {
            text.push_str(&format!("- {}\n", list_item(&entry.body)));
        }
    }
    // Human entries after the start of the newest attempt (all of them when
    // the task never ran).
    let since = detail.attempts.first().map(|a| a.started_at.as_str());
    let said: Vec<_> = detail
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::Human)
        .filter(|e| since.is_none_or(|since| e.created_at.as_str() > since))
        .collect();
    if !said.is_empty() {
        text.push_str("\n## Said since the last attempt\n");
        for entry in said {
            text.push_str(&format!("- {}: {}\n", entry.author, list_item(&entry.body)));
        }
    }
    if let Some(decision) = detail
        .decision
        .as_ref()
        .filter(|d| d.state == crate::tasks::DecisionState::Open)
    {
        let choices = decision
            .choices
            .iter()
            .map(|c| {
                let rec = if c.recommended { " (recommended)" } else { "" };
                format!("{} = {}{rec}", c.id, c.label)
            })
            .collect::<Vec<_>>()
            .join("; ");
        text.push_str(&format!(
            "\n## Open decision\n{}: {choices}\n",
            decision.title
        ));
    }
    text.push_str(&format!(
        "\n## How to report\nUse `drovr task` (skill drovr-tasks): note, check, verify, artifact,\ndecide, done. Your task id is {id}; commands without an id use it.\n"
    ));
    text
}

/// The workspace environment of a task pane (section 5.1, step 4).
pub(super) fn launch_env(display_id: &str, local: bool) -> HashMap<String, String> {
    let mut env = HashMap::from([
        ("DROVR_TASK".to_owned(), display_id.to_owned()),
        ("DROVR_AGENT".to_owned(), HARNESS.to_owned()),
    ]);
    if local {
        let mut path = tasks::TaskStore::default_path();
        if path.is_relative() {
            if let Ok(cwd) = std::env::current_dir() {
                path = cwd.join(path);
            }
        }
        env.insert("DROVR_TASK_MODE".into(), "db".into());
        env.insert("DROVR_TASKS_DB".into(), path.to_string_lossy().into_owned());
    } else {
        env.insert("DROVR_TASK_MODE".into(), "outbox".into());
    }
    env
}

impl ClientShellState {
    /// Machines a task can start on, active one first: online, and not
    /// Windows (the outbox and the context file need a POSIX shell; a
    /// Windows remote fails the probe).
    fn task_machines(&self) -> Vec<(ClientEndpointId, String)> {
        self.online_machines()
            .into_iter()
            .filter(|(endpoint_id, _)| !(endpoint_id.is_local() && cfg!(windows)))
            .collect()
    }

    /// Opens the machine menu for a start (section 5.1, step 1).
    pub(super) fn launch_task(
        &mut self,
        display_id: &str,
        at: (u16, u16),
        outcome: &mut ClientShellInput,
    ) {
        let detail = match tasks::read_store(|store| store.task_detail(display_id)) {
            Ok(Some(detail)) => detail,
            Ok(None) => {
                self.push_task_notice(format!("no task {display_id}"));
                return;
            }
            Err(error) => {
                self.push_task_notice(error.to_string());
                return;
            }
        };
        let machines = self.task_machines();
        if machines.is_empty() {
            self.push_task_notice("no machine online".into());
            return;
        }
        let live = detail
            .attempts
            .iter()
            .find(|attempt| attempt.ended_at.is_none())
            .map(|attempt| attempt.machine.clone());
        // A live task asks first: the menu is the confirmation.
        if let Some(machine) = &live {
            self.push_task_notice(format!(
                "{display_id} runs on {machine}; pick a machine to start another"
            ));
        }
        if machines.len() == 1 && live.is_none() {
            if let Some((endpoint_id, _)) = machines.into_iter().next() {
                self.launch_task_on(display_id, endpoint_id, outcome);
            }
            return;
        }
        self.open_menu(
            ClientContextMenuTarget::Task {
                display_id: display_id.to_owned(),
                menu: TaskMenu::Machine { machines },
            },
            at.0,
            at.1,
        );
        outcome.repaint = true;
    }

    /// Starts the task on one machine (section 5.1, steps 2 to 6).
    pub(super) fn launch_task_on(
        &mut self,
        display_id: &str,
        endpoint_id: ClientEndpointId,
        outcome: &mut ClientShellInput,
    ) {
        let Some(endpoint) = self.endpoint_by_id(&endpoint_id) else {
            return;
        };
        let machine = projects::machine_key(endpoint);
        if self.endpoint_for_machine(&machine).is_none() {
            self.push_task_notice(format!("{machine} is offline"));
            return;
        }
        if self
            .task_rt
            .launches
            .iter()
            .any(|launch| launch.display_id == display_id)
        {
            self.push_task_notice(format!("{display_id} is already starting"));
            return;
        }
        let detail = match tasks::read_store(|store| store.task_detail(display_id)) {
            Ok(Some(detail)) => detail,
            _ => {
                self.push_task_notice(format!("no task {display_id}"));
                return;
            }
        };
        let local = endpoint_id.is_local();
        let stage = if local {
            LaunchStage::Context
        } else {
            match self.task_rt.probe.get(&machine) {
                Some(Some(_)) => LaunchStage::Context,
                Some(None) => {
                    self.push_task_notice(format!(
                        "{machine}: drovr there has no task command; see docs/design/tasks.md 6.6"
                    ));
                    // Ask again, so a start after installing drovr there works.
                    self.task_rt.probe.remove(&machine);
                    self.task_rt.queue_probe(&machine);
                    return;
                }
                None => {
                    self.task_rt.queue_probe(&machine);
                    LaunchStage::Probe
                }
            }
        };
        self.task_rt.next_launch += 1;
        let launch = TaskLaunch {
            id: self.task_rt.next_launch,
            display_id: detail.task.display_id.clone(),
            endpoint_id,
            machine,
            project: detail.project.name.clone(),
            name: detail.task.name().to_owned(),
            label: launch_label(&detail.task.display_id, detail.task.name()),
            known: HashSet::new(),
            since: Instant::now(),
            pane: None,
            typed_at: None,
            stage: stage.clone(),
            context_path: None,
        };
        if stage == LaunchStage::Context {
            self.queue_context(launch.id, &launch.machine, local, &detail);
        }
        self.task_rt.launches.push(launch);
        outcome.repaint = true;
    }

    /// Queues the context file (and the remote snapshot) of a launch.
    fn queue_context(&mut self, launch: u64, machine: &str, local: bool, detail: &TaskDetail) {
        let mut script = FileScript::default();
        task_files(&mut script, detail, local);
        self.task_rt.written.insert(
            detail.task.display_id.clone(),
            (machine.to_owned(), detail.task.version),
        );
        self.task_rt
            .queue_write(machine, script.finish(local), Some(launch));
    }

    /// Advances every pending launch; called from `tick_tasks`.
    pub(super) fn tick_task_launches(&mut self, outcome: &mut ClientShellInput) {
        let ids: Vec<u64> = self.task_rt.launches.iter().map(|l| l.id).collect();
        for id in ids {
            self.tick_launch(id, outcome);
        }
    }

    fn tick_launch(&mut self, id: u64, outcome: &mut ClientShellInput) {
        let Some(index) = self.task_rt.launches.iter().position(|l| l.id == id) else {
            return;
        };
        let launch = &self.task_rt.launches[index];
        let elapsed = launch.since.elapsed();
        match launch.stage.clone() {
            LaunchStage::Probe | LaunchStage::Context if elapsed >= PREPARE_WITHIN => {
                let launch = self.task_rt.launches.remove(index);
                self.push_task_notice(format!(
                    "{}: {} did not answer",
                    launch.display_id, launch.machine
                ));
            }
            LaunchStage::Probe => match self.task_rt.probe.get(&launch.machine).copied() {
                Some(Some(_)) => {
                    let display_id = launch.display_id.clone();
                    let Ok(Some(detail)) =
                        tasks::read_store(|store| store.task_detail(&display_id))
                    else {
                        self.task_rt.launches.remove(index);
                        return;
                    };
                    let launch = &mut self.task_rt.launches[index];
                    launch.stage = LaunchStage::Context;
                    let (id, machine, local) = (
                        launch.id,
                        launch.machine.clone(),
                        launch.endpoint_id.is_local(),
                    );
                    self.queue_context(id, &machine, local, &detail);
                }
                Some(None) => {
                    let launch = self.task_rt.launches.remove(index);
                    self.push_task_notice(format!(
                        "{}: drovr there has no task command; see docs/design/tasks.md 6.6",
                        launch.machine
                    ));
                }
                None => {
                    let machine = launch.machine.clone();
                    if !matches!(
                        self.task_rt.inflight.get(&machine),
                        Some(super::task_sync::Inflight::Probe)
                    ) {
                        self.task_rt.queue_probe(&machine);
                    }
                }
            },
            LaunchStage::Context => {}
            LaunchStage::Create => {
                let launch = &self.task_rt.launches[index];
                let (endpoint_id, project, label, display_id) = (
                    launch.endpoint_id.clone(),
                    launch.project.clone(),
                    launch.label.clone(),
                    launch.display_id.clone(),
                );
                let cwd = self.project_cwd(&endpoint_id, &project);
                let env = launch_env(&display_id, endpoint_id.is_local());
                match self.request_workspace(
                    &endpoint_id,
                    Some(&project),
                    cwd,
                    &label,
                    env,
                    outcome,
                ) {
                    Some(known) => {
                        let launch = &mut self.task_rt.launches[index];
                        launch.known = known;
                        launch.since = Instant::now();
                        launch.stage = LaunchStage::Wait;
                    }
                    None => {
                        let launch = self.task_rt.launches.remove(index);
                        self.push_task_notice(format!(
                            "{}: {} is offline",
                            launch.display_id, launch.machine
                        ));
                    }
                }
            }
            LaunchStage::Wait => {
                if elapsed >= APPEAR_WITHIN {
                    let launch = self.task_rt.launches.remove(index);
                    self.push_task_notice(format!(
                        "{}: workspace did not appear on {}",
                        launch.display_id, launch.machine
                    ));
                    return;
                }
                self.find_launch_pane(index, outcome);
            }
            LaunchStage::Prompt => {
                let (endpoint_id, pane_id) = match &launch.pane {
                    Some((_, _, pane_id)) => (launch.endpoint_id.clone(), pane_id.clone()),
                    None => return,
                };
                let detected = self
                    .endpoint_by_id(&endpoint_id)
                    .and_then(|endpoint| endpoint.snapshot.as_deref())
                    .is_some_and(|snapshot| {
                        snapshot
                            .agents
                            .iter()
                            .any(|agent| agent.pane_id == pane_id && agent.agent.is_some())
                    });
                let late = launch
                    .typed_at
                    .is_some_and(|at| at.elapsed() >= PROMPT_AFTER);
                if !detected && !late {
                    return;
                }
                let launch = self.task_rt.launches.remove(index);
                let path = launch
                    .context_path
                    .clone()
                    .unwrap_or_else(|| format!("tasks/{}.md", launch.display_id));
                let text = format!(
                    "Work on drovr task {}: {}. Read {path} first. Report with drovr task (skill drovr-tasks).",
                    launch.display_id, launch.name
                );
                self.send_task_api(
                    &launch.machine,
                    vec![Method::AgentPrompt(AgentPromptParams {
                        target: pane_id,
                        text,
                        wait: None,
                    })],
                    outcome,
                );
            }
        }
    }

    /// Wait stage: the new workspace (label match, id not known before) and
    /// its root pane; then the agent command and the attempt.
    fn find_launch_pane(&mut self, index: usize, outcome: &mut ClientShellInput) {
        let launch = &self.task_rt.launches[index];
        let Some(endpoint) = self.endpoint_by_id(&launch.endpoint_id) else {
            return;
        };
        let Some(snapshot) = endpoint.snapshot.as_deref() else {
            return;
        };
        let Some(workspace) = snapshot.workspaces.iter().find(|workspace| {
            workspace.label == launch.label && !launch.known.contains(&workspace.workspace_id)
        }) else {
            return;
        };
        let Some(pane) = snapshot
            .panes
            .iter()
            .find(|pane| pane.workspace_id == workspace.workspace_id)
        else {
            return;
        };
        let workspace_key = projects::workspace_key(endpoint, workspace);
        let pane_id = pane.pane_id.clone();
        let pane_key = format!("{}/{pane_id}", launch.machine);
        let (machine, display_id) = (launch.machine.clone(), launch.display_id.clone());
        self.send_task_api(
            &machine,
            vec![
                Method::PaneSendText(PaneSendTextParams {
                    pane_id: pane_id.clone(),
                    text: AGENT_COMMAND.into(),
                }),
                Method::PaneSendKeys(PaneSendKeysParams {
                    pane_id: pane_id.clone(),
                    keys: vec!["Enter".into()],
                }),
            ],
            outcome,
        );
        let started = tasks::with_store(|store| {
            store.start_attempt(
                &display_id,
                &NewAttempt {
                    harness: HARNESS.into(),
                    machine: machine.clone(),
                    workspace_key: Some(workspace_key.clone()),
                    pane_key: Some(pane_key.clone()),
                    session_id: None,
                },
                &Actor::Human,
            )
        });
        if let Err(error) = started {
            self.task_rt.launches.remove(index);
            self.push_task_notice(format!("{display_id}: {error}"));
            return;
        }
        self.task_rt.dirty = true;
        let launch = &mut self.task_rt.launches[index];
        launch.pane = Some((workspace_key, pane_key, pane_id));
        launch.typed_at = Some(Instant::now());
        launch.stage = LaunchStage::Prompt;
        outcome.repaint = true;
    }

    /// Runs herdr API calls on a machine in the background, in order.
    pub(super) fn send_task_api(
        &mut self,
        machine: &str,
        methods: Vec<Method>,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(route) = self.task_route(machine) else {
            return false;
        };
        outcome.actions.push(ClientShellAction::TaskJob {
            route,
            job: super::task_ingest::TaskJob::Api {
                machine: machine.to_owned(),
                methods,
            },
        });
        true
    }

    /// The endpoint whose machine key is `machine`, when it is reachable
    /// (this Mac, or an online remote).
    pub(super) fn endpoint_for_machine(&self, machine: &str) -> Option<ClientEndpointId> {
        self.endpoints
            .iter()
            .find(|endpoint| projects::machine_key(endpoint) == machine)
            .filter(|endpoint| {
                endpoint.endpoint_id.is_local() || endpoint.status == ClientEndpointStatus::Online
            })
            .map(|endpoint| endpoint.endpoint_id.clone())
    }

    /// Focuses the pane on its machine (activating the endpoint); false when
    /// the pane is gone or the machine is offline.
    pub(super) fn focus_task_pane(
        &mut self,
        pane_key: &str,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if self.pane_exists(pane_key) != Some(true) {
            return false;
        }
        let Some((machine, pane_id)) = pane_key.split_once('/') else {
            return false;
        };
        let Some(endpoint_id) = self.endpoint_for_machine(machine) else {
            return false;
        };
        self.focus_or_activate(
            endpoint_id,
            ClientEndpointFocusTarget::Pane(pane_id.to_owned()),
            outcome,
        );
        outcome.repaint = true;
        true
    }

    /// Relays `text` to the live attempt's pane (section 5.4); false when not
    /// sent (no live attempt, pane gone, machine offline).
    pub(super) fn relay_to_task(
        &mut self,
        display_id: &str,
        text: &str,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let pane_key = tasks::read_store(|store| {
            Ok(store.task_detail(display_id)?.and_then(|detail| {
                detail
                    .attempts
                    .into_iter()
                    .find(|attempt| attempt.ended_at.is_none())
                    .and_then(|attempt| attempt.pane_key)
            }))
        });
        let Ok(Some(pane_key)) = pane_key else {
            return false;
        };
        if self.pane_exists(&pane_key) != Some(true) {
            return false;
        }
        let Some((machine, pane_id)) = pane_key.split_once('/') else {
            return false;
        };
        self.send_task_api(
            machine,
            vec![Method::AgentPrompt(AgentPromptParams {
                target: pane_id.to_owned(),
                text: text.to_owned(),
                wait: None,
            })],
            outcome,
        )
    }

    /// Queues the ruling reply file for a remote waiting CLI (section 5.4):
    /// `R/task-reply/{pane}-d{decision}.json` on the machine of the
    /// decision's attempt.
    pub(super) fn publish_ruling(&mut self, decision: &Decision, _outcome: &mut ClientShellInput) {
        let Some(display_id) = self.display_id_of(decision.task_id) else {
            return;
        };
        let Ok(Some(detail)) = tasks::read_store(|store| store.task_detail(&display_id)) else {
            return;
        };
        let attempt = detail
            .attempts
            .iter()
            .find(|attempt| Some(attempt.id) == decision.attempt_id)
            .or_else(|| detail.attempts.iter().find(|a| a.ended_at.is_none()));
        let Some((machine, pane_id)) = attempt
            .and_then(|attempt| attempt.pane_key.as_deref())
            .and_then(|key| key.split_once('/'))
        else {
            return;
        };
        if machine == "local" {
            return;
        }
        let result = crate::tasks::OpResult {
            ok: true,
            task: Some(display_id.clone()),
            status: Some(detail.task.status),
            message: ruling_line(decision),
            code: None,
            decision_id: Some(decision.id),
        };
        let mut script = FileScript::default();
        script.write(
            &format!("task-reply/{pane_id}-d{}.json", decision.id),
            &super::task_ingest::reply_json(&result, 0),
        );
        let machine = machine.to_owned();
        self.task_rt
            .queue_write(&machine, script.finish(false), None);
    }

    /// A notice in the shell's notice line.
    pub(super) fn push_task_notice(&mut self, message: String) -> bool {
        self.push_endpoint_notice(
            ClientEndpointNoticeKind::Rejected,
            "drovr.tasks",
            "Tasks",
            message,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::AgentStatus;
    use crate::client::endpoint::{ProfileId, SavedSshEndpoint};
    use crate::tasks::{Choice, NewTask, Ruling, Status};

    fn shell() -> ClientShellState {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(
            &crate::config::Config::default(),
        ));
        state.set_snapshot(Box::new(super::super::tests::snapshot()));
        state
    }

    /// A section named `project` holding the local workspace ws_1 (cwd /repo).
    fn section(project: &str) {
        projects::update(|layout| {
            layout.groups.retain(|group| group.name != project);
            layout.groups.push(projects::ProjectGroup {
                name: project.into(),
                members: vec!["local/ws_1:client-shell".into()],
                ..Default::default()
            });
        });
    }

    fn add(project: &str, title: &str) -> String {
        tasks::with_store(|store| {
            store.create_task(
                &NewTask {
                    project: project.into(),
                    title: Some(title.into()),
                    status: Some(Status::Ready),
                    ..NewTask::default()
                },
                &Actor::Human,
            )
        })
        .expect("task")
        .display_id
    }

    fn tick(state: &mut ClientShellState) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        state.tick_tasks(&mut outcome);
        outcome
    }

    fn written(state: &mut ClientShellState, machine: &str, root: &str) {
        state.receive_task_job(super::super::task_ingest::TaskJobDone {
            machine: machine.into(),
            kind: "write",
            result: Ok(format!("{root}\n")),
        });
    }

    /// Methods of the API jobs in `outcome`.
    fn api_methods(outcome: &ClientShellInput) -> Vec<&Method> {
        outcome
            .actions
            .iter()
            .filter_map(|action| match action {
                ClientShellAction::TaskJob {
                    job: super::super::task_ingest::TaskJob::Api { methods, .. },
                    ..
                } => Some(methods.iter()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn write_scripts(outcome: &ClientShellInput) -> Vec<&str> {
        outcome
            .actions
            .iter()
            .filter_map(|action| match action {
                ClientShellAction::TaskJob {
                    job: super::super::task_ingest::TaskJob::WriteFiles { script, .. },
                    ..
                } => Some(script.as_str()),
                _ => None,
            })
            .collect()
    }

    fn create_params(method: &Method) -> Option<&crate::api::schema::WorkspaceCreateParams> {
        match method {
            Method::WorkspaceCreate(params) => Some(params),
            _ => None,
        }
    }

    fn typed(outcome: &ClientShellInput) -> Vec<String> {
        api_methods(outcome)
            .into_iter()
            .filter_map(|method| match method {
                Method::PaneSendText(params) => Some(format!("{}:{}", params.pane_id, params.text)),
                _ => None,
            })
            .collect()
    }

    fn prompts(outcome: &ClientShellInput) -> Vec<(String, String)> {
        api_methods(outcome)
            .into_iter()
            .filter_map(|method| match method {
                Method::AgentPrompt(params) => Some((params.target.clone(), params.text.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_local_start_writes_the_context_then_creates_the_workspace_with_env() {
        section("Launch Local");
        let id = add("Launch Local", "Retry the sync job");
        let mut state = shell();
        let mut outcome = ClientShellInput::default();
        state.launch_task(&id, (3, 4), &mut outcome);
        assert_eq!(state.task_rt.launches.len(), 1, "one machine: no menu");
        let out = tick(&mut state);
        let scripts = write_scripts(&out);
        assert_eq!(scripts.len(), 1);
        assert!(
            scripts[0].contains(&format!("tasks/{id}.md")),
            "{}",
            scripts[0]
        );
        assert!(!scripts[0].contains(".json"), "no snapshot on the Mac");
        // Nothing is created before the context file is written.
        let out = tick(&mut state);
        assert!(out
            .actions
            .iter()
            .all(|a| !matches!(a, ClientShellAction::Endpoint { .. })));
        written(&mut state, "local", "/state/drovr");
        let out = tick(&mut state);
        let create = out
            .actions
            .iter()
            .find_map(|action| match action {
                ClientShellAction::Endpoint { request, .. } => create_params(&request.method),
                _ => None,
            })
            .expect("workspace.create on the active endpoint");
        assert_eq!(
            create.label.as_deref(),
            Some(format!("{id} Retry the sync job").as_str())
        );
        assert_eq!(create.cwd.as_deref(), Some("/repo"));
        assert!(create.focus);
        assert_eq!(create.env["DROVR_TASK"], id);
        assert_eq!(create.env["DROVR_AGENT"], "claude");
        assert_eq!(create.env["DROVR_TASK_MODE"], "db");
        assert!(std::path::Path::new(&create.env["DROVR_TASKS_DB"]).is_absolute());
        assert_eq!(create.env.len(), 4);
        assert!(api_methods(&out).is_empty(), "no typed export");
        assert_eq!(
            state.task_rt.launches[0].context_path.as_deref(),
            Some(format!("/state/drovr/tasks/{id}.md").as_str())
        );
    }

    fn remote(state: &mut ClientShellState) -> ClientEndpointId {
        let profile = SavedSshEndpoint {
            id: ProfileId::parse("0123456789abcdef0123456789abcdef").expect("id"),
            label: "mato".into(),
            target: "me@mato.example".into(),
            session: "agents".into(),
            enabled: true,
        };
        state.set_endpoint_catalog(std::slice::from_ref(&profile));
        let endpoint_id = ClientEndpointId::Ssh(profile.id);
        let mut snapshot = super::super::tests::snapshot();
        snapshot.boot_id = "mato-boot".into();
        snapshot.workspaces[0].workspace_id = "w9".into();
        snapshot.workspaces[0].label = "mato-home".into();
        snapshot.panes[0].workspace_id = "w9".into();
        snapshot.panes[0].pane_id = "p9".into();
        state.set_endpoint_snapshot(&endpoint_id, Box::new(snapshot));
        if let Some(endpoint) = state
            .endpoints
            .iter_mut()
            .find(|e| e.endpoint_id == endpoint_id)
        {
            endpoint.status = ClientEndpointStatus::Online;
        }
        endpoint_id
    }

    #[test]
    fn a_remote_start_probes_writes_context_and_snapshot_then_creates_over_the_api() {
        section("Launch Remote");
        let id = add("Launch Remote", "Attention hook");
        let mut state = shell();
        let mato = remote(&mut state);
        // Two machines: the click opens the machine menu.
        let mut outcome = ClientShellInput::default();
        state.launch_task(&id, (3, 4), &mut outcome);
        assert!(state.task_rt.launches.is_empty());
        assert!(matches!(
            state.overlay,
            Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
                target: ClientContextMenuTarget::Task {
                    menu: TaskMenu::Machine { .. },
                    ..
                },
                ..
            }))
        ));
        state.overlay = None;

        state.launch_task_on(&id, mato.clone(), &mut outcome);
        assert_eq!(state.task_rt.launches[0].stage, LaunchStage::Probe);
        let out = tick(&mut state);
        let kinds: Vec<&str> = out
            .actions
            .iter()
            .filter_map(|a| match a {
                ClientShellAction::TaskJob { job, .. } => Some(match job {
                    super::super::task_ingest::TaskJob::Probe { .. } => "probe",
                    super::super::task_ingest::TaskJob::Pull { .. } => "pull",
                    super::super::task_ingest::TaskJob::WriteFiles { .. } => "write",
                    super::super::task_ingest::TaskJob::Api { .. } => "api",
                }),
                _ => None,
            })
            .collect();
        assert_eq!(kinds, vec!["probe"], "one job per machine; the probe first");
        state.receive_task_job(super::super::task_ingest::TaskJobDone {
            machine: "mato".into(),
            kind: "probe",
            result: Ok("drovr-task 1\n".into()),
        });
        let out = tick(&mut state);
        assert_eq!(state.task_rt.launches[0].stage, LaunchStage::Context);
        let scripts = write_scripts(&out);
        assert_eq!(scripts.len(), 1);
        assert!(scripts[0].contains("DROVR_TASK_OUTBOX_DIR"));
        assert!(scripts[0].contains(&format!("tasks/{id}.json")));
        assert!(scripts[0].contains(&format!("tasks/{id}.md")));
        written(&mut state, "mato", "/Users/me/.local/state/herdr/drovr");
        let out = tick(&mut state);
        let create = api_methods(&out)
            .into_iter()
            .find_map(create_params)
            .expect("workspace.create over the API route");
        assert_eq!(create.env["DROVR_TASK_MODE"], "outbox");
        assert!(!create.env.contains_key("DROVR_TASKS_DB"));
        assert_eq!(create.cwd, None, "the project has no workspace on mato");
        assert!(out.actions.iter().any(|a| matches!(
            a,
            ClientShellAction::ActivateEndpoint { endpoint_id, .. } if *endpoint_id == mato
        )));
        assert_eq!(
            projects::layout()
                .groups
                .iter()
                .find(|g| g.name == "Launch Remote")
                .map(|g| g.members.contains(&format!("mato/{id} Attention hook"))),
            Some(true)
        );
    }

    #[test]
    fn a_machine_without_the_task_command_refuses_the_start() {
        section("Launch Refused");
        let id = add("Launch Refused", "Schema rename");
        let mut state = shell();
        let mato = remote(&mut state);
        state.task_rt.probe.insert("mato".into(), None);
        let mut outcome = ClientShellInput::default();
        state.launch_task_on(&id, mato, &mut outcome);
        assert!(state.task_rt.launches.is_empty());
        let notice = state.visible_endpoint_notice.as_ref().expect("notice");
        assert!(notice
            .body
            .starts_with("mato: drovr there has no task command"));
    }

    /// Two launches through the context stage to Wait.
    fn two_waiting(state: &mut ClientShellState) -> (String, String) {
        section("Launch Two");
        let a = add("Launch Two", "First");
        let b = add("Launch Two", "Second");
        let mut outcome = ClientShellInput::default();
        state.launch_task_on(&a, ClientEndpointId::Local, &mut outcome);
        state.launch_task_on(&b, ClientEndpointId::Local, &mut outcome);
        let out = tick(state);
        assert_eq!(write_scripts(&out).len(), 1, "both contexts in one job");
        written(state, "local", "/s");
        tick(state);
        assert!(state
            .task_rt
            .launches
            .iter()
            .all(|launch| launch.stage == LaunchStage::Wait));
        (a, b)
    }

    fn with_workspaces(state: &mut ClientShellState, labels: &[(&str, &str, &str)]) {
        let mut snapshot = super::super::tests::snapshot();
        for (workspace_id, label, pane_id) in labels {
            let mut workspace = snapshot.workspaces[0].clone();
            workspace.workspace_id = (*workspace_id).into();
            workspace.label = (*label).into();
            snapshot.workspaces.push(workspace);
            let mut pane = snapshot.panes[0].clone();
            pane.workspace_id = (*workspace_id).into();
            pane.pane_id = (*pane_id).into();
            snapshot.panes.push(pane);
        }
        state.set_snapshot(Box::new(snapshot));
    }

    #[test]
    fn two_pending_launches_find_their_own_workspace_and_type_once() {
        let mut state = shell();
        let (a, b) = two_waiting(&mut state);
        // Only the second one's workspace exists so far.
        with_workspaces(&mut state, &[("w2", &format!("{b} Second"), "p2")]);
        let out = tick(&mut state);
        assert_eq!(typed(&out), vec!["p2:cc".to_owned()]);
        with_workspaces(
            &mut state,
            &[
                ("w2", &format!("{b} Second"), "p2"),
                ("w1", &format!("{a} First"), "p1"),
            ],
        );
        let out = tick(&mut state);
        assert_eq!(typed(&out), vec!["p1:cc".to_owned()]);
        assert!(typed(&tick(&mut state)).is_empty(), "typed once each");
        let task = tasks::with_store(|store| store.task_detail(&a))
            .expect("read")
            .expect("task");
        assert_eq!(task.task.status, Status::Working);
        let attempt = &task.attempts[0];
        assert_eq!(attempt.pane_key.as_deref(), Some("local/p1"));
        assert_eq!(attempt.machine, "local");
        assert_eq!(
            task.task.workspace_key.as_deref(),
            Some(format!("local/w1:{a} First").as_str())
        );
    }

    #[test]
    fn the_first_prompt_waits_for_the_agent_or_five_seconds() {
        let mut state = shell();
        let (a, b) = two_waiting(&mut state);
        with_workspaces(
            &mut state,
            &[
                ("w1", &format!("{a} First"), "p1"),
                ("w2", &format!("{b} Second"), "p2"),
            ],
        );
        tick(&mut state);
        assert!(prompts(&tick(&mut state)).is_empty(), "no agent yet");
        // p1's agent shows: its prompt goes out.
        let mut snapshot = state.snapshot.as_deref().cloned().expect("snapshot");
        snapshot.agents.push(crate::protocol::ClientShellAgent {
            pane_id: "p1".into(),
            workspace_id: "w1".into(),
            tab_id: "tab_1".into(),
            name: None,
            display_agent: None,
            agent: Some("claude".into()),
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: AgentStatus::Idle,
            state_change_seq: 1,
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: false,
        });
        state.set_snapshot(Box::new(snapshot));
        let sent = prompts(&tick(&mut state));
        assert_eq!(
            sent,
            vec![(
                "p1".to_owned(),
                format!("Work on drovr task {a}: First. Read /s/tasks/{a}.md first. Report with drovr task (skill drovr-tasks).")
            )]
        );
        // p2 has no agent: its prompt goes 5 s after typing.
        for launch in &mut state.task_rt.launches {
            launch.typed_at = Some(Instant::now() - PROMPT_AFTER);
        }
        let sent = prompts(&tick(&mut state));
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "p2");
        assert!(state.task_rt.launches.is_empty());
    }

    #[test]
    fn a_workspace_that_never_appears_drops_the_launch_with_a_notice() {
        let mut state = shell();
        let (a, _) = two_waiting(&mut state);
        state.task_rt.launches[0].since = Instant::now() - APPEAR_WITHIN;
        tick(&mut state);
        assert_eq!(state.task_rt.launches.len(), 1);
        let notice = state.visible_endpoint_notice.as_ref().expect("notice");
        assert_eq!(
            notice.body,
            format!("{a}: workspace did not appear on local")
        );
    }

    #[test]
    fn relays_go_to_the_live_pane_as_prompts_and_wait_for_a_polling_cli() {
        section("Relay");
        let id = add("Relay", "Doc pane links");
        let mut state = shell();
        let mut outcome = ClientShellInput::default();
        assert!(
            !state.relay_to_task(&id, "you on x: hi", &mut outcome),
            "no attempt"
        );
        tasks::with_store(|store| {
            store.start_attempt(
                &id,
                &NewAttempt {
                    harness: "claude".into(),
                    machine: "local".into(),
                    workspace_key: None,
                    pane_key: Some("local/pane_1".into()),
                    session_id: None,
                },
                &Actor::Human,
            )
        })
        .expect("start");
        for text in [
            format!("you on {id}: start with the store"),
            ruling_relay(&id, "New decisions table"),
            format!("{id} sent back: tests are missing"),
        ] {
            let mut outcome = ClientShellInput::default();
            assert!(state.relay_to_task(&id, &text, &mut outcome));
            assert_eq!(prompts(&outcome), vec![("pane_1".to_owned(), text.clone())]);
        }
        // The pane is gone: nothing is sent.
        let mut snapshot = super::super::tests::snapshot();
        snapshot.panes.clear();
        state.set_snapshot(Box::new(snapshot));
        let mut outcome = ClientShellInput::default();
        assert!(!state.relay_to_task(&id, "x", &mut outcome));
        assert!(outcome.actions.is_empty());

        // A ruling is relayed when wait_until is unset or past only.
        let mut decision = tasks::with_store(|store| {
            store.request_decision(
                &id,
                &crate::tasks::NewDecision {
                    title: "Which table?".into(),
                    summary: String::new(),
                    choices: vec![Choice {
                        id: "new".into(),
                        label: "New decisions table".into(),
                        consequence: None,
                        recommended: true,
                    }],
                    allow_text: true,
                    default_choice: None,
                    expires_at: None,
                    wait_until: None,
                },
                &Actor::Agent("claude@local".into()),
            )
        })
        .expect("decision");
        let now = "2026-10-03T08:00:00Z";
        assert!(relay_due(&decision, now));
        decision.wait_until = Some("2026-10-03T07:59:59Z".into());
        assert!(relay_due(&decision, now));
        decision.wait_until = Some("2026-10-03T08:10:00Z".into());
        assert!(!relay_due(&decision, now));
        let ruled = tasks::with_store(|store| {
            store.rule_decision(
                decision.id,
                &Ruling::Choice("new".into()),
                "panel",
                &Actor::Human,
            )
        })
        .expect("rule");
        assert_eq!(ruling_label(&ruled), "New decisions table");
        assert_eq!(ruling_line(&ruled), "ruled new: New decisions table");
    }

    #[test]
    fn a_remote_ruling_writes_the_reply_file_of_the_attempts_pane() {
        section("Ruling");
        let id = add("Ruling", "Spec decision requests");
        let mut state = shell();
        remote(&mut state);
        tasks::with_store(|store| {
            store.start_attempt(
                &id,
                &NewAttempt {
                    harness: "claude".into(),
                    machine: "mato".into(),
                    workspace_key: None,
                    pane_key: Some("mato/p9".into()),
                    session_id: None,
                },
                &Actor::Human,
            )
        })
        .expect("start");
        let decision = tasks::with_store(|store| {
            store.request_decision(
                &id,
                &crate::tasks::NewDecision {
                    title: "Which table?".into(),
                    summary: String::new(),
                    choices: vec![Choice {
                        id: "new".into(),
                        label: "New table".into(),
                        consequence: None,
                        recommended: false,
                    }],
                    allow_text: true,
                    default_choice: None,
                    expires_at: None,
                    wait_until: None,
                },
                &Actor::Agent("claude@mato".into()),
            )
        })
        .expect("decision");
        let ruled = tasks::with_store(|store| {
            store.rule_decision(
                decision.id,
                &Ruling::Text("use entries".into()),
                "panel",
                &Actor::Human,
            )
        })
        .expect("rule");
        let mut outcome = ClientShellInput::default();
        state.publish_ruling(&ruled, &mut outcome);
        let out = tick(&mut state);
        let script = write_scripts(&out)
            .into_iter()
            .find(|script| script.contains("task-reply"))
            .expect("reply job")
            .to_owned();
        assert!(
            script.contains(&format!("task-reply/p9-d{}.json", decision.id)),
            "{script}"
        );
        assert!(
            script.contains("\"message\":\"ruled text: use entries\""),
            "{script}"
        );
        assert!(script.contains(&format!("\"decision_id\":{}", decision.id)));
    }

    #[test]
    fn the_context_file_lists_criteria_pinned_notes_and_the_decision() {
        section("Context");
        let id = tasks::with_store(|store| {
            store.create_task(
                &NewTask {
                    project: "Context".into(),
                    title: Some("Spec decision requests".into()),
                    body: "Add decision requests.\nOne open per task.".into(),
                    kind: Some(crate::tasks::Kind::Spec),
                    priority: crate::tasks::Priority::High,
                    criteria: vec!["schema added".into(), "docs updated".into()],
                    ..NewTask::default()
                },
                &Actor::Human,
            )
        })
        .expect("task")
        .display_id;
        tasks::with_store(|store| {
            store.check_criterion(&id, 1, CheckState::Passed, Some("ok"), &Actor::Human)?;
            let pinned = store.add_entry(
                &id,
                EntryKind::Human,
                "keep the outbox format stable",
                &Actor::Human,
            )?;
            store.pin_entry(pinned.id, true)?;
            store.add_entry(&id, EntryKind::Human, "start with the store", &Actor::Human)?;
            store.request_decision(
                &id,
                &crate::tasks::NewDecision {
                    title: "Which table holds decisions?".into(),
                    summary: String::new(),
                    choices: vec![
                        Choice {
                            id: "new".into(),
                            label: "New decisions table".into(),
                            consequence: None,
                            recommended: true,
                        },
                        Choice {
                            id: "reuse".into(),
                            label: "Reuse entries".into(),
                            consequence: None,
                            recommended: false,
                        },
                    ],
                    allow_text: true,
                    default_choice: None,
                    expires_at: None,
                    wait_until: None,
                },
                &Actor::Agent("claude@mato".into()),
            )
        })
        .expect("setup");
        let detail = tasks::with_store(|store| store.task_detail(&id))
            .expect("read")
            .expect("task");
        let text = context_text(&detail);
        let expected = format!(
            "# {id} Spec decision requests
Status: triage · Kind: spec · Priority: high · Project: Context

## Task
Add decision requests.
One open per task.

## Acceptance criteria
- [x] 1. schema added
- [ ] 2. docs updated

## Pinned notes
- keep the outbox format stable

## Said since the last attempt
- you: keep the outbox format stable
- you: start with the store

## Open decision
Which table holds decisions?: new = New decisions table (recommended); reuse = Reuse entries

## How to report
Use `drovr task` (skill drovr-tasks): note, check, verify, artifact,
decide, done. Your task id is {id}; commands without an id use it.
"
        );
        assert_eq!(text, expected);
        assert_eq!(
            launch_label("AC-12", "A very long task title that goes on and on"),
            "AC-12 A very long task title that goes o"
        );
    }
}
