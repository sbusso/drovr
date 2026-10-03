# Tasks: per-project board and task view

Status: draft 1, 2026-10-03. Contract for three parallel builders (A, B, C).
Inputs: the workspace and workspace-herdr copies from mato (schema, spec
sections 5-15, 25-26 and 43, apps/tui, packages/herdr-plugin),
docs/reports/2026-10-02-mato-projects.md and docs/design/inbox-pane.md.

## 1. Scope

drovr gains durable task records. A sidebar section is a project; its tasks
show in the right panel next to the inbox, as a board per project. Selecting a
task opens the task view inside the same panel.

In scope for v1:

- A per-project board with six lanes: Triage, Ready, Working, Blocked,
  Review, Done. Cancelled is a status, not a lane.
- The task view: header (id, status, auto flag, agent), title, description,
  acceptance criteria with check state and evidence, notes thread, attempts
  linked to agents, workspaces and panes on any machine, artifacts (documents
  open in the doc pane), and one open decision card.
- Start a task: create a workspace in the project's section on a chosen
  machine, start Claude with a context file and a first prompt, link both.
- `drovr task` CLI for agents and for the user, plus skill `drovr-tasks`.
- Status follows agent signals (working, waiting, finished), with a manual
  override per task.
- Remote agents (mato) report through an outbox the client pulls over the
  existing SSH bridge.
- One-shot import from a workspace SQLite database.

Left out of v1:

- Server, REST, MCP, SSE and WebSocket transports, auth, tokens, members,
  multi-tenant columns, roles. drovr is one user and one process.
- Triage proposals and confirm flow, categories, complexity, labels, budgets,
  requester and reviewer fields, series, channels, mentions, task blocks,
  parent and sub-tasks, FTS search.
- Git worktrees per task. v1 uses the project's working directory on the
  machine (see section 5 for the upgrade path).
- Rich artifact renderers (diff viewer, images). Documents open in the doc
  pane; links and other files open with the system opener on the Mac only.
- Decision rows in the inbox list. v1 shows decisions on the board card and in
  the task view; the inbox keeps its hook-based items unchanged.
- Drag and drop. Moves use the status chip menu.
- A stored inbox or events table. Status changes are written as event
  entries in the task thread.

## 2. Store

### 2.1 Engine and file

SQLite through `rusqlite` with the `bundled` feature (the drovr reader built
rusqlite 0.40.1 with `bundled` offline on Rust 1.96.1 in about 3 s). Add it
with `cargo add rusqlite --features bundled` while online. An offline add
re-resolves Cargo.lock and downgrades async-trait, event-listener, syn and
toml_edit; check that the Cargo.lock diff only adds rusqlite,
libsqlite3-sys and their new dependencies before committing.

Path, in this order:

