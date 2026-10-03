//! The tasks store (docs/design/tasks.md sections 2 and 3.3).
//!
//! Every write runs in one `BEGIN IMMEDIATE` transaction through
//! [`TaskStore::write`]; the bodies are free functions over a
//! `&Connection` so `apply_once` and the import can run several of them in
//! one transaction.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, Row, Transaction, TransactionBehavior};
use serde::Serialize;

use super::ops::{OpContext, OpResult, TaskOp};
use super::schema::{self, backup_path, vacuum_into};
use super::transitions::{self, check_move, gate};
use super::{
    cut_text, now_text, Actor, Artifact, Attempt, CheckState, Choice, Criterion, Decision,
    DecisionState, Entry, EntryKind, NewArtifact, NewAttempt, NewDecision, NewTask, OpenDecision,
    Outcome, Project, Review, Ruling, Status, StoreError, StoreResult, Task, TaskCard, TaskDetail,
    TaskFilter, TaskPatch, MAX_TEXT,
};

pub(crate) struct TaskStore {
    conn: Connection,
    /// The file, None for an in-memory store (no backups).
    path: Option<PathBuf>,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct ImportReport {
    pub tasks: u32,
    pub projects: u32,
    pub skipped: u32,
}

/// Daily backups kept next to the database.
const DAILY_BACKUPS: usize = 7;
/// Lane spacing of `tasks.position`.
const STEP: f64 = 1024.0;
/// Entries `task_detail` returns.
const DETAIL_ENTRIES: i64 = 200;

impl TaskStore {
    /// $DROVR_TASKS_DB, else state_dir()/drovr/tasks.db.
    pub(crate) fn default_path() -> PathBuf {
        match std::env::var_os("DROVR_TASKS_DB") {
            Some(path) if !path.is_empty() => PathBuf::from(path),
            _ => crate::config::state_dir().join("drovr").join("tasks.db"),
        }
    }

