# Tasks: per-project board and task view

Status: draft 2, 2026-10-03. Contract for three parallel builders (A, B, C).
Inputs: the workspace and workspace-herdr copies from mato (schema, spec
sections 5-15, 25-26 and 43, apps/tui, packages/herdr-plugin, apps/web
features/task), docs/reports/2026-10-02-mato-projects.md and
docs/design/inbox-pane.md.

Changes in draft 2 (review against the code on drovr-main):

- Open decisions are inbox rows (Waiting tab) and raise a notice; inbox rows of
  a pane that runs a task show the task id (section 4.5).
- Task view gains the waiting line, a pinned header and composer, and
  optimistic concurrency on title and body (`tasks.version`), so an agent's
  edit and a human's `$EDITOR` session cannot overwrite each other silently.
- Writers use `BEGIN IMMEDIATE`; migrations run under one immediate
  transaction and refuse a database newer than the binary; a backup is taken
  before each migration and once a day (section 2.1).
- Start-task passes `DROVR_TASK`, `DROVR_TASK_MODE`, `DROVR_TASKS_DB` and
  `DROVR_AGENT` through `workspace.create`'s `env` (stock 0.9.3 supports it)
  instead of typed `export` text, keeps its own pending launches, and sends
  the first prompt and relays with `agent.prompt` like inbox replies.
- The outbox carries a format version and a per-directory epoch, the client
  sweeps every remote outbox once a minute (ops of closed panes are not lost),
  and all SSH work runs on a background thread (section 6.3).
- Remote rollout, probing and version skew (section 6.6).
- An offline machine never ends attempts (section 5.3).
- Exact step-0 list, including the shell files that carry the new menu
  targets, the background job and the loop event; builders work in separate
  worktrees and merge A, B, C (section 7).

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
  open in the doc pane), one open decision card, and a waiting line that says
  what the task waits on (an inbox item on its pane, or an open decision).
- Open decisions in the inbox's Waiting tab, and the task id on inbox rows of
  task panes (section 4.5).
- Start a task: create a workspace in the project's section on a chosen
  machine, start Claude with a context file and a first prompt, link both.
- `drovr task` CLI for agents and for the user, plus skill `drovr-tasks`.
  `drovr task verify` runs criterion check commands in the agent's pane and
  records the verdict with the output as evidence.
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
- Answering a decision inside the inbox row. A decision row opens the task
  view on its decision card (one click); the existing hook-based inbox items
  are unchanged.
- Sidebar changes. `drovr_sidebar.rs` has uncommitted work from another
  session; the task id reaches the sidebar through the workspace label
  (`AC-12 Spec decision requests`) only.
- Drag and drop. Moves use the status chip menu.
- Running criterion checks from the panel. Agents run them with
  `drovr task verify` in their own pane, on their own machine.
- Tasks on Windows machines (the outbox and the context file need a POSIX
  shell over the SSH bridge). Start on a Windows endpoint is refused.
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

A task pane started by drovr always gets `DROVR_TASKS_DB` set to the absolute
path the client uses (section 5.1). Without it, a `cargo run` debug client
(`herdr-dev` state dir) and the release `drovr` an agent runs (`herdr` state
dir) would use two different files.

Connection settings: `journal_mode=WAL`, `synchronous=NORMAL`,
`foreign_keys=ON`. `busy_timeout` is 250 ms in the client and 5000 ms in the
CLI.

Concurrent writers. The client, every local `drovr task` process, and a second
drovr client on the same Mac may write at once. Rules:

- Every write method runs inside `BEGIN IMMEDIATE ... COMMIT` (the `write`
  helper of section 3.3). A deferred transaction that reads and then writes
  can fail with `SQLITE_BUSY_SNAPSHOT` without waiting for the busy timeout;
  an immediate one takes the write lock first and waits.
- Reads are plain statements outside a transaction, or one deferred
  transaction when they read several tables for one result (`task_detail`,
  `list`), so a reader never sees half of a concurrent write.
- Allocation (`next_number`, `entries.seq`, `position`) happens inside the
  writer's immediate transaction, never from a value read before it.
- A busy error after the timeout is `StoreError::Busy`. The client then
  keeps whatever the user typed (the composer, the one-line input, the
  `$EDITOR` file) and shows the notice `tasks db busy, try again`; nothing
  is retried in a loop on the UI thread. The CLI exits 1 with `tasks db busy`.
- A write to the current value (a move to the current status, the same
  patch) is a no-op that returns Ok without an event entry, so two clients
  applying the same automatic move write it once.

Creation. The client opens an existing file at start-up but creates the file
only on its first write (a task, a project row, an import). Reads against a
missing file return empty results. A drovr client started on another machine
therefore does not create a second store just by opening the panel; a
store there exists only if the user creates tasks there.

Only the Mac (the client machine the user creates tasks on) has a database.
`drovr task` on a remote machine writes an outbox (section 6.3).

Backups. Before a migration (section 2.5) the store writes
`{path}.v{version}.bak`. On the client's first write of each day it runs
`VACUUM INTO '{path}.{yyyymmdd}.bak'` (skipped when that file exists) and
deletes daily `.bak` files beyond the newest 7. A backup failure is logged
with `tracing::warn!` and does not block the write. Restoring is a manual
copy over `tasks.db` with every drovr process stopped.

Timestamps are written without fractional seconds (section 2.2), so text
comparison of two timestamps orders them; no code parses them.

### 2.2 Ids and time

- Row ids are `INTEGER PRIMARY KEY`. Code passes them as `i64`.
- A task's public id is `display_id` = `<project key>-<number>`, for example
  `AC-12`. The CLI and the panel use display ids only.
- `number` comes from `projects.next_number`, allocated in the same
  transaction as the insert.
- Timestamps are RFC 3339 UTC text (`time::OffsetDateTime::now_utc()`
  with the nanoseconds set to 0, formatted with
  `time::format_description::well_known::Rfc3339`), for example
  `2026-10-03T08:15:02Z`. One helper, `tasks::now_text() -> String`, makes
  them. Imported timestamps are cut to whole seconds and rewritten in this
  form. The existing `time` features (`formatting`) are enough.
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
  client shows the message as a notice (`push_task_notice`) and the tasks stay under the old name
  until the user renames again.
- Deleting a section leaves its project row and tasks. They reappear when a
  section with the same name exists again, and `drovr task list --project
  NAME` still lists them.
- The OTHER section (`projects::OTHER`) has no tasks. The Tasks view shows
  `Add this workspace to a section to track tasks.` `ensure_project` returns
  `Invalid` for an empty name or a name that starts with `\0` (OTHER's
  internal name), so no caller can create it by mistake.

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
append-only, index = version - 1. A published migration is never edited; a
change is a new entry. Until the first build that writes a real file ships,
v1 may still change (this draft changed it).

`migrate(conn: &mut Connection, path: Option<&Path>) -> StoreResult<()>`:

1. `BEGIN IMMEDIATE` (waits for the busy timeout, so a CLI and the client
   starting together migrate once: the second one finds the versions
   applied).
2. `CREATE TABLE IF NOT EXISTS migrations (...)`, then
   `SELECT COALESCE(MAX(version), 0)` inside the same transaction.
3. Found version above `MIGRATIONS.len()`: roll back and return
   `StoreError::TooNew { found, known }`. The CLI prints
   `tasks db is version {found}; this drovr knows {known}. Update drovr.` and
   exits 1; the panel shows the same text in place of the board and makes no
   writes.
4. Found version >= 1 and below `MIGRATIONS.len()`, and no file
   `{path}.v{found}.bak` yet: roll back, run
   `VACUUM INTO '{path}.v{found}.bak'` (it cannot run inside a transaction),
   then start again at step 1. The second pass finds the backup and goes on.
   In-memory stores (`path = None`) skip the backup.
5. Apply each missing version in order and insert `(version, applied_at)`,
   all in that one immediate transaction, then commit. A failure rolls back
   every version of this run.

`TaskStore::open` runs `migrate` before returning; nothing else creates
tables.

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
  version INTEGER NOT NULL DEFAULT 1,
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
  wait_until TEXT,
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
  neighbours; when the gap between them is below `1e-6` it first renumbers
  the lane to 1024, 2048, ... in the same transaction.