1. `$DROVR_TASKS_DB` when set (tests and the CLI's tests always set it).
2. `crate::config::state_dir().join("drovr").join("tasks.db")`, which is
   `~/.local/state/herdr/drovr/tasks.db` for release builds and
   `~/.local/state/herdr-dev/drovr/tasks.db` for debug builds.

Under `cfg(test)` the client's shared handle never opens a file: it is a
thread-local in-memory store (section 3.4).

Connection settings: `journal_mode=WAL`, `synchronous=NORMAL`,
`foreign_keys=ON`. `busy_timeout` is 250 ms in the client and 5000 ms in the
CLI. A busy error in the client shows the toast `tasks db busy, retry`.

Only the Mac (the client machine) has a database. A remote machine never
creates one; `drovr task` there writes an outbox (section 6.3).

### 2.2 Ids and time

- Row ids are `INTEGER PRIMARY KEY`. Code passes them as `i64`.
- A task's public id is `display_id` = `<project key>-<number>`, for example
  `AC-12`. The CLI and the panel use display ids only.
- `number` comes from `projects.next_number`, allocated in the same
  transaction as the insert.
- Timestamps are RFC 3339 UTC text (`time::OffsetDateTime::now_utc()` with
  `time::format_description::well_known::Rfc3339`), for example
  `2026-10-03T08:15:02Z`.
- `ext_id` holds a source id on import (workspace ULID) and makes the import
  idempotent.

### 2.3 Project identity and renames

A project row is created on first use by `ensure_project(name)`, where `name`
is the sidebar `ProjectGroup.name`.

- `key`: derived once from the name and never changed after the project has
  a task. Rule: take the ASCII letters and digits of each word, uppercased.
  Two or more words: first letter of each word, at most 4 ("Outsmartis ops"
  -> `OO`). One word: first 3 characters ("Infrastructure" -> `INF`). If the
  result starts with a digit, prefix `P`. If it has fewer than 2 characters,
  append `X`. If the key is taken, append 2, 3, ... (`OO2`).
- Renaming a section updates `projects.name` through `rename_project(old,
  new)`; the key and every display id stay. The rename site is
  `ClientRenameTarget::ProjectRename` in project_actions.rs (around line 940).
  If `new` already names another project row, the rename of the section still
  happens and the store returns `Refused { code: "project_exists" }`; the
  client shows the message as a toast and the tasks stay under the old name
  until the user renames again.
- Deleting a section leaves its project row and tasks. They reappear when a
  section with the same name exists again, and `drovr task list --project
  NAME` still lists them.
- The OTHER section (`projects::OTHER`) has no tasks. The Tasks view shows
  `Add this workspace to a section to track tasks.`

### 2.4 Links to workspaces and panes

drovr already keys workspaces and agents by stable strings (projects.rs):

- machine key: the endpoint label lowercased (`local`, `mato`)
- workspace key: `machine/{workspace_id}:{label}`, matched with
  `projects::same_workspace`
- pane key: `machine/{pane_id}`

`tasks.workspace_key` is the task's current workspace. Each attempt stores
`machine`, `workspace_key` and `pane_key`. These are the only links; there is
no token on the pane for the task id. A pane's task is the task of the open
attempt whose `pane_key` matches.

### 2.5 Schema (migration v1)

Migrations live in `src/tasks/schema.rs` as `const MIGRATIONS: &[&str]`,
append-only, index = version - 1. `migrate(conn)` creates `migrations` if
needed and applies every missing version in its own transaction, inserting
`(version, applied_at)`.

```sql
CREATE TABLE migrations (
  version INTEGER PRIMARY KEY,
  applied_at TEXT NOT NULL
);

-- v1
CREATE TABLE projects (
  id INTEGER PRIMARY KEY,
  key TEXT NOT NULL UNIQUE
    CHECK (key GLOB '[A-Z]*' AND key NOT GLOB '*[^A-Z0-9]*'
           AND length(key) BETWEEN 2 AND 10),
  name TEXT NOT NULL UNIQUE,
  next_number INTEGER NOT NULL DEFAULT 1,
  created_at TEXT NOT NULL
);

CREATE TABLE tasks (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id),
  number INTEGER NOT NULL,
  display_id TEXT NOT NULL UNIQUE,
  title TEXT,
  body TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL DEFAULT 'triage' CHECK (status IN
    ('triage','ready','working','blocked','review','done','cancelled')),
  kind TEXT CHECK (kind IN ('fix','feature','chore','research','spec')),
  priority TEXT NOT NULL DEFAULT 'normal'
    CHECK (priority IN ('urgent','high','normal','low')),
  executor TEXT,
  workspace_key TEXT,
  auto_status INTEGER NOT NULL DEFAULT 1,
  position REAL NOT NULL,
  ext_id TEXT UNIQUE,
  status_since TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  closed_at TEXT,
  archived_at TEXT,
  UNIQUE (project_id, number)
);
CREATE INDEX idx_tasks_lane ON tasks(project_id, status, position);
CREATE INDEX idx_tasks_workspace ON tasks(workspace_key)
  WHERE workspace_key IS NOT NULL;

CREATE TABLE criteria (
  id INTEGER PRIMARY KEY,
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  position INTEGER NOT NULL,
  text TEXT NOT NULL,
  check_cmd TEXT,
  state TEXT NOT NULL DEFAULT 'open'
    CHECK (state IN ('open','passed','failed')),
  evidence TEXT,
  checked_by TEXT,
  checked_at TEXT,
  UNIQUE (task_id, position)
);

CREATE TABLE attempts (
  id INTEGER PRIMARY KEY,
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  harness TEXT NOT NULL,
  machine TEXT NOT NULL,
  workspace_key TEXT,
  pane_key TEXT,
  session_id TEXT,
  started_at TEXT NOT NULL,
  ended_at TEXT,
  outcome TEXT CHECK (outcome IN
    ('succeeded','failed','stopped','needs_human')),
  note TEXT,
  tokens_in INTEGER,
  tokens_out INTEGER,
  cost_cents INTEGER
);
CREATE UNIQUE INDEX idx_attempts_one_open ON attempts(task_id)
  WHERE ended_at IS NULL;
CREATE INDEX idx_attempts_pane ON attempts(pane_key)
  WHERE ended_at IS NULL;

CREATE TABLE entries (
  id INTEGER PRIMARY KEY,
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('human','agent','event')),
  author TEXT NOT NULL,
  attempt_id INTEGER REFERENCES attempts(id),
  body TEXT NOT NULL,
  event_type TEXT,
  pinned INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL,
  UNIQUE (task_id, seq)
);

CREATE TABLE artifacts (
  id INTEGER PRIMARY KEY,
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  attempt_id INTEGER REFERENCES attempts(id),
  kind TEXT NOT NULL CHECK (kind IN ('doc','diff','link','file','report')),
  title TEXT NOT NULL,
  target TEXT NOT NULL,
  machine TEXT,
  summary TEXT,
  review TEXT NOT NULL DEFAULT 'unreviewed'
    CHECK (review IN ('unreviewed','accepted','rejected')),
  created_at TEXT NOT NULL
);

CREATE TABLE decisions (
  id INTEGER PRIMARY KEY,
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  attempt_id INTEGER REFERENCES attempts(id),
  title TEXT NOT NULL CHECK (length(title) <= 120),
  summary TEXT NOT NULL DEFAULT '' CHECK (length(summary) <= 1200),
  choices_json TEXT NOT NULL,
  allow_text INTEGER NOT NULL DEFAULT 1,
  default_choice TEXT,
  state TEXT NOT NULL DEFAULT 'open'
    CHECK (state IN ('open','ruled','withdrawn','expired')),
  ruling_choice TEXT,
  ruling_text TEXT,
  ruled_by TEXT,
  ruled_at TEXT,
  surface TEXT,
  expires_at TEXT,
  created_at TEXT NOT NULL
);
CREATE UNIQUE INDEX idx_decisions_one_open ON decisions(task_id)
  WHERE state = 'open';

CREATE TABLE applied_ops (
  source TEXT PRIMARY KEY,
  seq INTEGER NOT NULL,
  applied_at TEXT NOT NULL
);
```

Column rules the store enforces (not the schema):

- `entries.seq` is `COALESCE(MAX(seq), 0) + 1` per task, inside the insert.
- `tasks.position`: a new task or a moved task lands at the end of its lane
  (`MAX(position) + 1024`, or 1024). `reorder` sets the midpoint of its
  neighbours.
- `status_since` changes on every status change. `closed_at` is set on entry
  into done or cancelled and cleared on leaving them.
- `criteria.evidence` and `entries.body` are cut to 20 000 bytes on a char
  boundary, with `…` appended.
- `decisions.choices_json` is a JSON array of `Choice` (section 3.2), 1 to 8
  items, ids unique, at most one `recommended`.
- `executor` is the harness name used at start (`claude`, `codex`).

### 2.6 Statuses and moves

Lanes in order: `triage, ready, working, blocked, review, done`. `cancelled`
is closed and not drawn as a lane.

Three actors:

- `Human`: the panel, or the CLI run outside a herdr pane. May move any task
  to any status. `review -> ready` (send back) requires a note.
- `Agent`: the CLI run inside a herdr pane (`HERDR_PANE_ID` set), local or
  through the outbox.
- `Auto`: the client's signal sync (section 6.5). Skipped when
  `tasks.auto_status = 0`.

Allowed agent and auto moves (const table in `src/tasks/transitions.rs`):

| from    | to      | agent op              | auto trigger              |
|---------|---------|-----------------------|---------------------------|
| triage  | working | start                 | no                        |
| ready   | working | start                 | pane works                |
| working | blocked | decide, done needs_human | pane waits             |
| blocked | working | (decision ruled)      | pane works again          |
| working | review  | done succeeded + gate | pane finished + gate      |
| working | ready   | release, done failed/stopped | no                 |

Agents and auto never move to `done` or `cancelled`. Refusal texts (code,
message), used by both the CLI and the panel:

- `not_allowed`: `a task in {from} does not move to {to} from an agent`
- `note_required`: `sending a task back needs a note`
- `criteria_open`: `criteria {list} have no verdict` (list like `2, 4`)
- `criteria_failed`: `criteria {list} failed`
- `decision_open`: `{id} already has an open decision`
- `no_attempt`: `{id} has no open attempt`
- `project_exists`: `a project named {name} already exists`
- `not_found`: `no task {id}`

Gate (`gate(&[Criterion]) -> Gate`): passes when every criterion is
`passed`. A task with no criteria passes. Failed criteria are reported before
open ones.

A human move sets `auto_status = 0` on that task. Starting a new attempt
sets it back to 1. The task view has an `auto` chip that toggles it.

Every status change writes an event entry: `event_type = "status"`, `body =
"{from} → {to}"` plus ` ({actor})` for agent and auto moves, plus `: {note}`
when a note was given.

### 2.7 Import from workspace

`drovr task import PATH [--map KEY=Section]... [--dry-run]` reads a workspace
`workspace.sqlite` (schema v8 to v11) read-only and writes into the drovr
store in one transaction.

- Projects: each workspace project maps to the section given by `--map`,
  else to the section whose name equals the workspace project name, else to a
  new project row named after it. The workspace key is kept as the drovr key
  when the drovr project has no tasks yet and the key is free.
- Tasks: `ext_id = tasks.id`; rows whose `ext_id` exists are skipped. Status
  `needs_human` -> `blocked`, `duplicate` -> `cancelled` (with an event entry
  `imported as duplicate of {display_id}`). `kind`, `priority`, `title`,
  `body_md`, `position`, `created_at`, `updated_at`, `closed_at`,
  `archived_at`, `status_since` (fallback `updated_at`) copy across. `number`
  and `display_id` keep the source values when the key was kept, else they
  are allocated. `executor` = the executor member's handle.
- Criteria: text, position, state, `last_output` -> evidence, `check_json`
  command -> `check_cmd`.
- Entries: kinds human, agent and event copy with author handle; questions
  become agent entries with the summary and options as text. Deleted entries
  are skipped.
- Attempts: `runtime` or `harness`, timestamps, outcome (`stale` ->
  `stopped`), note, tokens, cost. `machine = "import"`, no pane key.
- Artifacts: type `document` -> `doc`, `diff`, `link`, `report`; other types
  -> `file`. `target` = link, else `(inline)`.
- Open questions of kind `decision` -> decisions with state open.
- Prints `imported N tasks into M projects (K skipped)`; `--dry-run` rolls
  back and prints the same line.

## 3. Store module and API

### 3.1 Files

```
src/tasks/mod.rs          types, shared handle, re-exports
src/tasks/schema.rs       MIGRATIONS, migrate()
src/tasks/store.rs        TaskStore
src/tasks/transitions.rs  check_move(), gate(), refusal texts
src/tasks/ops.rs          TaskOp, OpResult, outbox line format
src/tasks/cli.rs          `drovr task` argument parsing and output
src/tasks/outbox.rs       outbox writer (remote CLI) and reply polling
src/tasks/import.rs       workspace import
```

`src/tasks` sits at the crate root, next to `doc_view`, because both the CLI
and the client use it. Everything below is `pub(crate)`.

### 3.2 Types (`src/tasks/mod.rs`)

```rust
pub(crate) type TaskId = i64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status { Triage, Ready, Working, Blocked, Review, Done, Cancelled }

impl Status {
    pub(crate) const LANES: [Status; 6] =
        [Self::Triage, Self::Ready, Self::Working, Self::Blocked, Self::Review, Self::Done];
    pub(crate) fn as_str(self) -> &'static str;          // "triage" ...
    pub(crate) fn parse(text: &str) -> Option<Status>;   // also accepts "doing" = Working
    pub(crate) fn label(self) -> &'static str;           // "Triage" ...
    pub(crate) fn is_closed(self) -> bool;               // Done | Cancelled
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind { Fix, Feature, Chore, Research, Spec }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Priority { Urgent, High, #[default] Normal, Low }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CheckState { Open, Passed, Failed }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome { Succeeded, Failed, Stopped, NeedsHuman }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EntryKind { Human, Agent, Event }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ArtifactKind { Doc, Diff, Link, File, Report }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Review { Unreviewed, Accepted, Rejected }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DecisionState { Open, Ruled, Withdrawn, Expired }

/// Who acts. The string is the author written into entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Actor {
    Human,          // author "you"
    Agent(String),  // author e.g. "claude@mato"
    Auto,           // author "drovr"
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Project { pub id: i64, pub key: String, pub name: String, pub next_number: i64 }

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Task {
    pub id: TaskId,
    pub project_id: i64,
    pub number: i64,
    pub display_id: String,
    pub title: Option<String>,
    pub body: String,
    pub status: Status,
    pub kind: Option<Kind>,
    pub priority: Priority,
    pub executor: Option<String>,
    pub workspace_key: Option<String>,
    pub auto_status: bool,
    pub position: f64,
    pub status_since: String,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub archived_at: Option<String>,
}

impl Task {
    /// Title, else the first non-empty body line, else "Untitled".
    pub(crate) fn name(&self) -> &str;
}

/// One board row. Counts are computed in the list query.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct TaskCard {
    pub task: Task,
    pub criteria_total: u32,
    pub criteria_passed: u32,
    pub criteria_failed: u32,
    pub open_decision: bool,
    pub last_outcome: Option<Outcome>,
    /// Open attempt, if any: (harness, machine, pane_key).
    pub live: Option<(String, String, Option<String>)>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Criterion {
    pub id: i64,
    pub task_id: TaskId,
    pub position: i64,          // 1-based, as shown to users
    pub text: String,
    pub check_cmd: Option<String>,
    pub state: CheckState,
    pub evidence: Option<String>,
    pub checked_by: Option<String>,
    pub checked_at: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Entry {
    pub id: i64,
    pub task_id: TaskId,
    pub seq: i64,
    pub kind: EntryKind,
    pub author: String,
    pub attempt_id: Option<i64>,
    pub body: String,
    pub event_type: Option<String>,
    pub pinned: bool,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Attempt {
    pub id: i64,
    pub task_id: TaskId,
    pub harness: String,
    pub machine: String,
    pub workspace_key: Option<String>,
    pub pane_key: Option<String>,
    pub session_id: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub outcome: Option<Outcome>,
    pub note: Option<String>,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    pub cost_cents: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Artifact {
    pub id: i64,
    pub task_id: TaskId,
    pub attempt_id: Option<i64>,
    pub kind: ArtifactKind,
    pub title: String,
    pub target: String,         // absolute path on `machine`, or a URL
    pub machine: Option<String>,
    pub summary: Option<String>,
    pub review: Review,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Choice {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consequence: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recommended: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Decision {
    pub id: i64,
    pub task_id: TaskId,
    pub attempt_id: Option<i64>,
    pub title: String,
    pub summary: String,
    pub choices: Vec<Choice>,
    pub allow_text: bool,
    pub default_choice: Option<String>,
    pub state: DecisionState,
    pub ruling_choice: Option<String>,
    pub ruling_text: Option<String>,
    pub ruled_by: Option<String>,
    pub ruled_at: Option<String>,
    pub surface: Option<String>,   // "panel" | "cli" | "expiry"
    pub expires_at: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct TaskDetail {
    pub task: Task,
    pub project: Project,
    pub criteria: Vec<Criterion>,     // by position
    pub entries: Vec<Entry>,          // by seq, last 200
    pub attempts: Vec<Attempt>,       // newest first
    pub artifacts: Vec<Artifact>,     // newest first
    pub decision: Option<Decision>,   // the open one, else the latest
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TaskFilter {
    pub project: Option<String>,        // section name
    /// Matches on the `machine/{workspace_id}:` prefix, so a renamed
    /// workspace still matches (same rule as projects::same_workspace).
    pub workspace_key: Option<String>,
    pub statuses: Vec<Status>,          // empty = all except archived
    pub include_archived: bool,
    pub done_limit: Option<u32>,        // newest N done/cancelled by closed_at
}

#[derive(Clone, Debug, Default)]
pub(crate) struct NewTask {
    pub project: String,
    pub title: Option<String>,
    pub body: String,
    pub kind: Option<Kind>,
    pub priority: Priority,
    pub status: Option<Status>,         // default Triage
    pub criteria: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TaskPatch {
    pub title: Option<Option<String>>,
    pub body: Option<String>,
    pub kind: Option<Option<Kind>>,
    pub priority: Option<Priority>,
    pub auto_status: Option<bool>,
    pub archived: Option<bool>,
}

#[derive(Clone, Debug)]
pub(crate) struct NewAttempt {
    pub harness: String,
    pub machine: String,
    pub workspace_key: Option<String>,
    pub pane_key: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct NewArtifact {
    pub kind: ArtifactKind,
    pub title: String,
    pub target: String,
    pub machine: Option<String>,
    pub summary: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct NewDecision {
    pub title: String,
    pub summary: String,
    pub choices: Vec<Choice>,
    pub allow_text: bool,
    pub default_choice: Option<String>,
    pub expires_at: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) enum Ruling { Choice(String), Text(String) }

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Refusal { pub code: &'static str, pub message: String }

#[derive(Debug)]
pub(crate) enum StoreError {
    Sqlite(rusqlite::Error),
    Busy,                       // SQLITE_BUSY after busy_timeout
    NotFound(String),           // display id or row id as text
    Refused(Refusal),
    Invalid(String),            // bad input, message for the user
}
impl std::fmt::Display for StoreError { .. }
impl std::error::Error for StoreError {}
impl From<rusqlite::Error> for StoreError { .. }  // maps BUSY to Busy

pub(crate) type StoreResult<T> = Result<T, StoreError>;
```

### 3.3 TaskStore (`src/tasks/store.rs`)

```rust
pub(crate) struct TaskStore { conn: rusqlite::Connection }

impl TaskStore {
    /// $DROVR_TASKS_DB, else state_dir()/drovr/tasks.db.
    pub(crate) fn default_path() -> PathBuf;
    /// Opens or creates the file (and its directory), sets pragmas, migrates.
    pub(crate) fn open(path: &Path, busy_ms: u32) -> StoreResult<TaskStore>;
    pub(crate) fn open_in_memory() -> StoreResult<TaskStore>;
    /// PRAGMA data_version; changes when another connection commits.
    pub(crate) fn data_version(&self) -> StoreResult<i64>;

    // Projects
    pub(crate) fn ensure_project(&self, name: &str) -> StoreResult<Project>;
    pub(crate) fn project(&self, name: &str) -> StoreResult<Option<Project>>;
    pub(crate) fn projects(&self) -> StoreResult<Vec<Project>>;
    pub(crate) fn rename_project(&self, old: &str, new: &str) -> StoreResult<()>;

    // Tasks
    pub(crate) fn create_task(&self, new: &NewTask, actor: &Actor) -> StoreResult<Task>;
    pub(crate) fn task(&self, display_id: &str) -> StoreResult<Option<Task>>;
    pub(crate) fn task_detail(&self, display_id: &str) -> StoreResult<Option<TaskDetail>>;
    pub(crate) fn list(&self, filter: &TaskFilter) -> StoreResult<Vec<TaskCard>>;
    /// Counts per lane for a project, for headers and the project picker.
    pub(crate) fn lane_counts(&self, project: &str) -> StoreResult<[u32; 6]>;
    pub(crate) fn update_task(&self, display_id: &str, patch: &TaskPatch, actor: &Actor) -> StoreResult<Task>;
    pub(crate) fn move_task(&self, display_id: &str, to: Status, actor: &Actor, note: Option<&str>) -> StoreResult<Task>;
    /// Places the task in `status` between `after` and `before` (display ids).
    pub(crate) fn reorder(&self, display_id: &str, status: Status, after: Option<&str>, before: Option<&str>) -> StoreResult<()>;
    pub(crate) fn link_workspace(&self, display_id: &str, workspace_key: Option<&str>) -> StoreResult<()>;
    /// The task of the open attempt on this pane.
    pub(crate) fn task_for_pane(&self, pane_key: &str) -> StoreResult<Option<Task>>;

    // Criteria (position is 1-based)
    pub(crate) fn set_criteria(&self, display_id: &str, texts: &[String], actor: &Actor) -> StoreResult<Vec<Criterion>>;
    pub(crate) fn add_criterion(&self, display_id: &str, text: &str, actor: &Actor) -> StoreResult<Criterion>;
    pub(crate) fn check_criterion(&self, display_id: &str, position: i64, state: CheckState, evidence: Option<&str>, actor: &Actor) -> StoreResult<Criterion>;

    // Thread
    pub(crate) fn add_entry(&self, display_id: &str, kind: EntryKind, body: &str, actor: &Actor) -> StoreResult<Entry>;
    pub(crate) fn pin_entry(&self, entry_id: i64, pinned: bool) -> StoreResult<()>;

    // Attempts
    /// Ends any open attempt as Stopped, opens a new one, sets executor,
    /// workspace_key and auto_status = 1, moves triage/ready -> working.
    pub(crate) fn start_attempt(&self, display_id: &str, new: &NewAttempt, actor: &Actor) -> StoreResult<Attempt>;
    /// Succeeded: gate, then working -> review (refused with the attempt left open
    /// when the gate fails). Failed/Stopped: -> ready. NeedsHuman: -> blocked.
    /// Withdraws an open decision of this attempt.
    pub(crate) fn finish_attempt(&self, display_id: &str, outcome: Outcome, note: Option<&str>, actor: &Actor) -> StoreResult<Task>;
    pub(crate) fn release(&self, display_id: &str, note: &str, actor: &Actor) -> StoreResult<Task>;
    pub(crate) fn set_attempt_usage(&self, attempt_id: i64, tokens_in: Option<i64>, tokens_out: Option<i64>, cost_cents: Option<i64>, session_id: Option<&str>) -> StoreResult<()>;

    // Artifacts
    pub(crate) fn attach_artifact(&self, display_id: &str, new: &NewArtifact, actor: &Actor) -> StoreResult<Artifact>;
    pub(crate) fn review_artifact(&self, artifact_id: i64, review: Review) -> StoreResult<()>;

    // Decisions
    /// One open per task; moves working -> blocked; validates choices.
    pub(crate) fn request_decision(&self, display_id: &str, new: &NewDecision, actor: &Actor) -> StoreResult<Decision>;
    /// First ruling wins (a second returns Refused "decision_ruled").
    /// Moves blocked -> working when an attempt is open, else -> ready.
    pub(crate) fn rule_decision(&self, decision_id: i64, ruling: &Ruling, surface: &str, actor: &Actor) -> StoreResult<Decision>;
    pub(crate) fn withdraw_decision(&self, display_id: &str, actor: &Actor) -> StoreResult<()>;
    pub(crate) fn decision(&self, decision_id: i64) -> StoreResult<Option<Decision>>;
    /// Rules expired open decisions with their default, else marks them expired.
    /// Returns the ids it changed.
    pub(crate) fn expire_decisions(&self, now: &str) -> StoreResult<Vec<i64>>;

    // Ops (CLI and outbox)
    pub(crate) fn apply(&self, op: &TaskOp, ctx: &OpContext) -> StoreResult<OpResult>;
    /// Applies `op` only when `seq` is above the stored seq for `source`;
    /// records the seq in the same transaction. Ok(None) = already applied.
    pub(crate) fn apply_once(&self, source: &str, seq: u64, op: &TaskOp, ctx: &OpContext) -> StoreResult<Option<OpResult>>;
    pub(crate) fn applied_seq(&self, source: &str) -> StoreResult<u64>;

    // Import
    pub(crate) fn import_workspace(&self, path: &Path, map: &[(String, String)], dry_run: bool) -> StoreResult<ImportReport>;
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct ImportReport { pub tasks: u32, pub projects: u32, pub skipped: u32 }
```

Every write method runs in one transaction, bumps `tasks.updated_at`, and
writes the event entries listed in 2.6. Refusals never leave partial writes.

### 3.4 Shared handle (`src/tasks/mod.rs`)

```rust
/// Runs `f` with the process-wide store. Opens default_path() with
/// busy_ms = 250 on first use. Under cfg(test) the store is a thread-local
/// TaskStore::open_in_memory().
pub(crate) fn with_store<R>(f: impl FnOnce(&TaskStore) -> StoreResult<R>) -> StoreResult<R>;
```

Release builds keep the store in `OnceLock<Mutex<Option<TaskStore>>>`; an open
failure is returned on every call and retried at most once per 10 s.

### 3.5 Ops (`src/tasks/ops.rs`)

One enum serves the local CLI, the outbox and the client. Serialized with
`#[serde(tag = "op", rename_all = "snake_case")]`. `task: None` means "the
task of this pane" (resolved by `apply` from `OpContext.pane_key`, then from
`$DROVR_TASK` for the local CLI).