    /// Opens or creates the file (and its directory), sets pragmas, migrates.
    pub(crate) fn open(path: &Path, busy_ms: u32) -> StoreResult<TaskStore> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|err| {
                StoreError::Invalid(format!("cannot create {}: {err}", parent.display()))
            })?;
        }
        // Two processes opening a new file at once can get SQLITE_BUSY from
        // the switch to WAL without the busy handler running; retry within
        // the same budget.
        let budget = Duration::from_millis(u64::from(busy_ms));
        let start = std::time::Instant::now();
        loop {
            match Self::open_once(path, budget) {
                Err(StoreError::Busy) if start.elapsed() < budget => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                result => return result,
            }
        }
    }

    fn open_once(path: &Path, busy: Duration) -> StoreResult<TaskStore> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(busy)?;
        let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;")?;
        schema::migrate(&mut conn, Some(path))?;
        Ok(TaskStore {
            conn,
            path: Some(path.to_owned()),
        })
    }

    /// Same, but Ok(None) when the file does not exist (nothing is created).
    pub(crate) fn open_existing(path: &Path, busy_ms: u32) -> StoreResult<Option<TaskStore>> {
        if !path.exists() {
            return Ok(None);
        }
        Self::open(path, busy_ms).map(Some)
    }

    pub(crate) fn open_in_memory() -> StoreResult<TaskStore> {
        let mut conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        schema::migrate(&mut conn, None)?;
        Ok(TaskStore { conn, path: None })
    }

    /// Daily `VACUUM INTO` backup of section 2.1; no-op when today's exists.
    pub(crate) fn backup_daily(&self) -> StoreResult<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let day: String = now_text()
            .chars()
            .take(10)
            .filter(char::is_ascii_digit)
            .collect();
        let target = backup_path(path, &day);
        if target.exists() {
            return Ok(());
        }
        vacuum_into(&self.conn, &target)?;
        prune_daily_backups(path);
        Ok(())
    }

    /// PRAGMA data_version; changes when another connection commits.
    pub(crate) fn data_version(&self) -> StoreResult<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA data_version", [], |row| row.get(0))?)
    }

    /// BEGIN IMMEDIATE; f; COMMIT on Ok, ROLLBACK on Err (also when f
    /// panics, through the transaction's drop).
    fn write<R>(&self, f: impl FnOnce(&Connection) -> StoreResult<R>) -> StoreResult<R> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    /// One deferred transaction, so a read of several tables sees one commit.
    fn read<R>(&self, f: impl FnOnce(&Connection) -> StoreResult<R>) -> StoreResult<R> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        f(&tx)
    }

    // Projects

    pub(crate) fn ensure_project(&self, name: &str) -> StoreResult<Project> {
        if let Some(project) = self.project(name)? {
            return Ok(project);
        }
        self.write(|c| ensure_project(c, name))
    }

    pub(crate) fn project(&self, name: &str) -> StoreResult<Option<Project>> {
        project_by_name(&self.conn, name)
    }

    pub(crate) fn projects(&self) -> StoreResult<Vec<Project>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, key, name, next_number FROM projects ORDER BY name")?;
        let rows = stmt.query_map([], project_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub(crate) fn rename_project(&self, old: &str, new: &str) -> StoreResult<()> {
        validate_project_name(new)?;
        if old == new {
            return Ok(());
        }
        self.write(|c| {
            if project_by_name(c, new)?.is_some() {
                return Err(transitions::project_exists(new));
            }
            c.execute(
                "UPDATE projects SET name = ?2 WHERE name = ?1",
                params![old, new],
            )?;
            Ok(())
        })
    }

    // Tasks

    pub(crate) fn create_task(&self, new: &NewTask, actor: &Actor) -> StoreResult<Task> {
        self.write(|c| create_task(c, new, actor))
    }

    pub(crate) fn task(&self, display_id: &str) -> StoreResult<Option<Task>> {
        find_task(&self.conn, display_id)
    }

    pub(crate) fn task_detail(&self, display_id: &str) -> StoreResult<Option<TaskDetail>> {
        self.read(|c| task_detail(c, display_id))
    }

    pub(crate) fn list(&self, filter: &TaskFilter) -> StoreResult<Vec<TaskCard>> {
        self.read(|c| list(c, filter))
    }

    /// Counts per lane for a project, for headers and the project picker.
    /// Unarchived tasks only; cancelled counts under Done.
    pub(crate) fn lane_counts(&self, project: &str) -> StoreResult<[u32; 6]> {
        let mut stmt = self.conn.prepare(
            "SELECT t.status, COUNT(*) FROM tasks t JOIN projects p ON p.id = t.project_id
             WHERE p.name = ?1 AND t.archived_at IS NULL GROUP BY t.status",
        )?;
        let mut counts = [0u32; 6];
        let rows = stmt.query_map([project], |row| {
            Ok((row.get::<_, Status>(0)?, row.get::<_, u32>(1)?))
        })?;
        for row in rows {
            let (status, count) = row?;
            counts[status.lane()] += count;
        }
        Ok(counts)
    }

    pub(crate) fn update_task(
        &self,
        display_id: &str,
        patch: &TaskPatch,
        actor: &Actor,
    ) -> StoreResult<Task> {
        self.write(|c| update_task(c, display_id, patch, actor))
    }

    pub(crate) fn move_task(
        &self,
        display_id: &str,
        to: Status,
        actor: &Actor,
        note: Option<&str>,
    ) -> StoreResult<Task> {
        self.write(|c| move_task(c, display_id, to, actor, note))
    }

    /// Places the task in `status` between `after` and `before` (display ids).
    /// A different `status` is a human move first.
    pub(crate) fn reorder(
        &self,
        display_id: &str,
        status: Status,
        after: Option<&str>,
        before: Option<&str>,
    ) -> StoreResult<()> {
        self.write(|c| reorder(c, display_id, status, after, before))
    }

    pub(crate) fn link_workspace(
        &self,
        display_id: &str,
        workspace_key: Option<&str>,
    ) -> StoreResult<()> {
        self.write(|c| {
            let task = load_task(c, display_id)?;
            if task.workspace_key.as_deref() == workspace_key {
                return Ok(());
            }
            c.execute(
                "UPDATE tasks SET workspace_key = ?2 WHERE id = ?1",
                params![task.id, workspace_key],
            )?;
            touch(c, task.id)
        })
    }

    /// The task of the open attempt on this pane.
    pub(crate) fn task_for_pane(&self, pane_key: &str) -> StoreResult<Option<Task>> {
        task_for_pane(&self.conn, pane_key)
    }

    // Criteria (position is 1-based)

    pub(crate) fn set_criteria(
        &self,
        display_id: &str,
        texts: &[String],
        actor: &Actor,
    ) -> StoreResult<Vec<Criterion>> {
        let _ = actor;
        self.write(|c| {
            let task = load_task(c, display_id)?;
            set_criteria(c, task.id, texts)?;
            touch(c, task.id)?;
            criteria_of(c, task.id)
        })
    }

    pub(crate) fn add_criterion(
        &self,
        display_id: &str,
        text: &str,
        actor: &Actor,
    ) -> StoreResult<Criterion> {
        let _ = actor;
        self.write(|c| {
            let task = load_task(c, display_id)?;
            let criterion = add_criterion(c, task.id, text)?;
            touch(c, task.id)?;
            Ok(criterion)
        })
    }

    pub(crate) fn check_criterion(
        &self,
        display_id: &str,
        position: i64,
        state: CheckState,
        evidence: Option<&str>,
        actor: &Actor,
    ) -> StoreResult<Criterion> {
        self.write(|c| check_criterion(c, display_id, position, state, evidence, actor))
    }

    // Thread

    pub(crate) fn add_entry(
        &self,
        display_id: &str,
        kind: EntryKind,
        body: &str,
        actor: &Actor,
    ) -> StoreResult<Entry> {
        self.write(|c| {
            let task = load_task(c, display_id)?;
            if body.trim().is_empty() {
                return Err(StoreError::Invalid("the note is empty".into()));
            }
            let attempt = match actor {
                Actor::Agent(_) => open_attempt(c, task.id)?.map(|a| a.id),
                _ => None,
            };
            let entry = insert_entry(c, task.id, kind, &actor.author(), attempt, body, None)?;
            touch(c, task.id)?;
            Ok(entry)
        })
    }

    pub(crate) fn pin_entry(&self, entry_id: i64, pinned: bool) -> StoreResult<()> {
        self.write(|c| {
            let task_id: i64 = c
                .query_row(
                    "SELECT task_id FROM entries WHERE id = ?1",
                    [entry_id],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| StoreError::Invalid(format!("no note {entry_id}")))?;
            c.execute(
                "UPDATE entries SET pinned = ?2 WHERE id = ?1",
                params![entry_id, pinned],
            )?;
            touch(c, task_id)
        })
    }

    // Attempts

    /// Ends any open attempt as Stopped, opens a new one, sets executor,
    /// workspace_key and auto_status = 1, moves triage/ready -> working.
    pub(crate) fn start_attempt(
        &self,
        display_id: &str,
        new: &NewAttempt,
        actor: &Actor,
    ) -> StoreResult<Attempt> {
        self.write(|c| start_attempt(c, display_id, new, actor))
    }

    /// Succeeded: gate, then working -> review (refused with the attempt left
    /// open when the gate fails). Failed/Stopped: -> ready. NeedsHuman:
    /// -> blocked. Withdraws an open decision of this attempt.
    pub(crate) fn finish_attempt(
        &self,
        display_id: &str,
        outcome: Outcome,
        note: Option<&str>,
        actor: &Actor,
    ) -> StoreResult<Task> {
        self.write(|c| finish_attempt(c, display_id, outcome, note, actor))
    }

    pub(crate) fn release(&self, display_id: &str, note: &str, actor: &Actor) -> StoreResult<Task> {
        self.write(|c| release(c, display_id, note, actor))
    }

    pub(crate) fn set_attempt_usage(
        &self,
        attempt_id: i64,
        tokens_in: Option<i64>,
        tokens_out: Option<i64>,
        cost_cents: Option<i64>,
        session_id: Option<&str>,
    ) -> StoreResult<()> {
        self.write(|c| {
            let task_id: i64 = c
                .query_row(
                    "SELECT task_id FROM attempts WHERE id = ?1",
                    [attempt_id],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| StoreError::Invalid(format!("no attempt {attempt_id}")))?;
            let changed = c.execute(
                "UPDATE attempts SET
                   tokens_in = COALESCE(?2, tokens_in),
                   tokens_out = COALESCE(?3, tokens_out),
                   cost_cents = COALESCE(?4, cost_cents),
                   session_id = COALESCE(?5, session_id)
                 WHERE id = ?1 AND NOT (
                   tokens_in IS COALESCE(?2, tokens_in)
                   AND tokens_out IS COALESCE(?3, tokens_out)
                   AND cost_cents IS COALESCE(?4, cost_cents)
                   AND session_id IS COALESCE(?5, session_id))",
                params![attempt_id, tokens_in, tokens_out, cost_cents, session_id],
            )?;
            if changed > 0 {
                touch(c, task_id)?;
            }
            Ok(())
        })
    }

    // Artifacts

    pub(crate) fn attach_artifact(
        &self,
        display_id: &str,
        new: &NewArtifact,
        actor: &Actor,
    ) -> StoreResult<Artifact> {
        self.write(|c| attach_artifact(c, display_id, new, actor))
    }

    pub(crate) fn review_artifact(&self, artifact_id: i64, review: Review) -> StoreResult<()> {
        self.write(|c| {
            let task_id: i64 = c
                .query_row(
                    "SELECT task_id FROM artifacts WHERE id = ?1",
                    [artifact_id],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| StoreError::Invalid(format!("no artifact {artifact_id}")))?;
            let changed = c.execute(
                "UPDATE artifacts SET review = ?2 WHERE id = ?1 AND review <> ?2",
                params![artifact_id, review],
            )?;
            if changed > 0 {
                touch(c, task_id)?;
            }
            Ok(())
        })
    }

    // Decisions

    /// One open per task; moves working -> blocked; validates choices.
    pub(crate) fn request_decision(
        &self,
        display_id: &str,
        new: &NewDecision,
        actor: &Actor,
    ) -> StoreResult<Decision> {
        self.write(|c| request_decision(c, display_id, new, actor))
    }

    /// First ruling wins (a second returns Refused "decision_ruled").
    /// Moves blocked -> working when an attempt is open, else -> ready.
    pub(crate) fn rule_decision(
        &self,
        decision_id: i64,
        ruling: &Ruling,
        surface: &str,
        actor: &Actor,
    ) -> StoreResult<Decision> {
        self.write(|c| rule_decision(c, decision_id, ruling, surface, actor))
    }

    pub(crate) fn withdraw_decision(&self, display_id: &str, actor: &Actor) -> StoreResult<()> {
        self.write(|c| {
            let task = load_task(c, display_id)?;
            let Some(decision) = open_decision(c, task.id)? else {
                return Err(StoreError::Invalid(format!(
                    "{} has no open decision",
                    task.display_id
                )));
            };
            close_decision(c, &decision, DecisionState::Withdrawn, actor)?;
            unblock(c, &task, actor)?;
            touch(c, task.id)
        })
    }

    pub(crate) fn decision(&self, decision_id: i64) -> StoreResult<Option<Decision>> {
        decision_by_id(&self.conn, decision_id)
    }

    /// Open decisions, oldest first; `project` = section name, None = all.
    pub(crate) fn open_decisions(&self, project: Option<&str>) -> StoreResult<Vec<OpenDecision>> {
        let sql = format!(
            "SELECT {DECISION_COLS}, {TASK_COLS}, p.name, oa.pane_key
             FROM decisions d
             JOIN tasks t ON t.id = d.task_id
             JOIN projects p ON p.id = t.project_id
             LEFT JOIN attempts oa ON oa.task_id = t.id AND oa.ended_at IS NULL
             WHERE d.state = 'open' AND (?1 IS NULL OR p.name = ?1)
             ORDER BY d.created_at, d.id"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([project], |row| {
            let decision = decision_row(row)?;
            let task = task_row_at(row, DECISION_WIDTH)?;
            Ok(OpenDecision {
                decision,
                display_id: task.display_id.clone(),
                task_name: task.name().to_owned(),
                project: row.get(DECISION_WIDTH + TASK_WIDTH)?,
                pane_key: row.get(DECISION_WIDTH + TASK_WIDTH + 1)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// `decide --wait` in db mode: sets or clears `wait_until`.
    pub(crate) fn set_decision_wait(
        &self,
        decision_id: i64,
        wait_until: Option<&str>,
    ) -> StoreResult<()> {
        self.write(|c| {
            let changed = c.execute(
                "UPDATE decisions SET wait_until = ?2 WHERE id = ?1",
                params![decision_id, wait_until],
            )?;
            if changed == 0 {
                return Err(StoreError::Invalid(format!("no decision {decision_id}")));
            }
            Ok(())
        })
    }

    /// Rules expired open decisions with their default, else marks them
    /// expired. Returns the ids it changed.
    pub(crate) fn expire_decisions(&self, now: &str) -> StoreResult<Vec<i64>> {
        self.write(|c| {
            let ids: Vec<i64> = {
                let mut stmt = c.prepare(
                    "SELECT id FROM decisions WHERE state = 'open'
                     AND expires_at IS NOT NULL AND expires_at <= ?1 ORDER BY id",
                )?;
                let rows = stmt.query_map([now], |row| row.get(0))?;
                rows.collect::<Result<_, _>>()?
            };
            for &id in &ids {
                let Some(decision) = decision_by_id(c, id)? else {
                    continue;
                };
                match decision.default_choice.clone() {
                    Some(choice) => {
                        rule_decision(c, id, &Ruling::Choice(choice), "expiry", &Actor::Auto)?;
                    }
                    None => {
                        close_decision(c, &decision, DecisionState::Expired, &Actor::Auto)?;
                        touch(c, decision.task_id)?;
                    }
                }
            }
            Ok(ids)
        })
    }

    // Ops (CLI and outbox)

    pub(crate) fn apply(&self, op: &TaskOp, ctx: &OpContext) -> StoreResult<OpResult> {
        self.write(|c| apply(c, op, ctx))
    }

    /// Applies `op` only when `seq` is above the stored seq for `source`;
    /// records the seq in the same transaction. Ok(None) = already applied.
    ///
    /// A refused, invalid or not-found op is recorded as applied and comes
    /// back as `Ok(Some(OpResult { ok: false, .. }))` (its writes rolled
    /// back), so the outbox file can be answered and removed. Busy and
    /// database errors are not recorded and return Err, to be retried.
    pub(crate) fn apply_once(
        &self,
        source: &str,
        seq: u64,
        op: &TaskOp,
        ctx: &OpContext,
    ) -> StoreResult<Option<OpResult>> {
        let seq = i64::try_from(seq).map_err(|_| StoreError::Invalid("seq too large".into()))?;
        self.write(|c| {
            let stored: Option<i64> = c
                .query_row(
                    "SELECT seq FROM applied_ops WHERE source = ?1",
                    [source],
                    |row| row.get(0),
                )
                .optional()?;
            if stored.is_some_and(|stored| seq <= stored) {
                return Ok(None);
            }
            c.execute_batch("SAVEPOINT op")?;
            let result = match apply(c, op, ctx) {
                Ok(result) => {
                    c.execute_batch("RELEASE op")?;
                    result
                }
                Err(
                    err @ (StoreError::Busy | StoreError::Sqlite(_) | StoreError::TooNew { .. }),
                ) => {
                    return Err(err);
                }
                Err(err) => {
                    c.execute_batch("ROLLBACK TO op; RELEASE op")?;
                    OpResult::failed(&err, op.task())
                }
            };
            c.execute(
                "INSERT INTO applied_ops (source, seq, applied_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (source) DO UPDATE SET seq = excluded.seq,
                   applied_at = excluded.applied_at",
                params![source, seq, now_text()],
            )?;
            Ok(Some(result))
        })
    }

    pub(crate) fn applied_seq(&self, source: &str) -> StoreResult<u64> {
        let seq: Option<i64> = self
            .conn
            .query_row(
                "SELECT seq FROM applied_ops WHERE source = ?1",
                [source],
                |row| row.get(0),
            )
            .optional()?;
        Ok(seq.unwrap_or(0).max(0) as u64)
    }

    // Import

    pub(crate) fn import_workspace(
        &self,
        path: &Path,
        map: &[(String, String)],
        dry_run: bool,
    ) -> StoreResult<ImportReport> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let report = super::import::import(&tx, path, map)?;
        if !dry_run {
            tx.commit()?;
        }
        Ok(report)
    }

    #[cfg(test)]
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }
}

fn prune_daily_backups(path: &Path) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        return;
    };
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let prefix = format!("{name}.");
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut daily: Vec<PathBuf> = read
        .filter_map(Result::ok)
        .filter(|entry| {
            let file = entry.file_name();
            let Some(file) = file.to_str() else {
                return false;
            };
            file.strip_prefix(&prefix)
                .and_then(|rest| rest.strip_suffix(".bak"))
                .is_some_and(|day| day.len() == 8 && day.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(|entry| entry.path())
        .collect();
    daily.sort();
    let excess = daily.len().saturating_sub(DAILY_BACKUPS);
    for old in &daily[..excess] {
        if let Err(err) = std::fs::remove_file(old) {
            tracing::warn!(%err, path = %old.display(), "cannot remove old tasks db backup");
        }
    }
}

// Rows

pub(super) const TASK_COLS: &str = "t.id, t.project_id, t.number, t.display_id, t.title, t.body, \
     t.status, t.kind, t.priority, t.executor, t.workspace_key, t.auto_status, t.position, \
     t.version, t.status_since, t.created_at, t.updated_at, t.closed_at, t.archived_at";
const TASK_WIDTH: usize = 19;

fn task_row(row: &Row<'_>) -> rusqlite::Result<Task> {
    task_row_at(row, 0)
}

fn task_row_at(row: &Row<'_>, at: usize) -> rusqlite::Result<Task> {
    Ok(Task {
        id: row.get(at)?,
        project_id: row.get(at + 1)?,
        number: row.get(at + 2)?,
        display_id: row.get(at + 3)?,
        title: row.get(at + 4)?,
        body: row.get(at + 5)?,
        status: row.get(at + 6)?,
        kind: row.get(at + 7)?,
        priority: row.get(at + 8)?,
        executor: row.get(at + 9)?,
        workspace_key: row.get(at + 10)?,
        auto_status: row.get(at + 11)?,
        position: row.get(at + 12)?,
        version: row.get(at + 13)?,
        status_since: row.get(at + 14)?,
        created_at: row.get(at + 15)?,
        updated_at: row.get(at + 16)?,
        closed_at: row.get(at + 17)?,
        archived_at: row.get(at + 18)?,
    })
}

fn project_row(row: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        key: row.get(1)?,
        name: row.get(2)?,
        next_number: row.get(3)?,
    })
}

const CRITERION_COLS: &str =
    "id, task_id, position, text, check_cmd, state, evidence, checked_by, checked_at";

fn criterion_row(row: &Row<'_>) -> rusqlite::Result<Criterion> {
    Ok(Criterion {
        id: row.get(0)?,
        task_id: row.get(1)?,
        position: row.get(2)?,
        text: row.get(3)?,
        check_cmd: row.get(4)?,
        state: row.get(5)?,
        evidence: row.get(6)?,
        checked_by: row.get(7)?,
        checked_at: row.get(8)?,
    })
}

const ENTRY_COLS: &str =
    "id, task_id, seq, kind, author, attempt_id, body, event_type, pinned, created_at";

fn entry_row(row: &Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry {
        id: row.get(0)?,
        task_id: row.get(1)?,
        seq: row.get(2)?,
        kind: row.get(3)?,
        author: row.get(4)?,
        attempt_id: row.get(5)?,
        body: row.get(6)?,
        event_type: row.get(7)?,
        pinned: row.get(8)?,
        created_at: row.get(9)?,
    })
}

