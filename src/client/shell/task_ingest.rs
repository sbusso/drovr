//! drovr fork: background jobs of the tasks feature and the outbox ingest
//! (docs/design/tasks.md, sections 5.1, 6.3 and 6.6).
//!
//! Every shell or API call to a machine runs on a background thread: the
//! SSH bridge blocks for up to 15 s. A job's answer comes back to the loop
//! as [`TaskJobDone`] and lands in [`ClientShellState::receive_task_job`].
//!
//! Remote agents write each `drovr task` op to a file under
//! `R/task-outbox/{pane}/{seq}.json` on their own machine; a pull prints the
//! files, the client applies them with `apply_once`, then one file job writes
//! the reply files and snapshots and removes the applied op files. The op
//! files go only after the database commit, so a crash in between re-pulls
//! ops that `apply_once` skips.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use super::inbox::ApiRoute;
use super::projects;
use super::*;
use crate::remote::shell_quote;
use crate::tasks::ops::{parse_outbox_line, OutboxParse, OUTBOX_V};
use crate::tasks::{self, OpContext, OpResult, StoreError, StoreResult, TaskDetail, TaskOp};

/// Root of the tasks files on a remote machine (section 6.3's `R`), as POSIX
/// shell. The remote CLI resolves the same path from a release build.
const REMOTE_ROOT: &str =
    r#"R="${DROVR_TASK_OUTBOX_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/herdr/drovr}""#;
/// At most this many op files per pull.
const PULL_LIMIT: usize = 200;
/// Seconds an API call of a job may take.
const API_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// A job on one machine. `Pull`, `WriteFiles` and `Probe` run shell (`/bin/sh`
/// locally, the SSH bridge remotely), at most one per machine at a time;
/// `Api` runs herdr API calls in order and does not wait for the shell job.
#[derive(Debug)]
pub(crate) enum TaskJob {
    /// Print every outbox file of these panes (None = all panes) above the
    /// given applied seq per (pane, epoch); at most 200 files.
    Pull {
        machine: String,
        panes: Option<Vec<String>>,
        applied: Vec<(String, String, u64)>,
    },
    /// Write reply, snapshot and context files, then remove the applied op
    /// files and every op file at or below the applied seq. Prints `R`.
    WriteFiles { machine: String, script: String },
    /// `drovr task proto` on the machine (section 6.6).
    Probe { machine: String },
    /// herdr API calls, in order (workspace.create on a machine that is not
    /// the active one, typing the agent command, prompts and relays).
    Api {
        machine: String,
        methods: Vec<crate::api::schema::Method>,
    },
}

impl TaskJob {
    fn machine(&self) -> &str {
        match self {
            Self::Pull { machine, .. }
            | Self::WriteFiles { machine, .. }
            | Self::Probe { machine }
            | Self::Api { machine, .. } => machine,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Pull { .. } => "pull",
            Self::WriteFiles { .. } => "write",
            Self::Probe { .. } => "probe",
            Self::Api { .. } => "api",
        }
    }

