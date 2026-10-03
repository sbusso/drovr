//! drovr fork: the client side of tasks that runs whether the panel is open
//! or not (docs/design/tasks.md, sections 5.2, 5.3, 6.3 to 6.6).
//!
//! [`ClientShellState::tick_tasks`] runs from `tick_drovr` (100 ms). It keeps
//! a small cache of the tasks that have an open attempt, moves their status
//! from the agent signals of their panes, copies usage onto attempts,
//! rewrites context files, ends attempts whose workspace is gone, raises a
//! notice for each new open decision, and schedules the per-machine jobs of
//! `task_ingest.rs` (probe, outbox pulls, file writes).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::api::schema::AgentStatus;

use super::agent_signal::{AgentSignal, ItemKind};
use super::inbox::PanelView;
use super::projects;
use super::task_ingest::{task_files, FileScript, TaskJob};
use super::task_launch::TaskLaunch;
use super::*;
use crate::tasks::{self, Actor, EntryKind, Status, StoreError, TaskFilter};

/// A signal must hold this long before it moves a task.
const STABLE_FOR: Duration = Duration::from_secs(2);
/// The cache is reloaded when `data_version` moved, checked this often.
const CHECK_EVERY: Duration = Duration::from_secs(1);
/// ... and at least this often: `data_version` does not move for the panel's
/// own commits on the shared connection.
const RELOAD_EVERY: Duration = Duration::from_secs(5);
const EXPIRE_EVERY: Duration = Duration::from_secs(30);
const SWEEP_EVERY: Duration = Duration::from_secs(60);
/// A task's workspace missing this long from an online machine ends the
/// attempt.
const GONE_FOR: Duration = Duration::from_secs(30);

/// The shell job in flight on a machine.
#[derive(Debug)]
pub(super) enum Inflight {
    Probe,
    Pull,
    /// A file job; `launches` wait on it for their context file.
    Write {
        launches: Vec<u64>,
    },
}

/// Shell work waiting for a machine's one job slot.
#[derive(Debug, Default)]
pub(super) struct MachineQueue {
    probe: bool,
    /// A pull: Some(None) = every pane (sweep), Some(Some(set)) = these panes.
    pull: Option<Option<HashSet<String>>>,
    writes: Vec<(String, Option<u64>)>,
}

/// A task with an open attempt, as the sync needs it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct LiveTask {
    pub(super) display_id: String,
    pub(super) status: Status,
    pub(super) auto_status: bool,
    pub(super) open_decision: bool,
    pub(super) version: i64,
    pub(super) workspace_key: Option<String>,
    pub(super) attempt_id: i64,
    pub(super) machine: String,
    pub(super) pane_key: Option<String>,
}

/// Tokens in, tokens out and session id of an attempt.
type Usage = (Option<i64>, Option<i64>, Option<String>);

/// What a machine looked like at the last tick.
#[derive(Debug)]
struct MachineSeen {
    online: bool,
    boot_id: Option<String>,
    swept: Option<Instant>,
}

#[derive(Debug, Default)]
pub(super) struct TaskRuntime {
    pub(super) launches: Vec<TaskLaunch>,
    pub(super) next_launch: u64,
    /// pane key -> the agent state stamp a move was last applied for.
    pub(super) stamps: HashMap<String, (AgentStatus, Option<u64>)>,
    /// pane key -> the stamp seen and since when (the 2 s rule).
    pending: HashMap<String, ((AgentStatus, Option<u64>), Instant)>,
    /// machine -> `drovr task proto` answer (None = no task command).
    pub(super) probe: HashMap<String, Option<u32>>,
    pub(super) busy: HashSet<String>,
    pub(super) inflight: HashMap<String, Inflight>,
    pub(super) queues: HashMap<String, MachineQueue>,
    /// pane key -> last `drovr_tq` value seen.
    tokens: HashMap<String, String>,
    machines: HashMap<String, MachineSeen>,
    /// display id -> since when its workspace is missing.
    missing: HashMap<String, Instant>,
    pub(super) live: Vec<LiveTask>,
    /// (display id, workspace key) of linked tasks with no open attempt.
    linked: Vec<(String, String)>,
    loaded: Option<Instant>,
    checked: Option<Instant>,
    data_version: Option<i64>,
    pub(super) dirty: bool,
    /// Open decision ids already seen; None until the first scan.
    seen_decisions: Option<HashSet<i64>>,
    expired: Option<Instant>,
    /// display id -> (machine, task version) of the last context write.
    pub(super) written: HashMap<String, (String, i64)>,
    /// attempt id -> usage last copied.
    usage: HashMap<i64, Usage>,
    /// Notices shown once per run.
    pub(super) notified: HashSet<String>,
    /// source "{machine}/{pane}/{epoch}" -> highest seq pulled and handled.
    pub(super) applied: HashMap<String, u64>,
}

impl TaskRuntime {
    fn queue(&mut self, machine: &str) -> &mut MachineQueue {
        self.queues.entry(machine.to_owned()).or_default()
    }

    /// Queues a file job; `launch` waits on it for its context file.
    pub(super) fn queue_write(&mut self, machine: &str, script: String, launch: Option<u64>) {
        self.queue(machine).writes.push((script, launch));
    }

    pub(super) fn queue_probe(&mut self, machine: &str) {
        if !matches!(self.inflight.get(machine), Some(Inflight::Probe)) {
            self.queue(machine).probe = true;
        }
    }

    /// Queues a pull of `pane` (None: a sweep of every pane).
    pub(super) fn queue_pull(&mut self, machine: &str, pane: Option<&str>) {
        let queue = self.queue(machine);
        match (pane, &mut queue.pull) {
            (None, slot) => *slot = Some(None),
            (Some(_), Some(None)) => {}
            (Some(pane), Some(Some(panes))) => {
                panes.insert(pane.to_owned());
            }
            (Some(pane), slot @ None) => *slot = Some(Some(HashSet::from([pane.to_owned()]))),
        }
    }