const ATTEMPT_COLS: &str = "id, task_id, harness, machine, workspace_key, pane_key, session_id, \
     started_at, ended_at, outcome, note, tokens_in, tokens_out, cost_cents";

fn attempt_row(row: &Row<'_>) -> rusqlite::Result<Attempt> {
    Ok(Attempt {
        id: row.get(0)?,
        task_id: row.get(1)?,
        harness: row.get(2)?,
        machine: row.get(3)?,
        workspace_key: row.get(4)?,
        pane_key: row.get(5)?,
        session_id: row.get(6)?,
        started_at: row.get(7)?,
        ended_at: row.get(8)?,
        outcome: row.get(9)?,
        note: row.get(10)?,
        tokens_in: row.get(11)?,
        tokens_out: row.get(12)?,
        cost_cents: row.get(13)?,
    })
}

const ARTIFACT_COLS: &str =
    "id, task_id, attempt_id, kind, title, target, machine, summary, review, created_at";

fn artifact_row(row: &Row<'_>) -> rusqlite::Result<Artifact> {
    Ok(Artifact {
        id: row.get(0)?,
        task_id: row.get(1)?,
        attempt_id: row.get(2)?,
        kind: row.get(3)?,
        title: row.get(4)?,
        target: row.get(5)?,
        machine: row.get(6)?,
        summary: row.get(7)?,
        review: row.get(8)?,
        created_at: row.get(9)?,
    })
}

const DECISION_COLS: &str = "d.id, d.task_id, d.attempt_id, d.title, d.summary, d.choices_json, \
     d.allow_text, d.default_choice, d.state, d.ruling_choice, d.ruling_text, d.ruled_by, \
     d.ruled_at, d.surface, d.expires_at, d.wait_until, d.created_at";
const DECISION_WIDTH: usize = 17;

fn decision_row(row: &Row<'_>) -> rusqlite::Result<Decision> {
    let choices: String = row.get(5)?;
    let choices = serde_json::from_str(&choices).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(err))
    })?;
    Ok(Decision {
        id: row.get(0)?,
        task_id: row.get(1)?,
        attempt_id: row.get(2)?,
        title: row.get(3)?,
        summary: row.get(4)?,
        choices,
        allow_text: row.get(6)?,
        default_choice: row.get(7)?,
        state: row.get(8)?,
        ruling_choice: row.get(9)?,
        ruling_text: row.get(10)?,
        ruled_by: row.get(11)?,
        ruled_at: row.get(12)?,
        surface: row.get(13)?,
        expires_at: row.get(14)?,
        wait_until: row.get(15)?,
        created_at: row.get(16)?,
    })
}