- `tasks.version` goes up by 1 on every write to the task row (any column),
  in the same statement (`version = version + 1`). `update_task` with
  `TaskPatch.expected_version = Some(v)` and a row at another version
  returns `Refused { code: "stale" }` and writes nothing. The panel always
  passes the version it loaded; the CLI passes none (agents append, they do
  not hold a draft open).
- `decisions.wait_until`: set by `decide --wait` to now + the wait timeout;
  the client relays a ruling into the agent's pane only when `wait_until` is
  NULL or past (section 5.4), so a waiting CLI and a typed relay never
  deliver the same ruling twice.
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
- `stale`: `{id} changed since you opened it` (section 2.5, `version`)
- `decision_ruled`: `that decision was already answered`

Attempts and closing:

- A human move into `done` or `cancelled` ends an open attempt in the same
  transaction: outcome `succeeded` when the task came from `review`, else
  `stopped` with the note `closed by you`. It also withdraws an open
  decision.
- `release` ends the open attempt as `stopped` with the note and moves the
  task to `ready`.
- A human move to `ready` from `review` (send back) keeps an open attempt
  open: the agent continues in the same pane after the relay of section 5.4,
  and its next working signal moves the task back to `working`.

Gate (`gate(&[Criterion]) -> Gate`): passes when every criterion is
`passed`. A task with no criteria passes. Failed criteria are reported before
open ones.

A human move sets `auto_status = 0` on that task. Starting a new attempt
sets it back to 1. The task view has an `auto` chip that toggles it.

Every status change writes an event entry: `event_type = "status"`, `body =
"{from} → {to}"` plus ` ({actor})` for agent and auto moves, plus `: {note}`
when a note was given. Exception: `Auto` moves between `working` and
`blocked` write no entry (a permission prompt every few minutes would bury
the thread); they still update `status_since`.

Decisions write entries too: `event_type = "decision"`, body
`asked: {title}` on request, `answered: {label or text} ({surface})` on a
ruling, `withdrawn` or `expired`. The thread therefore shows the decision
where it happened, as the workspace task page does.

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

/// RFC 3339 UTC, whole seconds (section 2.2).
pub(crate) fn now_text() -> String;

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
    pub version: i64,
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
    pub wait_until: Option<String>,
    pub created_at: String,
}

/// An open decision with what the inbox row and the notice draw.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct OpenDecision {
    pub decision: Decision,
    pub display_id: String,
    pub task_name: String,
    pub project: String,
    /// Pane of the task's open attempt, if any ("machine/pane_id").
    pub pane_key: Option<String>,
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
    /// A `machine/{workspace_id}:` prefix (the caller builds it from the
    /// endpoint label and the workspace id), so a renamed workspace still
    /// matches. The store compares `substr(workspace_key, 1, len) = prefix`,
    /// not LIKE (`_` in ids would be a wildcard).
    pub workspace_key: Option<String>,
    pub statuses: Vec<Status>,          // empty = every status
    pub include_archived: bool,         // false: archived_at IS NULL only
    pub done_limit: Option<u32>,        // newest N done/cancelled by closed_at
    pub text: Option<String>,           // case-insensitive, in name or display id
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
    /// The version the editor loaded; a different row version is refused
    /// with `stale` (section 2.5). None: no check (CLI).
    pub expected_version: Option<i64>,
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
    pub wait_until: Option<String>,
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
    TooNew { found: i64, known: i64 },  // schema newer than this binary
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
    /// Same, but Ok(None) when the file does not exist (nothing is created).
    pub(crate) fn open_existing(path: &Path, busy_ms: u32) -> StoreResult<Option<TaskStore>>;
    pub(crate) fn open_in_memory() -> StoreResult<TaskStore>;
    /// Daily `VACUUM INTO` backup of section 2.1; no-op when today's exists.
    pub(crate) fn backup_daily(&self) -> StoreResult<()>;
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
    /// Open decisions, oldest first; `project` = section name, None = all.
    pub(crate) fn open_decisions(&self, project: Option<&str>) -> StoreResult<Vec<OpenDecision>>;
    /// `decide --wait` in db mode: sets or clears `wait_until`.
    pub(crate) fn set_decision_wait(&self, decision_id: i64, wait_until: Option<&str>) -> StoreResult<()>;
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

Every write method runs in one immediate transaction (section 2.1), bumps
`tasks.updated_at` and `tasks.version`, and writes the event entries listed
in 2.6. Refusals never leave partial writes.

Methods take `&self`. rusqlite's `transaction_with_behavior` needs `&mut`,
so every write goes through one private helper:

```rust
/// BEGIN IMMEDIATE; f; COMMIT on Ok, ROLLBACK on Err (also when f panics,
/// through a drop guard).
fn write<R>(&self, f: impl FnOnce(&rusqlite::Connection) -> StoreResult<R>) -> StoreResult<R>;
```

### 3.4 Shared handle (`src/tasks/mod.rs`)

```rust
/// Writes. Runs `f` with the process-wide store, opening (and creating)
/// default_path() with busy_ms = 250 on first use, then backup_daily().
/// Under cfg(test) the store is a thread-local TaskStore::open_in_memory().
pub(crate) fn with_store<R>(f: impl FnOnce(&TaskStore) -> StoreResult<R>) -> StoreResult<R>;
/// Reads. Same handle, but when the file does not exist yet it returns
/// Ok(R::default()) without creating it.
pub(crate) fn read_store<R: Default>(f: impl FnOnce(&TaskStore) -> StoreResult<R>) -> StoreResult<R>;
```

Release builds keep the store in `OnceLock<Mutex<Option<TaskStore>>>`; an open
failure is returned on every call and retried at most once per 10 s. Only the
client's UI thread uses this handle; background jobs (section 6.3) open their
own connection with `TaskStore::open(path, 5000)` and drop it when done.

### 3.5 Ops (`src/tasks/ops.rs`)

One enum serves the local CLI, the outbox and the client. Serialized with
`#[serde(tag = "op", rename_all = "snake_case")]`. The CLI fills `task` from
the argument, else from `$DROVR_TASK`, before it applies or queues the op.
`task: None` reaches `apply` only when neither was given; `apply` then
resolves it from `OpContext.pane_key` (`task_for_pane`), else returns
`Invalid("no task id: pass ID or set DROVR_TASK")`.

`TaskOp` is append-only once the outbox format ships: a new variant or field
is added with `#[serde(default)]`; nothing is renamed or removed. A new
variant needs `OUTBOX_V` to go up (section 6.3).

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
             allow_text: bool, expires_at: Option<String>,
             /// `--wait`: seconds the CLI waits; the store sets wait_until.
             #[serde(default)] wait_secs: Option<u32> },
    Withdraw { task: Option<String> },
}

/// Outbox format version; the client accepts ops with `v <= OUTBOX_V`.
pub(crate) const OUTBOX_V: u32 = 1;

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

/// One outbox file (section 6.3). `epoch` names the outbox directory's
/// counter (section 6.3); `source` for `apply_once` is
/// "{machine}/{pane}/{epoch}".
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct OutboxOp {
    pub v: u32,
    pub epoch: String,
    pub seq: u64,
    pub ts: u64,
    pub pane: String,
    pub op: TaskOp,
}

/// `apply` result to CLI exit code: ok 0; Refused 3; NotFound 4;
/// Invalid 2; Busy, Sqlite, TooNew 1. The outbox reply carries the code.
pub(crate) fn exit_code(result: &StoreResult<OpResult>) -> i32;
```

A fixed line the tests parse:

```json
{"v":1,"epoch":"k3f9q2","seq":7,"ts":1791100000,"pane":"p12","op":{"op":"check","task":"AC-12","position":2,"state":"passed","evidence":"cargo test: 41 passed"}}
```

`Start` from an `OpContext` fills `NewAttempt` with `machine`, `pane_key` and
the workspace key of that pane when the client applies it (the client knows
the pane's workspace; the local CLI leaves it `None`).

## 4. Panel

### 4.1 Views

The right panel gets two views, `Inbox | Tasks`. Everything else about the
panel (width, overlay, focus, border drag, tick) stays in inbox.rs.

Widths. The panel is 48 columns or more (`inbox::MIN_WIDTH`). "Inner width"
below is `area.width - 3` (border, left pad, right pad), so 45 or more.

Header, line 0 of the panel, drawn by `inbox::draw` in both views:

- Left: `Inbox` and `Tasks`, two labels two spaces apart; the active one in
  the accent style (bold), the other `overlay0`. Each is a hit
  (`InboxHits.views`).
- Inbox view: the rest of line 0 is today's (counts, tabs, `≡ group`),
  starting after the two labels. At inner width under 60 the counts are
  dropped first (the tabs already switch to `W D A`).
- Tasks view: `· {project}` after the labels (`overlay0`, cut with `…`), and
  `+ new` at the right edge (hit `TasksHits.new`). No inbox tabs.
- Line 1: the filter chip, as today, in both views; `✕` clears it.

Code:

- `InboxState.view: PanelView` with
  `#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)] pub(super) enum PanelView { #[default] Inbox, Tasks }`
  and `InboxState.tasks: tasks_panel::TasksState` (B, `Default`).