    /// The shell script of a shell job.
    fn script(&self) -> Option<String> {
        match self {
            Self::Pull {
                machine,
                panes,
                applied,
            } => Some(pull_script(machine == "local", panes.as_deref(), applied)),
            Self::WriteFiles { script, .. } => Some(script.clone()),
            Self::Probe { .. } => Some(PROBE_SCRIPT.to_owned()),
            Self::Api { .. } => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct TaskJobDone {
    pub machine: String,
    pub kind: &'static str,
    pub result: Result<String, String>,
}

type LoopEvents = tokio::sync::mpsc::Sender<crate::client::events::ClientLoopEvent>;

/// Runs the job (local: /bin/sh; remote: bridge.run_sh) and posts
/// TaskJobDone to the loop. Without `events` (tests) the result is dropped.
pub(crate) fn run_job(route: ApiRoute, job: TaskJob, events: Option<LoopEvents>) {
    std::thread::spawn(move || {
        let done = execute(&route, job);
        if let Err(error) = &done.result {
            tracing::info!(machine = %done.machine, kind = done.kind, %error, "tasks job failed");
        }
        if let Some(events) = events {
            let _ = events.blocking_send(crate::client::events::ClientLoopEvent::TaskJobDone(done));
        }
    });
}

fn execute(route: &ApiRoute, job: TaskJob) -> TaskJobDone {
    let machine = job.machine().to_owned();
    let kind = job.kind();
    let result = match (job.script(), job) {
        (Some(script), _) => run_sh(route, &script),
        (None, TaskJob::Api { methods, .. }) => run_api(route, methods),
        (None, _) => Err("nothing to run".into()),
    };
    TaskJobDone {
        machine,
        kind,
        result,
    }
}

/// stdout of a script that exits 0, else the error.
pub(super) fn run_sh(route: &ApiRoute, script: &str) -> Result<String, String> {
    let output = match route {
        ApiRoute::Local => std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::null())
            .output(),
        ApiRoute::Remote(bridge) => bridge.run_sh(script),
    }
    .map_err(|error| error.to_string())?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(if stderr.is_empty() {
        format!("exit {}", output.status.code().unwrap_or(-1))
    } else {
        stderr
    })
}

fn run_api(route: &ApiRoute, methods: Vec<crate::api::schema::Method>) -> Result<String, String> {
    let client = match route {
        ApiRoute::Local => Ok(crate::api::client::ApiClient::local()),
        ApiRoute::Remote(bridge) => bridge.api_client(),
    }
    .map_err(|error| error.to_string())?;
    for (index, method) in methods.into_iter().enumerate() {
        let request = crate::api::schema::Request {
            id: format!("drovr:task:{index}"),
            method,
        };
        let value = client
            .request_value_with_timeout(&request, API_TIMEOUT)
            .map_err(|error| error.to_string())?;
        if let Some(error) = value.get("error") {
            return Err(error["message"]
                .as_str()
                .unwrap_or("request failed")
                .to_owned());
        }
    }
    Ok(String::new())
}

// ------------------------------------------------------------- scripts

/// Section 6.6: the remote `drovr task proto`.
const PROBE_SCRIPT: &str =
    r#"drovr=$(command -v drovr || echo "$HOME/.local/bin/drovr"); "$drovr" task proto"#;

/// The answer of a probe: `drovr-task {n}`, else None.
pub(super) fn parse_probe(result: &Result<String, String>) -> Option<u32> {
    let output = result.as_ref().ok()?;
    let mut words = output.split_whitespace();
    (words.next()? == "drovr-task").then_some(())?;
    let version = words.next()?.parse().ok()?;
    words.next().is_none().then_some(version)
}

/// The tasks root on this Mac: where the local CLI queues ops
/// (`state_dir()/drovr` unless `$DROVR_TASK_OUTBOX_DIR`).
pub(super) fn local_root() -> PathBuf {
    crate::tasks::outbox::root()
}

/// The `R=...` line for a machine's scripts.
pub(super) fn root_line(local: bool) -> String {
    if local {
        format!("R={}", shell_quote(&local_root().to_string_lossy()))
    } else {
        REMOTE_ROOT.to_owned()
    }
}

/// Prints `{pane}\t{epoch}\t{seq}\t{json}` for each op file above the
/// applied seq of its (pane, epoch), in numeric order per pane, at most
/// [`PULL_LIMIT`] lines. `panes` None lists every pane directory (a sweep).
pub(super) fn pull_script(
    local: bool,
    panes: Option<&[String]>,
    applied: &[(String, String, u64)],
) -> String {
    let dirs = match panes {
        None => r#""$O"/*/"#.to_owned(),
        Some(panes) => panes
            .iter()
            .map(|pane| format!(r#""$O"/{}/"#, shell_quote(pane)))
            .collect::<Vec<_>>()
            .join(" "),
    };
    let cases = applied
        .iter()
        .map(|(pane, epoch, seq)| {
            format!(
                "    {}) a={seq} ;;\n",
                shell_quote(&format!("{pane} {epoch}"))
            )
        })
        .collect::<String>();
    format!(
        r#"{root}
O="$R/task-outbox"
[ -d "$O" ] || exit 0
n=0
for d in {dirs}; do
  [ -d "$d" ] || continue
  d=${{d%/}}
  p=${{d##*/}}
  e=$(cat "$d/epoch" 2>/dev/null) || continue
  a=0
  case "$p $e" in
{cases}  esac
  for s in $(ls "$d" | sed -n 's/^\([0-9][0-9]*\)\.json$/\1/p' | sort -n); do
    [ "$s" -gt "$a" ] || continue
    [ "$n" -lt {PULL_LIMIT} ] || exit 0
    printf '%s\t%s\t%s\t%s\n' "$p" "$e" "$s" "$(tr -d '\n' < "$d/$s.json")"
    n=$((n+1))
  done
done
"#,
        root = root_line(local),
    )
}

/// One file job: writes and removals collected for one machine, run as one
/// script that ends by printing `R`.
#[derive(Debug, Default)]
pub(super) struct FileScript {
    parts: Vec<String>,
}

impl FileScript {
    pub(super) fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// Writes `content` to `R/{rel}` through a temporary file and a rename.
    /// `rel` is made of display ids, pane ids and fixed names; it is quoted
    /// anyway.
    pub(super) fn write(&mut self, rel: &str, content: &str) {
        let (dir, name) = rel.rsplit_once('/').unwrap_or(("", rel));
        let dir_q = shell_quote(dir);
        let tmp = shell_quote(&format!("{dir}/.{name}.tmp"));
        let target = shell_quote(rel);
        let body = content.strip_suffix('\n').unwrap_or(content);
        let mut tag = String::from("DROVR_EOF");
        while body.lines().any(|line| line == tag) {
            tag.push('_');
        }
        self.parts.push(format!(
            "mkdir -p \"$R/\"{dir_q} && cat > \"$R/\"{tmp} <<'{tag}' && mv -f \"$R/\"{tmp} \"$R/\"{target}\n{body}\n{tag}\n"
        ));
    }

    /// Removes the op files of (pane, epoch) at or below `seq`, after moving
    /// the `bad` ones aside, only while the directory still has that epoch.
    pub(super) fn clean_outbox(&mut self, pane: &str, epoch: &str, seq: u64, bad: &[u64]) {
        let moves = bad
            .iter()
            .map(|seq| format!("  mv -f \"$d/{seq}.json\" \"$d/{seq}.bad\" 2>/dev/null\n"))
            .collect::<String>();
        self.parts.push(format!(
            r#"d="$R/task-outbox/"{pane}
if [ "$(cat "$d/epoch" 2>/dev/null)" = {epoch} ]; then
{moves}  for f in "$d"/*.json; do
    s=${{f##*/}}; s=${{s%.json}}
    case "$s" in ''|*[!0-9]*) continue ;; esac
    if [ "$s" -le {seq} ]; then rm -f "$f"; fi
  done
fi
"#,
            pane = shell_quote(pane),
            epoch = shell_quote(epoch),
        ));
    }

    /// The script, with the removal of reply files older than a day.
    pub(super) fn finish(self, local: bool) -> String {
        let mut script = root_line(local);
        script.push('\n');
        for part in self.parts {
            script.push_str(&part);
        }
        script.push_str(
            "[ -d \"$R/task-reply\" ] && find \"$R/task-reply\" -type f -mtime +0 -exec rm -f {} + 2>/dev/null\nprintf '%s\\n' \"$R\"\n",
        );
        script
    }
}

/// A reply file's JSON: the OpResult plus the CLI's exit code.
pub(super) fn reply_json(result: &OpResult, exit: i32) -> String {
    let mut value = serde_json::to_value(result).unwrap_or_default();
    if let Some(object) = value.as_object_mut() {
        object.insert("exit".into(), exit.into());
    }
    value.to_string()
}

/// The OpResult the CLI prints for a store error.
fn error_result(task: Option<String>, error: &StoreError) -> OpResult {
    OpResult {
        ok: false,
        task,
        status: None,
        message: error.to_string(),
        code: Some(match error {
            StoreError::Refused(refusal) => refusal.code.to_owned(),
            StoreError::NotFound(_) => "not_found".into(),
            StoreError::Invalid(_) => "invalid".into(),
            StoreError::Busy => "busy".into(),
            StoreError::TooNew { .. } => "too_new".into(),
            StoreError::Sqlite(_) => "error".into(),
        }),
        decision_id: None,
    }
}

/// Snapshot and context files of a task on a machine: `tasks/{id}.json`
/// (remote only; `drovr task show` and `verify` read it) and `tasks/{id}.md`.
pub(super) fn task_files(script: &mut FileScript, detail: &TaskDetail, local: bool) {
    let id = &detail.task.display_id;
    if !local {
        if let Ok(json) = serde_json::to_string(detail) {
            script.write(&format!("tasks/{id}.json"), &json);
        }
    }
    script.write(
        &format!("tasks/{id}.md"),
        &super::task_launch::context_text(detail),
    );
}

// --------------------------------------------------------------- ingest

/// One line of a pull.
#[derive(Debug, PartialEq)]
pub(super) struct Pulled {
    pub(super) pane: String,
    pub(super) epoch: String,
    pub(super) seq: u64,
    pub(super) json: String,
}

/// Lines of a pull; malformed lines are skipped.
pub(super) fn parse_pull(output: &str) -> Vec<Pulled> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(4, '\t');
            Some(Pulled {
                pane: fields.next()?.to_owned(),
                epoch: fields.next()?.to_owned(),
                seq: fields.next()?.parse().ok()?,
                json: fields.next()?.to_owned(),
            })
        })
        .collect()
}