    /// The next job of an idle machine, writes first (they answer pulls and
    /// launches), then the probe, then a pull.
    pub(super) fn next_job(&mut self, machine: &str) -> Option<(TaskJob, Inflight)> {
        if self.busy.contains(machine) {
            return None;
        }
        let queue = self.queues.get_mut(machine)?;
        if !queue.writes.is_empty() {
            let writes = std::mem::take(&mut queue.writes);
            let launches = writes.iter().filter_map(|(_, launch)| *launch).collect();
            let script = writes
                .into_iter()
                .map(|(script, _)| script)
                .collect::<Vec<_>>()
                .join("\n");
            return Some((
                TaskJob::WriteFiles {
                    machine: machine.to_owned(),
                    script,
                },
                Inflight::Write { launches },
            ));
        }
        if std::mem::take(&mut queue.probe) {
            return Some((
                TaskJob::Probe {
                    machine: machine.to_owned(),
                },
                Inflight::Probe,
            ));
        }
        let panes = queue.pull.take()?;
        let applied = self
            .applied
            .iter()
            .filter_map(|(source, seq)| {
                let rest = source.strip_prefix(&format!("{machine}/"))?;
                let (pane, epoch) = rest.split_once('/')?;
                let wanted = panes.as_ref().is_none_or(|panes| panes.contains(pane));
                wanted.then(|| (pane.to_owned(), epoch.to_owned(), *seq))
            })
            .collect();
        let mut panes: Option<Vec<String>> = panes.map(|set| set.into_iter().collect());
        if let Some(panes) = panes.as_mut() {
            panes.sort();
        }
        Some((
            TaskJob::Pull {
                machine: machine.to_owned(),
                panes,
                applied,
            },
            Inflight::Pull,
        ))
    }
}

/// How an agent's signal reads for its task (section 6.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Signal {
    Works,
    Waits,
    Finished,
    Exited,
    Other,
}

pub(super) fn signal_of(status: AgentStatus, item: Option<ItemKind>) -> Signal {
    match item {
        Some(
            ItemKind::Permission
            | ItemKind::Question
            | ItemKind::Plan
            | ItemKind::Asks
            | ItemKind::Dialog,
        ) => Signal::Waits,
        Some(ItemKind::Finished) => Signal::Finished,
        Some(ItemKind::Exited) => Signal::Exited,
        _ if status == AgentStatus::Working => Signal::Works,
        _ => Signal::Other,
    }
}

/// What the sync does for a signal on a task in `status` (section 6.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SyncStep {
    Move(Status),
    /// working -> review once the gate passes.
    Review,
    Entry(&'static str),
    None,
}

pub(super) fn sync_step(signal: Signal, status: Status, open_decision: bool) -> SyncStep {
    match (signal, status) {
        (Signal::Works, Status::Ready) => SyncStep::Move(Status::Working),
        // A task blocked on its own decision stays blocked.
        (Signal::Works, Status::Blocked) if !open_decision => SyncStep::Move(Status::Working),
        (Signal::Waits, Status::Working) => SyncStep::Move(Status::Blocked),
        (Signal::Finished, Status::Working) => SyncStep::Review,
        (Signal::Exited, Status::Working | Status::Blocked) => SyncStep::Entry("agent exited"),
        _ => SyncStep::None,
    }
}

/// The state hook's `drovr_state` says the agent's turn finished.
fn hook_finished(agent: &crate::protocol::ClientShellAgent) -> bool {
    matches!(agent.agent.as_deref(), Some("claude" | "codex"))
        && projects::agent_token(agent, "drovr_state")
            .is_some_and(|value| value.trim().starts_with("finished|"))
}

/// `machine/w7:label` -> ("machine", "w7").
fn split_workspace_key(key: &str) -> Option<(&str, &str)> {
    let (machine, rest) = key.split_once('/')?;
    Some((machine, rest.split_once(':').map_or(rest, |(id, _)| id)))
}

/// Usage of a Claude session on a pane: (input, output) over the reported
/// days, and the session id.
fn pane_usage(agent: &crate::protocol::ClientShellAgent) -> Usage {
    if agent.agent.as_deref() != Some("claude") {
        return (None, None, None);
    }
    let session = projects::agent_token(agent, "drovr_session").map(str::to_owned);
    let mut totals: Option<(i64, i64)> = None;
    for (name, value) in &agent.tokens {
        if !name.trim_start_matches('$').starts_with("drovr_u_") {
            continue;
        }
        let mut parts = value.split(',').map(|part| part.trim().parse::<i64>());
        if let (Some(Ok(input)), Some(Ok(output))) = (parts.next(), parts.next()) {
            let sum = totals.get_or_insert((0, 0));
            sum.0 += input;
            sum.1 += output;
        }
    }
    (totals.map(|t| t.0), totals.map(|t| t.1), session)
}

impl ClientShellState {
    /// Periodic tasks work (section 6.5's cadence).
    pub(super) fn tick_tasks(&mut self, outcome: &mut ClientShellInput) {
        let now = Instant::now();
        self.observe_task_machines(now);
        self.tick_task_launches(outcome);
        let due = self.task_rt.dirty
            || self
                .task_rt
                .checked
                .is_none_or(|at| now.saturating_duration_since(at) >= CHECK_EVERY);
        if due {
            self.task_rt.checked = Some(now);
            if self.reload_live_tasks(now) {
                outcome.repaint |= self.decision_notices();
                self.copy_task_usage();
                self.refresh_task_contexts();
            }
            self.end_gone_attempts(now);
        }
        self.sync_task_status(now);
        if self
            .task_rt
            .expired
            .is_none_or(|at| now.saturating_duration_since(at) >= EXPIRE_EVERY)
        {
            self.task_rt.expired = Some(now);
            self.expire_task_decisions(outcome);
        }
        self.pump_task_jobs(outcome);
    }