- `InboxHits.views: Vec<(Rect, PanelView)>`.
- In `inbox::render`, after the header and chip are drawn, when
  `view == Tasks`: call
  ```rust
  pub(super) fn render(
      tasks: &mut TasksState,
      endpoints: &[ClientShellEndpoint],
      filter: Option<&InboxFilter>,
      focused: bool,
      palette: &Palette,
      buffer: &mut Buffer,
      body: Rect,            // the panel below the header and chip lines
  );
  ```
  `tasks_panel::render` draws only inside `body` and stores its hits in
  `tasks.hits`. It never touches the database; it reads the cache of
  `TasksState` and the endpoints (for the live agent's radar colour).
- `handle_inbox_key`: `BackTab` (shift+Tab) toggles the view in both views
  when no editor is open (inbox compose, or a Tasks input); with an editor
  open the key goes to the editor as today. In Tasks, every other key goes to
  `self.handle_tasks_key(key, outcome) -> bool` first; false falls through to
  the inbox's generic keys (Esc blur, close).
- `handle_inbox_mouse`: border drag and header view labels first; then, in
  Tasks, `self.handle_tasks_mouse(mouse, outcome) -> bool`.
- The view is remembered in the session only.
- `open_tasks_panel(project, outcome)` (B): sets `filter =
  Project(project)`, `view = Tasks`, opens and focuses the panel.

The project shown is the panel filter:

- `InboxFilter::Project(name)`: the board of that project.
- `InboxFilter::Workspace { .. }`: the tasks whose `workspace_key` matches
  that workspace; with exactly one, its task view opens directly.
- No filter: a project list, one line per section in sidebar order
  (`projects::layout().display_order()`) with lane counts
  (`Acme  2 ▸ 3 ● 1 ⚠ 1 ◎`; glyph per lane: Triage `·`, Ready `▸`, Working
  `●`, Blocked `⚠`, Review `◎`; zero lanes omitted). One click selects the
  project (sets the filter to `Project`).

TasksState (B owns the struct; C reads nothing from it):

```rust
#[derive(Debug, Default)]
pub(super) struct TasksState {
    cards: Vec<TaskCard>,                 // shown project, lane then position
    counts: Vec<(String, [u32; 6])>,      // project list
    detail: Option<TaskDetail>,           // open task view
    decisions: Vec<OpenDecision>,         // all projects, for the inbox rows
    /// "machine/pane_id" -> display id of the live attempt, for inbox rows.
    pub(super) pane_tasks: HashMap<String, String>,
    data_version: Option<i64>,
    checked: Option<Instant>,
    dirty: bool,                          // set after a panel write
    error: Option<String>,                // TooNew or open failure, drawn instead of the board
    pub(super) hits: TasksHits,
    // selection, scroll, collapsed lanes, inputs, menus: B's choice
}
```

Data refresh: `tick_inbox` calls `self.refresh_tasks(false)` (B) while the
panel is open, in either view. It reads `PRAGMA data_version` through
`read_store` at most every 500 ms and reloads `cards`, `counts`, `detail`,
`decisions` and `pane_tasks` when the value changed, when `dirty` is set, or
when the filter changed. `PRAGMA data_version` does not change for this
connection's own commits, hence `dirty`. Drawing never queries the database.

### 4.2 Board

Layout by inner width (section 4.1):

- Under 90 columns: grouped list. Lane header lines in lane order, cards under
  them. Triage and Done start collapsed when they hold more than 3 cards.
- 90 columns or more: four columns `Ready | Working | Blocked | Review`, each
  `(width - 3) / 4` wide, with one-line `Triage n ▸` above and `Done n ▸`
  below. Clicking either expands it as a full-width list above or below the
  columns.

The Done lane holds `done` and `cancelled` tasks, newest `closed_at` first,
at most 20 (`done_limit`); cancelled cards draw their name struck through in
`overlay0`. Its header count is the number of closed, unarchived tasks.
`lane_counts` counts cancelled under Done.

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
  attempt), `✓ accept` (review), nothing otherwise. The radar colour comes
  from the agent on `live.pane_key` in `endpoints` (status and
  `AgentSignal::item`), as the sidebar draws it; no pane found = `overlay0`.

Narrow cards. When a line does not fit, parts are dropped in this order
until it does: line 2 the `@machine` suffix, then the kind word, then the
outcome mark, then the agent name (the `●` stays); line 1 cuts the name with
`…`. The id, the criteria count, `?` and the button never drop; at inner
width 45 the button shortens to its glyph (`▶`, `↗`, `✓`). In the columns
layout the same order applies per column.

Hover. The card under the pointer draws its button label in the accent
colour; nothing else changes (no fill).

Selection follows the inbox: first line shaded with `active_row_bg`, a
`╭ ╰` frame on the left of the card's lines. No full-card fill.

One click acts:

- Click a card: open the task view.
- Click the right-edge button: run it (start, focus pane, accept).
- Click a lane header: collapse or expand.
- Click `+ new` (header line): a one-line input
  (`inbox_editor::NoteEditor`) parsed as `[#kind] [!priority] text`;
  Enter creates the task in Triage of the shown project. `!!` = urgent,
  `!` = high. A failed write keeps the input open with its text.
- Right-click a card: the card menu (below).
- Wheel scrolls the board.

Keys (Tasks view focused): `j/k` next/previous card across lanes, `h/l`
previous/next column (columns layout) or lane (list), `Enter` open, `n` new,
`s` start, `p` focus pane, `a` accept (review only), `m` move menu, `space`
collapse the selected card's lane, `/` filter the board by text (name and
display id, Esc clears), `Esc` clears the project filter, then blurs.

Menus reuse the shell context menu overlay (`open_menu`, drawn and driven by
context_menu.rs), with one target and these actions (all added in step 0):

```rust
// state.rs, ClientContextMenuTarget
Task { display_id: String, menu: super::tasks_panel::TaskMenu },

// tasks_panel.rs (B)
#[derive(Debug)]
pub(super) enum TaskMenu {
    Card,                                         // right-click a card
    Move { current: crate::tasks::Status },
    Kind,
    Priority,
    Machine { machines: Vec<(ClientEndpointId, String)> },  // built by C
}

// state.rs, ClientContextMenuAction (Copy)
TaskOpen, TaskStart, TaskFocusPane, TaskMoveMenu, TaskCopyId,
TaskMove(crate::tasks::Status),
TaskKind(Option<crate::tasks::Kind>),
TaskPriority(crate::tasks::Priority),
TaskOnMachine(usize),
```

- Card menu: Open, Start (when startable), Focus pane (when live), Move…,
  Copy id. Copy pushes `ClientShellAction::ClipboardWrite(bytes)` and calls
  `show_copy_feedback`, as selection copy does.
- Move menu (`m`, a click on the status chip, or Move…): the six lanes plus
  Cancelled; the current one is labelled `· {lane}` and does nothing.
  Picking `Ready` from Review asks for a note first (one-line input).
- Kind menu: `fix feature chore research spec`, then `none`. Priority menu:
  `urgent high normal low`.
- Items come from `tasks_panel::task_menu_items(&TaskMenu) -> Vec<ClientContextMenuItem>`
  and activation goes to
  `self.activate_task_menu(display_id, menu, action, (x, y), outcome)` (B),
  which forwards `TaskOnMachine(i)` to C's `launch_task_on`.

Refusals and write errors show in the panel's status line (the last body
line) for 4 s; busy errors also raise the notice of section 2.1.