/// The outcome of applying one pull.
#[derive(Debug, Default)]
pub(super) struct Ingest {
    /// Files to write and remove.
    pub(super) script: FileScript,
    /// Display ids whose snapshot and context changed.
    pub(super) changed: Vec<String>,
    pub(super) notices: Vec<String>,
    /// A busy store stopped the batch; the rest stays for the next pull.
    pub(super) stopped: bool,
    /// (pane, epoch, seq): the highest op handled per pane, whose files the
    /// script removes.
    pub(super) handled: Vec<(String, String, u64)>,
}

/// The pane's detected agent and workspace key, for the op context.
pub(super) struct PaneInfo {
    pub(super) agent: Option<String>,
    pub(super) workspace_key: Option<String>,
}

/// `apply_once(source, seq, op, ctx)` against the store.
pub(super) type ApplyOp<'a> =
    dyn FnMut(&str, u64, &TaskOp, &OpContext) -> StoreResult<Option<OpResult>> + 'a;

/// Applies the ops of one pull (section 6.3, steps 1 to 3). `pane_info`
/// looks up a pane on the machine (None when it is gone).
pub(super) fn ingest(
    machine: &str,
    pulled: &[Pulled],
    pane_info: &dyn Fn(&str) -> Option<PaneInfo>,
    apply: &mut ApplyOp<'_>,
    link: &mut dyn FnMut(&str, &str),
) -> Ingest {
    let mut out = Ingest::default();
    // (pane, epoch) -> (highest processed seq, bad seqs), in pull order.
    let mut done: BTreeMap<(String, String), (u64, Vec<u64>)> = BTreeMap::new();
    let mut held: HashSet<(String, String)> = HashSet::new();
    let mut changed = HashSet::new();
    let mut newer_noted = false;
    for line in pulled {
        let group = (line.pane.clone(), line.epoch.clone());
        if held.contains(&group) {
            continue;
        }
        let op = match parse_outbox_line(&line.json) {
            Ok(op) => op,
            Err(OutboxParse::Newer(_)) => {
                // Later ops of this pane wait behind it.
                held.insert(group);
                if !newer_noted {
                    newer_noted = true;
                    out.notices.push(format!(
                        "{machine} runs a newer drovr task; update drovr on this Mac"
                    ));
                }
                continue;
            }
            Err(OutboxParse::Bad(_)) => {
                let entry = done.entry(group).or_default();
                entry.0 = entry.0.max(line.seq);
                entry.1.push(line.seq);
                out.notices
                    .push(format!("bad task op from {machine}/{}", line.pane));
                continue;
            }
        };
        let info = pane_info(&line.pane);
        let agent = info
            .as_ref()
            .and_then(|info| info.agent.clone())
            .unwrap_or_else(|| "agent".into());
        let ctx = OpContext {
            actor: tasks::Actor::Agent(format!("{agent}@{machine}")),
            machine: machine.to_owned(),
            pane_key: Some(format!("{machine}/{}", line.pane)),
        };
        let source = format!("{machine}/{}/{}", line.pane, line.epoch);
        let result = apply(&source, line.seq, &op.op, &ctx);
        let reply = match result {
            Ok(None) => None,
            Ok(Some(result)) => {
                if let (TaskOp::Start { .. }, Some(task), Some(key)) = (
                    &op.op,
                    result.task.as_deref(),
                    info.as_ref().and_then(|info| info.workspace_key.as_deref()),
                ) {
                    link(task, key);
                }
                Some((result, 0))
            }
            Err(StoreError::Busy | StoreError::Sqlite(_) | StoreError::TooNew { .. }) => {
                out.stopped = true;
                break;
            }
            Err(error) => Some((error_result(op_task(&op.op), &error), exit_for(&error))),
        };
        if let Some((result, exit)) = reply {
            if let Some(task) = &result.task {
                changed.insert(task.clone());
            }
            out.script.write(
                &format!("task-reply/{}-{}-{}.json", line.pane, line.epoch, line.seq),
                &reply_json(&result, exit),
            );
        }
        let entry = done.entry(group).or_default();
        entry.0 = entry.0.max(line.seq);
    }
    for ((pane, epoch), (seq, bad)) in done {
        out.script.clean_outbox(&pane, &epoch, seq, &bad);
        out.handled.push((pane, epoch, seq));
    }
    let mut changed: Vec<String> = changed.into_iter().collect();
    changed.sort();
    out.changed = changed;
    out
}