    /// Reloads the live-task cache when the store changed (or every
    /// [`RELOAD_EVERY`]). True when it was reloaded.
    fn reload_live_tasks(&mut self, now: Instant) -> bool {
        let version = tasks::read_store(|store| store.data_version()).ok();
        let stale = self
            .task_rt
            .loaded
            .is_none_or(|at| now.saturating_duration_since(at) >= RELOAD_EVERY);
        if !self.task_rt.dirty && !stale && version == self.task_rt.data_version {
            return false;
        }
        self.task_rt.dirty = false;
        self.task_rt.loaded = Some(now);
        self.task_rt.data_version = version;
        let live = tasks::read_store(|store| {
            let mut live = Vec::new();
            let mut linked = Vec::new();
            for card in store.list(&TaskFilter::default())? {
                let Some((_, machine, pane_key)) = card.live else {
                    if let Some(key) = card.task.workspace_key {
                        linked.push((card.task.display_id, key));
                    }
                    continue;
                };
                let Some(detail) = store.task_detail(&card.task.display_id)? else {
                    continue;
                };
                let Some(attempt) = detail.attempts.iter().find(|a| a.ended_at.is_none()) else {
                    continue;
                };
                live.push(LiveTask {
                    display_id: card.task.display_id.clone(),
                    status: card.task.status,
                    auto_status: card.task.auto_status,
                    open_decision: card.open_decision,
                    version: card.task.version,
                    workspace_key: card.task.workspace_key.clone(),
                    attempt_id: attempt.id,
                    machine,
                    pane_key,
                });
            }
            Ok((live, linked))
        });
        match live {
            Ok((live, linked)) => {
                self.task_rt.live = live;
                self.task_rt.linked = linked;
            }
            Err(error) => tracing::debug!(%error, "cannot read live tasks"),
        }
        true
    }

    /// Section 4.5's notice: `{id} asks: {title}` once per new open decision
    /// the panel does not show. The first scan only records ids.
    fn decision_notices(&mut self) -> bool {
        let Ok(open) = tasks::read_store(|store| store.open_decisions(None)) else {
            return false;
        };
        let ids: HashSet<i64> = open.iter().map(|open| open.decision.id).collect();
        let Some(seen) = self.task_rt.seen_decisions.replace(ids) else {
            return false;
        };
        let shown = (self.inbox.open && self.inbox.view == PanelView::Tasks)
            .then(|| match &self.inbox.filter {
                Some(super::agent_signal::InboxFilter::Project(name)) => Some(name.clone()),
                _ => None,
            })
            .flatten();
        let mut repaint = false;
        for open in open {
            if seen.contains(&open.decision.id) || shown.as_deref() == Some(open.project.as_str()) {
                continue;
            }
            repaint |=
                self.push_task_notice(format!("{} asks: {}", open.display_id, open.decision.title));
        }
        repaint
    }

    /// Section 6.4: session id and token counters onto the live attempt.
    fn copy_task_usage(&mut self) {
        let mut writes = Vec::new();
        for live in &self.task_rt.live {
            let Some(agent) = live
                .pane_key
                .as_deref()
                .and_then(|key| self.agent_for_pane_key(key))
            else {
                continue;
            };
            let usage = pane_usage(agent);
            if usage == (None, None, None)
                || self.task_rt.usage.get(&live.attempt_id) == Some(&usage)
            {
                continue;
            }
            writes.push((live.attempt_id, usage));
        }
        for (attempt_id, usage) in writes {
            let (input, output, session) = usage.clone();
            let result = tasks::read_store(|store| {
                store.set_attempt_usage(attempt_id, input, output, None, session.as_deref())
            });
            match result {
                Ok(()) => {
                    self.task_rt.usage.insert(attempt_id, usage);
                }
                Err(error) => tracing::debug!(%error, attempt_id, "cannot copy usage"),
            }
        }
    }

    /// Section 5.2: rewrite the context file (and the remote snapshot) of a
    /// live task when its version moved since the last write. Ceiling: every
    /// write to the task row moves the version (notes, moves), so a context
    /// is rewritten more often than its content changes; comparing the
    /// rendered text would avoid the extra writes.
    fn refresh_task_contexts(&mut self) {
        let mut scripts: HashMap<String, FileScript> = HashMap::new();
        let live = self.task_rt.live.clone();
        for task in live {
            if self.task_rt.written.get(&task.display_id)
                == Some(&(task.machine.clone(), task.version))
            {
                continue;
            }
            if self.endpoint_for_machine(&task.machine).is_none()
                || self
                    .task_rt
                    .launches
                    .iter()
                    .any(|l| l.display_id == task.display_id)
            {
                continue;
            }
            let Ok(Some(detail)) = tasks::read_store(|store| store.task_detail(&task.display_id))
            else {
                continue;
            };
            let local = task.machine == "local";
            task_files(
                scripts.entry(task.machine.clone()).or_default(),
                &detail,
                local,
            );
            self.task_rt.written.insert(
                task.display_id.clone(),
                (task.machine.clone(), task.version),
            );
        }
        for (machine, script) in scripts {
            let local = machine == "local";
            self.task_rt
                .queue_write(&machine, script.finish(local), None);
        }
    }