```rust
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum TaskOp {
    Add { project: String, title: Option<String>, body: String,
          kind: Option<Kind>, priority: Option<Priority>, criteria: Vec<String> },
    Update { task: Option<String>, title: Option<String>, body: Option<String>,
             kind: Option<Kind>, priority: Option<Priority> },
    Status { task: Option<String>, to: Status, note: Option<String> },
    /// CLI default harness: $DROVR_AGENT, else "claude".
    Start { task: Option<String>, harness: String, session_id: Option<String> },
    Note { task: Option<String>, body: String },
    Criteria { task: Option<String>, set: Vec<String>, add: Vec<String> },
    Check { task: Option<String>, position: i64, state: CheckState, evidence: Option<String> },
    Artifact { task: Option<String>, kind: ArtifactKind, title: String,
               target: String, summary: Option<String> },
    Done { task: Option<String>, outcome: Outcome, note: Option<String> },
    Release { task: Option<String>, note: String },
    Decide { task: Option<String>, title: String, summary: String,
             choices: Vec<Choice>, default_choice: Option<String>,
             allow_text: bool, expires_at: Option<String> },
    Withdraw { task: Option<String> },
}

pub(crate) struct OpContext {
    pub actor: Actor,
    pub machine: String,            // "local", "mato"
    pub pane_key: Option<String>,   // "machine/pane_id"
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct OpResult {
    pub ok: bool,
    pub task: Option<String>,       // display id
    pub status: Option<Status>,
    pub message: String,            // one line for the CLI
    pub code: Option<String>,       // refusal code when ok = false
    pub decision_id: Option<i64>,
}

/// One outbox file (section 6.3).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct OutboxOp { pub seq: u64, pub ts: u64, pub pane: String, pub op: TaskOp }
```