/// The CLI exit code of a store error (`ops::exit_code`).
fn exit_for(error: &StoreError) -> i32 {
    match error {
        StoreError::Refused(_) => 3,
        StoreError::NotFound(_) => 4,
        StoreError::Invalid(_) => 2,
        _ => 1,
    }
}

/// The display id an op names, if any.
fn op_task(op: &TaskOp) -> Option<String> {
    match op {
        TaskOp::Add { .. } => None,
        TaskOp::Update { task, .. }
        | TaskOp::Status { task, .. }
        | TaskOp::Start { task, .. }
        | TaskOp::Note { task, .. }
        | TaskOp::Criteria { task, .. }
        | TaskOp::Check { task, .. }
        | TaskOp::Artifact { task, .. }
        | TaskOp::Done { task, .. }
        | TaskOp::Release { task, .. }
        | TaskOp::Decide { task, .. }
        | TaskOp::Withdraw { task } => task.clone(),
    }
}

impl ClientShellState {
    /// The route of a machine's jobs; None when it is offline or has no
    /// bridge.
    pub(super) fn task_route(&self, machine: &str) -> Option<ApiRoute> {
        let endpoint_id = self.endpoint_for_machine(machine)?;
        if endpoint_id.is_local() {
            return Some(ApiRoute::Local);
        }
        self.endpoint_by_id(&endpoint_id)?
            .bridge
            .clone()
            .map(ApiRoute::Remote)
    }