    /// Machines turning online (probe and sweep), offline or restarted
    /// (section 5.3's clocks reset), the 60 s sweep and `drovr_tq` changes.
    fn observe_task_machines(&mut self, now: Instant) {
        let mut seen = Vec::new();
        let mut rings = Vec::new();
        for endpoint in &self.endpoints {
            let machine = projects::machine_key(endpoint);
            let online =
                endpoint.endpoint_id.is_local() || endpoint.status == ClientEndpointStatus::Online;
            let boot_id = endpoint
                .snapshot
                .as_deref()
                .map(|snapshot| snapshot.boot_id.clone());
            seen.push((
                machine.clone(),
                online,
                boot_id,
                endpoint.endpoint_id.is_local(),
                endpoint.bridge.is_some(),
            ));
            if online && !endpoint.endpoint_id.is_local() {
                for agent in endpoint
                    .snapshot
                    .as_deref()
                    .map_or(&[][..], |snapshot| &snapshot.agents[..])
                {
                    if let Some(value) = projects::agent_token(agent, "drovr_tq") {
                        rings.push((machine.clone(), agent.pane_id.clone(), value.to_owned()));
                    }
                }
            }
        }
        for (machine, online, boot_id, local, bridged) in seen {
            let previous = self.task_rt.machines.get(&machine);
            let changed = previous
                .is_none_or(|previous| previous.online != online || previous.boot_id != boot_id);
            if changed {
                // Offline, reconnect or restart: no attempt ends on old clocks.
                let linked = self.task_rt.linked.iter().filter(|(_, key)| {
                    split_workspace_key(key).is_some_and(|(on, _)| on == machine)
                });
                let ids: Vec<String> = self
                    .task_rt
                    .live
                    .iter()
                    .filter(|live| live.machine == machine)
                    .map(|live| live.display_id.clone())
                    .chain(linked.map(|(id, _)| id.clone()))
                    .collect();
                for id in ids {
                    self.task_rt.missing.remove(&id);
                }
            }
            let mut swept = previous.and_then(|previous| previous.swept);
            if online && !local && bridged {
                if changed && previous.is_none_or(|previous| !previous.online) {
                    self.task_rt.queue_probe(&machine);
                }
                if changed
                    || swept.is_none_or(|at| now.saturating_duration_since(at) >= SWEEP_EVERY)
                {
                    self.task_rt.queue_pull(&machine, None);
                    swept = Some(now);
                }
            }
            self.task_rt.machines.insert(
                machine,
                MachineSeen {
                    online,
                    boot_id,
                    swept,
                },
            );
        }
        for (machine, pane, value) in rings {
            let key = format!("{machine}/{pane}");
            if self.task_rt.tokens.get(&key) != Some(&value) {
                self.task_rt.tokens.insert(key, value);
                self.task_rt.queue_pull(&machine, Some(&pane));
            }
        }
    }

    /// Section 5.3: a workspace missing for 30 s from an online machine
    /// whose boot id did not change ends its task's attempt. The task's
    /// workspace link goes too, live or not: herdr reuses workspace ids after
    /// a restart, and a stale link would tie the task to an unrelated
    /// workspace (its filter, `drovr task add`'s default project, this
    /// check). Ceiling: a workspace closed while no client runs keeps its
    /// links until its id is reused; matching on a server boot id stored in
    /// the key would close that gap.
    fn end_gone_attempts(&mut self, now: Instant) {
        let mut gone = Vec::new();
        let linked = self
            .task_rt
            .live
            .iter()
            .filter_map(|live| {
                let key = live.workspace_key.as_deref()?;
                let (machine, _) = split_workspace_key(key)?;
                (machine == live.machine).then_some((live.display_id.as_str(), key, Some(live)))
            })
            .chain(
                self.task_rt
                    .linked
                    .iter()
                    .map(|(id, key)| (id.as_str(), key.as_str(), None)),
            );
        for (display_id, key, live) in linked {
            let Some((machine, workspace_id)) = split_workspace_key(key) else {
                continue;
            };
            let Some(snapshot) = self
                .endpoint_for_machine(machine)
                .and_then(|endpoint_id| self.endpoint_by_id(&endpoint_id))
                .and_then(|endpoint| endpoint.snapshot.as_deref())
            else {
                continue;
            };
            if snapshot
                .workspaces
                .iter()
                .any(|workspace| workspace.workspace_id == workspace_id)
            {
                self.task_rt.missing.remove(display_id);
                continue;
            }
            let since = *self
                .task_rt
                .missing
                .entry(display_id.to_owned())
                .or_insert(now);
            if now.saturating_duration_since(since) >= GONE_FOR {
                gone.push((display_id.to_owned(), live.cloned()));
            }
        }
        for (display_id, live) in gone {
            self.task_rt.missing.remove(&display_id);
            self.task_rt.dirty = true;
            // Ceiling: the store ends an attempt only together with a move, so
            // a task in review (or with auto off) keeps its stale attempt; the
            // Attempts tab offers `release` for it.
            let release = live.is_some_and(|live| {
                live.auto_status && matches!(live.status, Status::Working | Status::Blocked)
            });
            let result = tasks::with_store(|store| {
                if release {
                    store.release(&display_id, "workspace closed", &Actor::Auto)?;
                }
                store.link_workspace(&display_id, None)
            });
            if let Err(error) = result {
                tracing::debug!(%error, task = %display_id, "cannot end the attempt");
            }
        }
    }

    /// The agent on a `machine/pane_id` key, when its machine is online.
    pub(super) fn agent_for_pane_key(
        &self,
        key: &str,
    ) -> Option<&crate::protocol::ClientShellAgent> {
        let (machine, pane_id) = key.split_once('/')?;
        let endpoint_id = self.endpoint_for_machine(machine)?;
        self.endpoint_by_id(&endpoint_id)?
            .snapshot
            .as_deref()?
            .agents
            .iter()
            .find(|agent| agent.pane_id == pane_id)
    }