### 4.3 Task view

Opens inside the panel in place of the board. `Esc` or the `←` on the header
returns to the board with the same selection. `]` and `[`, or a click on `›`
and `‹` at the right of the header, step to the next/previous card in board
order (the list it was opened from), as `j/k` do on the workspace task page.

Scrolling. The header line (region 1) and the composer line (last line of
the body, Notes tab only) stay in place; everything between them scrolls as
one column with the wheel and `j/k`. Opening a task scrolls to the top;
returning from a write keeps the scroll offset.

Regions from top to bottom:

1. Header: `← AC-12  [Review ▾]  auto  ● claude@mato 14m  ↗ pane   ‹ ›`.
   The status chip opens the move menu. `auto` is shown in accent when on,
   struck through in `overlay0` when off; click toggles. The agent part
   shows the live attempt (radar colour) and how long it has been in the
   status. When the line does not fit, parts drop in this order: the age,
   `@machine`, `‹ ›`, the agent name (the `●` stays), `auto` becomes `A`.
   `←`, the id, the status chip and `↗` never drop.
1a. Waiting line, only when the task waits on someone, one line in yellow:
   `waiting on you: permission · 3m` when the live attempt's pane has a
   waiting inbox item (`ItemKind::waiting()`, from the same
   `AgentSignal::item` the inbox uses), or `waiting on you: decision · 12m`
   when a decision is open. Click: the permission case switches the panel to
   the Inbox view with that item selected (its answer keys are there); the
   decision case scrolls to the decision card. Drawn from the cache and the
   endpoints; no query.
2. Title, bold. Click to edit (one-line input). `Untitled` in `overlay0` when
   empty. Saving passes `expected_version`; on `stale` the input stays open
   with the text and the status line says `AC-12 changed; Enter saves over
   it` (a second Enter saves without the check).
3. Meta line, wrapping: `kind fix  pri high  ws acme/AC-12  $0.42`. Each value
   is clickable: kind and priority open a small menu; `ws` focuses the
   workspace.
4. Description, folded to a third of the panel height, `▾ more` to unfold.
   `e` (or click `edit`) writes the body to
   `state_dir()/drovr/tasks/edit/{display_id}.md` and opens `$EDITOR` on it
   with `project_actions::open_local_editor(pane_id, path)` (a split under
   a local pane: the live attempt's pane when it is local, else the focused
   local pane; no local pane = status line `open a local pane to edit`). The
   panel remembers `(path, mtime, version)`; on each mtime change it saves
   the body with `expected_version = version` and takes the new version. On
   `stale` it does not save: the file stays, and the status line says
   `AC-12 changed while you edited; your text is in {path}`. The file is
   deleted after a successful save once the editor pane is gone.
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
     machine (local: `ClientShellAction::OpenLocalDocument`; remote:
     `ClientShellAction::OpenRemoteDocument` with the endpoint's bridge),
     in the workspace of the live attempt's pane when it exists, else the
     machine's focused workspace and pane; an offline machine gives the
     status line `{machine} is offline`. `link` with the system opener
     (`ClientShellAction::OpenSafeWebUrl`), `diff`
     and `file` in the doc pane when the path ends in `.md`, else copies the
     path to the clipboard (`ClipboardWrite` + `show_copy_feedback`).
9. Footer actions when not live: `▶ Start on…` (section 5). A closed task
   shows `archive` (sets `archived`; the card leaves the Done lane).

Every input (title, note, reply, send-back note, composer) keeps its text
when the write fails; it clears only on Ok.

Keys: `Esc` back, `m` move, `e` edit description, `1`-`8` rule, `r` reply,
`a` accept, `b` send back, `t` next tab, `s` start, `p` focus pane, `c`
focus the composer, `j/k` scroll, `]`/`[` next/previous task.

### 4.4 Sketches

60 columns, grouped list, Acme selected, AC-12 selected:

```
│ Inbox  Tasks · Acme                            + new
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
│ Inbox  Tasks · Acme                                                                + new
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
│ Inbox  Tasks · Acme                                 + new
│ ← AC-12  [Blocked ▾]  auto  ● claude@mato 12m ↗ pane ‹ ›
│ waiting on you: decision · 3m
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

48 columns (the minimum panel, inner width 45), board and task view:

```
│ Inbox  Tasks · Acme                    + new
│ Ready 3
│ AC-14 Retry the sync job                    !
│   ○0/2                                      ▶
│╭AC-12 Spec decision requests░░░░░░░░░░░░░░░░^
│╰  ✓2/3  ?                                   ▶
│ Working 1
│ AC-11 Attention hook
│   ✓1/4  ● claude                            ↗
│ Done 14 ▸

│ ← AC-12  [Blocked ▾]  A  ●  ↗ pane
│ waiting on you: decision · 3m
│ Spec decision requests
│ kind spec  pri high
│ ws mato/AC-12  $0.42
```

### 4.5 Inbox integration (B, in inbox.rs)

The inbox stays the place for "waiting on you". Two additions, both drawn
from `TasksState` (no query while drawing):

1. Decision rows. In the Waiting and All tabs, open decisions
   (`TasksState.decisions`, filtered by the panel filter's project) are
   drawn above the hook items, one line each:
   `? AC-12 Which table holds decisions?          3m` (`?` yellow, id
   `overlay0`, title cut with `…`, age right-aligned). With `≡ group` on
   they sit at the top of their project's group. They count in the header's
   waiting count. Selection follows the inbox rule (frame + shaded first
   line). One click: `view = Tasks`, open that task's view scrolled to the
   decision card. `j/k` in the inbox move through decision rows too (they
   come first in the order); `Enter` opens like the click. Hits:
   `InboxHits.decisions: Vec<(Rect, String /*display id*/)>`.
2. Task id on hook items. An inbox item whose pane key is in
   `TasksState.pane_tasks` shows the display id in `overlay0` before the
   workspace name. A click on the id opens the task view; the rest of the
   row behaves as today.

Notice. When a decision opens while the panel does not show that task, C's
`tick_tasks` raises a notice `{id} asks: {title}` through
`push_task_notice` (section 7.4). It tracks the open decision ids it has
seen in `TaskRuntime`; the first scan after start-up records ids without a
notice.

## 5. Start-task

Port of workspace-herdr start-task, without MCP, tokens or worktrees.

### 5.1 Flow

Triggered by `▶ start` on a card, `▶ Start on…` in the task view, `s`, or
Start in the card menu. All of it is C's `launch_task` / `launch_task_on`.

1. Machine. `launch_task(display_id, at, outcome)` lists
   `online_machines()` without Windows endpoints. One machine: go on with
   it. Several: open the `Task { menu: TaskMenu::Machine { machines } }`
   context menu at `at` (the click position); picking one calls
   `launch_task_on(display_id, endpoint_id, outcome)`. None: status line
   `no machine online`.
2. Preflight (remote only). The machine's probe result (section 6.6) must
   say `drovr-task 1` or newer. Unknown yet: run the probe job first and
   start when it answers. Missing: refuse with the notice
   `{machine}: drovr there has no task command; see docs/design/tasks.md 6.6`.
   A task that already has a live attempt asks first: the panel status line
   shows `AC-12 runs on mato.  [start another]  [cancel]` (clickable; `y`
   and `n` also work); start another ends the old attempt as `stopped`.
3. Context file, written before the workspace is created, at
   `<root>/tasks/{display_id}.md` where `<root>` is
   `crate::config::state_dir().join("drovr")` locally and
   `${XDG_STATE_HOME:-$HOME/.local/state}/herdr/drovr` on a remote machine
   (section 6.3's `R`), written by a background job (`TaskJob::WriteFiles`,
   section 6.3) with a quoted heredoc. The launch continues when the job
   answers Ok; an error stops it with the notice `cannot write context file
   on {machine}: {error}`. Content, in this order, sections omitted when
   empty:
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
   Use `drovr task` (skill drovr-tasks): note, check, verify, artifact,
   decide, done. Your task id is {display_id}; commands without an id use it.
   ```
4. Workspace. C extracts from `create_workspace_on` (project_actions.rs) a
   shared helper and makes `create_workspace_on` call it:
   ```rust
   /// Sends workspace.create (focus true) to the machine, assigns
   /// `machine/label` to `project`, activates the endpoint when it is not
   /// the active one. Returns the workspace ids known before the request.
   pub(super) fn request_workspace(
       &mut self,
       endpoint_id: &ClientEndpointId,
       project: Option<&str>,
       cwd: Option<String>,
       label: &str,
       env: HashMap<String, String>,
       outcome: &mut ClientShellInput,
   ) -> Option<HashSet<String>>;
   /// The project's `new_workspace_cwd` on that machine (the lookup that
   /// prompt_new_workspace does today, moved here and reused by it).
   pub(super) fn project_cwd(&self, endpoint_id: &ClientEndpointId, project: &str) -> Option<String>;
   ```
   `label` = `{display_id} {name}` cut to 40 characters. `env`:
   - `DROVR_TASK` = display id
   - `DROVR_AGENT` = `claude`
   - local: `DROVR_TASK_MODE=db`, `DROVR_TASKS_DB` = the client's absolute
     store path
   - remote: `DROVR_TASK_MODE=outbox`

   `workspace.create`'s `env` exists in stock herdr 0.9.3
   (`WorkspaceCreateParams.env`) and reaches the root pane's process, so no
   `export` text is typed into a shell. Task launches do not use
   `projects::PendingLaunch` (one global slot, which a second launch would
   overwrite); C keeps them in `TaskRuntime.launches: Vec<TaskLaunch>`:
   ```rust
   pub(super) struct TaskLaunch {
       pub(super) display_id: String,
       pub(super) endpoint_id: ClientEndpointId,
       pub(super) label: String,
       pub(super) known: HashSet<String>,
       pub(super) since: Instant,
       /// Set once the workspace and its root pane are found.
       pub(super) pane: Option<(String /*workspace key*/, String /*pane key*/, String /*pane id*/)>,
       pub(super) typed_at: Option<Instant>,
   }
   ```
5. Agent command. When `tick_tasks` finds the new workspace (label match,
   id not in `known`, as `tick_drovr_launch` does) and its root pane, it
   sends `PaneSendText { text: "cc" }` + `PaneSendKeys ["Enter"]` through
   `endpoint_request`, sets `typed_at`, and applies
   `start_attempt(display_id, NewAttempt { harness: "claude", machine,
   workspace_key, pane_key, session_id: None }, Human)`. This moves the task
   to working and sets `workspace_key` and `auto_status = 1`. A launch not
   found within 60 s is dropped with the notice `{id}: workspace did not
   appear on {machine}`.
6. First prompt. When the pane's agent is detected (agent status not
   `None`), or 5 s after `typed_at`, whichever comes first, send
   `Method::AgentPrompt { target: pane_id, text, wait: None }` (the call the
   inbox uses for replies) with
   `Work on drovr task {id}: {name}. Read {context path} first. Report with
   drovr task (skill drovr-tasks).` Then the launch is removed.