// Reads

pub(super) fn project_by_name(c: &Connection, name: &str) -> StoreResult<Option<Project>> {
    Ok(c.query_row(
        "SELECT id, key, name, next_number FROM projects WHERE name = ?1",
        [name],
        project_row,
    )
    .optional()?)
}

fn normalize_id(display_id: &str) -> String {
    display_id.trim().to_ascii_uppercase()
}

pub(super) fn find_task(c: &Connection, display_id: &str) -> StoreResult<Option<Task>> {
    let sql = format!("SELECT {TASK_COLS} FROM tasks t WHERE t.display_id = ?1");
    Ok(c.query_row(&sql, [normalize_id(display_id)], task_row)
        .optional()?)
}

pub(super) fn load_task(c: &Connection, display_id: &str) -> StoreResult<Task> {
    find_task(c, display_id)?.ok_or_else(|| StoreError::NotFound(normalize_id(display_id)))
}

fn task_by_id(c: &Connection, id: i64) -> StoreResult<Task> {
    let sql = format!("SELECT {TASK_COLS} FROM tasks t WHERE t.id = ?1");
    Ok(c.query_row(&sql, [id], task_row)?)
}

fn task_for_pane(c: &Connection, pane_key: &str) -> StoreResult<Option<Task>> {
    let sql = format!(
        "SELECT {TASK_COLS} FROM tasks t JOIN attempts a ON a.task_id = t.id
         WHERE a.pane_key = ?1 AND a.ended_at IS NULL ORDER BY a.started_at DESC, a.id DESC LIMIT 1"
    );
    Ok(c.query_row(&sql, [pane_key], task_row).optional()?)
}

pub(super) fn criteria_of(c: &Connection, task_id: i64) -> StoreResult<Vec<Criterion>> {
    let sql = format!("SELECT {CRITERION_COLS} FROM criteria WHERE task_id = ?1 ORDER BY position");
    let mut stmt = c.prepare(&sql)?;
    let rows = stmt.query_map([task_id], criterion_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn open_attempt(c: &Connection, task_id: i64) -> StoreResult<Option<Attempt>> {
    let sql =
        format!("SELECT {ATTEMPT_COLS} FROM attempts WHERE task_id = ?1 AND ended_at IS NULL");
    Ok(c.query_row(&sql, [task_id], attempt_row).optional()?)
}

fn attempt_number(c: &Connection, task_id: i64, attempt_id: i64) -> StoreResult<i64> {
    Ok(c.query_row(
        "SELECT COUNT(*) FROM attempts WHERE task_id = ?1 AND id <= ?2",
        params![task_id, attempt_id],
        |row| row.get(0),
    )?)
}

fn decision_by_id(c: &Connection, id: i64) -> StoreResult<Option<Decision>> {
    let sql = format!("SELECT {DECISION_COLS} FROM decisions d WHERE d.id = ?1");
    Ok(c.query_row(&sql, [id], decision_row).optional()?)
}

fn open_decision(c: &Connection, task_id: i64) -> StoreResult<Option<Decision>> {
    let sql = format!(
        "SELECT {DECISION_COLS} FROM decisions d WHERE d.task_id = ?1 AND d.state = 'open'"
    );
    Ok(c.query_row(&sql, [task_id], decision_row).optional()?)
}

fn task_detail(c: &Connection, display_id: &str) -> StoreResult<Option<TaskDetail>> {
    let Some(task) = find_task(c, display_id)? else {
        return Ok(None);
    };
    let project = c.query_row(
        "SELECT id, key, name, next_number FROM projects WHERE id = ?1",
        [task.project_id],
        project_row,
    )?;
    let criteria = criteria_of(c, task.id)?;
    let mut entries: Vec<Entry> = {
        let sql = format!(
            "SELECT {ENTRY_COLS} FROM entries WHERE task_id = ?1 ORDER BY seq DESC LIMIT ?2"
        );
        let mut stmt = c.prepare(&sql)?;
        let rows = stmt.query_map(params![task.id, DETAIL_ENTRIES], entry_row)?;
        rows.collect::<Result<_, _>>()?
    };
    entries.reverse();
    let attempts = {
        let sql = format!(
            "SELECT {ATTEMPT_COLS} FROM attempts WHERE task_id = ?1 ORDER BY started_at DESC, id DESC"
        );
        let mut stmt = c.prepare(&sql)?;
        let rows = stmt.query_map([task.id], attempt_row)?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let artifacts = {
        let sql = format!(
            "SELECT {ARTIFACT_COLS} FROM artifacts WHERE task_id = ?1 ORDER BY created_at DESC, id DESC"
        );
        let mut stmt = c.prepare(&sql)?;
        let rows = stmt.query_map([task.id], artifact_row)?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let decision = {
        let sql = format!(
            "SELECT {DECISION_COLS} FROM decisions d WHERE d.task_id = ?1
             ORDER BY d.state = 'open' DESC, d.id DESC LIMIT 1"
        );
        c.query_row(&sql, [task.id], decision_row).optional()?
    };
    Ok(Some(TaskDetail {
        task,
        project,
        criteria,
        entries,
        attempts,
        artifacts,
        decision,
    }))
}

fn list(c: &Connection, filter: &TaskFilter) -> StoreResult<Vec<TaskCard>> {
    let sql = format!(
        "SELECT {TASK_COLS},
           (SELECT COUNT(*) FROM criteria c WHERE c.task_id = t.id),
           (SELECT COUNT(*) FROM criteria c WHERE c.task_id = t.id AND c.state = 'passed'),
           (SELECT COUNT(*) FROM criteria c WHERE c.task_id = t.id AND c.state = 'failed'),
           EXISTS (SELECT 1 FROM decisions d WHERE d.task_id = t.id AND d.state = 'open'),
           (SELECT a.outcome FROM attempts a WHERE a.task_id = t.id AND a.ended_at IS NOT NULL
              ORDER BY a.ended_at DESC, a.id DESC LIMIT 1),
           oa.harness, oa.machine, oa.pane_key
         FROM tasks t
         JOIN projects p ON p.id = t.project_id
         LEFT JOIN attempts oa ON oa.task_id = t.id AND oa.ended_at IS NULL
         WHERE (?1 IS NULL OR p.name = ?1)
           AND (?2 IS NULL OR substr(t.workspace_key, 1, length(?2)) = ?2)
           AND (?3 OR t.archived_at IS NULL)"
    );
    let mut stmt = c.prepare(&sql)?;
    let rows = stmt.query_map(
        params![
            filter.project,
            filter.workspace_key,
            filter.include_archived
        ],
        |row| {
            let at = TASK_WIDTH;
            let harness: Option<String> = row.get(at + 5)?;
            let machine: Option<String> = row.get(at + 6)?;
            Ok(TaskCard {
                task: task_row(row)?,
                criteria_total: row.get(at)?,
                criteria_passed: row.get(at + 1)?,
                criteria_failed: row.get(at + 2)?,
                open_decision: row.get(at + 3)?,
                last_outcome: row.get(at + 4)?,
                live: harness.map(|h| {
                    (
                        h,
                        machine.unwrap_or_default(),
                        row.get(at + 7).ok().flatten(),
                    )
                }),
            })
        },
    )?;
    let text = filter
        .text
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_lowercase);
    let mut cards: Vec<TaskCard> = rows
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|card| filter.statuses.is_empty() || filter.statuses.contains(&card.task.status))
        .filter(|card| {
            text.as_deref().is_none_or(|text| {
                card.task.display_id.to_lowercase().contains(text)
                    || card.task.name().to_lowercase().contains(text)
            })
        })
        .collect();
    cards.sort_by(|a, b| {
        let (ta, tb) = (&a.task, &b.task);
        ta.status
            .lane()
            .cmp(&tb.status.lane())
            .then_with(|| {
                if ta.status.is_closed() {
                    tb.closed_at.cmp(&ta.closed_at)
                } else {
                    ta.position.total_cmp(&tb.position)
                }
            })
            .then_with(|| ta.id.cmp(&tb.id))
    });
    if let Some(limit) = filter.done_limit {
        let mut closed = 0;
        cards.retain(|card| {
            if !card.task.status.is_closed() {
                return true;
            }
            closed += 1;
            closed <= limit
        });
    }
    Ok(cards)
}

// Writes

/// Bumps `version` and `updated_at`; every write method calls it once.
pub(super) fn touch(c: &Connection, task_id: i64) -> StoreResult<()> {
    c.execute(
        "UPDATE tasks SET version = version + 1, updated_at = ?2 WHERE id = ?1",
        params![task_id, now_text()],
    )?;
    Ok(())
}

fn validate_project_name(name: &str) -> StoreResult<()> {
    if name.trim().is_empty() || name.starts_with('\0') {
        return Err(StoreError::Invalid(
            "Add this workspace to a section to track tasks.".into(),
        ));
    }
    Ok(())
}

/// The project key rule of section 2.3, before collision suffixes.
pub(crate) fn derive_key(name: &str) -> String {
    let words: Vec<String> = name
        .split(|ch: char| ch.is_whitespace() || matches!(ch, '-' | '_' | '/' | '.'))
        .map(|word| {
            word.chars()
                .filter(char::is_ascii_alphanumeric)
                .map(|ch| ch.to_ascii_uppercase())
                .collect::<String>()
        })
        .filter(|word| !word.is_empty())
        .collect();
    let mut key: String = match words.as_slice() {
        [] => String::new(),
        [one] => one.chars().take(3).collect(),
        many => many
            .iter()
            .filter_map(|w| w.chars().next())
            .take(4)
            .collect(),
    };
    if key.starts_with(|ch: char| ch.is_ascii_digit()) {
        key.insert(0, 'P');
    }
    while key.len() < 2 {
        key.push('X');
    }
    key
}

pub(super) fn key_taken(c: &Connection, key: &str) -> StoreResult<bool> {
    Ok(c.query_row(
        "SELECT EXISTS (SELECT 1 FROM projects WHERE key = ?1)",
        [key],
        |row| row.get(0),
    )?)
}

fn free_key(c: &Connection, base: &str) -> StoreResult<String> {
    if !key_taken(c, base)? {
        return Ok(base.to_owned());
    }
    let mut n = 2;
    loop {
        let key = format!("{base}{n}");
        if !key_taken(c, &key)? {
            return Ok(key);
        }
        n += 1;
    }
}

pub(super) fn ensure_project(c: &Connection, name: &str) -> StoreResult<Project> {
    validate_project_name(name)?;
    if let Some(project) = project_by_name(c, name)? {
        return Ok(project);
    }
    let key = free_key(c, &derive_key(name))?;
    insert_project(c, name, &key)
}

pub(super) fn insert_project(c: &Connection, name: &str, key: &str) -> StoreResult<Project> {
    c.execute(
        "INSERT INTO projects (key, name, created_at) VALUES (?1, ?2, ?3)",
        params![key, name, now_text()],
    )?;
    project_by_name(c, name)?.ok_or_else(|| StoreError::NotFound(name.to_owned()))
}

fn end_position(c: &Connection, project_id: i64, status: Status) -> StoreResult<f64> {
    let max: Option<f64> = c.query_row(
        "SELECT MAX(position) FROM tasks WHERE project_id = ?1 AND status = ?2",
        params![project_id, status],
        |row| row.get(0),
    )?;
    Ok(max.map_or(STEP, |max| max + STEP))
}

/// Splits `text (check: CMD)` into the text and the command; backticks
/// around CMD are dropped.
pub(crate) fn split_check(text: &str) -> (String, Option<String>) {
    let trimmed = text.trim();
    if let Some(start) = trimmed.rfind("(check:") {
        if let Some(inner) = trimmed[start + 7..].strip_suffix(')') {
            let cmd = inner.trim().trim_matches('`').trim();
            let head = trimmed[..start].trim_end();
            if !cmd.is_empty() && !head.is_empty() {
                return (head.to_owned(), Some(cmd.to_owned()));
            }
        }
    }
    (trimmed.to_owned(), None)
}

fn create_task(c: &Connection, new: &NewTask, actor: &Actor) -> StoreResult<Task> {
    let project = ensure_project(c, &new.project)?;
    let title = new
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty());
    if title.is_none() && new.body.trim().is_empty() {
        return Err(StoreError::Invalid("a task needs a title or a body".into()));
    }
    let status = new.status.unwrap_or(Status::Triage);
    let number = project.next_number;
    let display_id = format!("{}-{number}", project.key);
    let task_id = insert_task(
        c,
        &InsertTask {
            project_id: project.id,
            number,
            display_id: &display_id,
            title,
            body: &new.body,
            status,
            kind: new.kind,
            priority: new.priority,
            position: end_position(c, project.id, status)?,
            ext_id: None,
            times: None,
        },
    )?;
    c.execute(
        "UPDATE projects SET next_number = MAX(next_number, ?2 + 1) WHERE id = ?1",
        params![project.id, number],
    )?;
    set_criteria(c, task_id, &new.criteria)?;
    let author = actor.author();
    if !matches!(actor, Actor::Human) {
        insert_entry(
            c,
            task_id,
            EntryKind::Event,
            &author,
            None,
            &format!("created ({author})"),
            Some("created"),
        )?;
    }
    task_by_id(c, task_id)
}

pub(super) struct InsertTask<'a> {
    pub project_id: i64,
    pub number: i64,
    pub display_id: &'a str,
    pub title: Option<&'a str>,
    pub body: &'a str,
    pub status: Status,
    pub kind: Option<super::Kind>,
    pub priority: super::Priority,
    pub position: f64,
    pub ext_id: Option<&'a str>,
    /// None = now.
    pub times: Option<TaskTimes>,
}