    /// Whether the pane of `key` still exists; None when its machine is
    /// offline or has no snapshot.
    pub(super) fn pane_exists(&self, key: &str) -> Option<bool> {
        let (machine, pane_id) = key.split_once('/')?;
        let endpoint_id = self.endpoint_for_machine(machine)?;
        let snapshot = self.endpoint_by_id(&endpoint_id)?.snapshot.as_deref()?;
        Some(
            snapshot.panes.iter().any(|pane| pane.pane_id == pane_id)
                || snapshot.agents.iter().any(|agent| agent.pane_id == pane_id),
        )
    }

    /// Section 6.5: status from the agent signals of live panes.
    fn sync_task_status(&mut self, now: Instant) {
        let unix = super::agent_signal::unix_now();
        let mut steps = Vec::new();
        for live in &self.task_rt.live {
            if !live.auto_status {
                continue;
            }
            let Some(key) = live.pane_key.as_deref() else {
                continue;
            };
            let (stamp, signal) = match self.agent_for_pane_key(key) {
                Some(agent) => {
                    let parsed = AgentSignal::parse(agent);
                    let status = agent.agent_status;
                    let mut item = parsed.item(status, unix, u64::MAX, false);
                    // herdr turns Done into Idle once the pane is seen (it may
                    // be the focused pane); the hook's `finished` still says
                    // the turn ended.
                    if item.is_none() && status != AgentStatus::Working && hook_finished(agent) {
                        item = Some(ItemKind::Finished);
                    }
                    ((status, parsed.since()), signal_of(status, item))
                }
                // The pane is gone from an online machine.
                None if self.pane_exists(key) == Some(false) => {
                    ((AgentStatus::Unknown, Some(0)), Signal::Exited)
                }
                None => continue,
            };
            let pending = self.task_rt.pending.get(key);
            match pending {
                Some((seen, _)) if *seen == stamp => {}
                _ => {
                    self.task_rt.pending.insert(key.to_owned(), (stamp, now));
                    continue;
                }
            }
            let stable =
                pending.is_some_and(|(_, at)| now.saturating_duration_since(*at) >= STABLE_FOR);
            if !stable || self.task_rt.stamps.get(key) == Some(&stamp) {
                continue;
            }
            steps.push((key.to_owned(), stamp, live.display_id.clone(), signal));
        }
        for (key, stamp, id, signal) in steps {
            self.task_rt.stamps.insert(key, stamp);
            self.apply_sync(&id, signal);
        }
    }

    /// Applies one stable signal to a task, after reading it again (the
    /// cache may be up to [`RELOAD_EVERY`] old; a human move turns auto off).
    pub(super) fn apply_sync(&mut self, id: &str, signal: Signal) {
        let Ok(Some(detail)) = tasks::read_store(|store| store.task_detail(id)) else {
            return;
        };
        let task = &detail.task;
        let open_decision = detail
            .decision
            .as_ref()
            .is_some_and(|d| d.state == crate::tasks::DecisionState::Open);
        if !task.auto_status {
            return;
        }
        let result = match sync_step(signal, task.status, open_decision) {
            SyncStep::None => return,
            SyncStep::Move(to) => {
                tasks::with_store(|store| store.move_task(id, to, &Actor::Auto, None).map(drop))
            }
            SyncStep::Review => {
                let open: Vec<String> = detail
                    .criteria
                    .iter()
                    .filter(|c| c.state != crate::tasks::CheckState::Passed)
                    .map(|c| c.position.to_string())
                    .collect();
                if open.is_empty() {
                    tasks::with_store(|store| {
                        store
                            .move_task(id, Status::Review, &Actor::Auto, None)
                            .map(drop)
                    })
                } else {
                    let body = format!("finished with criteria {} open", open.join(", "));
                    tasks::with_store(|store| {
                        store
                            .add_entry(id, EntryKind::Event, &body, &Actor::Auto)
                            .map(drop)
                    })
                }
            }
            SyncStep::Entry(body) => tasks::with_store(|store| {
                store
                    .add_entry(id, EntryKind::Event, body, &Actor::Auto)
                    .map(drop)
            }),
        };
        match result {
            Ok(()) => self.task_rt.dirty = true,
            Err(StoreError::Refused(refusal)) => {
                tracing::debug!(task = id, code = refusal.code, "auto move refused")
            }
            Err(error) => tracing::debug!(%error, task = id, "auto move failed"),
        }
    }

    /// Every 30 s: expired decisions get their default ruling (or expire);
    /// rulings reach the agent and a remote waiting CLI.
    fn expire_task_decisions(&mut self, outcome: &mut ClientShellInput) {
        let now = tasks::now_text();
        let changed = match tasks::read_store(|store| store.expire_decisions(&now)) {
            Ok(changed) => changed,
            Err(error) => {
                tracing::debug!(%error, "cannot expire decisions");
                return;
            }
        };
        for decision_id in changed {
            self.task_rt.dirty = true;
            let Ok(Some(decision)) = tasks::read_store(|store| store.decision(decision_id)) else {
                continue;
            };
            if decision.state != crate::tasks::DecisionState::Ruled {
                continue;
            }
            let Some(id) = self.display_id_of(decision.task_id) else {
                continue;
            };
            if super::task_launch::relay_due(&decision, &now) {
                let label = super::task_launch::ruling_label(&decision);
                let text = super::task_launch::ruling_relay(&id, &label);
                self.relay_to_task(&id, &text, outcome);
            }
            self.publish_ruling(&decision, outcome);
        }
    }

    /// The display id of a task row id (closed and archived included).
    pub(super) fn display_id_of(&self, task_id: crate::tasks::TaskId) -> Option<String> {
        let filter = TaskFilter {
            include_archived: true,
            ..TaskFilter::default()
        };
        tasks::read_store(|store| store.list(&filter))
            .ok()?
            .into_iter()
            .find(|card| card.task.id == task_id)
            .map(|card| card.task.display_id)
    }