    /// A job finished (section 6.3). True when something drawn changed.
    pub(crate) fn receive_task_job(&mut self, done: TaskJobDone) -> bool {
        let machine = done.machine.clone();
        if done.kind == "api" {
            if let Err(error) = &done.result {
                return self.push_task_notice(format!("{machine}: {error}"));
            }
            return false;
        }
        self.task_rt.busy.remove(&machine);
        let inflight = self.task_rt.inflight.remove(&machine);
        match done.kind {
            "probe" => self.receive_probe(&machine, parse_probe(&done.result)),
            "pull" => match done.result {
                Ok(output) => self.receive_pull(&machine, &output),
                Err(_) => false,
            },
            "write" => {
                let launches = match inflight {
                    Some(super::task_sync::Inflight::Write { launches }) => launches,
                    _ => Vec::new(),
                };
                self.receive_written(&machine, &launches, done.result)
            }
            _ => false,
        }
    }

    fn receive_probe(&mut self, machine: &str, version: Option<u32>) -> bool {
        self.task_rt.probe.insert(machine.to_owned(), version);
        if version.is_some_and(|version| version < OUTBOX_V)
            && self.task_rt.notified.insert(format!("old:{machine}"))
        {
            return self.push_task_notice(format!("{machine} runs an older drovr task; update it"));
        }
        false
    }