/// Timestamps of an imported task.
#[derive(Clone)]
pub(super) struct TaskTimes {
    pub status_since: String,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub archived_at: Option<String>,
}

pub(super) fn insert_task(c: &Connection, task: &InsertTask<'_>) -> StoreResult<i64> {
    let now = now_text();
    let times = task.times.clone().unwrap_or_else(|| TaskTimes {
        status_since: now.clone(),
        created_at: now.clone(),
        updated_at: now.clone(),
        closed_at: task.status.is_closed().then(|| now.clone()),
        archived_at: None,
    });
    c.execute(
        "INSERT INTO tasks (project_id, number, display_id, title, body, status, kind, priority,
           position, ext_id, status_since, created_at, updated_at, closed_at, archived_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            task.project_id,
            task.number,
            task.display_id,
            task.title,
            task.body,
            task.status,
            task.kind,
            task.priority,
            task.position,
            task.ext_id,
            times.status_since,
            times.created_at,
            times.updated_at,
            times.closed_at,
            times.archived_at
        ],
    )?;
    Ok(c.last_insert_rowid())
}

pub(super) fn insert_entry(
    c: &Connection,
    task_id: i64,
    kind: EntryKind,
    author: &str,
    attempt_id: Option<i64>,
    body: &str,
    event_type: Option<&str>,
) -> StoreResult<Entry> {
    insert_entry_at(
        c,
        task_id,
        kind,
        author,
        attempt_id,
        body,
        event_type,
        &now_text(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn insert_entry_at(
    c: &Connection,
    task_id: i64,
    kind: EntryKind,
    author: &str,
    attempt_id: Option<i64>,
    body: &str,
    event_type: Option<&str>,
    created_at: &str,
) -> StoreResult<Entry> {
    let body = cut_text(body, MAX_TEXT);
    c.execute(
        "INSERT INTO entries (task_id, seq, kind, author, attempt_id, body, event_type, created_at)
         VALUES (?1, (SELECT COALESCE(MAX(seq), 0) + 1 FROM entries WHERE task_id = ?1),
                 ?2, ?3, ?4, ?5, ?6, ?7)",
        params![task_id, kind, author, attempt_id, body, event_type, created_at],
    )?;
    let id = c.last_insert_rowid();
    let sql = format!("SELECT {ENTRY_COLS} FROM entries WHERE id = ?1");
    Ok(c.query_row(&sql, [id], entry_row)?)
}

fn event(
    c: &Connection,
    task_id: i64,
    actor: &Actor,
    event_type: &str,
    body: &str,
) -> StoreResult<()> {
    insert_entry(
        c,
        task_id,
        EntryKind::Event,
        &actor.author(),
        None,
        body,
        Some(event_type),
    )?;
    Ok(())
}

fn set_criteria(c: &Connection, task_id: i64, texts: &[String]) -> StoreResult<()> {
    c.execute("DELETE FROM criteria WHERE task_id = ?1", [task_id])?;
    for text in texts {
        add_criterion(c, task_id, text)?;
    }
    Ok(())
}

fn add_criterion(c: &Connection, task_id: i64, text: &str) -> StoreResult<Criterion> {
    let (text, check_cmd) = split_check(text);
    if text.is_empty() {
        return Err(StoreError::Invalid("a criterion needs text".into()));
    }
    c.execute(
        "INSERT INTO criteria (task_id, position, text, check_cmd)
         VALUES (?1, (SELECT COALESCE(MAX(position), 0) + 1 FROM criteria WHERE task_id = ?1),
                 ?2, ?3)",
        params![task_id, text, check_cmd],
    )?;
    let id = c.last_insert_rowid();
    let sql = format!("SELECT {CRITERION_COLS} FROM criteria WHERE id = ?1");
    Ok(c.query_row(&sql, [id], criterion_row)?)
}

fn check_criterion(
    c: &Connection,
    display_id: &str,
    position: i64,
    state: CheckState,
    evidence: Option<&str>,
    actor: &Actor,
) -> StoreResult<Criterion> {
    let task = load_task(c, display_id)?;
    let evidence = evidence
        .map(str::trim_end)
        .filter(|text| !text.is_empty())
        .map(|text| cut_text(text, MAX_TEXT));
    let (checked_by, checked_at) = match state {
        CheckState::Open => (None, None),
        _ => (Some(actor.author()), Some(now_text())),
    };
    let changed = c.execute(
        "UPDATE criteria SET state = ?3, evidence = ?4, checked_by = ?5, checked_at = ?6
         WHERE task_id = ?1 AND position = ?2",
        params![task.id, position, state, evidence, checked_by, checked_at],
    )?;
    if changed == 0 {
        return Err(StoreError::Invalid(format!(
            "{} has no criterion {position}",
            task.display_id
        )));
    }
    touch(c, task.id)?;
    let sql = format!("SELECT {CRITERION_COLS} FROM criteria WHERE task_id = ?1 AND position = ?2");
    Ok(c.query_row(&sql, params![task.id, position], criterion_row)?)
}

fn update_task(
    c: &Connection,
    display_id: &str,
    patch: &TaskPatch,
    actor: &Actor,
) -> StoreResult<Task> {
    let _ = actor;
    let task = load_task(c, display_id)?;
    if patch.expected_version.is_some_and(|v| v != task.version) {
        return Err(transitions::stale(&task.display_id));
    }
    let title = match &patch.title {
        Some(title) => title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned),
        None => task.title.clone(),
    };
    let body = patch.body.clone().unwrap_or_else(|| task.body.clone());
    let kind = patch.kind.unwrap_or(task.kind);
    let priority = patch.priority.unwrap_or(task.priority);
    let auto_status = patch.auto_status.unwrap_or(task.auto_status);
    let archived_at = match patch.archived {
        Some(true) => task.archived_at.clone().or_else(|| Some(now_text())),
        Some(false) => None,
        None => task.archived_at.clone(),
    };
    if title == task.title
        && body == task.body
        && kind == task.kind
        && priority == task.priority
        && auto_status == task.auto_status
        && archived_at == task.archived_at
    {
        return Ok(task);
    }
    if title.is_none() && body.trim().is_empty() {
        return Err(StoreError::Invalid("a task needs a title or a body".into()));
    }
    c.execute(
        "UPDATE tasks SET title = ?2, body = ?3, kind = ?4, priority = ?5, auto_status = ?6,
           archived_at = ?7 WHERE id = ?1",
        params![
            task.id,
            title,
            body,
            kind,
            priority,
            auto_status,
            archived_at
        ],
    )?;
    touch(c, task.id)?;
    task_by_id(c, task.id)
}

/// Writes the status columns and the status event entry. No permission
/// check, no version bump (the caller touches once).
fn set_status(
    c: &Connection,
    task: &Task,
    to: Status,
    actor: &Actor,
    note: Option<&str>,
) -> StoreResult<()> {
    let from = task.status;
    if from == to {
        return Ok(());
    }
    let now = now_text();
    let closed_at = match (from.is_closed(), to.is_closed()) {
        (_, false) => None,
        (true, true) => task.closed_at.clone().or(Some(now.clone())),
        (false, true) => Some(now.clone()),
    };
    // A closed task leaves its workspace: herdr reuses workspace ids after a
    // restart, and a reopened task links again when it starts.
    c.execute(
        "UPDATE tasks SET status = ?2, status_since = ?3, closed_at = ?4, position = ?5,
           workspace_key = CASE WHEN ?6 THEN NULL ELSE workspace_key END
         WHERE id = ?1",
        params![
            task.id,
            to,
            now,
            closed_at,
            end_position(c, task.project_id, to)?,
            to.is_closed()
        ],
    )?;
    let quiet = matches!(actor, Actor::Auto)
        && matches!(
            (from, to),
            (Status::Working, Status::Blocked) | (Status::Blocked, Status::Working)
        );
    if !quiet {
        let mut body = format!("{} → {}", from.as_str(), to.as_str());
        if !matches!(actor, Actor::Human) {
            body.push_str(&format!(" ({})", actor.author()));
        }
        if let Some(note) = note.map(str::trim).filter(|n| !n.is_empty()) {
            body.push_str(&format!(": {note}"));
        }
        event(c, task.id, actor, "status", &body)?;
    }
    Ok(())
}

fn end_attempt(
    c: &Connection,
    attempt: &Attempt,
    outcome: Outcome,
    note: Option<&str>,
) -> StoreResult<()> {
    c.execute(
        "UPDATE attempts SET ended_at = ?2, outcome = ?3, note = COALESCE(?4, note) WHERE id = ?1",
        params![
            attempt.id,
            now_text(),
            outcome,
            note.map(str::trim).filter(|n| !n.is_empty())
        ],
    )?;
    Ok(())
}

fn close_decision(
    c: &Connection,
    decision: &Decision,
    state: DecisionState,
    actor: &Actor,
) -> StoreResult<()> {
    c.execute(
        "UPDATE decisions SET state = ?2 WHERE id = ?1",
        params![decision.id, state],
    )?;
    event(c, decision.task_id, actor, "decision", state.as_str())
}

/// Leaving Blocked after a ruling or a withdrawal: working with an open
/// attempt, else ready.
fn unblock(c: &Connection, task: &Task, actor: &Actor) -> StoreResult<()> {
    if task.status != Status::Blocked {
        return Ok(());
    }
    let to = if open_attempt(c, task.id)?.is_some() {
        Status::Working
    } else {
        Status::Ready
    };
    set_status(c, task, to, actor, None)
}

fn move_task(
    c: &Connection,
    display_id: &str,
    to: Status,
    actor: &Actor,
    note: Option<&str>,
) -> StoreResult<Task> {
    let task = load_task(c, display_id)?;
    if task.status == to {
        return Ok(task);
    }
    if matches!(actor, Actor::Auto) && !task.auto_status {
        return Ok(task);
    }
    check_move(task.status, to, actor, note)?;
    match actor {
        Actor::Human => {
            if to.is_closed() {
                if let Some(attempt) = open_attempt(c, task.id)? {
                    if task.status == Status::Review {
                        end_attempt(c, &attempt, Outcome::Succeeded, None)?;
                    } else {
                        end_attempt(c, &attempt, Outcome::Stopped, Some("closed by you"))?;
                    }
                }
                if let Some(decision) = open_decision(c, task.id)? {
                    close_decision(c, &decision, DecisionState::Withdrawn, actor)?;
                }
            }
            c.execute("UPDATE tasks SET auto_status = 0 WHERE id = ?1", [task.id])?;
        }
        Actor::Agent(_) | Actor::Auto => {
            if to == Status::Review {
                if let Some(refusal) = gate(&criteria_of(c, task.id)?).refusal() {
                    return Err(refusal);
                }
            }
            if matches!(actor, Actor::Auto)
                && task.status == Status::Blocked
                && open_decision(c, task.id)?.is_some()
            {
                // Blocked on a decision stays blocked until it is ruled.
                return Ok(task);
            }
        }
    }
    set_status(c, &task, to, actor, note)?;
    touch(c, task.id)?;
    task_by_id(c, task.id)
}

fn reorder(
    c: &Connection,
    display_id: &str,
    status: Status,
    after: Option<&str>,
    before: Option<&str>,
) -> StoreResult<()> {
    let mut task = load_task(c, display_id)?;
    if task.status != status {
        task = move_task(c, display_id, status, &Actor::Human, None)?;
    }
    let neighbour = |id: Option<&str>| -> StoreResult<Option<Task>> {
        let Some(id) = id else {
            return Ok(None);
        };
        let other = load_task(c, id)?;
        if other.project_id != task.project_id || other.status != status || other.id == task.id {
            return Err(StoreError::Invalid(format!(
                "{} is not a neighbour in {}",
                other.display_id,
                status.as_str()
            )));
        }
        Ok(Some(other))
    };
    let (mut a, mut b) = (neighbour(after)?, neighbour(before)?);
    if let (Some(x), Some(y)) = (&a, &b) {
        if (y.position - x.position).abs() < 1e-6 {
            renumber_lane(c, task.project_id, status)?;
            a = neighbour(after)?;
            b = neighbour(before)?;
        }
    }
    let position = match (&a, &b) {
        (Some(a), Some(b)) => (a.position + b.position) / 2.0,
        (Some(a), None) => a.position + STEP,
        (None, Some(b)) => b.position - STEP,
        (None, None) => end_position(c, task.project_id, status)?,
    };
    c.execute(
        "UPDATE tasks SET position = ?2 WHERE id = ?1",
        params![task.id, position],
    )?;
    touch(c, task.id)
}

fn renumber_lane(c: &Connection, project_id: i64, status: Status) -> StoreResult<()> {
    let ids: Vec<i64> = {
        let mut stmt = c.prepare(
            "SELECT id FROM tasks WHERE project_id = ?1 AND status = ?2 ORDER BY position, id",
        )?;
        let rows = stmt.query_map(params![project_id, status], |row| row.get(0))?;
        rows.collect::<Result<_, _>>()?
    };
    for (index, id) in ids.iter().enumerate() {
        c.execute(
            "UPDATE tasks SET position = ?2 WHERE id = ?1",
            params![id, (index as f64 + 1.0) * STEP],
        )?;
    }
    Ok(())
}

fn start_attempt(
    c: &Connection,
    display_id: &str,
    new: &NewAttempt,
    actor: &Actor,
) -> StoreResult<Attempt> {
    let task = load_task(c, display_id)?;
    if task.status.is_closed() {
        return Err(StoreError::Invalid(format!(
            "{} is {}; reopen it first",
            task.display_id,
            task.status.as_str()
        )));
    }
    if new.harness.trim().is_empty() || new.machine.trim().is_empty() {
        return Err(StoreError::Invalid(
            "an attempt needs a harness and a machine".into(),
        ));
    }
    if let Some(old) = open_attempt(c, task.id)? {
        end_attempt(c, &old, Outcome::Stopped, Some("a new attempt started"))?;
        if let Some(decision) = open_decision(c, task.id)? {
            if decision.attempt_id == Some(old.id) {
                close_decision(c, &decision, DecisionState::Withdrawn, actor)?;
            }
        }
    }
    c.execute(
        "INSERT INTO attempts (task_id, harness, machine, workspace_key, pane_key, session_id, started_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            task.id,
            new.harness.trim(),
            new.machine.trim(),
            new.workspace_key,
            new.pane_key,
            new.session_id,
            now_text()
        ],
    )?;
    let attempt_id = c.last_insert_rowid();
    c.execute(
        "UPDATE tasks SET executor = ?2, workspace_key = COALESCE(?3, workspace_key),
           auto_status = 1 WHERE id = ?1",
        params![task.id, new.harness.trim(), new.workspace_key],
    )?;
    let number = attempt_number(c, task.id, attempt_id)?;
    event(
        c,
        task.id,
        actor,
        "attempt",
        &format!(
            "attempt {number} started: {}@{}",
            new.harness.trim(),
            new.machine.trim()
        ),
    )?;
    if matches!(task.status, Status::Triage | Status::Ready) {
        set_status(c, &task, Status::Working, actor, None)?;
    }
    touch(c, task.id)?;
    let sql = format!("SELECT {ATTEMPT_COLS} FROM attempts WHERE id = ?1");
    Ok(c.query_row(&sql, [attempt_id], attempt_row)?)
}

fn finish_attempt(
    c: &Connection,
    display_id: &str,
    outcome: Outcome,
    note: Option<&str>,
    actor: &Actor,
) -> StoreResult<Task> {
    let task = load_task(c, display_id)?;
    let Some(attempt) = open_attempt(c, task.id)? else {
        return Err(transitions::no_attempt(&task.display_id));
    };
    if outcome == Outcome::Succeeded {
        if let Some(refusal) = gate(&criteria_of(c, task.id)?).refusal() {
            return Err(refusal);
        }
    }
    end_attempt(c, &attempt, outcome, note)?;
    if let Some(decision) = open_decision(c, task.id)? {
        if decision.attempt_id == Some(attempt.id) {
            close_decision(c, &decision, DecisionState::Withdrawn, actor)?;
        }
    }
    let number = attempt_number(c, task.id, attempt.id)?;
    event(
        c,
        task.id,
        actor,
        "attempt",
        &format!("attempt {number} {}", outcome.as_str()),
    )?;
    let to = match outcome {
        Outcome::Succeeded => Status::Review,
        Outcome::Failed | Outcome::Stopped => Status::Ready,
        Outcome::NeedsHuman => Status::Blocked,
    };
    // Only active tasks follow the attempt; a task the human already moved
    // to review or closed keeps its status.
    if matches!(
        task.status,
        Status::Triage | Status::Ready | Status::Working | Status::Blocked
    ) {
        // The decision withdrawn above may have changed nothing on the row;
        // re-read so set_status sees the current status.
        let current = task_by_id(c, task.id)?;
        set_status(c, &current, to, actor, note)?;
    }
    touch(c, task.id)?;
    task_by_id(c, task.id)
}

fn release(c: &Connection, display_id: &str, note: &str, actor: &Actor) -> StoreResult<Task> {
    let task = load_task(c, display_id)?;
    if note.trim().is_empty() {
        return Err(StoreError::Invalid("release needs a note".into()));
    }
    let Some(attempt) = open_attempt(c, task.id)? else {
        return Err(transitions::no_attempt(&task.display_id));
    };
    end_attempt(c, &attempt, Outcome::Stopped, Some(note))?;
    if let Some(decision) = open_decision(c, task.id)? {
        close_decision(c, &decision, DecisionState::Withdrawn, actor)?;
    }
    if matches!(task.status, Status::Working | Status::Blocked) {
        set_status(c, &task, Status::Ready, actor, Some(note))?;
    } else {
        event(
            c,
            task.id,
            actor,
            "attempt",
            &format!("released: {}", note.trim()),
        )?;
    }
    touch(c, task.id)?;
    task_by_id(c, task.id)
}

fn attach_artifact(
    c: &Connection,
    display_id: &str,
    new: &NewArtifact,
    actor: &Actor,
) -> StoreResult<Artifact> {
    let task = load_task(c, display_id)?;
    if new.target.trim().is_empty() || new.title.trim().is_empty() {
        return Err(StoreError::Invalid(
            "an artifact needs a title and a target".into(),
        ));
    }
    let attempt_id = open_attempt(c, task.id)?.map(|a| a.id);
    c.execute(
        "INSERT INTO artifacts (task_id, attempt_id, kind, title, target, machine, summary, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            task.id,
            attempt_id,
            new.kind,
            new.title.trim(),
            new.target.trim(),
            new.machine,
            new.summary.as_deref().map(str::trim).filter(|s| !s.is_empty()),
            now_text()
        ],
    )?;
    let id = c.last_insert_rowid();
    event(
        c,
        task.id,
        actor,
        "artifact",
        &format!("attached {} {}", new.kind.as_str(), new.title.trim()),
    )?;
    touch(c, task.id)?;
    let sql = format!("SELECT {ARTIFACT_COLS} FROM artifacts WHERE id = ?1");
    Ok(c.query_row(&sql, [id], artifact_row)?)
}