`Start` from an `OpContext` fills `NewAttempt` with `machine`, `pane_key` and
the workspace key of that pane when the client applies it (the client knows
the pane's workspace; the local CLI leaves it `None`).

## 4. Panel

### 4.1 Views

The right panel gets two views, `Inbox | Tasks`. The header word `Inbox`
becomes two clickable labels; the active one uses the accent colour, the other
`overlay0`. Everything else about the panel (width, overlay, focus, border
drag, project filter, tick) stays in inbox.rs.

- `InboxState.view: PanelView` with `enum PanelView { Inbox, Tasks }`
  (default Inbox) and `InboxState.tasks: TasksState` (from tasks_panel.rs).
- `InboxHits.views: Vec<(Rect, PanelView)>`.
- `render` and `draw` call `tasks_panel::render(...)` when `view == Tasks`.
- `handle_inbox_key`: `BackTab` (shift+Tab) toggles the view in both views.
  In Tasks, every other key goes to `tasks_panel` first.
- `handle_inbox_mouse`: header view labels first; then, in Tasks,
  `tasks_panel` hits.
- The view is remembered in the session only.

The project shown is the panel filter:

- `InboxFilter::Project(name)`: the board of that project.
- `InboxFilter::Workspace { .. }`: the tasks whose `workspace_key` matches
  that workspace; with exactly one, its task view opens directly.
- No filter: a project list, one line per section in sidebar order with lane
  counts (`Acme  2 ▸ 3 ● 1 ⚠ 1 ◎`). One click selects the project
  (sets the filter to `Project`).

Data refresh: `tick_inbox` checks `TaskStore::data_version()` at most every
500 ms while the Tasks view is visible and reloads cards (and the open
detail) when it changed or after a write from the panel. Drawing never
queries the database.

### 4.2 Board

Layout by panel inner width:

- Under 90 columns: grouped list. Lane header lines in lane order, cards under
  them. Triage and Done start collapsed when they hold more than 3 cards.
- 90 columns or more: four columns `Ready | Working | Blocked | Review`, each
  `(width - 3) / 4` wide, with one-line `Triage n ▸` above and `Done n ▸`
  below. Clicking either expands it as a full-width list above or below the
  columns.

Lane header: `Ready 3` (bold when it holds the selection), `▸` when collapsed.
Click toggles collapse. Collapsed lanes are stored per project in
`sidebar.toml` `[tasks] collapsed = ["Acme:done"]` through `projects::update`
(B adds `tasks: TasksSettings` to `ProjectLayout`; see section 7.4).

Card, 2 lines:

- Line 1: display id (`overlay0`), name (title, or first body line in
  `overlay0`), priority mark at the right edge (`!` urgent in red, `^` high in
  yellow, nothing otherwise).
- Line 2: kind word, criteria `✓2/3` (green when complete, red `✗` count when
  any failed), `?` when a decision is open (yellow), last outcome mark
  (`✓` succeeded, `✗` failed, `s` stopped, `h` needs human), the live agent
  `● claude@mato` (radar colours: green working, yellow waiting, blue
  finished and unseen, `overlay0` idle), and at the right edge one action
  button: `▶ start` (triage, ready, no live attempt), `↗ pane` (live
  attempt), `✓ accept` (review), nothing otherwise.

Selection follows the inbox: first line shaded with `active_row_bg`, a
`╭ ╰` frame on the left of the card's lines. No full-card fill.

One click acts:

- Click a card: open the task view.
- Click the right-edge button: run it (start, focus pane, accept).
- Click a lane header: collapse or expand.
- Click `+ new` (header line under the tabs): a one-line input
  (`inbox_editor::NoteEditor`) parsed as `[#kind] [!priority] text`;
  Enter creates the task in Triage of the shown project. `!!` = urgent,
  `!` = high.
- Wheel scrolls the board.

Keys (Tasks view focused): `j/k` next/previous card across lanes, `h/l`
previous/next column (columns layout) or lane (list), `Enter` open, `n` new,
`s` start, `p` focus pane, `a` accept (review only), `m` move menu, `space`
collapse the selected card's lane, `/` filter the board by text (name and
display id, Esc clears), `Esc` clears the project filter, then blurs.