    /// Starts the next job of every idle machine.
    fn pump_task_jobs(&mut self, outcome: &mut ClientShellInput) {
        let machines: Vec<String> = self.task_rt.queues.keys().cloned().collect();
        for machine in machines {
            let Some(route) = self.task_route(&machine) else {
                // Offline: shell work waits; pulls and probes run again on
                // connect.
                if let Some(queue) = self.task_rt.queues.get_mut(&machine) {
                    queue.probe = false;
                    queue.pull = None;
                }
                continue;
            };
            let Some((job, inflight)) = self.task_rt.next_job(&machine) else {
                continue;
            };
            self.task_rt.busy.insert(machine.clone());
            self.task_rt.inflight.insert(machine, inflight);
            outcome
                .actions
                .push(ClientShellAction::TaskJob { route, job });
        }
        self.task_rt
            .queues
            .retain(|_, queue| queue.probe || queue.pull.is_some() || !queue.writes.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::{NewAttempt, NewTask};

    #[test]
    fn the_sync_table_moves_each_row_and_nothing_else() {
        use Signal::*;
        use Status::*;
        assert_eq!(sync_step(Works, Ready, false), SyncStep::Move(Working));
        assert_eq!(sync_step(Works, Blocked, false), SyncStep::Move(Working));
        assert_eq!(sync_step(Works, Blocked, true), SyncStep::None);
        assert_eq!(sync_step(Waits, Working, false), SyncStep::Move(Blocked));
        assert_eq!(sync_step(Finished, Working, false), SyncStep::Review);
        assert_eq!(
            sync_step(Exited, Working, false),
            SyncStep::Entry("agent exited")
        );
        assert_eq!(
            sync_step(Exited, Blocked, true),
            SyncStep::Entry("agent exited")
        );
        for status in [Triage, Review, Done, Cancelled] {
            for signal in [Works, Waits, Finished, Exited, Other] {
                assert_eq!(
                    sync_step(signal, status, false),
                    SyncStep::None,
                    "{signal:?} {status:?}"
                );
            }
        }
        assert_eq!(sync_step(Works, Working, false), SyncStep::None);
        assert_eq!(sync_step(Other, Working, false), SyncStep::None);
        // Signals from the inbox kinds.
        assert_eq!(signal_of(AgentStatus::Working, None), Works);
        assert_eq!(
            signal_of(AgentStatus::Working, Some(ItemKind::Stuck)),
            Works
        );
        for kind in [
            ItemKind::Permission,
            ItemKind::Question,
            ItemKind::Plan,
            ItemKind::Asks,
            ItemKind::Dialog,
        ] {
            assert_eq!(signal_of(AgentStatus::Blocked, Some(kind)), Waits);
        }
        assert_eq!(
            signal_of(AgentStatus::Done, Some(ItemKind::Finished)),
            Finished
        );
        assert_eq!(signal_of(AgentStatus::Idle, Some(ItemKind::Exited)), Exited);
        assert_eq!(signal_of(AgentStatus::Idle, None), Other);
    }

    fn agent(
        pane: &str,
        status: AgentStatus,
        tokens: &[(&str, String)],
    ) -> crate::protocol::ClientShellAgent {
        crate::protocol::ClientShellAgent {
            pane_id: pane.into(),
            workspace_id: "ws_1".into(),
            tab_id: "tab_1".into(),
            name: None,
            display_agent: None,
            agent: Some("claude".into()),
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: status,
            state_change_seq: 1,
            state_labels: Vec::new(),
            tokens: tokens
                .iter()
                .map(|(name, value)| ((*name).to_owned(), value.clone()))
                .collect(),
            focused: false,
        }
    }

    /// A shell whose local machine has `agents` in workspace ws_1.
    fn shell(agents: Vec<crate::protocol::ClientShellAgent>) -> ClientShellState {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(
            &crate::config::Config::default(),
        ));
        let mut snapshot = super::super::tests::snapshot();
        snapshot.agents = agents;
        state.set_snapshot(Box::new(snapshot));
        state
    }

    fn set_agents(state: &mut ClientShellState, agents: Vec<crate::protocol::ClientShellAgent>) {
        let mut snapshot = super::super::tests::snapshot();
        snapshot.agents = agents;
        state.set_snapshot(Box::new(snapshot));
    }

    fn live_task(status: Status, criteria: &[&str]) -> String {
        let id = tasks::with_store(|store| {
            store.create_task(
                &NewTask {
                    project: "Acme".into(),
                    title: Some("Attention hook".into()),
                    status: Some(status),
                    criteria: criteria.iter().map(|c| (*c).to_owned()).collect(),
                    ..NewTask::default()
                },
                &Actor::Human,
            )
        })
        .expect("task")
        .display_id;
        tasks::with_store(|store| {
            store.start_attempt(
                &id,
                &NewAttempt {
                    harness: "claude".into(),
                    machine: "local".into(),
                    workspace_key: Some("local/ws_1:client-shell".into()),
                    pane_key: Some("local/pane_1".into()),
                    session_id: None,
                },
                &Actor::Human,
            )
        })
        .expect("start");
        id
    }

    fn status(id: &str) -> Status {
        tasks::with_store(|store| store.task(id))
            .expect("read")
            .expect("task")
            .status
    }

    fn tick(state: &mut ClientShellState) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        state.tick_tasks(&mut outcome);
        outcome
    }

    /// Ages the pending stamp past the 2 s rule.
    fn age(state: &mut ClientShellState) {
        for (_, at) in state.task_rt.pending.values_mut() {
            *at -= STABLE_FOR;
        }
    }