pub(crate) fn validate_choices(
    choices: &[Choice],
    default_choice: Option<&str>,
) -> StoreResult<()> {
    if choices.is_empty() || choices.len() > 8 {
        return Err(StoreError::Invalid(
            "a decision takes 1 to 8 choices".into(),
        ));
    }
    let mut ids = std::collections::HashSet::new();
    for choice in choices {
        if choice.id.trim().is_empty() || choice.label.trim().is_empty() {
            return Err(StoreError::Invalid(
                "every choice needs an id and a label".into(),
            ));
        }
        if !ids.insert(choice.id.as_str()) {
            return Err(StoreError::Invalid(format!(
                "choice id {} is used twice",
                choice.id
            )));
        }
    }
    if choices.iter().filter(|c| c.recommended).count() > 1 {
        return Err(StoreError::Invalid("recommend at most one choice".into()));
    }
    if let Some(default) = default_choice {
        if !ids.contains(default) {
            return Err(StoreError::Invalid(format!(
                "default {default} is not a choice"
            )));
        }
    }
    Ok(())
}

fn request_decision(
    c: &Connection,
    display_id: &str,
    new: &NewDecision,
    actor: &Actor,
) -> StoreResult<Decision> {
    let task = load_task(c, display_id)?;
    let title = new.title.trim();
    if title.is_empty() || title.chars().count() > 120 {
        return Err(StoreError::Invalid(
            "a decision title takes 1 to 120 characters".into(),
        ));
    }
    if new.summary.chars().count() > 1200 {
        return Err(StoreError::Invalid(
            "a decision summary takes at most 1200 characters".into(),
        ));
    }
    validate_choices(&new.choices, new.default_choice.as_deref())?;
    if open_decision(c, task.id)?.is_some() {
        return Err(transitions::decision_open(&task.display_id));
    }
    let attempt_id = open_attempt(c, task.id)?.map(|a| a.id);
    let choices = serde_json::to_string(&new.choices)
        .map_err(|err| StoreError::Invalid(format!("choices: {err}")))?;
    c.execute(
        "INSERT INTO decisions (task_id, attempt_id, title, summary, choices_json, allow_text,
           default_choice, expires_at, wait_until, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            task.id,
            attempt_id,
            title,
            new.summary.trim(),
            choices,
            new.allow_text,
            new.default_choice,
            new.expires_at,
            new.wait_until,
            now_text()
        ],
    )?;
    let id = c.last_insert_rowid();
    event(c, task.id, actor, "decision", &format!("asked: {title}"))?;
    if task.status == Status::Working {
        set_status(c, &task, Status::Blocked, actor, None)?;
    }
    touch(c, task.id)?;
    decision_by_id(c, id)?.ok_or_else(|| StoreError::NotFound(id.to_string()))
}