Move menu (`m` or a click on the status chip in the task view): the six lanes
plus Cancelled, the current one dimmed. Picking `Ready` from Review asks for a
note first (one-line input). Refusals show in the panel's status line for 4 s.

### 4.3 Task view

Opens inside the panel in place of the board. `Esc` or the `←` on the header
returns to the board with the same selection. `j/k` in the header region step
to the next/previous card of the board order.

Regions from top to bottom:

1. Header: `← AC-12  [Review ▾]  auto  ● claude@mato 14m  ↗ pane`. The status
   chip opens the move menu. `auto` is shown in accent when on, struck
   through in `overlay0` when off; click toggles. The agent part shows the
   live attempt (radar colour) and how long it has been in the status.
2. Title, bold. Click to edit (one-line input). `Untitled` in `overlay0` when
   empty.
3. Meta line, wrapping: `kind fix  pri high  ws acme/AC-12  $0.42`. Each value
   is clickable: kind and priority open a small menu; `ws` focuses the
   workspace.
4. Description, folded to a third of the panel height, `▾ more` to unfold.
   `e` (or click `edit`) opens it in the external editor through the inbox
   editor path (`inbox_editor`), then saves the body.
5. Criteria: `Criteria 2/3`, then one line each: mark (`✓` green, `✗` red,
   `○` `overlay0`), text (passed text `overlay0`), `chk` suffix when
   `check_cmd` is set, `e` at the right edge when there is evidence. Click a
   row: unfold the evidence under it (wrapped, at most 8 lines, `…`). Click
   the mark: cycle open -> passed -> failed -> open (human verdict). `+ add`
   shows in triage, ready and working.
6. Decision card when a decision is open: frame with only the first line
   shaded. Line 1 `? {title}  {age}`; summary in `overlay0`; one line per
   choice `1 Run migration  (rec)` with the consequence in `overlay0` after
   `—`; `r reply` when free text is allowed. Click a choice or press `1`-`8`
   to rule; `r` opens a one-line input. After ruling, the view moves to the
   next task of the board that has an open decision, else stays.
7. Review card when the status is review and no decision is open:
   `[Accept] [Send back] [Take over]`. Accept moves to done. Send back opens a
   note input, then moves to ready and relays the note to the agent pane.
   Take over moves to working and sets `auto_status = 0`.
8. Tabs `Notes | Attempts | Artifacts` (click or `t` cycles).
   - Notes: pinned entries first, one line each with the prefix `▲`.
     Then entries oldest to newest, the last screenful shown: `you  2h` or
     `claude@mato  1h  attempt 2` in bold, body wrapped. Consecutive event
     entries collapse to one `overlay0` line `▸ 4 events`; click expands.
     The composer is the last line: `> ` input, Enter adds a human note and
     relays it to the live attempt's pane (section 5.4).
   - Attempts: one line each, newest first: `claude  mato  succeeded  2h
     14m  $0.42  ↗`. `↗` focuses the pane when it still exists. A stale
     attempt (open, pane gone) shows `release`.
   - Artifacts: one line each: kind, title, summary in `overlay0`, review
     state. Click opens: `doc` and `report` in the doc pane on the artifact's
     machine (local: `project_actions::open_local_document`; remote:
     `EndpointBridge::open_document`), `link` with the system opener, `diff`
     and `file` in the doc pane when the path ends in `.md`, else copies the
     path to the clipboard and says so.