Several launches may be pending at once; each is matched by its own label
and `known` set.

Upgrade path (not v1): replace step 4's cwd with a herdr `worktree.create`
on branch `task/{id lowercase}`, falling back to `worktree.open`.

### 5.2 Context refresh

The client rewrites the context file when the task's body, criteria or
pinned notes change and an attempt is live on that machine, through one
`TaskJob::WriteFiles` per machine (local machine: the same job on
`/bin/sh`). The snapshot `R/tasks/{id}.json` of section 6.3 is written in
the same job. `tick_tasks` notices the change through `data_version` and
compares `tasks.version` per live task with the version it last wrote.

### 5.3 Worktree or workspace removed

When a workspace whose key matches an open attempt is missing from its
machine's snapshot for 30 s while that machine stays Online with the same
`boot_id`, the client ends the attempt as `stopped` with the note
`workspace closed` and, if the task is working or blocked and
`auto_status = 1`, moves it to ready.

An offline machine, a reconnect, or a new `boot_id` (server restart) resets
the 30 s clock for every attempt on that machine; none of them ends an
attempt. A machine that is offline for days leaves its attempts open; the
Attempts tab shows them as stale with `release`, which the user clicks.

### 5.4 Relay

Human text reaches the agent's pane when an attempt is live:

- a note from the composer: `you on {id}: {body}`
- a ruling: `Decision on {id}: {label}` or `Decision on {id}: {text}`
- a send back: `{id} sent back: {note}`

Sent with `Method::AgentPrompt { target: pane_id, text, wait: None }` to the
attempt's pane through `endpoint_request`, as inbox replies are. Not sent
when the pane is gone or its machine is offline; the note stays in the
thread and the status line says `saved; {machine} is offline`.

A ruling is relayed only when the decision's `wait_until` is NULL or past:
a `decide --wait` CLI that is still polling receives the ruling itself, and
a relay would deliver it twice. A remote waiting CLI gets the ruling from
the reply file of section 6.3, which the client writes right after the
ruling (a `TaskJob::WriteFiles`), not at the next ingest.

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
drovr task verify [ID] [N]...
drovr task artifact [ID] PATH|URL [--title T] [--kind doc|diff|link|file|report]
                    [--summary S]
drovr task done [ID] [--outcome succeeded|failed|stopped|needs_human] [--note TEXT]
drovr task release [ID] --note TEXT
drovr task decide [ID] --title T [--summary S] --choice ID:LABEL[:CONSEQUENCE]...
                  [--recommend ID] [--default ID] [--no-text]
                  [--expires MINUTES] [--wait [SECS]]
drovr task import PATH [--map KEY=SECTION]... [--dry-run]
drovr task proto
```

Rules:

- `ID` is optional. A first positional matching `^[A-Za-z][A-Za-z0-9]*-[0-9]+$`
  is the id (uppercased) when the command takes more positionals than were
  given without it.
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
  (`harness` from `$DROVR_AGENT`, which start-task sets, default `agent`;
  `machine` is `local` in db mode, filled by the client in outbox mode),
  else `Human`.
- `verify`: for each criterion N given (default: every criterion with a
  `check_cmd`), runs `sh -c {check_cmd}` in the current directory with a
  300 s limit, captures stdout and stderr (last 20 000 bytes), and records
  `check N pass` (exit 0) or `check N fail` with `$ {cmd}\n{output}\nexit
  {code}` as evidence, one `Check` op per criterion. In db mode it reads the
  commands from the store; in outbox mode from the snapshot `R/tasks/{id}.json`
  (no snapshot: exit 4 `no task data on this machine yet`). Prints one line
  per criterion. The commands come from the task, which only the user and
  agents on the user's machines write; `verify` runs them with the agent's
  own rights, in the agent's own pane, as the agent could anyway.
- `proto`: prints `drovr-task {OUTBOX_V}` and exits 0. The client's probe
  (section 6.6) runs it.
- Output: one line, `{id} {status}  {message}`. `--json` prints the
  `OpResult`, `TaskDetail` or `Vec<TaskCard>` as JSON.
- Exit codes: 0 done, 1 error, 2 usage, 3 refused, 4 not found. In outbox
  mode a queued op exits 0 and prints `{id} queued`.
- `decide --wait`: polls every 500 ms until the decision leaves `open` or the
  timeout (default 600 s) passes; prints the ruling as
  `ruled {choice id}: {label}` or `ruled text: {text}`; on timeout prints
  `waiting` and exits 0.

Mode: `$DROVR_TASK_MODE` = `db` or `outbox` (start-task always sets it).
Unset (an agent the user started by hand): `db` when `$DROVR_TASKS_DB` is
set or `TaskStore::default_path()` exists, else `outbox`. In db mode the CLI
opens the file with `TaskStore::open_existing`; a missing file exits 1 with
`no tasks db at {path}`. Only `add` and `import` run outside a herdr pane
(Human actor) create it with `TaskStore::open`.

### 6.2 Skill

`skills/drovr-tasks/SKILL.md`, same front-matter shape as
`skills/drovr-docs/SKILL.md`, installed by `scripts/drovr-install-hooks` the
same way. Content:

- When `DROVR_TASK` is set or the first prompt names a drovr task, read the
  context file, then report with `drovr task`.
- Record progress with `note` at milestones, not every step.
- Run `drovr task verify` for criteria that have a check command. Report
  every other criterion with `check N pass|fail --evidence`, with the
  command output or a one-line reason as evidence. Do not mark a criterion
  passed without evidence.
- If `drovr task` answers `unknown command` or is missing, say so once in
  your reply and continue the work; do not retry.
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
`${XDG_STATE_HOME:-$HOME/.local/state}/herdr/drovr` (the release
`state_dir()` on macOS and Linux; the client's scripts use the shell form,
the remote CLI uses `state_dir().join("drovr")` from a release build, which
is the same path):

```
R/task-outbox/{pane_id}/epoch          6 random [a-z0-9] chars, created with seq
R/task-outbox/{pane_id}/seq            last allocated seq (text integer)
R/task-outbox/{pane_id}/.lock/         mkdir lock around seq allocation
R/task-outbox/{pane_id}/{seq}.json     one OutboxOp, one line, written as
                                       .{seq}.tmp then renamed