fn rule_decision(
    c: &Connection,
    decision_id: i64,
    ruling: &Ruling,
    surface: &str,
    actor: &Actor,
) -> StoreResult<Decision> {
    let decision = decision_by_id(c, decision_id)?
        .ok_or_else(|| StoreError::NotFound(decision_id.to_string()))?;
    if decision.state != DecisionState::Open {
        return Err(transitions::decision_ruled());
    }
    let (choice, text, shown) = match ruling {
        Ruling::Choice(id) => {
            let Some(choice) = decision.choices.iter().find(|c| &c.id == id) else {
                return Err(StoreError::Invalid(format!(
                    "{id} is not one of the choices"
                )));
            };
            (Some(id.clone()), None, choice.label.clone())
        }
        Ruling::Text(text) => {
            let text = text.trim();
            if text.is_empty() {
                return Err(StoreError::Invalid("the reply is empty".into()));
            }
            if !decision.allow_text {
                return Err(StoreError::Invalid(
                    "this decision takes one of its choices".into(),
                ));
            }
            (None, Some(text.to_owned()), text.to_owned())
        }
    };
    c.execute(
        "UPDATE decisions SET state = 'ruled', ruling_choice = ?2, ruling_text = ?3,
           ruled_by = ?4, ruled_at = ?5, surface = ?6 WHERE id = ?1",
        params![
            decision_id,
            choice,
            text,
            actor.author(),
            now_text(),
            surface
        ],
    )?;
    event(
        c,
        decision.task_id,
        actor,
        "decision",
        &format!("answered: {shown} ({surface})"),
    )?;
    let task = task_by_id(c, decision.task_id)?;
    unblock(c, &task, actor)?;
    touch(c, task.id)?;
    decision_by_id(c, decision_id)?.ok_or_else(|| StoreError::NotFound(decision_id.to_string()))
}