9. Footer actions when not live: `▶ Start on…` (section 5).

Keys: `Esc` back, `m` move, `e` edit description, `1`-`8` rule, `r` reply,
`a` accept, `b` send back, `t` next tab, `s` start, `p` focus pane, `c`
focus the composer, `j/k` scroll.

### 4.4 Sketches

60 columns, grouped list, Acme selected, AC-12 selected:

```
│ Inbox  Tasks · Acme                    + new      ×
│ Triage 2 ▸
│ Ready 3
│ AC-14 Retry the sync job                          !
│   fix  ○0/2                                ▶ start
│╭AC-12 Spec decision requests░░░░░░░░░░░░░░░░░░░░░░^
│╰  spec  ✓2/3  ?  ✗                         ▶ start
│ AC-09 Inbox width setting
│   chore                                    ▶ start
│ Working 1
│ AC-11 Attention hook
│   feature  ✓1/4  ● claude@mato              ↗ pane
│ Blocked 0
│ Review 1
│ AC-10 Doc pane links
│   fix  ✓3/3  ✓  ● claude@local            ✓ accept
│ Done 14 ▸
```

100 columns, columns layout:

```
│ Inbox  Tasks · Acme                                                         + new      ×
│ Triage 2 ▸
│ Ready 3                 │ Working 1              │ Blocked 1              │ Review 1
│ AC-14 Retry the sync j !│ AC-11 Attention hook   │ AC-08 Schema rename    │ AC-10 Doc pane links
│  fix ○0/2      ▶ start  │  feat ✓1/4 ● claude    │  ? ● claude@mato       │  fix ✓3/3     ✓ accept
│╭AC-12 Spec decision r ^░│                        │                        │
│╰ spec ✓2/3 ?   ▶ start  │                        │                        │
│ AC-09 Inbox width sett  │                        │                        │
│  chore         ▶ start  │                        │                        │
│ Done 14 ▸
```

Task view, 60 columns:

```
│ Inbox  Tasks · Acme                                       ×
│ ← AC-12  [Blocked ▾]  auto  ● claude@mato 12m      ↗ pane
│ Spec decision requests
│ kind spec  pri high  ws mato/AC-12  $0.42
│ Add decision requests to the store and the panel: one
│ open decision per task, first ruling wins…       ▾ more
│ Criteria 2/3
│  ✓ schema added                                       e
│  ✗ rule returns refusal on a second ruling     chk   e
│  ○ docs updated
│  + add
│╭? Which table holds decisions?                       3m
││  New table keeps questions simple.
││  1 New decisions table  (rec) — one more migration
││  2 Reuse entries — no migration, harder queries
│╰  r reply
│ Notes  Attempts  Artifacts
│ ▲ keep the outbox format stable
│ you  2h
│   Start with the store, the panel comes after.
│ ▸ 3 events
│ claude@mato  12m  attempt 1
│   Asked which table to use; waiting.
│ > _
```

At 100 columns the task view is the same single column; the meta line holds
more pairs and the description folds at the same third of the height.

## 5. Start-task

Port of workspace-herdr start-task, without MCP, tokens or worktrees.

### 5.1 Flow

Triggered by `▶ start` on a card, `▶ Start on…` in the task view, or `s`.

1. Machine: when more than one machine is online, show the machine menu that
   `open_new_workspace_picker` already uses; else use the only one.
2. Workspace: `create_workspace_on(endpoint_id, Some(project), true, cwd,
   label, outcome)` with `cwd` = the project's `new_workspace_cwd` on that
   machine and `label` = `{display_id} {name}` cut to 40 characters. The new
   workspace joins the section through the existing `projects::update(assign
   ...)` call.
3. Context file, written before the agent starts, at
   `<herdr state dir>/drovr/tasks/{display_id}.md` on that machine (local:
   `crate::config::state_dir()`; remote: `${XDG_STATE_HOME:-$HOME/.local/state}/herdr`
   through `EndpointBridge::run_sh` with a quoted heredoc). Content, in this
   order, sections omitted when empty:
   ```
   # {display_id} {name}
   Status: {status} · Kind: {kind} · Priority: {priority} · Project: {project}

   ## Task
   {body}

   ## Acceptance criteria
   - [x] 1. {text}   (check: `{check_cmd}`)
   - [ ] 2. {text}

   ## Pinned notes
   - {body}

   ## Said since the last attempt
   - {author}: {body}          (human entries after the last attempt start)

   ## Open decision
   {title}: {choices}

   ## How to report
   Use `drovr task` (skill drovr-tasks): note, check, artifact, decide, done.
   Your task id is {display_id}; commands without an id use it.
   ```
4. Command: `PendingLaunch` gains `task: Option<TaskLaunch>`:
   ```rust
   pub(super) struct TaskLaunch {
       pub(super) display_id: String,
       pub(super) prompt: String,
       pub(super) remote: bool,
   }
   ```
   The command sent to the new pane is
   `export DROVR_TASK={id}{remote}; cc` where `{remote}` is
   ` DROVR_TASK_MODE=outbox` for a remote machine (two exports on one line).
5. Link: as soon as `tick_drovr_launch` finds the new workspace and its root
   pane, the client applies `start_attempt(display_id, NewAttempt { harness:
   "claude", machine, workspace_key, pane_key, session_id: None }, Human)`.
   This moves the task to working, sets `workspace_key` and `auto_status = 1`.
6. First prompt: when the pane's agent is detected (agent status not `None`)
   or 5 s after the command, whichever comes first, send
   `Work on drovr task {id}: {name}. Read {context path} first. Report with
   drovr task (skill drovr-tasks).` with `PaneSendText` then `PaneSendKeys
   Enter`.

Starting a task that already has a live attempt asks `{id} is running on
{machine}. Start another?`; yes ends the old attempt as stopped.

Upgrade path (not v1): replace step 2's cwd with a herdr `worktree.create`
on branch `task/{id lowercase}`, falling back to `worktree.open`.

### 5.2 Context refresh

The client rewrites the context file when the task's body, criteria or
pinned notes change and an attempt is live on that machine. The snapshot of
section 6.3 is written at the same time.

### 5.3 Worktree or workspace removed

When a workspace whose key matches an open attempt disappears from every
snapshot for 30 s, the client ends the attempt as `stopped` with the note
`workspace closed` and, if the task is working or blocked and
`auto_status = 1`, moves it to ready.

### 5.4 Relay

Human text reaches the agent's pane when an attempt is live:

- a note from the composer: `you on {id}: {body}`
- a ruling: `Decision on {id}: {label}` or `Decision on {id}: {text}`
- a send back: `{id} sent back: {note}`

Sent with `PaneSendText` + `PaneSendKeys Enter` to the attempt's pane. Not
sent when the pane is gone; the note stays in the thread.

## 6. Agent updates

### 6.1 CLI

Dispatched in `src/main.rs` next to `doc open`:
`if args[1] == "task" { std::process::exit(tasks::cli::run(&args[2..])?) }`.
Hand-parsed like `doc_view::open`; no clap.

```
drovr task list [--project NAME] [--status S[,S...]] [--all] [--json]
drovr task show [ID] [--json]
drovr task add TITLE [--project NAME] [--body TEXT|-] [--kind K]
               [--priority P] [--criterion TEXT]... [--json]
drovr task status [ID] STATUS [--note TEXT]
drovr task start [ID] [--harness NAME] [--session ID]
drovr task note [ID] TEXT|-
drovr task criteria [ID] (--add TEXT... | --set TEXT...)
drovr task check [ID] N pass|fail [--evidence TEXT|-]
drovr task artifact [ID] PATH|URL [--title T] [--kind doc|diff|link|file|report]
                    [--summary S]
drovr task done [ID] [--outcome succeeded|failed|stopped|needs_human] [--note TEXT]
drovr task release [ID] --note TEXT
drovr task decide [ID] --title T [--summary S] --choice ID:LABEL[:CONSEQUENCE]...
                  [--recommend ID] [--default ID] [--no-text]
                  [--expires MINUTES] [--wait [SECS]]
drovr task import PATH [--map KEY=SECTION]... [--dry-run]
```