    #[test]
    fn a_stable_signal_moves_once_per_stamp_and_auto_off_blocks_it() {
        let id = live_task(Status::Ready, &[]);
        // start_attempt moved it to working; put it back in ready for the test.
        tasks::with_store(|store| store.move_task(&id, Status::Ready, &Actor::Auto, None))
            .expect("ready");
        let working = format!("working|{}", super::super::agent_signal::unix_now());
        let mut state = shell(vec![agent(
            "pane_1",
            AgentStatus::Working,
            &[("drovr_state", working.clone())],
        )]);
        tick(&mut state);
        assert_eq!(status(&id), Status::Ready, "not stable for 2 s yet");
        age(&mut state);
        tick(&mut state);
        assert_eq!(status(&id), Status::Working);

        // The same stamp never moves again, even after a human move back.
        tasks::with_store(|store| store.move_task(&id, Status::Ready, &Actor::Auto, None))
            .expect("ready again");
        age(&mut state);
        tick(&mut state);
        assert_eq!(status(&id), Status::Ready);

        // A waiting prompt (new stamp) blocks a working task.
        tasks::with_store(|store| store.move_task(&id, Status::Working, &Actor::Auto, None))
            .expect("working");
        set_agents(
            &mut state,
            vec![agent(
                "pane_1",
                AgentStatus::Blocked,
                &[
                    ("drovr_state", working.clone()),
                    ("drovr_wait", "permission|ab||x".into()),
                ],
            )],
        );
        state.task_rt.dirty = true;
        tick(&mut state);
        age(&mut state);
        tick(&mut state);
        assert_eq!(status(&id), Status::Blocked);

        // Auto off: a working signal leaves it alone.
        tasks::with_store(|store| {
            store.update_task(
                &id,
                &crate::tasks::TaskPatch {
                    auto_status: Some(false),
                    ..Default::default()
                },
                &Actor::Human,
            )
        })
        .expect("auto off");
        set_agents(
            &mut state,
            vec![agent(
                "pane_1",
                AgentStatus::Working,
                &[(
                    "drovr_state",
                    format!("working|{}", super::super::agent_signal::unix_now() + 5),
                )],
            )],
        );
        state.task_rt.dirty = true;
        tick(&mut state);
        age(&mut state);
        tick(&mut state);
        assert_eq!(status(&id), Status::Blocked);
    }

    #[test]
    fn finishing_with_open_criteria_writes_an_entry_instead_of_review() {
        let id = live_task(Status::Ready, &["schema added", "docs updated"]);
        assert_eq!(status(&id), Status::Working);
        // The focused pane's Done reads as Idle once seen; the hook's
        // `finished` carries it.
        let finished = format!("finished|{}", super::super::agent_signal::unix_now());
        let mut state = shell(vec![agent(
            "pane_1",
            AgentStatus::Idle,
            &[("drovr_state", finished)],
        )]);
        tick(&mut state);
        age(&mut state);
        tick(&mut state);
        assert_eq!(status(&id), Status::Working);
        let detail = tasks::with_store(|store| store.task_detail(&id))
            .expect("read")
            .expect("task");
        let bodies: Vec<&str> = detail.entries.iter().map(|e| e.body.as_str()).collect();
        assert!(
            bodies.contains(&"finished with criteria 1, 2 open"),
            "{bodies:?}"
        );

        // All passed: the next finished stamp moves it to review.
        for position in [1, 2] {
            tasks::with_store(|store| {
                store.check_criterion(
                    &id,
                    position,
                    crate::tasks::CheckState::Passed,
                    Some("ok"),
                    &Actor::Human,
                )
            })
            .expect("check");
        }
        state.apply_sync(&id, Signal::Finished);
        assert_eq!(status(&id), Status::Review);
    }

    #[test]
    fn a_gone_pane_writes_agent_exited() {
        let id = live_task(Status::Ready, &[]);
        let mut state = shell(Vec::new());
        // pane_1 exists in the snapshot (no agent yet): nothing happens.
        tick(&mut state);
        age(&mut state);
        tick(&mut state);
        let mut snapshot = super::super::tests::snapshot();
        snapshot.panes.clear();
        state.set_snapshot(Box::new(snapshot));
        tick(&mut state);
        age(&mut state);
        tick(&mut state);
        let detail = tasks::with_store(|store| store.task_detail(&id))
            .expect("read")
            .expect("task");
        let exits = detail
            .entries
            .iter()
            .filter(|e| e.body == "agent exited")
            .count();
        assert_eq!(exits, 1);
    }

    #[test]
    fn only_a_workspace_missing_30_s_on_the_same_boot_ends_the_attempt() {
        let id = live_task(Status::Ready, &[]);
        let mut state = shell(Vec::new());
        tick(&mut state);
        let mut snapshot = super::super::tests::snapshot();
        snapshot.workspaces.clear();
        snapshot.panes.clear();
        state.set_snapshot(Box::new(snapshot.clone()));
        state.task_rt.dirty = true;
        tick(&mut state);
        assert!(state.task_rt.missing.contains_key(&id));
        // A restart (new boot id) resets the clock.
        snapshot.boot_id = "boot-2".into();
        state.set_snapshot(Box::new(snapshot.clone()));
        let back = Instant::now() - GONE_FOR;
        state.task_rt.missing.insert(id.clone(), back);
        state.task_rt.dirty = true;
        tick(&mut state);
        assert_eq!(status(&id), Status::Working);
        // Offline (no snapshot route): nothing ends either.
        state.endpoints[0].status = ClientEndpointStatus::Reconnecting;
        // Local is always reachable; the clock is still fresh after the reset.
        state.task_rt.dirty = true;
        tick(&mut state);
        assert_eq!(status(&id), Status::Working);
        // 30 s on the same boot: ended and back to ready.
        state
            .task_rt
            .missing
            .insert(id.clone(), Instant::now() - GONE_FOR);
        state.task_rt.dirty = true;
        tick(&mut state);
        assert_eq!(status(&id), Status::Ready);
        let detail = tasks::with_store(|store| store.task_detail(&id))
            .expect("read")
            .expect("task");
        assert!(detail.attempts.iter().all(|a| a.ended_at.is_some()));
        assert_eq!(detail.task.workspace_key, None, "the link goes too");
    }