// Ops

fn resolve_task(c: &Connection, task: &Option<String>, ctx: &OpContext) -> StoreResult<String> {
    if let Some(id) = task.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
        return Ok(normalize_id(id));
    }
    if let Some(pane) = &ctx.pane_key {
        if let Some(task) = task_for_pane(c, pane)? {
            return Ok(task.display_id);
        }
    }
    Err(StoreError::Invalid(
        "no task id: pass ID or set DROVR_TASK".into(),
    ))
}

fn apply(c: &Connection, op: &TaskOp, ctx: &OpContext) -> StoreResult<OpResult> {
    let actor = &ctx.actor;
    match op {
        TaskOp::Add {
            project,
            title,
            body,
            kind,
            priority,
            criteria,
        } => {
            let task = create_task(
                c,
                &NewTask {
                    project: project.clone(),
                    title: title.clone(),
                    body: body.clone(),
                    kind: *kind,
                    priority: priority.unwrap_or_default(),
                    status: None,
                    criteria: criteria.clone(),
                },
                actor,
            )?;
            Ok(OpResult::ok(&task, "added"))
        }
        TaskOp::Update {
            task,
            title,
            body,
            kind,
            priority,
        } => {
            let id = resolve_task(c, task, ctx)?;
            let patch = TaskPatch {
                title: title.clone().map(Some),
                body: body.clone(),
                kind: kind.map(Some),
                priority: *priority,
                ..TaskPatch::default()
            };
            let task = update_task(c, &id, &patch, actor)?;
            Ok(OpResult::ok(&task, "updated"))
        }
        TaskOp::Status { task, to, note } => {
            let id = resolve_task(c, task, ctx)?;
            let task = move_task(c, &id, *to, actor, note.as_deref())?;
            Ok(OpResult::ok(&task, "moved"))
        }
        TaskOp::Start {
            task,
            harness,
            session_id,
        } => {
            let id = resolve_task(c, task, ctx)?;
            let attempt = start_attempt(
                c,
                &id,
                &NewAttempt {
                    harness: harness.clone(),
                    machine: ctx.machine.clone(),
                    workspace_key: None,
                    pane_key: ctx.pane_key.clone(),
                    session_id: session_id.clone(),
                },
                actor,
            )?;
            let task = task_by_id(c, attempt.task_id)?;
            let number = attempt_number(c, task.id, attempt.id)?;
            Ok(OpResult::ok(&task, &format!("attempt {number} started")))
        }
        TaskOp::Note { task, body } => {
            let id = resolve_task(c, task, ctx)?;
            let kind = match actor {
                Actor::Human => EntryKind::Human,
                _ => EntryKind::Agent,
            };
            let task = load_task(c, &id)?;
            if body.trim().is_empty() {
                return Err(StoreError::Invalid("the note is empty".into()));
            }
            let attempt = match actor {
                Actor::Agent(_) => open_attempt(c, task.id)?.map(|a| a.id),
                _ => None,
            };
            insert_entry(c, task.id, kind, &actor.author(), attempt, body, None)?;
            touch(c, task.id)?;
            Ok(OpResult::ok(&task, "noted"))
        }
        TaskOp::Criteria { task, set, add } => {
            let id = resolve_task(c, task, ctx)?;
            let task = load_task(c, &id)?;
            if set.is_empty() && add.is_empty() {
                return Err(StoreError::Invalid("pass --add or --set".into()));
            }
            if !set.is_empty() {
                set_criteria(c, task.id, set)?;
            }
            for text in add {
                add_criterion(c, task.id, text)?;
            }
            touch(c, task.id)?;
            let count = criteria_of(c, task.id)?.len();
            Ok(OpResult::ok(&task, &format!("{count} criteria")))
        }
        TaskOp::Check {
            task,
            position,
            state,
            evidence,
        } => {
            let id = resolve_task(c, task, ctx)?;
            let criterion = check_criterion(c, &id, *position, *state, evidence.as_deref(), actor)?;
            let task = task_by_id(c, criterion.task_id)?;
            Ok(OpResult::ok(
                &task,
                &format!("criterion {position} {}", state.as_str()),
            ))
        }
        TaskOp::Artifact {
            task,
            kind,
            title,
            target,
            summary,
        } => {
            let id = resolve_task(c, task, ctx)?;
            let artifact = attach_artifact(
                c,
                &id,
                &NewArtifact {
                    kind: *kind,
                    title: title.clone(),
                    target: target.clone(),
                    machine: Some(ctx.machine.clone()),
                    summary: summary.clone(),
                },
                actor,
            )?;
            let task = task_by_id(c, artifact.task_id)?;
            Ok(OpResult::ok(
                &task,
                &format!("attached {} {}", kind.as_str(), artifact.title),
            ))
        }
        TaskOp::Done {
            task,
            outcome,
            note,
        } => {
            let id = resolve_task(c, task, ctx)?;
            let task = finish_attempt(c, &id, *outcome, note.as_deref(), actor)?;
            Ok(OpResult::ok(
                &task,
                &format!("attempt {}", outcome.as_str()),
            ))
        }
        TaskOp::Release { task, note } => {
            let id = resolve_task(c, task, ctx)?;
            let task = release(c, &id, note, actor)?;
            Ok(OpResult::ok(&task, "released"))
        }
        TaskOp::Decide {
            task,
            title,
            summary,
            choices,
            default_choice,
            allow_text,
            expires_at,
            wait_secs,
        } => {
            let id = resolve_task(c, task, ctx)?;
            let wait_until = wait_secs.map(|secs| {
                super::time_text(
                    time::OffsetDateTime::now_utc() + time::Duration::seconds(i64::from(secs)),
                )
            });
            let decision = request_decision(
                c,
                &id,
                &NewDecision {
                    title: title.clone(),
                    summary: summary.clone(),
                    choices: choices.clone(),
                    allow_text: *allow_text,
                    default_choice: default_choice.clone(),
                    expires_at: expires_at.clone(),
                    wait_until,
                },
                actor,
            )?;
            let task = task_by_id(c, decision.task_id)?;
            let mut result = OpResult::ok(&task, &format!("decision {} asked", decision.id));
            result.decision_id = Some(decision.id);
            Ok(result)
        }
        TaskOp::Withdraw { task } => {
            let id = resolve_task(c, task, ctx)?;
            let task = load_task(c, &id)?;
            let Some(decision) = open_decision(c, task.id)? else {
                return Err(StoreError::Invalid(format!(
                    "{} has no open decision",
                    task.display_id
                )));
            };
            close_decision(c, &decision, DecisionState::Withdrawn, actor)?;
            unblock(c, &task, actor)?;
            touch(c, task.id)?;
            let task = task_by_id(c, task.id)?;
            Ok(OpResult::ok(&task, "decision withdrawn"))
        }
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