Rules:

- `ID` is optional. A first positional matching `^[A-Z][A-Z0-9]*-[0-9]+$` is
  the id when the command takes more positionals than were given without it.
  Without an id: `$DROVR_TASK`, else the task of the open attempt on
  `$HERDR_PANE_ID` (local mode), else exit 2 with `no task id: pass ID or
  set DROVR_TASK`.
- `-` reads the value from stdin (up to 20 000 bytes).
- `--project` defaults to the section whose workspace holds
  `$HERDR_WORKSPACE_ID` (local mode, via the store's open attempts), else
  exit 2.
- `artifact` turns a relative path into an absolute one from the current
  directory; `--kind` defaults to `doc` for `.md`, `link` for `http(s)://`,
  `diff` for `.diff`/`.patch`, else `file`; `--title` defaults to the file
  name.
- Actor: `Agent("{harness}@{machine}")` when `$HERDR_PANE_ID` is set
  (`harness` from `$DROVR_AGENT`, default `agent`; `machine` is `local` in
  db mode, filled by the client in outbox mode), else `Human`.
- Output: one line, `{id} {status}  {message}`. `--json` prints the
  `OpResult`, `TaskDetail` or `Vec<TaskCard>` as JSON.
- Exit codes: 0 done, 1 error, 2 usage, 3 refused, 4 not found. In outbox
  mode a queued op exits 0 and prints `{id} queued`.
- `decide --wait`: polls every 500 ms until the decision leaves `open` or the
  timeout (default 600 s) passes; prints the ruling as
  `ruled {choice id}: {label}` or `ruled text: {text}`; on timeout prints
  `waiting` and exits 0.

Mode: `$DROVR_TASK_MODE` = `db` or `outbox`. Unset: `db` when
`TaskStore::default_path()` exists, else `outbox`.

### 6.2 Skill

`skills/drovr-tasks/SKILL.md`, same front-matter shape as
`skills/drovr-docs/SKILL.md`, installed by `scripts/drovr-install-hooks` the
same way. Content:

- When `DROVR_TASK` is set or the first prompt names a drovr task, read the
  context file, then report with `drovr task`.
- Record progress with `note` at milestones, not every step.
- Report every criterion with `check N pass|fail --evidence`, with the
  command output or a one-line reason as evidence. Do not mark a criterion
  passed without evidence.
- Attach documents you write for the user with `artifact` (and open them
  with drovr-docs as before).
- Ask for a decision with `decide` only when blocked; keep the title under
  120 characters, give 2-4 choices, recommend one, and use `--wait`.
- Finish with `done --outcome succeeded` when every criterion passed; a
  refusal lists the criteria still open. Use `failed`, `stopped` or
  `needs_human` otherwise, with a note.
- Never move a task to done or cancelled; the user closes tasks.

### 6.3 Remote agents: outbox pulled over the SSH bridge

Choice: the remote CLI writes each op to a file on its own machine, and the
client pulls the files over the existing SSH bridge; a pane token only rings
the bell. Reason: pane tokens are short and last-write-wins, so notes,
evidence and bursts of ops would be lost, while files keep every op and the
bridge already runs shell on each remote.

Remote layout, root `R` = `$DROVR_TASK_OUTBOX_DIR`, else
`${XDG_STATE_HOME:-$HOME/.local/state}/herdr/drovr`:

```
R/task-outbox/{pane_id}/seq            last allocated seq (text integer)
R/task-outbox/{pane_id}/.lock/         mkdir lock around seq allocation
R/task-outbox/{pane_id}/{seq}.json     one OutboxOp, one line, written as
                                       .{seq}.tmp then renamed
R/task-reply/{pane_id}-{seq}.json      OpResult written by the client
R/task-reply/{pane_id}-d{decision}.json ruling of a decision (OpResult with
                                       message = ruling line)
R/tasks/{display_id}.json              TaskDetail snapshot written by the client
R/tasks/{display_id}.md                context file (section 5.1)
```

The `seq` file is never deleted, so a reused pane id keeps counting up and
`applied_ops` stays correct.

Remote CLI in outbox mode:

1. Allocate seq (mkdir lock, retry every 20 ms up to 2 s, stale lock older
   than 10 s removed), write `{seq}.json`.
2. Ring: `herdr pane report-metadata $HERDR_PANE_ID --source drovr-task
   --token drovr_tq={seq}|{unix}` (`$HERDR_BIN_PATH` or `herdr`).
3. Wait up to 3 s for `R/task-reply/{pane}-{seq}.json` (poll 200 ms); print
   its message and exit with its code mapping. Without a reply, print
   `{id} queued` and exit 0. `decide --wait` then polls
   `R/task-reply/{pane}-d*.json` for the decision named in the first reply.
4. `show` and `list` read `R/tasks/*.json`; `list` prints only those tasks.
   Without snapshots: `no task data on this machine yet`, exit 4.

Token format: name `drovr_tq`, source `drovr-task`, value
`{seq}|{unix}`, both decimal. The client parses it with
`projects::agent_token(agent, "drovr_tq")`.

Client ingest (every tick, at most one pull in flight per endpoint):

1. For each remote agent whose `drovr_tq` seq is above
   `applied_seq("{machine}/{pane_id}")`, run one `run_sh` script that prints
   every `R/task-outbox/{pane}/*.json` with seq above the applied seq, sorted
   numerically, at most 200 files, one per line.
2. Parse each line as `OutboxOp`; apply with
   `apply_once("{machine}/{pane}", seq, op, ctx)` where `ctx = OpContext {
   actor: Agent("{agent name}@{machine}"), machine, pane_key }`. The agent
   name is the pane's detected agent (`claude`, `codex`), else `agent`.
3. One second `run_sh`: write each reply file, the snapshots and context
   files of tasks that changed, then `rm` exactly the applied op files.
4. A line that fails to parse is moved to `{seq}.bad` by the same script and
   a toast says `bad task op from {machine}/{pane}`.

Local agents (Mac panes) write the database directly in `db` mode; the
client sees the change through `data_version`.

### 6.4 Usage

`drovr-usage-hook` already reports `drovr_session` and token counters per
pane. The client copies them onto the live attempt
(`set_attempt_usage`) when they change: session id from `drovr_session`,
cost and tokens when present. No new hook.

### 6.5 Status from agent signals

`task_sync` runs in the client tick for every agent whose pane key has an open
attempt and whose task has `auto_status = 1`. It reads the same
`AgentSignal` / `ItemKind` the inbox uses.

| signal on the pane                                   | task status    | move (Auto)                 |
|------------------------------------------------------|----------------|-----------------------------|
| agent status Working, no waiting item                | ready, blocked | -> working                  |
| ItemKind Permission, Question, Plan, Asks, Dialog    | working        | -> blocked                  |
| ItemKind Finished                                    | working        | -> review when gate passes  |
| ItemKind Finished, gate fails                        | working        | none; event entry `finished with criteria {list} open` |
| ItemKind Exited, or the pane is gone                 | working, blocked | none; event entry `agent exited` |

- A move applies once per change of the pane's `drovr_state` stamp (the
  `<unix>` field), held in an in-memory map `pane_key -> stamp`.
- A signal must be stable for 2 s before it moves the task.
- Blocked caused by an open decision is left alone: a working signal does not
  move a task with an open decision.