R/task-reply/{pane_id}-{epoch}-{seq}.json  OpResult plus "exit", written by the client
R/task-reply/{pane_id}-d{decision}.json    ruling of a decision (OpResult with
                                       message = ruling line)
R/tasks/{display_id}.json              TaskDetail snapshot written by the client
R/tasks/{display_id}.md                context file (section 5.1)
```

The `seq` and `epoch` files are never deleted by drovr, so a reused pane id
keeps counting up. If someone deletes the directory, the CLI creates a new
epoch with seq starting at 1; because `apply_once`'s source is
`{machine}/{pane}/{epoch}`, the new ops are not mistaken for applied ones.

Remote CLI in outbox mode:

1. Allocate seq (mkdir lock, retry every 20 ms up to 2 s, stale lock older
   than 10 s removed), create `epoch` when missing, write `{seq}.json` with
   `v = OUTBOX_V`, fsync, rename.
2. Ring: `herdr pane report-metadata $HERDR_PANE_ID --source drovr-task
   --seq {unix nanos} --token drovr_tq={epoch}.{seq}|{unix}`
   (`$HERDR_BIN_PATH` or `herdr`). A failed ring is ignored: the sweep
   below still finds the file.
3. Wait up to 3 s for `R/task-reply/{pane}-{epoch}-{seq}.json` (poll 200 ms);
   print its message and exit with its `exit`. Without a reply, print
   `{id} queued` and exit 0. `decide --wait` then polls
   `R/task-reply/{pane}-d*.json` for the decision named in the first reply;
   when the first reply did not come, it polls the snapshot
   `R/tasks/{id}.json` for a decision it created (title match) to learn the
   id.
4. `show` and `list` read `R/tasks/*.json`; `list` prints only those tasks.
   Without snapshots: `no task data on this machine yet`, exit 4.

Token format: name `drovr_tq`, source `drovr-task`, value
`{epoch}.{seq}|{unix}`. The client parses it with
`projects::agent_token(agent, "drovr_tq")`.

Client ingest. All SSH work runs on a background thread, never in the tick:
`EndpointBridge::run_sh` blocks for up to 15 s. C adds one action and one
loop event (step 0 adds the variants):

```rust
// state.rs, ClientShellAction
TaskJob { route: super::inbox::ApiRoute, job: super::task_ingest::TaskJob },
// events.rs, ClientLoopEvent
TaskJobDone(crate::client::shell::TaskJobDone),

// task_ingest.rs (C)
pub(crate) enum TaskJob {
    /// Pull: print every outbox file of these panes (None = all panes)
    /// above the given applied seq per (pane, epoch); at most 200 files.
    Pull { machine: String, panes: Option<Vec<String>>, applied: Vec<(String, String, u64)> },
    /// Write reply, snapshot and context files, then remove the applied op
    /// files and every op file at or below the applied seq.
    WriteFiles { machine: String, script: String },
    /// `drovr task proto` on the machine (section 6.6).
    Probe { machine: String },
}
pub(crate) struct TaskJobDone { pub machine: String, pub kind: &'static str, pub result: Result<String, String> }
/// Runs the job (local: /bin/sh; remote: bridge.run_sh) and posts
/// TaskJobDone to the loop. Without `events` (tests) the result is dropped.
pub(crate) fn run_job(route: ApiRoute, job: TaskJob, events: Option<LoopEvents>);
// impl ClientShellState (C)
pub(crate) fn receive_task_job(&mut self, done: TaskJobDone) -> bool; // true = repaint
```

`src/client/shell_runtime.rs` runs `TaskJob` like `InboxTask` (spawn with
`shell.drovr_events`), and `src/client/mod.rs` hands `TaskJobDone` to
`receive_task_job` like `InboxReply`. At most one job per machine is in
flight (`TaskRuntime.busy: HashSet<String>`).

When to pull, per remote machine:

- an agent's `drovr_tq` value changed since the last pull (in-memory map
  `pane_key -> token value`);
- on connect (the machine turns Online) and then every 60 s, a sweep:
  `Pull { panes: None }`, which lists every `R/task-outbox/*/` directory.
  The sweep is what picks up ops from panes that closed before the client
  saw their token, and ops queued while the client was not running.

Applying a pull, in `receive_task_job` on the UI thread:

1. Each output line is `{pane}\t{epoch}\t{seq}\t{json}`, numeric order per
   (pane, epoch). Parse the JSON in two steps: first `{"v": u32}` only. `v`
   above `OUTBOX_V`: leave the file, notice once per machine
   `{machine} runs a newer drovr task; update drovr on this Mac`. Else parse
   `OutboxOp`; a failure moves the file to `{seq}.bad` (in the cleanup
   script) with the notice `bad task op from {machine}/{pane}`.
2. Apply with `apply_once("{machine}/{pane}/{epoch}", seq, op, ctx)` where
   `ctx = OpContext { actor: Agent("{agent}@{machine}"), machine, pane_key }`;
   `agent` is the pane's detected agent (`claude`, `codex`), else `agent`.
   A pane no longer in the snapshot still applies (actor `agent@{machine}`).
   A `Busy` error stops the batch; the files stay and the next pull retries
   them. Each op commits on its own, so a crash after op 3 of 5 leaves 1-3
   applied and recorded, and 4-5 pulled again.
3. Then one `WriteFiles` job: reply files of the applied ops, snapshots and
   context files of the tasks that changed, then `rm` of the applied op
   files, of any op file at or below the applied seq of its (pane, epoch),
   and the `.bad` renames. The op files go only after the database commit
   (step 2), so a crash between the two re-pulls ops that `apply_once`
   skips.
4. Reply files older than 1 day are removed by the same cleanup script.

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

- A move applies once per change of the pane's state stamp,
  `AgentSignal::since()` (the `drovr_state` time, or the later beat),
  together with the agent status; held in `TaskRuntime.stamps:
  HashMap<String /*pane key*/, (AgentStatus, Option<u64>)>`.
- A signal must be stable for 2 s before it moves the task.
- Blocked caused by an open decision is left alone: a working signal does not
  move a task with an open decision.
- "doing" is the CLI's alias for working (`Status::parse`).
- Manual override: any human move sets `auto_status = 0`; the `auto` chip or
  a new start sets it back.
- `tick_tasks` runs from `tick_drovr` (100 ms timer) whether the panel is
  open or not. Its own cadence: sync on every call (it only reads
  snapshots, and writes on a change); `data_version` check and decision
  notices every 1 s; `expire_decisions(now)` every 30 s; the remote sweep
  every 60 s per machine. Each write it makes is one short immediate
  transaction.

### 6.6 Remote machines

Each remote machine needs three things, installed by the user (drovr never
installs or upgrades software on a remote by itself):

1. A drovr binary with the `task` command:
   `ssh mato 'bash -s -- drovr-vX' < scripts/drovr-install`, where
   `drovr-vX` is the first release that contains this feature. Until a
   release exists, copy a build:
   `scp target/release/herdr mato:.local/share/drovr/drovr` (same OS and
   architecture only; mato is an Apple silicon Mac like this one).
2. The `drovr-tasks` skill: `ssh mato 'bash -s' < scripts/drovr-install-hooks`
   (A adds the skill to the installer next to drovr-docs). The installer
   downloads from GitHub `main`; before that branch has the skill, copy it:
   `scp -r skills/drovr-tasks mato:.claude/skills/`.
3. The hooks already in place (state, usage); nothing new.

Probe. On connect, the client runs `TaskJob::Probe` on each remote machine:
`drovr=$(command -v drovr || echo "$HOME/.local/bin/drovr"); "$drovr" task proto`.
The answer `drovr-task {n}` is kept in `TaskRuntime.probe: HashMap<String,
Option<u32>>`. No answer, another output, or a non-zero exit = `None`.

- `None`: start-task on that machine is refused (section 5.1, step 2); the
  sweep still runs (it finds nothing).
- `n < OUTBOX_V`: start is allowed when the client can still read format
  `n` (it can read every older `v`); the notice
  `{machine} runs an older drovr task; update it` shows once.
- `n > OUTBOX_V`: start is allowed; ops with a newer `v` stay on the remote
  until this Mac is updated (section 6.3).

Agents started by hand on a remote (not through start-task) have no
`DROVR_TASK`; `drovr task` there works when the agent passes an id.

## 7. Build split

Each builder works in its own git worktree, never in the shared checkout
(another session has uncommitted work there):
`git worktree add ../drovr-worktrees/tasks-{a,b,c} -b tasks/{a,b,c} drovr-main`
after step 0 is committed on `drovr-main`. Merge order: A, then B rebased on
A, then C rebased on B. Conflicts can only appear in the step-0 lines of
section 7.5; each owner keeps the other builders' lines.

### 7.1 Step 0 (Builder A, one commit on drovr-main, before B and C start)

A commits the contract skeleton so B and C build against real signatures.
Everything compiles, `cargo test` passes, and no behaviour changes for a
user who never opens Tasks.

- `Cargo.toml`/`Cargo.lock` with rusqlite (`bundled`).
- `src/tasks/*.rs` with every type and signature of section 3. Working
  bodies for: `Status`, `Actor`, serde derives, `now_text`, `migrate`,
  `open`, `open_existing`, `open_in_memory`, `with_store`, `read_store`,
  `data_version`, `ensure_project`, `create_task`, `task`, `task_detail`,
  `list`, `lane_counts`, `open_decisions`. Every other body is
  `Err(StoreError::Invalid("not built yet".into()))` (not
  `unimplemented!()`, so a B or C test hitting one fails cleanly).
- `mod tasks;` in `src/main.rs` and the `task` dispatch, next to `doc open`,
  returning exit 2 `drovr task: not built yet` until A's CLI lands.
- `src/client/shell.rs`: `mod tasks_panel; mod task_launch; mod task_sync;
  mod task_ingest;` and `pub(crate) use task_ingest::TaskJobDone;`.
- Stub files with the items of 7.4 and 7.5, bodies empty or returning
  `false`/`None`:
  - `tasks_panel.rs`: `TasksState`, `TasksHits`, `TaskMenu`, `render`,
    `task_menu_items`, and the `impl ClientShellState` functions of 7.4
    marked (B).
  - `task_launch.rs`, `task_sync.rs`, `task_ingest.rs`: `TaskRuntime`,
    `TaskLaunch`, `TaskJob`, `TaskJobDone`, `run_job`, and the functions of
    7.4 marked (C).
- `src/client/shell/state.rs`: the `ClientContextMenuTarget::Task` variant,
  a field `task: Option<String>` on the `ProjectWorkspace` and `Agent`
  variants (set to `None` at their two construction sites in
  project_actions.rs), the nine `ClientContextMenuAction::Task*` variants,
  the
  `ClientShellAction::TaskJob` variant, and the field
  `pub(super) task_rt: super::task_sync::TaskRuntime` on `ClientShellState`
  with `Default::default()` in its constructor.
- `src/client/shell/inbox.rs`: `view: PanelView` and
  `tasks: super::tasks_panel::TasksState` on `InboxState` (no behaviour).
- `src/client/shell/context_menu.rs`: route `Task { .. }` items to
  `tasks_panel::task_menu_items` and activation to `activate_task_menu`,
  before the fallback arm.
- `src/client/events.rs`: `ClientLoopEvent::TaskJobDone`.
- `src/client/mod.rs`: the `TaskJobDone` arm, shaped like `InboxReply`.
- `src/client/shell_runtime.rs`: the `TaskJob` arm, shaped like
  `InboxTask`.
- `src/client/shell/project_actions.rs`: visibility only. `open_menu`,
  `endpoint_request` and `online_machines` become `pub(super)`; the call
  `self.tick_tasks(outcome);` is added in `tick_drovr` after
  `self.tick_inbox(outcome);`.

If B or C must start before step 0 lands, they stub exactly these items
locally and drop the stubs when rebasing.

### 7.2 Builder A: store, CLI, skill, import

Owns:

- `src/tasks/mod.rs`, `schema.rs`, `store.rs`, `transitions.rs`, `ops.rs`,
  `cli.rs`, `outbox.rs`, `import.rs`
- `skills/drovr-tasks/SKILL.md`
- `Cargo.toml`, `Cargo.lock` (rusqlite only)
- the `task` dispatch lines in `src/main.rs`
- the skill install lines in `scripts/drovr-install-hooks`
- the step-0 edits listed in 7.1 (after step 0, the owners of 7.5 take them
  over)

### 7.3 Builder B: panel and inbox integration

Owns:

- `src/client/shell/tasks_panel.rs`; B may split drawing into
  `src/client/shell/tasks_panel/board.rs` and
  `src/client/shell/tasks_panel/view.rs`
- edits in `src/client/shell/inbox.rs`: header labels, `PanelView`
  switching, dispatch to the Tasks functions, `refresh_tasks` in
  `tick_inbox`, decision rows and task ids (section 4.5)
- `TasksSettings` in `src/client/shell/projects.rs`: the struct and one
  field on `ProjectLayout`
  (`#[serde(default, skip_serializing_if = "TasksSettings::is_default")] pub(super) tasks: TasksSettings`
  with `collapsed: Vec<String>` entries `"{project}:{lane}"`). Nothing else
  in that file.

### 7.4 Builder C: start-task, sync, ingest

Owns:

- `src/client/shell/task_launch.rs`, `task_sync.rs`, `task_ingest.rs`
- edits in `src/client/shell/project_actions.rs`: `request_workspace` and
  `project_cwd` (extracted, section 5.1), the `rename_project` call at the
  `ProjectRename` apply site in `save_project_prompt` (called with the old
  and new name only when the section rename was applied), a `Tasks` item in
  the project menu (`ClientContextMenuTarget::Project { name, .. }` with
  `Action::TaskOpen` calls `open_tasks_panel(name)`), and a `Task {id}` item
  in the workspace and agent menus when the target's `task` field is set
  (`Action::TaskOpen` calls `open_task_view(id)`). C fills `task` in
  `workspace_target` and `agent_target` with one `read_store` lookup
  (`list` with the workspace prefix, or `task_for_pane`).
- the `TaskJob` arm in `shell_runtime.rs` and the `TaskJobDone` arm in
  `client/mod.rs` after step 0

Cross-builder functions (all `impl ClientShellState`, `pub(super)` unless
noted), stubbed in step 0:

```rust
// task_launch.rs (C)
pub(super) fn launch_task(&mut self, display_id: &str, at: (u16, u16), outcome: &mut ClientShellInput);
pub(super) fn launch_task_on(&mut self, display_id: &str, endpoint_id: ClientEndpointId, outcome: &mut ClientShellInput);
pub(super) fn endpoint_for_machine(&self, machine: &str) -> Option<ClientEndpointId>;
/// Focuses the pane on its machine (activating the endpoint); false when
/// the pane is gone or the machine is offline.
pub(super) fn focus_task_pane(&mut self, pane_key: &str, outcome: &mut ClientShellInput) -> bool;
/// Relays `text` to the live attempt's pane (section 5.4); false when not sent.
pub(super) fn relay_to_task(&mut self, display_id: &str, text: &str, outcome: &mut ClientShellInput) -> bool;
/// Queues a WriteFiles job with the ruling reply file for a remote waiting CLI.
pub(super) fn publish_ruling(&mut self, decision: &Decision, outcome: &mut ClientShellInput);
/// A notice in the shell's notice line (push_endpoint_notice, key "drovr.tasks").
pub(super) fn push_task_notice(&mut self, message: String) -> bool;
// task_sync.rs (C)
pub(super) fn tick_tasks(&mut self, outcome: &mut ClientShellInput);
// task_ingest.rs (C)
pub(crate) fn receive_task_job(&mut self, done: TaskJobDone) -> bool;

// tasks_panel.rs (B)
pub(super) fn open_tasks_panel(&mut self, project: String, outcome: &mut ClientShellInput);
/// Opens the task view of `display_id` (switching project and view).
pub(super) fn open_task_view(&mut self, display_id: &str, outcome: &mut ClientShellInput);
pub(super) fn handle_tasks_key(&mut self, key: &crate::input::TerminalKey, outcome: &mut ClientShellInput) -> bool;
pub(super) fn handle_tasks_mouse(&mut self, mouse: MouseEvent, outcome: &mut ClientShellInput) -> bool;
pub(super) fn activate_task_menu(&mut self, display_id: String, menu: TaskMenu, action: ClientContextMenuAction, at: (u16, u16), outcome: &mut ClientShellInput);
pub(super) fn refresh_tasks(&mut self, force: bool);
pub(super) fn task_menu_items(menu: &TaskMenu) -> Vec<ClientContextMenuItem>; // free fn
```

Who calls what: B's panel calls `launch_task`, `focus_task_pane`,
`relay_to_task` (after a note, a send back, a ruling whose `wait_until`
allows it), `publish_ruling` (after every ruling on a remote attempt) and
`push_task_notice`. C's code calls `open_tasks_panel` (project menu) and
`open_task_view` (workspace and agent menu items). Store writes go through
`tasks::with_store` from either side; neither builder calls the other's
private helpers.

### 7.5 Shared touch points

| file                                   | owner after step 0 | what |
|----------------------------------------|-------|------|
| Cargo.toml, Cargo.lock                 | A     | rusqlite |
| src/main.rs                            | A     | `mod tasks`, `task` dispatch |
| src/client/shell.rs                    | A (step 0 only) | four `mod` lines, one `use` |
| src/client/shell/state.rs              | A (step 0 only) | menu target and actions, `TaskJob` action, `task_rt` field |
| src/client/events.rs                   | A (step 0 only) | `TaskJobDone` event |
| src/client/mod.rs                      | C     | `TaskJobDone` arm |
| src/client/shell_runtime.rs            | C     | `TaskJob` arm |
| src/client/shell/context_menu.rs       | A (step 0 only) | `Task` routing |
| src/client/shell/inbox.rs              | B     | view tabs, dispatch, refresh, decision rows, task ids |
| src/client/shell/projects.rs           | B     | `TasksSettings` on `ProjectLayout` (C does not touch this file; `PendingLaunch` is unchanged) |
| src/client/shell/project_actions.rs    | C     | `request_workspace`, `project_cwd`, rename hook, menu items; step 0: visibility, `task: None`, the `tick_tasks` call |
| scripts/drovr-install-hooks            | A     | install the drovr-tasks skill |

After step 0 nobody edits a file marked "step 0 only" without telling the
other builders; a needed change goes into the owner's next commit.

Other sessions have uncommitted work in `src/client/shell/drovr_sidebar.rs`,
`src/config/sidebar.rs`, `src/doc_view/mod.rs`, `src/doc_view/select.rs`,
`scripts/drovr-workflow-hook` and its test, and `.github/README.md`. No
builder edits, stages, stashes or reformats those files, in the shared
checkout or in a worktree. Run `rustfmt` on your own files only
(`rustfmt --edition 2021 <files>`), never `cargo fmt`.

## 8. Tests

All tests use temp directories: `DROVR_TASKS_DB`, `DROVR_TASK_OUTBOX_DIR`,
`XDG_STATE_HOME` and `XDG_CONFIG_HOME` point into a unique dir under `std::env::temp_dir()` (the pattern in
`src/detect/manifest_update.rs` tests; no new dev dependency), guarded by
`crate::config::test_config_env_lock()`. No test touches `~/.claude`,
`~/.codex`, `~/.config` or the herdr state of this machine, and none reaches
mato.

Builder A (`src/tasks/*` inline tests):

- migrate on an empty file and again on a migrated file (no-op); version rows.
- migrate race: two threads open the same temp file at once; both succeed,
  one set of version rows.
- a file with a version above `MIGRATIONS.len()` returns `TooNew` and is
  left unchanged; an older version gets `{path}.v{n}.bak` before migrating.
- concurrent writers: two `TaskStore::open(path, 5000)` connections on two
  threads each add 200 notes to the same task; 400 entries, seqs 1..=400
  with no gap or duplicate; the same with `create_task` gives numbers
  1..=400.
- `open_existing` on a missing path returns None and creates nothing;
  `read_store` returns the default.
- `update_task` with a stale `expected_version` returns `stale` and leaves
  the body; with the current one it saves and bumps the version.
- a move to the current status is Ok, writes no entry, keeps `version`.
- a human move to done ends the open attempt (succeeded from review,
  stopped otherwise) and withdraws an open decision.
- reorder renumbers a lane when the gap is below 1e-6.
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
- `apply_once` twice with the same seq applies once; the same seq under a
  new epoch (another source string) applies again.
- `OutboxOp` with `v = OUTBOX_V + 1` is rejected by the two-step parse
  without touching the store.
- every `TaskOp` round-trips through serde; a fixed JSON line from section 6.3
  parses.
- CLI: argument parsing for each subcommand (id detection, lowercase id,
  `-` stdin, defaults), exit codes, db mode end to end against a temp db,
  db mode with a missing file exits 1 and creates nothing, outbox mode
  writes `{seq}.json`, the seq and epoch files, reuses the counter after op
  files are removed, and makes a new epoch after the directory is removed.
- `verify` against a temp db: a passing and a failing `check_cmd`
  (`true`, `sh -c 'echo no; exit 3'`) record passed and failed with the
  output and exit code in the evidence.
- `proto` prints `drovr-task 1`.
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
- a data_version change reloads cards on the next tick; a panel write sets
  `dirty` and reloads without a data_version change.
- widths 48 and 45-inner: no line is wider than the body, the id, criteria
  count and button glyph are drawn, and parts drop in the order of 4.2.
- the header line and the composer stay in place while the task view
  scrolls.
- a failed write (stub returning Busy) keeps the input text.
- a stale title save keeps the input open; a second Enter saves.
- decision rows: drawn first in Waiting, counted in the header, a click
  opens the task view on the decision; the task id shows on a hook item
  whose pane is in `pane_tasks`.

Builder C (`task_launch`, `task_sync`, `task_ingest` inline tests):

- the `workspace.create` request for local and remote endpoints: label,
  cwd, and the env map of section 5.1 (no typed `export`).
- two launches pending at once each find their own workspace and type
  `cc` once.
- the first prompt goes out as `AgentPrompt` when the agent is detected,
  else after 5 s.
- an offline machine, a reconnect and a new boot id do not end attempts;
  a workspace missing for 30 s on an online machine with the same boot id
  does.
- a ruling is relayed when `wait_until` is NULL or past and not while it is
  in the future.
- probe parsing (`drovr-task 1`, missing command, garbage) and the start
  refusal for `None`.
- decision notices: none for ids seen at start-up, one per new open id.
- the context file text for a task with criteria, pinned notes and a
  decision.
- sync table: each row moves or does not move; `auto_status = 0` blocks
  every move; the 2 s stability rule; one move per stamp.
- ingest: the pull and sweep script text (quoted paths, numeric sort, 200
  cap, every pane directory for a sweep); parsing a multi-line pull output
  with two epochs; a bad line produces a `.bad` rename; a newer `v` leaves
  the file and raises one notice; a Busy stop leaves the remaining files;
  the cleanup script removes only applied files and files at or below the
  applied seq. Run the pull and cleanup scripts with `/bin/sh` against a
  temp `DROVR_TASK_OUTBOX_DIR` (the local route), not over SSH.
- relay text for note, ruling and send back.
- rename hook calls `rename_project` with the old and new name.

Before handing over, each builder runs `just check` (or `cargo test` and
`cargo clippy --all-targets` with no new warnings when the Windows stage
cannot run on this Mac; say so in the hand-over). Builder A checks that
`libsqlite3-sys` with `bundled` builds in the Windows stage of `just check`
when it is available; if it does not, A reports it before B and C start,
since every builder then needs the same `cfg` gate. `cargo` is at
`~/.rustup/toolchains/1.96.1-aarch64-apple-darwin/bin` when it is not on
`PATH`.