    /// Applies a pull and queues the file job that answers it.
    fn receive_pull(&mut self, machine: &str, output: &str) -> bool {
        let pulled = parse_pull(output);
        if pulled.is_empty() {
            return false;
        }
        let endpoint = self
            .endpoint_for_machine(machine)
            .and_then(|endpoint_id| self.endpoint_by_id(&endpoint_id));
        let pane_info = |pane: &str| -> Option<PaneInfo> {
            let endpoint = endpoint?;
            let snapshot = endpoint.snapshot.as_deref()?;
            let agent = snapshot.agents.iter().find(|agent| agent.pane_id == pane);
            let workspace_id = agent.map(|agent| agent.workspace_id.clone()).or_else(|| {
                snapshot
                    .panes
                    .iter()
                    .find(|p| p.pane_id == pane)
                    .map(|p| p.workspace_id.clone())
            })?;
            Some(PaneInfo {
                agent: agent.and_then(|agent| agent.agent.clone()),
                workspace_key: snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.workspace_id == workspace_id)
                    .map(|workspace| projects::workspace_key(endpoint, workspace)),
            })
        };
        let mut apply = |source: &str, seq: u64, op: &TaskOp, ctx: &OpContext| {
            tasks::with_store(|store| store.apply_once(source, seq, op, ctx))
        };
        let mut link = |task: &str, key: &str| {
            if let Err(error) = tasks::with_store(|store| store.link_workspace(task, Some(key))) {
                tracing::debug!(%error, task, "cannot link the task's workspace");
            }
        };
        let mut result = ingest(machine, &pulled, &pane_info, &mut apply, &mut link);
        for (pane, epoch, seq) in &result.handled {
            let slot = self
                .task_rt
                .applied
                .entry(format!("{machine}/{pane}/{epoch}"))
                .or_default();
            *slot = (*slot).max(*seq);
        }
        // Snapshots and context files of the tasks the ops changed.
        let local = machine == "local";
        for id in &result.changed {
            if let Ok(Some(detail)) = tasks::read_store(|store| store.task_detail(id)) {
                task_files(&mut result.script, &detail, local);
                self.task_rt
                    .written
                    .insert(id.clone(), (machine.to_owned(), detail.task.version));
            }
        }
        self.task_rt.dirty = true;
        let mut repaint = false;
        for notice in result.notices {
            if self.task_rt.notified.insert(format!("{machine}:{notice}")) {
                repaint |= self.push_task_notice(notice);
            }
        }
        if !result.script.is_empty() {
            self.task_rt
                .queue_write(machine, result.script.finish(local), None);
        }
        repaint | !result.changed.is_empty()
    }

    /// A file job answered: launches waiting on their context file go on.
    fn receive_written(
        &mut self,
        machine: &str,
        launches: &[u64],
        result: Result<String, String>,
    ) -> bool {
        let mut repaint = false;
        let root = result.as_ref().ok().map(|out| out.trim().to_owned());
        let mut failed = Vec::new();
        for launch in self
            .task_rt
            .launches
            .iter_mut()
            .filter(|launch| launches.contains(&launch.id))
        {
            match &root {
                Some(root) if !root.is_empty() => {
                    launch.context_path = Some(format!("{root}/tasks/{}.md", launch.display_id));
                    launch.stage = super::task_launch::LaunchStage::Create;
                }
                _ => failed.push(launch.id),
            }
        }
        if !failed.is_empty() {
            let error = result.err().unwrap_or_else(|| "no answer".into());
            self.task_rt
                .launches
                .retain(|launch| !failed.contains(&launch.id));
            repaint |=
                self.push_task_notice(format!("cannot write context file on {machine}: {error}"));
        }
        repaint
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::ops::OutboxOp;
    use crate::tasks::{Actor, NewTask, Status};

    /// A unique empty directory under the system temp dir.
    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "drovr-task-ingest-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Runs a script with `/bin/sh` and `DROVR_TASK_OUTBOX_DIR` at `root`.
    fn sh(root: &std::path::Path, script: &str) -> String {
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env("DROVR_TASK_OUTBOX_DIR", root)
            .output()
            .expect("run sh");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn op_line(epoch: &str, seq: u64, pane: &str, task: &str, body: &str) -> String {
        serde_json::to_string(&OutboxOp {
            v: OUTBOX_V,
            epoch: epoch.into(),
            seq,
            ts: 1_791_100_000,
            pane: pane.into(),
            op: TaskOp::Note {
                task: Some(task.into()),
                body: body.into(),
            },
        })
        .expect("serialize")
    }

    fn outbox(root: &std::path::Path, pane: &str, epoch: &str, files: &[(u64, String)]) {
        let dir = root.join("task-outbox").join(pane);
        std::fs::create_dir_all(&dir).expect("outbox dir");
        std::fs::write(dir.join("epoch"), format!("{epoch}\n")).expect("epoch");
        for (seq, line) in files {
            std::fs::write(dir.join(format!("{seq}.json")), format!("{line}\n")).expect("op");
        }
    }

    fn names(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn pull_script_quotes_paths_sorts_numerically_and_caps() {
        let script = pull_script(
            false,
            Some(&["p1".into(), "p 2".into()]),
            &[("p1".into(), "k3f9q2".into(), 7)],
        );
        assert!(script.contains(r#""$O"/p1/ "$O"/'p 2'/"#), "{script}");
        assert!(script.contains("'p1 k3f9q2') a=7 ;;"), "{script}");
        assert!(script.contains("sort -n"));
        assert!(script.contains("-lt 200"));
        assert!(script.starts_with(REMOTE_ROOT));
        let sweep = pull_script(false, None, &[]);
        assert!(sweep.contains(r#"for d in "$O"/*/; do"#), "{sweep}");

        let root = temp_root("pull");
        let lines: Vec<(u64, String)> = (1..=12)
            .map(|seq| {
                (
                    seq,
                    op_line("k3f9q2", seq, "p1", "AC-1", &format!("n{seq}")),
                )
            })
            .collect();
        outbox(&root, "p1", "k3f9q2", &lines);
        outbox(
            &root,
            "p2",
            "zz0000",
            &[(3, op_line("zz0000", 3, "p2", "AC-1", "x"))],
        );
        // A temporary file is never listed.
        std::fs::write(root.join("task-outbox/p1/.13.tmp"), "{}").expect("tmp");
        let out = sh(
            &root,
            &pull_script(false, None, &[("p1".into(), "k3f9q2".into(), 7)]),
        );
        let pulled = parse_pull(&out);
        let seqs: Vec<(String, u64)> = pulled.iter().map(|p| (p.pane.clone(), p.seq)).collect();
        assert_eq!(
            seqs,
            vec![
                ("p1".into(), 8),
                ("p1".into(), 9),
                ("p1".into(), 10),
                ("p1".into(), 11),
                ("p1".into(), 12),
                ("p2".into(), 3),
            ]
        );
        assert_eq!(pulled[0].json, lines[7].1);
        // Only listed panes.
        let out = sh(&root, &pull_script(false, Some(&["p2".into()]), &[]));
        assert_eq!(parse_pull(&out).len(), 1);
        // Cap.
        let many: Vec<(u64, String)> = (1..=205)
            .map(|seq| (seq, op_line("e1", seq, "p9", "AC-1", "x")))
            .collect();
        outbox(&root, "p9", "e1", &many);
        let out = sh(&root, &pull_script(false, Some(&["p9".into()]), &[]));
        assert_eq!(parse_pull(&out).len(), 200);
        // No outbox at all: nothing, exit 0.
        let empty = temp_root("pull-empty");
        assert_eq!(sh(&empty, &pull_script(false, None, &[])), "");
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(empty);
    }

    fn add_task() -> String {
        tasks::with_store(|store| {
            store.create_task(
                &NewTask {
                    project: "Acme".into(),
                    title: Some("Spec decision requests".into()),
                    criteria: vec!["schema added".into()],
                    ..NewTask::default()
                },
                &Actor::Human,
            )
        })
        .expect("task")
        .display_id
    }

    fn apply_store(
        source: &str,
        seq: u64,
        op: &TaskOp,
        ctx: &OpContext,
    ) -> StoreResult<Option<OpResult>> {
        tasks::with_store(|store| store.apply_once(source, seq, op, ctx))
    }

    #[test]
    fn ingest_applies_two_epochs_once_and_answers_each_op() {
        let id = add_task();
        let pulled = parse_pull(&format!(
            "p1\tk3f9q2\t1\t{}\np1\tk3f9q2\t2\t{}\np1\tnew001\t1\t{}\ngarbage line\n",
            op_line("k3f9q2", 1, "p1", &id, "first"),
            op_line("k3f9q2", 2, "p1", &id, "second"),
            op_line("new001", 1, "p1", &id, "after the directory was removed"),
        ));
        assert_eq!(pulled.len(), 3);
        let info = |_: &str| {
            Some(PaneInfo {
                agent: Some("claude".into()),
                workspace_key: None,
            })
        };
        let mut link = |_: &str, _: &str| {};
        let result = ingest("mato", &pulled, &info, &mut apply_store, &mut link);
        assert!(!result.stopped);
        assert_eq!(result.changed, vec![id.clone()]);
        let detail = tasks::with_store(|store| store.task_detail(&id))
            .expect("read")
            .expect("task");
        let notes: Vec<(&str, &str)> = detail
            .entries
            .iter()
            .filter(|e| e.kind == crate::tasks::EntryKind::Agent)
            .map(|e| (e.author.as_str(), e.body.as_str()))
            .collect();
        assert_eq!(
            notes,
            vec![
                ("claude@mato", "first"),
                ("claude@mato", "second"),
                ("claude@mato", "after the directory was removed"),
            ]
        );
        let script = result.script.finish(false);
        assert!(script.contains("task-reply/p1-k3f9q2-2.json"), "{script}");
        assert!(script.contains("task-reply/p1-new001-1.json"), "{script}");
        assert!(script.contains(r#""exit":0"#), "{script}");
        // The same pull again applies nothing and writes no reply.
        let again = ingest("mato", &pulled, &info, &mut apply_store, &mut link);
        assert!(again.changed.is_empty());
        assert!(!again.script.finish(false).contains("task-reply/p1"));
    }

    #[test]
    fn a_bad_line_is_moved_aside_and_a_newer_version_waits_with_one_notice() {
        let id = add_task();
        let newer = op_line("e1", 2, "p1", &id, "x").replacen(r#""v":1"#, r#""v":2"#, 1);
        let pulled = vec![
            Pulled {
                pane: "p1".into(),
                epoch: "e1".into(),
                seq: 1,
                json: "{\"v\":1,\"op\":{\"op\":\"nope\"}}".into(),
            },
            Pulled {
                pane: "p1".into(),
                epoch: "e1".into(),
                seq: 2,
                json: newer.clone(),
            },
            Pulled {
                pane: "p1".into(),
                epoch: "e1".into(),
                seq: 3,
                json: op_line("e1", 3, "p1", &id, "behind the newer one"),
            },
            Pulled {
                pane: "p2".into(),
                epoch: "e2".into(),
                seq: 1,
                json: newer.replace("\"pane\":\"p1\"", "\"pane\":\"p2\""),
            },
        ];
        let info = |_: &str| None;
        let mut link = |_: &str, _: &str| {};
        let result = ingest("mato", &pulled, &info, &mut apply_store, &mut link);
        assert_eq!(
            result.notices,
            vec![
                "bad task op from mato/p1".to_owned(),
                "mato runs a newer drovr task; update drovr on this Mac".to_owned(),
            ]
        );
        assert!(result.changed.is_empty(), "nothing behind v2 applied");
        assert_eq!(result.handled, vec![("p1".into(), "e1".into(), 1)]);

        // Run the cleanup against real files: 1 goes to .bad, 2 and 3 stay.
        let root = temp_root("bad");
        outbox(
            &root,
            "p1",
            "e1",
            &[
                (1, pulled[0].json.clone()),
                (2, newer),
                (3, pulled[2].json.clone()),
            ],
        );
        sh(&root, &result.script.finish(false));
        assert_eq!(
            names(&root.join("task-outbox/p1")),
            vec!["1.bad", "2.json", "3.json", "epoch"]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_busy_store_stops_the_batch_and_leaves_the_rest() {
        let pulled = parse_pull(&format!(
            "p1\te1\t1\t{}\np1\te1\t2\t{}\np1\te1\t3\t{}\n",
            op_line("e1", 1, "p1", "AC-1", "a"),
            op_line("e1", 2, "p1", "AC-1", "b"),
            op_line("e1", 3, "p1", "AC-1", "c"),
        ));
        let mut calls = 0;
        let mut apply = |_: &str, seq: u64, _: &TaskOp, _: &OpContext| {
            calls += 1;
            if seq == 2 {
                Err(StoreError::Busy)
            } else {
                Ok(Some(OpResult {
                    ok: true,
                    task: Some("AC-1".into()),
                    status: Some(Status::Working),
                    message: "noted".into(),
                    code: None,
                    decision_id: None,
                }))
            }
        };
        let info = |_: &str| None;
        let mut link = |_: &str, _: &str| {};
        let result = ingest("mato", &pulled, &info, &mut apply, &mut link);
        assert!(result.stopped);
        assert_eq!(calls, 2);
        assert_eq!(result.handled, vec![("p1".into(), "e1".into(), 1)]);
        let root = temp_root("busy");
        let files: Vec<(u64, String)> = pulled.iter().map(|p| (p.seq, p.json.clone())).collect();
        outbox(&root, "p1", "e1", &files);
        sh(&root, &result.script.finish(false));
        assert_eq!(
            names(&root.join("task-outbox/p1")),
            vec!["2.json", "3.json", "epoch"]
        );
        let replies = names(&root.join("task-reply"));
        assert_eq!(replies, vec!["p1-e1-1.json"]);
        let reply = std::fs::read_to_string(root.join("task-reply/p1-e1-1.json")).expect("reply");
        let value: serde_json::Value = serde_json::from_str(&reply).expect("json");
        assert_eq!(value["exit"], 0);
        assert_eq!(value["message"], "noted");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_removes_only_applied_files_of_the_same_epoch() {
        let root = temp_root("clean");
        let files: Vec<(u64, String)> = [1, 2, 3, 10]
            .iter()
            .map(|seq| (*seq, op_line("e1", *seq, "p1", "AC-1", "x")))
            .collect();
        outbox(&root, "p1", "e1", &files);
        outbox(
            &root,
            "p2",
            "e9",
            &[(1, op_line("e9", 1, "p2", "AC-1", "x"))],
        );
        let mut script = FileScript::default();
        script.clean_outbox("p1", "e1", 3, &[]);
        // Another epoch than the directory's: nothing goes.
        script.clean_outbox("p2", "old", 5, &[]);
        let out = sh(&root, &script.finish(false));
        assert_eq!(out.trim(), root.to_string_lossy());
        assert_eq!(
            names(&root.join("task-outbox/p1")),
            vec!["10.json", "epoch"]
        );
        assert_eq!(names(&root.join("task-outbox/p2")), vec!["1.json", "epoch"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn written_files_keep_their_content_and_a_start_links_the_workspace() {
        let root = temp_root("write");
        let mut script = FileScript::default();
        let tricky = "line one\nDROVR_EOF\n$HOME `x` 'q'\n";
        script.write("tasks/AC-1.md", tricky);
        sh(&root, &script.finish(false));
        assert_eq!(
            std::fs::read_to_string(root.join("tasks/AC-1.md")).expect("read"),
            tricky
        );
        assert_eq!(names(&root.join("tasks")), vec!["AC-1.md"]);
        let _ = std::fs::remove_dir_all(root);

        let id = add_task();
        let start = TaskOp::Start {
            task: Some(id.clone()),
            harness: "claude".into(),
            session_id: None,
        };
        let line = serde_json::to_string(&OutboxOp {
            v: 1,
            epoch: "e1".into(),
            seq: 1,
            ts: 0,
            pane: "p1".into(),
            op: start,
        })
        .expect("json");
        let pulled = vec![Pulled {
            pane: "p1".into(),
            epoch: "e1".into(),
            seq: 1,
            json: line,
        }];
        let info = |_: &str| {
            Some(PaneInfo {
                agent: Some("codex".into()),
                workspace_key: Some("mato/w7:AC-1 Spec".into()),
            })
        };
        let mut linked = Vec::new();
        let mut link = |task: &str, key: &str| linked.push((task.to_owned(), key.to_owned()));
        ingest("mato", &pulled, &info, &mut apply_store, &mut link);
        assert_eq!(linked, vec![(id.clone(), "mato/w7:AC-1 Spec".to_owned())]);
        let task = tasks::with_store(|store| store.task(&id))
            .expect("read")
            .expect("task");
        assert_eq!(task.status, Status::Working);
    }

    #[test]
    fn probe_answers_parse() {
        assert_eq!(parse_probe(&Ok("drovr-task 1\n".into())), Some(1));
        assert_eq!(parse_probe(&Ok("drovr-task 3".into())), Some(3));
        assert_eq!(parse_probe(&Err("sh: drovr: not found".into())), None);
        assert_eq!(parse_probe(&Ok("drovr: unknown command task".into())), None);
        assert_eq!(parse_probe(&Ok("drovr-task one".into())), None);
        assert_eq!(parse_probe(&Ok("drovr-task 1 extra".into())), None);
        assert_eq!(parse_probe(&Ok(String::new())), None);
    }
}