- "doing" is the CLI's alias for working (`Status::parse`).
- Manual override: any human move sets `auto_status = 0`; the `auto` chip or
  a new start sets it back.

## 7. Build split

### 7.1 Step 0 (Builder A, first commit, before B and C start)

A commits the contract skeleton so B and C build against real signatures:

- `Cargo.toml`/`Cargo.lock` with rusqlite.
- `src/tasks/*.rs` with every type and signature of section 3; bodies may be
  `unimplemented!()` except `Status`, `Actor`, serde derives and
  `TaskStore::open_in_memory` + `migrate`.
- `mod tasks;` in `src/main.rs`.
- Empty modules registered in `src/client/shell.rs`: `mod tasks_panel;`
  (B), `mod task_launch;`, `mod task_sync;`, `mod task_ingest;` (C), each
  holding the stub items listed in 7.4 so every caller compiles.

If B or C must start before step 0 lands, they stub `src/tasks` locally with
exactly these signatures and drop their stub when rebasing.

### 7.2 Builder A: store, CLI, skill, import

Owns:

- `src/tasks/mod.rs`, `schema.rs`, `store.rs`, `transitions.rs`, `ops.rs`,
  `cli.rs`, `outbox.rs`, `import.rs`
- `skills/drovr-tasks/SKILL.md`
- `Cargo.toml`, `Cargo.lock` (rusqlite only)
- the `task` dispatch lines in `src/main.rs`
- the skill install lines in `scripts/drovr-install-hooks`

### 7.3 Builder B: panel

Owns:

- `src/client/shell/tasks_panel.rs` (TasksState, keys, mouse, render entry);
  B may split drawing into `src/client/shell/tasks_panel/board.rs` and
  `src/client/shell/tasks_panel/view.rs`
- edits in `src/client/shell/inbox.rs` (PanelView, header labels, dispatch,
  refresh in `tick_inbox`)
- `TasksSettings` in `src/client/shell/projects.rs` (one field on
  `ProjectLayout` plus the struct; nothing else in that file)

### 7.4 Builder C: start-task, sync, ingest

Owns:

- `src/client/shell/task_launch.rs`, `task_sync.rs`, `task_ingest.rs`
- edits in `src/client/shell/project_actions.rs`: `PendingLaunch` use in
  `tick_drovr_launch`, the `tick_tasks` call in the drovr tick, the
  `rename_project` call at the `ProjectRename` apply site, a `Tasks` item in
  the project menu that calls `open_tasks_panel`
- `TaskLaunch` and the `task` field on `PendingLaunch` in
  `src/client/shell/projects.rs` (that struct only)

Cross-builder functions (all `impl ClientShellState`, `pub(super)`), stubbed
in step 0:

```rust
// task_launch.rs (C)
pub(super) fn launch_task(&mut self, display_id: &str, outcome: &mut ClientShellInput);
pub(super) fn endpoint_for_machine(&self, machine: &str) -> Option<ClientEndpointId>;
pub(super) fn focus_task_pane(&mut self, pane_key: &str, outcome: &mut ClientShellInput) -> bool;
pub(super) fn relay_to_task(&mut self, display_id: &str, text: &str, outcome: &mut ClientShellInput);
// task_sync.rs (C)
pub(super) fn tick_tasks(&mut self, outcome: &mut ClientShellInput);
// tasks_panel.rs (B)
pub(super) fn open_tasks_panel(&mut self, project: String, outcome: &mut ClientShellInput);
```

### 7.5 Shared touch points

| file                                   | owner | what                                   |
|----------------------------------------|-------|----------------------------------------|
| Cargo.toml, Cargo.lock                 | A     | rusqlite                               |
| src/main.rs                            | A     | `mod tasks`, `task` dispatch           |
| src/client/shell.rs                    | A     | all four `mod` lines (step 0)          |
| src/client/shell/inbox.rs              | B     | view tabs and dispatch                 |
| src/client/shell/projects.rs           | B: `TasksSettings` on `ProjectLayout`; C: `PendingLaunch.task`, `TaskLaunch` |
| src/client/shell/project_actions.rs    | C     | launch, tick, rename, menu item        |
| scripts/drovr-install-hooks            | A     | install the drovr-tasks skill          |

Other sessions have uncommitted work in `src/client/shell/drovr_sidebar.rs`,
`src/config/sidebar.rs`, `src/doc_view/mod.rs`, `src/doc_view/select.rs`,
`scripts/drovr-workflow-hook` and its test, and `.github/README.md`. No
builder edits, stages, stashes or reformats those files. Run `cargo fmt` on
your own files only (`rustfmt <files>`).

## 8. Tests

All tests use temp directories: `DROVR_TASKS_DB`, `DROVR_TASK_OUTBOX_DIR`,
`XDG_STATE_HOME` and `XDG_CONFIG_HOME` point into a unique dir under `std::env::temp_dir()` (the pattern in
`src/detect/manifest_update.rs` tests; no new dev dependency), guarded by
`crate::config::test_config_env_lock()`. No test touches `~/.claude`,
`~/.codex`, `~/.config` or the herdr state of this machine, and none reaches
mato.

Builder A (`src/tasks/*` inline tests):

- migrate on an empty file and again on a migrated file (no-op); version rows.
- key derivation cases (one word, two words, digits first, collision).
- create two tasks: numbers 1, 2; display ids; positions increase.
- rename_project keeps display ids; rename onto an existing name refuses.
- every row of the agent/auto table allowed; agent to done refused; human
  review -> ready without a note refused.
- finish succeeded: gate refuses with open and failed lists, attempt stays
  open; passes with all criteria passed; passes with no criteria.
- one open attempt per task; start ends the previous one as stopped.
- decisions: 0 or 9 choices refused, two recommended refused, second open
  refused, first ruling wins, expiry with and without a default.
- `apply_once` twice with the same seq applies once.
- every `TaskOp` round-trips through serde; a fixed JSON line from section 6.3
  parses.
- CLI: argument parsing for each subcommand (id detection, `-` stdin,
  defaults), exit codes, db mode end to end against a temp db, outbox mode
  writes `{seq}.json` and the seq file, reuses the counter after files are
  removed.
- import: a small workspace fixture built in the test with the v8 schema
  subset (projects, tasks, criteria, entries, attempts, artifacts,
  questions): counts, status mapping, idempotent second run.

Builder B (`tasks_panel` inline tests, render into a `Buffer`):

- width 60 draws the grouped list, width 100 draws four columns.
- the selected card has a shaded first line and an unshaded second line.
- clicking a card opens the task view; clicking the start button calls the
  launch path (assert on a recorded action, not a real launch).
- BackTab toggles Inbox and Tasks; the inbox key handling is unchanged in the
  Inbox view (an existing inbox test still passes).
- review -> ready from the move menu asks for a note before writing.
- the decision card lists choices with `(rec)` and rules on `1`.
- a data_version change reloads cards on the next tick.

Builder C (`task_launch`, `task_sync`, `task_ingest` inline tests):

- the launch command line for local and remote endpoints.
- the context file text for a task with criteria, pinned notes and a
  decision.
- sync table: each row moves or does not move; `auto_status = 0` blocks
  every move; the 2 s stability rule; one move per stamp.
- ingest: the pull script and the cleanup script text (quoted paths, numeric
  sort, 200 cap); parsing a multi-line pull output; a bad line produces a
  `.bad` rename in the cleanup script; applied seq advances.
- relay text for note, ruling and send back.
- rename hook calls `rename_project` with the old and new name.

Before handing over, each builder runs `cargo test` for the crate and
`cargo clippy --all-targets` with no new warnings. `cargo` is at
`~/.rustup/toolchains/1.96.1-aarch64-apple-darwin/bin` when it is not on
`PATH`.