    #[test]
    fn a_gone_workspace_unlinks_a_task_without_an_attempt_and_closing_unlinks() {
        let id = live_task(Status::Ready, &[]);
        let key = |id: &str| {
            tasks::with_store(|store| store.task(id))
                .expect("read")
                .expect("task")
                .workspace_key
        };
        // Released by hand (the attempt ends) but still linked.
        tasks::with_store(|store| store.release(&id, "later", &Actor::Human)).expect("release");
        assert!(key(&id).is_some());
        let mut state = shell(Vec::new());
        tick(&mut state);
        let mut snapshot = super::super::tests::snapshot();
        snapshot.workspaces.clear();
        snapshot.panes.clear();
        state.set_snapshot(Box::new(snapshot));
        state.task_rt.dirty = true;
        tick(&mut state);
        assert!(key(&id).is_some(), "not before 30 s");
        state
            .task_rt
            .missing
            .insert(id.clone(), Instant::now() - GONE_FOR);
        state.task_rt.dirty = true;
        tick(&mut state);
        assert_eq!(key(&id), None);
        // Closing a task drops its link at once.
        let other = live_task(Status::Ready, &[]);
        tasks::with_store(|store| store.move_task(&other, Status::Cancelled, &Actor::Human, None))
            .expect("cancel");
        assert_eq!(key(&other), None);
    }

    #[test]
    fn decision_notices_skip_the_first_scan_and_show_each_new_one() {
        let id = live_task(Status::Ready, &[]);
        let ask = |title: &str| {
            tasks::with_store(|store| {
                store.request_decision(
                    &id,
                    &crate::tasks::NewDecision {
                        title: title.into(),
                        summary: String::new(),
                        choices: vec![crate::tasks::Choice {
                            id: "a".into(),
                            label: "A".into(),
                            consequence: None,
                            recommended: false,
                        }],
                        allow_text: true,
                        default_choice: None,
                        expires_at: None,
                        wait_until: None,
                    },
                    &Actor::Agent("claude@local".into()),
                )
            })
            .expect("decision")
        };
        let first = ask("Which table?");
        let mut state = shell(Vec::new());
        tick(&mut state);
        assert!(
            state.visible_endpoint_notice.is_none(),
            "start-up scan is silent"
        );
        tasks::with_store(|store| {
            store.rule_decision(
                first.id,
                &crate::tasks::Ruling::Choice("a".into()),
                "panel",
                &Actor::Human,
            )
        })
        .expect("rule");
        ask("Run the migration?");
        state.task_rt.dirty = true;
        tick(&mut state);
        let notice = state.visible_endpoint_notice.as_ref().expect("notice");
        assert_eq!(notice.body, format!("{id} asks: Run the migration?"));
    }

    #[test]
    fn usage_tokens_reach_the_live_attempt() {
        let id = live_task(Status::Ready, &[]);
        let mut state = shell(vec![agent(
            "pane_1",
            AgentStatus::Working,
            &[
                ("drovr_session", "sess-1".into()),
                ("drovr_u_20261002", "100,20,5,5,3".into()),
                ("drovr_u_20261003", "50,10,0,0,1".into()),
            ],
        )]);
        tick(&mut state);
        let detail = tasks::with_store(|store| store.task_detail(&id))
            .expect("read")
            .expect("task");
        let attempt = &detail.attempts[0];
        assert_eq!(attempt.tokens_in, Some(150));
        assert_eq!(attempt.tokens_out, Some(30));
        assert_eq!(attempt.session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn jobs_queue_one_per_machine_writes_first() {
        let mut rt = TaskRuntime::default();
        rt.queue_pull("mato", Some("p1"));
        rt.queue_pull("mato", Some("p2"));
        rt.queue_probe("mato");
        rt.queue_write("mato", "echo a".into(), Some(4));
        rt.queue_write("mato", "echo b".into(), None);
        rt.applied.insert("mato/p1/e1".into(), 7);
        rt.applied.insert("mato/p9/e1".into(), 3);
        rt.applied.insert("local/p1/e1".into(), 2);
        let Some((TaskJob::WriteFiles { script, .. }, Inflight::Write { launches })) =
            rt.next_job("mato")
        else {
            panic!("a write first");
        };
        assert_eq!(script, "echo a\necho b");
        assert_eq!(launches, vec![4]);
        rt.busy.insert("mato".into());
        assert!(rt.next_job("mato").is_none(), "one job per machine");
        rt.busy.clear();
        assert!(matches!(
            rt.next_job("mato"),
            Some((TaskJob::Probe { .. }, Inflight::Probe))
        ));
        let Some((TaskJob::Pull { panes, applied, .. }, _)) = rt.next_job("mato") else {
            panic!("then the pull");
        };
        assert_eq!(panes, Some(vec!["p1".to_owned(), "p2".to_owned()]));
        assert_eq!(applied, vec![("p1".to_owned(), "e1".to_owned(), 7)]);
        // A sweep covers every pane.
        rt.queue_pull("mato", Some("p1"));
        rt.queue_pull("mato", None);
        rt.queue_pull("mato", Some("p2"));
        let Some((TaskJob::Pull { panes, .. }, _)) = rt.next_job("mato") else {
            panic!("sweep");
        };
        assert_eq!(panes, None);
        assert!(rt.next_job("mato").is_none());
    }
}
