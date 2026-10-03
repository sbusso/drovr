//! One-shot import from a workspace `workspace.sqlite`, schema v8 to v11
//! (docs/design/tasks.md section 2.7).
//!
//! The source is opened read-only; every write lands in the caller's
//! transaction, so a dry run rolls back and a failure leaves nothing.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;

use super::store::{
    ensure_project, find_task, insert_entry_at, insert_project, insert_task, key_taken,
    project_by_name, InsertTask, TaskTimes,
};
use super::{
    cut_text, now_text, ArtifactKind, CheckState, Choice, EntryKind, ImportReport, Kind, Outcome,
    Priority, Project, Review, Status, StoreError, StoreResult, MAX_TEXT,
};

const OLDEST: i64 = 8;
const NEWEST: i64 = 11;

pub(super) fn import(
    c: &Connection,
    path: &Path,
    map: &[(String, String)],
) -> StoreResult<ImportReport> {
    let src = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|err| StoreError::Invalid(format!("cannot open {}: {err}", path.display())))?;
    let version: i64 = src
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM migrations",
            [],
            |row| row.get(0),
        )
        .map_err(|err| {
            StoreError::Invalid(format!("{} is not a workspace db: {err}", path.display()))
        })?;
    if !(OLDEST..=NEWEST).contains(&version) {
        return Err(StoreError::Invalid(format!(
            "workspace db version {version}; the import reads {OLDEST} to {NEWEST}"
        )));
    }
    Import {
        c,
        src: &src,
        map,
        handles: handles(&src)?,
        report: ImportReport::default(),
        projects_used: HashSet::new(),
        task_ids: HashMap::new(),
    }
    .run()
}

struct Import<'a> {
    c: &'a Connection,
    src: &'a Connection,
    map: &'a [(String, String)],
    /// Member id -> handle.
    handles: HashMap<String, String>,
    report: ImportReport,
    projects_used: HashSet<i64>,
    /// Source task id -> drovr (row id, display id), this run only.
    task_ids: HashMap<String, (i64, String)>,
}

struct SourceTask {
    id: String,
    number: i64,
    display_id: String,
    title: Option<String>,
    body: String,
    status: String,
    kind: Option<String>,
    priority: String,
    executor_id: Option<String>,
    thread_id: String,
    duplicate_of_id: Option<String>,
    position: f64,
    created_at: String,
    updated_at: String,
    closed_at: Option<String>,
    archived_at: Option<String>,
    status_since: Option<String>,
}

impl Import<'_> {
    fn run(mut self) -> StoreResult<ImportReport> {
        let projects: Vec<(String, String, String)> = {
            let mut stmt = self
                .src
                .prepare("SELECT id, key, name FROM projects ORDER BY created_at, id")?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        let mut duplicates = Vec::new();
        for (source_id, key, name) in projects {
            self.project(&source_id, &key, &name, &mut duplicates)?;
        }
        for (task_id, duplicate_of) in duplicates {
            let Some((row_id, _)) = self.task_ids.get(&task_id).cloned() else {
                continue;
            };
            let of = self
                .task_ids
                .get(&duplicate_of)
                .map(|(_, display)| display.clone())
                .or_else(|| self.source_display_id(&duplicate_of))
                .unwrap_or_else(|| "another task".into());
            insert_entry_at(
                self.c,
                row_id,
                EntryKind::Event,
                "import",
                None,
                &format!("imported as duplicate of {of}"),
                Some("import"),
                &now_text(),
            )?;
        }
        self.report.projects = self.projects_used.len() as u32;
        Ok(self.report)
    }

    fn source_display_id(&self, id: &str) -> Option<String> {
        self.src
            .query_row("SELECT display_id FROM tasks WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .ok()
    }

    fn project(
        &mut self,
        source_id: &str,
        key: &str,
        name: &str,
        duplicates: &mut Vec<(String, String)>,
    ) -> StoreResult<()> {
        let tasks = self.source_tasks(source_id)?;
        let fresh: Vec<SourceTask> = tasks
            .into_iter()
            .filter_map(|task| match self.imported(&task.id) {
                Ok(true) => {
                    self.report.skipped += 1;
                    None
                }
                Ok(false) => Some(Ok(task)),
                Err(err) => Some(Err(err)),
            })
            .collect::<StoreResult<_>>()?;
        if fresh.is_empty() {
            return Ok(());
        }
        let section = self
            .map
            .iter()
            .find(|(map_key, _)| map_key.eq_ignore_ascii_case(key))
            .map_or(name, |(_, section)| section.as_str());
        let project = self.target_project(section, key)?;
        let keep_numbers = project.key == key;
        self.projects_used.insert(project.id);
        for task in fresh {
            if task.status == "duplicate" {
                if let Some(of) = &task.duplicate_of_id {
                    duplicates.push((task.id.clone(), of.clone()));
                }
            }
            self.task(&project, keep_numbers, task)?;
            self.report.tasks += 1;
        }
        Ok(())
    }

    /// The drovr project for `section`; the workspace key is kept when that
    /// project has no tasks yet and the key is free.
    fn target_project(&self, section: &str, key: &str) -> StoreResult<Project> {
        let usable = valid_key(key);
        let Some(existing) = project_by_name(self.c, section)? else {
            if usable && !key_taken(self.c, key)? {
                return insert_project(self.c, section, key);
            }
            return ensure_project(self.c, section);
        };
        if existing.key == key || !usable || key_taken(self.c, key)? {
            return Ok(existing);
        }
        let has_tasks: bool = self.c.query_row(
            "SELECT EXISTS (SELECT 1 FROM tasks WHERE project_id = ?1)",
            [existing.id],
            |row| row.get(0),
        )?;
        if has_tasks {
            return Ok(existing);
        }
        self.c.execute(
            "UPDATE projects SET key = ?2 WHERE id = ?1",
            params![existing.id, key],
        )?;
        Ok(project_by_name(self.c, section)?.unwrap_or(existing))
    }

    fn imported(&self, source_id: &str) -> StoreResult<bool> {
        Ok(self.c.query_row(
            "SELECT EXISTS (SELECT 1 FROM tasks WHERE ext_id = ?1)",
            [source_id],
            |row| row.get(0),
        )?)
    }

    fn source_tasks(&self, project_id: &str) -> StoreResult<Vec<SourceTask>> {
        let kind = column_or(self.src, "tasks", "kind", "NULL")?;
        let archived = column_or(self.src, "tasks", "archived_at", "NULL")?;
        let since = column_or(self.src, "tasks", "status_since", "NULL")?;
        let sql = format!(
            "SELECT id, number, display_id, title, body_md, status, {kind}, priority, executor_id,
               thread_id, duplicate_of_id, position, created_at, updated_at, closed_at,
               {archived}, {since}
             FROM tasks WHERE project_id = ?1 ORDER BY number"
        );
        let mut stmt = self.src.prepare(&sql)?;
        let rows = stmt.query_map([project_id], |row| {
            Ok(SourceTask {
                id: row.get(0)?,
                number: row.get(1)?,
                display_id: row.get(2)?,
                title: row.get(3)?,
                body: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                status: row.get(5)?,
                kind: row.get(6)?,
                priority: row.get(7)?,
                executor_id: row.get(8)?,
                thread_id: row.get(9)?,
                duplicate_of_id: row.get(10)?,
                position: row.get(11)?,
                created_at: row.get(12)?,
                updated_at: row.get(13)?,
                closed_at: row.get(14)?,
                archived_at: row.get(15)?,
                status_since: row.get(16)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn task(&mut self, project: &Project, keep_numbers: bool, task: SourceTask) -> StoreResult<()> {
        let status = match task.status.as_str() {
            "needs_human" => Status::Blocked,
            "duplicate" => Status::Cancelled,
            other => Status::parse(other).unwrap_or(Status::Triage),
        };
        let wanted = format!("{}-{}", project.key, task.number);
        let (number, display_id) = if keep_numbers && find_task(self.c, &wanted)?.is_none() {
            (task.number, wanted)
        } else {
            let number: i64 = self.c.query_row(
                "SELECT next_number FROM projects WHERE id = ?1",
                [project.id],
                |row| row.get(0),
            )?;
            (number, format!("{}-{number}", project.key))
        };
        let _ = task.display_id;
        let updated = stamp(&task.updated_at);
        let closed = task
            .closed_at
            .as_deref()
            .map(stamp)
            .or_else(|| status.is_closed().then(|| updated.clone()));
        let row_id = insert_task(
            self.c,
            &InsertTask {
                project_id: project.id,
                number,
                display_id: &display_id,
                title: task
                    .title
                    .as_deref()
                    .map(str::trim)
                    .filter(|t| !t.is_empty()),
                body: &task.body,
                status,
                kind: task.kind.as_deref().and_then(Kind::parse),
                priority: Priority::parse(&task.priority).unwrap_or_default(),
                position: task.position,
                ext_id: Some(&task.id),
                times: Some(TaskTimes {
                    status_since: task
                        .status_since
                        .as_deref()
                        .map(stamp)
                        .unwrap_or_else(|| updated.clone()),
                    created_at: stamp(&task.created_at),
                    updated_at: updated,
                    closed_at: if status.is_closed() { closed } else { None },
                    archived_at: task.archived_at.as_deref().map(stamp),
                }),
            },
        )?;
        self.c.execute(
            "UPDATE projects SET next_number = MAX(next_number, ?2 + 1) WHERE id = ?1",
            params![project.id, number],
        )?;
        if let Some(executor) = task
            .executor_id
            .as_ref()
            .and_then(|id| self.handles.get(id))
        {
            self.c.execute(
                "UPDATE tasks SET executor = ?2 WHERE id = ?1",
                params![row_id, executor],
            )?;
        }
        self.task_ids
            .insert(task.id.clone(), (row_id, display_id.clone()));
        self.criteria(&task.id, row_id)?;
        let attempts = self.attempts(&task.id, row_id)?;
        self.entries(&task.thread_id, row_id, &attempts)?;
        self.artifacts(&task.id, row_id, &attempts)?;
        self.decision(&task.id, row_id, &attempts)?;
        Ok(())
    }

    fn criteria(&self, source_task: &str, task_id: i64) -> StoreResult<()> {
        let mut stmt = self.src.prepare(
            "SELECT text, check_json, state, checked_by, checked_at, last_output
             FROM criteria WHERE task_id = ?1 ORDER BY position, id",
        )?;
        let rows = stmt.query_map([source_task], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?;
        for (index, row) in rows.enumerate() {
            let (text, check, state, checked_by, checked_at, output) = row?;
            let check_cmd = check.as_deref().and_then(check_command);
            self.c.execute(
                "INSERT INTO criteria (task_id, position, text, check_cmd, state, evidence,
                   checked_by, checked_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    task_id,
                    index as i64 + 1,
                    text,
                    check_cmd,
                    CheckState::parse(&state).unwrap_or(CheckState::Open),
                    output.map(|text| cut_text(&text, MAX_TEXT)),
                    checked_by.and_then(|id| self.handles.get(&id).cloned()),
                    checked_at.as_deref().map(stamp),
                ],
            )?;
        }
        Ok(())
    }

    /// Imports the attempts; returns source id -> drovr id.
    fn attempts(&self, source_task: &str, task_id: i64) -> StoreResult<HashMap<String, i64>> {
        let harness = if has_column(self.src, "attempts", "harness")? {
            "harness"
        } else {
            "runtime"
        };
        let session = if has_column(self.src, "attempts", "session_id")? {
            "session_id"
        } else {
            "runtime_ref"
        };
        let sql = format!(
            "SELECT id, {harness}, {session}, started_at, ended_at, outcome, outcome_note,
               tokens_in, tokens_out, cost_cents
             FROM attempts WHERE task_id = ?1 ORDER BY started_at, id"
        );
        let mut stmt = self.src.prepare(&sql)?;
        let rows = stmt.query_map([source_task], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, Option<i64>>(8)?,
                row.get::<_, Option<i64>>(9)?,
            ))
        })?;
        let mut ids = HashMap::new();
        for row in rows {
            let (id, harness, session, started, ended, outcome, note, tin, tout, cost) = row?;
            // An attempt still open in the workspace has no pane here; it is
            // ended so the board does not show a live agent that is not.
            let (ended, outcome, note) = match ended {
                Some(ended) => (
                    stamp(&ended),
                    match outcome.as_deref() {
                        Some("stale") | None => Outcome::Stopped,
                        Some(other) => Outcome::parse(other).unwrap_or(Outcome::Stopped),
                    },
                    note,
                ),
                None => (
                    now_text(),
                    Outcome::Stopped,
                    Some(note.unwrap_or_else(|| "open at import".into())),
                ),
            };
            self.c.execute(
                "INSERT INTO attempts (task_id, harness, machine, session_id, started_at, ended_at,
                   outcome, note, tokens_in, tokens_out, cost_cents)
                 VALUES (?1, ?2, 'import', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    task_id,
                    harness
                        .filter(|h| !h.is_empty())
                        .unwrap_or_else(|| "agent".into()),
                    session,
                    stamp(&started),
                    ended,
                    outcome,
                    note,
                    tin,
                    tout,
                    cost
                ],
            )?;
            ids.insert(id, self.c.last_insert_rowid());
        }
        Ok(ids)
    }

    fn entries(
        &self,
        thread_id: &str,
        task_id: i64,
        attempts: &HashMap<String, i64>,
    ) -> StoreResult<()> {
        let mut stmt = self.src.prepare(
            "SELECT e.kind, e.author_id, e.attempt_id, e.body_md, e.event_type, e.pinned,
               e.created_at, q.summary, q.options_json
             FROM entries e LEFT JOIN questions q ON q.entry_id = e.id
             WHERE e.thread_id = ?1 AND e.deleted_at IS NULL ORDER BY e.seq",
        )?;
        let rows = stmt.query_map([thread_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, bool>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
            ))
        })?;
        for row in rows {
            let (kind, author, attempt, body, event_type, pinned, created, summary, options) = row?;
            let (kind, body) = match kind.as_str() {
                "question" => {
                    let mut text = summary.unwrap_or_default();
                    if let Some(body) = body.filter(|b| !b.trim().is_empty()) {
                        text.push_str("\n\n");
                        text.push_str(&body);
                    }
                    for option in options.as_deref().map(options_of).unwrap_or_default() {
                        text.push_str(&format!("\n- {}", option.label));
                    }
                    (EntryKind::Agent, text)
                }
                other => {
                    let kind = EntryKind::parse(other).unwrap_or(EntryKind::Event);
                    let body = body
                        .filter(|b| !b.trim().is_empty())
                        .or_else(|| event_type.clone())
                        .unwrap_or_default();
                    (kind, body)
                }
            };
            if body.trim().is_empty() {
                continue;
            }
            let author = self
                .handles
                .get(&author)
                .cloned()
                .unwrap_or_else(|| "import".into());
            let entry = insert_entry_at(
                self.c,
                task_id,
                kind,
                &author,
                attempt.and_then(|id| attempts.get(&id).copied()),
                &body,
                event_type.as_deref(),
                &stamp(&created),
            )?;
            if pinned {
                self.c
                    .execute("UPDATE entries SET pinned = 1 WHERE id = ?1", [entry.id])?;
            }
        }
        Ok(())
    }

    fn artifacts(
        &self,
        source_task: &str,
        task_id: i64,
        attempts: &HashMap<String, i64>,
    ) -> StoreResult<()> {
        let mut stmt = self.src.prepare(
            "SELECT type, title, link, summary_line, review_state, attempt_id, created_at
             FROM artifacts WHERE task_id = ?1 ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([source_task], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?;
        for row in rows {
            let (kind, title, link, summary, review, attempt, created) = row?;
            let kind = match kind.as_str() {
                "document" => ArtifactKind::Doc,
                "diff" => ArtifactKind::Diff,
                "link" | "pull_request" => ArtifactKind::Link,
                "report" => ArtifactKind::Report,
                _ => ArtifactKind::File,
            };
            self.c.execute(
                "INSERT INTO artifacts (task_id, attempt_id, kind, title, target, summary, review,
                   created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    task_id,
                    attempt.and_then(|id| attempts.get(&id).copied()),
                    kind,
                    title,
                    link.filter(|l| !l.is_empty())
                        .unwrap_or_else(|| "(inline)".into()),
                    summary,
                    Review::parse(&review).unwrap_or(Review::Unreviewed),
                    stamp(&created),
                ],
            )?;
        }
        Ok(())
    }

    /// The open decision question of the task, if any (schema v10+).
    fn decision(
        &self,
        source_task: &str,
        task_id: i64,
        attempts: &HashMap<String, i64>,
    ) -> StoreResult<()> {
        let allow = column_or(self.src, "questions", "allow_free_text", "1")?;
        let default = column_or(self.src, "questions", "default_choice_id", "NULL")?;
        let sql = format!(
            "SELECT q.summary, e.body_md, q.options_json, {allow}, {default}, q.expires_at,
               e.attempt_id, e.created_at
             FROM questions q JOIN entries e ON e.id = q.entry_id
             WHERE q.task_id = ?1 AND q.kind = 'decision' AND q.state = 'open'
             ORDER BY e.created_at LIMIT 1"
        );
        let row = self
            .src
            .query_row(&sql, [source_task], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                ))
            })
            .optional()?;
        let Some((title, summary, options, allow_text, default, expires, attempt, created)) = row
        else {
            return Ok(());
        };
        let choices: Vec<Choice> = options
            .as_deref()
            .map(options_of)
            .unwrap_or_default()
            .into_iter()
            .map(|option| Choice {
                id: option.value,
                label: option.label,
                consequence: option.consequence,
                recommended: option.recommended.unwrap_or(false),
            })
            .collect();
        let default = default.filter(|d| choices.iter().any(|c| &c.id == d));
        if super::store::validate_choices(&choices, default.as_deref()).is_err() {
            return Ok(());
        }
        let title: String = title.chars().take(120).collect();
        let summary: String = summary.unwrap_or_default().chars().take(1200).collect();
        self.c.execute(
            "INSERT INTO decisions (task_id, attempt_id, title, summary, choices_json, allow_text,
               default_choice, expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                task_id,
                attempt.and_then(|id| attempts.get(&id).copied()),
                title,
                summary,
                serde_json::to_string(&choices).unwrap_or_else(|_| "[]".into()),
                allow_text,
                default,
                expires.as_deref().map(stamp),
                stamp(&created),
            ],
        )?;
        Ok(())
    }
}

fn handles(src: &Connection) -> StoreResult<HashMap<String, String>> {
    let mut stmt = src.prepare("SELECT id, handle FROM members")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn has_column(src: &Connection, table: &str, column: &str) -> StoreResult<bool> {
    let mut stmt = src.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `column` when the table has it, else the fallback SQL expression.
fn column_or(src: &Connection, table: &str, column: &str, fallback: &str) -> StoreResult<String> {
    Ok(if has_column(src, table, column)? {
        column.to_owned()
    } else {
        fallback.to_owned()
    })
}

fn valid_key(key: &str) -> bool {
    (2..=10).contains(&key.len())
        && key.starts_with(|ch: char| ch.is_ascii_uppercase())
        && key
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// A workspace timestamp in this store's form: RFC 3339 UTC, whole seconds.
/// The workspace writes UTC (`2026-01-02T03:04:05.678Z`); the fraction is
/// cut. Text of another shape is kept as it is.
fn stamp(text: &str) -> String {
    let bytes = text.as_bytes();
    let utc = text.ends_with('Z') || text.ends_with("+00:00");
    let shaped = bytes.len() >= 20
        && bytes[10] == b'T'
        && matches!(bytes[19], b'.' | b'Z' | b'+')
        && text[..19]
            .bytes()
            .enumerate()
            .all(|(i, b)| matches!(i, 4 | 7 | 10 | 13 | 16) || b.is_ascii_digit());
    if utc && shaped {
        format!("{}Z", &text[..19])
    } else {
        text.to_owned()
    }
}

#[derive(Deserialize)]
struct SourceOption {
    value: String,
    label: String,
    #[serde(default)]
    consequence: Option<String>,
    #[serde(default)]
    recommended: Option<bool>,
}

fn options_of(json: &str) -> Vec<SourceOption> {
    serde_json::from_str(json).unwrap_or_default()
}

/// The command of a `{"kind":"command","command":...}` check.
fn check_command(json: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Check {
        kind: String,
        #[serde(default)]
        command: Option<String>,
    }
    let check: Check = serde_json::from_str(json).ok()?;
    (check.kind == "command")
        .then_some(check.command)
        .flatten()
        .filter(|cmd| !cmd.trim().is_empty())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::tasks::test_support::TempDir;
    use crate::tasks::{TaskFilter, TaskStore};

    /// The v8 subset of the workspace schema the import reads.
    const V8: &str = "
CREATE TABLE migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);
INSERT INTO migrations VALUES (8, '2026-01-01T00:00:00.000Z');
CREATE TABLE members (id TEXT PRIMARY KEY, workspace_id TEXT, kind TEXT, handle TEXT NOT NULL,
  display_name TEXT, created_at TEXT);
CREATE TABLE projects (id TEXT PRIMARY KEY, workspace_id TEXT, key TEXT NOT NULL,
  name TEXT NOT NULL, next_number INTEGER NOT NULL DEFAULT 1, created_at TEXT NOT NULL);
CREATE TABLE tasks (id TEXT PRIMARY KEY, workspace_id TEXT, project_id TEXT NOT NULL,
  number INTEGER NOT NULL, display_id TEXT NOT NULL, title TEXT, body_md TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL, priority TEXT NOT NULL DEFAULT 'normal', executor_id TEXT,
  thread_id TEXT NOT NULL, duplicate_of_id TEXT, position REAL NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL, closed_at TEXT, archived_at TEXT,
  kind TEXT);
CREATE TABLE criteria (id TEXT PRIMARY KEY, task_id TEXT NOT NULL, position INTEGER NOT NULL,
  text TEXT NOT NULL, check_json TEXT, state TEXT NOT NULL DEFAULT 'open', checked_by TEXT,
  checked_at TEXT, last_output TEXT);
CREATE TABLE entries (id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, seq INTEGER NOT NULL,
  kind TEXT NOT NULL, author_id TEXT NOT NULL, attempt_id TEXT, body_md TEXT, event_type TEXT,
  event_json TEXT, pinned INTEGER NOT NULL DEFAULT 0, deleted_at TEXT, created_at TEXT NOT NULL);
CREATE TABLE questions (entry_id TEXT PRIMARY KEY, task_id TEXT, kind TEXT NOT NULL,
  summary TEXT NOT NULL, options_json TEXT, expires_at TEXT, state TEXT NOT NULL DEFAULT 'open');
CREATE TABLE attempts (id TEXT PRIMARY KEY, task_id TEXT NOT NULL, agent_id TEXT, runtime TEXT,
  runtime_ref TEXT, started_at TEXT NOT NULL, ended_at TEXT, outcome TEXT, outcome_note TEXT,
  tokens_in INTEGER NOT NULL DEFAULT 0, tokens_out INTEGER NOT NULL DEFAULT 0,
  cost_cents INTEGER NOT NULL DEFAULT 0);
CREATE TABLE artifacts (id TEXT PRIMARY KEY, task_id TEXT NOT NULL, attempt_id TEXT,
  type TEXT NOT NULL, title TEXT NOT NULL, link TEXT, summary_line TEXT,
  review_state TEXT NOT NULL DEFAULT 'unreviewed', created_at TEXT NOT NULL);

INSERT INTO members VALUES ('m1', 'w', 'human', 'stephane', 'S', 'x'),
  ('m2', 'w', 'agent', 'claude', 'C', 'x');
INSERT INTO projects VALUES ('p1', 'w', 'AC', 'Acme', 4, '2026-01-01T00:00:00.000Z'),
  ('p2', 'w', 'OPS', 'Operations', 2, '2026-01-01T00:00:00.000Z');
INSERT INTO tasks (id, project_id, number, display_id, title, body_md, status, priority,
  executor_id, thread_id, duplicate_of_id, position, created_at, updated_at, closed_at, kind)
VALUES
  ('t1', 'p1', 1, 'AC-1', 'Spec decisions', 'Write it.', 'needs_human', 'high', 'm2', 'h1', NULL,
   10, '2026-01-02T03:04:05.678Z', '2026-01-03T00:00:00.000Z', NULL, 'spec'),
  ('t2', 'p1', 2, 'AC-2', NULL, 'Same as AC-1', 'duplicate', 'normal', NULL, 'h2', 't1',
   20, '2026-01-02T00:00:00.000Z', '2026-01-04T00:00:00.000Z', '2026-01-04T00:00:00.000Z', NULL),
  ('t3', 'p1', 3, 'AC-3', 'Ship', '', 'done', 'low', NULL, 'h3', NULL,
   30, '2026-01-02T00:00:00.000Z', '2026-01-05T00:00:00.000Z', '2026-01-05T00:00:00.000Z', 'chore'),
  ('t4', 'p2', 1, 'OPS-1', 'Rotate keys', '', 'ready', 'urgent', NULL, 'h4', NULL,
   5, '2026-01-02T00:00:00.000Z', '2026-01-02T00:00:00.000Z', NULL, 'bogus');
INSERT INTO criteria VALUES
  ('c1', 't1', 1, 'schema added', '{\"kind\":\"command\",\"command\":\"cargo test\"}', 'passed',
   'm2', '2026-01-03T00:00:00.000Z', 'ok'),
  ('c2', 't1', 2, 'docs', NULL, 'open', NULL, NULL, NULL);
INSERT INTO attempts VALUES
  ('a1', 't1', 'm2', 'claude', 'sess-1', '2026-01-02T05:00:00.000Z', '2026-01-02T06:00:00.000Z',
   'stale', 'gone', 10, 20, 42);
INSERT INTO entries VALUES
  ('e1', 'h1', 1, 'human', 'm1', NULL, 'Start with the store.', NULL, NULL, 1, NULL,
   '2026-01-02T04:00:00.000Z'),
  ('e2', 'h1', 2, 'agent', 'm2', 'a1', 'On it.', NULL, NULL, 0, NULL, '2026-01-02T05:00:00.000Z'),
  ('e3', 'h1', 3, 'question', 'm2', 'a1', NULL, NULL, NULL, 0, NULL, '2026-01-02T05:30:00.000Z'),
  ('e4', 'h1', 4, 'human', 'm1', NULL, 'deleted', NULL, NULL, 0, 'x', '2026-01-02T05:40:00.000Z');
INSERT INTO questions VALUES ('e3', 't1', 'question', 'Which table?',
  '[{\"value\":\"new\",\"label\":\"New table\"},{\"value\":\"reuse\",\"label\":\"Reuse entries\"}]',
  NULL, 'open');
INSERT INTO artifacts VALUES
  ('r1', 't1', 'a1', 'document', 'Design', '/tmp/design.md', 'the design', 'accepted',
   '2026-01-02T05:50:00.000Z'),
  ('r2', 't1', NULL, 'image', 'Shot', NULL, NULL, 'unreviewed', '2026-01-02T05:51:00.000Z');
";

    pub(crate) fn fixture(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("workspace.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(V8).unwrap();
        path
    }

    #[test]
    fn import_maps_counts_statuses_and_is_idempotent() {
        let dir = TempDir::new("import");
        let source = fixture(dir.path());
        let store = TaskStore::open_in_memory().unwrap();
        let map = vec![("OPS".to_owned(), "Infrastructure".to_owned())];

        let dry = store.import_workspace(&source, &map, true).unwrap();
        assert_eq!((dry.tasks, dry.projects, dry.skipped), (4, 2, 0));
        assert!(store.projects().unwrap().is_empty(), "dry run rolls back");

        let report = store.import_workspace(&source, &map, false).unwrap();
        assert_eq!((report.tasks, report.projects, report.skipped), (4, 2, 0));

        let ac1 = store.task_detail("AC-1").unwrap().unwrap();
        assert_eq!(ac1.project.name, "Acme");
        assert_eq!(ac1.task.status, Status::Blocked);
        assert_eq!(ac1.task.kind, Some(Kind::Spec));
        assert_eq!(ac1.task.priority, Priority::High);
        assert_eq!(ac1.task.executor.as_deref(), Some("claude"));
        assert_eq!(ac1.task.created_at, "2026-01-02T03:04:05Z");
        assert_eq!(ac1.task.status_since, "2026-01-03T00:00:00Z");
        assert_eq!(ac1.criteria.len(), 2);
        assert_eq!(ac1.criteria[0].check_cmd.as_deref(), Some("cargo test"));
        assert_eq!(ac1.criteria[0].state, CheckState::Passed);
        assert_eq!(ac1.criteria[0].evidence.as_deref(), Some("ok"));
        assert_eq!(ac1.criteria[0].checked_by.as_deref(), Some("claude"));
        let bodies: Vec<&str> = ac1.entries.iter().map(|e| e.body.as_str()).collect();
        assert_eq!(
            bodies,
            vec![
                "Start with the store.",
                "On it.",
                "Which table?\n- New table\n- Reuse entries"
            ]
        );
        assert!(ac1.entries[0].pinned);
        assert_eq!(ac1.entries[0].author, "stephane");
        assert_eq!(ac1.entries[2].kind, EntryKind::Agent);
        assert_eq!(ac1.attempts.len(), 1);
        let attempt = &ac1.attempts[0];
        assert_eq!(attempt.harness, "claude");
        assert_eq!(attempt.machine, "import");
        assert_eq!(attempt.outcome, Some(Outcome::Stopped));
        assert_eq!(attempt.cost_cents, Some(42));
        assert_eq!(ac1.entries[1].attempt_id, Some(attempt.id));
        assert_eq!(ac1.artifacts.len(), 2);
        let kinds: Vec<ArtifactKind> = ac1.artifacts.iter().map(|a| a.kind).collect();
        assert_eq!(kinds, vec![ArtifactKind::File, ArtifactKind::Doc]);
        assert_eq!(ac1.artifacts[0].target, "(inline)");
        assert_eq!(ac1.artifacts[1].review, Review::Accepted);

        let ac2 = store.task_detail("AC-2").unwrap().unwrap();
        assert_eq!(ac2.task.status, Status::Cancelled);
        assert!(ac2
            .entries
            .iter()
            .any(|e| e.body == "imported as duplicate of AC-1"));
        let ac3 = store.task("AC-3").unwrap().unwrap();
        assert_eq!(ac3.status, Status::Done);
        assert_eq!(ac3.closed_at.as_deref(), Some("2026-01-05T00:00:00Z"));

        let ops = store
            .list(&TaskFilter {
                project: Some("Infrastructure".into()),
                ..TaskFilter::default()
            })
            .unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].task.display_id, "OPS-1");
        assert_eq!(ops[0].task.kind, None, "unknown kinds drop");
        assert_eq!(store.project("Acme").unwrap().unwrap().next_number, 4);

        let again = store.import_workspace(&source, &map, false).unwrap();
        assert_eq!((again.tasks, again.projects, again.skipped), (0, 0, 4));
    }

    #[test]
    fn a_project_with_tasks_keeps_its_key_and_numbers_are_allocated() {
        let dir = TempDir::new("import-key");
        let source = fixture(dir.path());
        let store = TaskStore::open_in_memory().unwrap();
        let existing = store
            .create_task(
                &crate::tasks::NewTask {
                    project: "Acme".into(),
                    title: Some("mine".into()),
                    ..Default::default()
                },
                &crate::tasks::Actor::Human,
            )
            .unwrap();
        assert_eq!(existing.display_id, "ACM-1");
        store.import_workspace(&source, &[], false).unwrap();
        let ids: Vec<String> = store
            .list(&TaskFilter {
                project: Some("Acme".into()),
                ..TaskFilter::default()
            })
            .unwrap()
            .into_iter()
            .map(|card| card.task.display_id)
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["ACM-1", "ACM-2", "ACM-3", "ACM-4"]);
    }

    #[test]
    fn an_open_decision_question_becomes_a_decision() {
        let dir = TempDir::new("import-decision");
        let source = fixture(dir.path());
        {
            let conn = Connection::open(&source).unwrap();
            conn.execute_batch(
                "UPDATE migrations SET version = 10;
                 ALTER TABLE questions ADD COLUMN allow_free_text INTEGER NOT NULL DEFAULT 1;
                 ALTER TABLE questions ADD COLUMN default_choice_id TEXT;
                 ALTER TABLE attempts RENAME COLUMN runtime TO harness;
                 ALTER TABLE attempts RENAME COLUMN runtime_ref TO session_id;
                 ALTER TABLE tasks ADD COLUMN status_since TEXT;
                 UPDATE questions SET kind = 'decision', allow_free_text = 0,
                   default_choice_id = 'new';",
            )
            .unwrap();
        }
        let store = TaskStore::open_in_memory().unwrap();
        store.import_workspace(&source, &[], false).unwrap();
        let detail = store.task_detail("AC-1").unwrap().unwrap();
        let decision = detail.decision.unwrap();
        assert_eq!(decision.title, "Which table?");
        assert_eq!(decision.state, crate::tasks::DecisionState::Open);
        assert_eq!(decision.choices.len(), 2);
        assert!(!decision.allow_text);
        assert_eq!(decision.default_choice.as_deref(), Some("new"));
        assert_eq!(store.open_decisions(None).unwrap().len(), 1);
    }

    #[test]
    fn other_versions_are_refused() {
        let dir = TempDir::new("import-version");
        let source = fixture(dir.path());
        Connection::open(&source)
            .unwrap()
            .execute("UPDATE migrations SET version = 7", [])
            .unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let err = store.import_workspace(&source, &[], false).unwrap_err();
        assert!(err.to_string().contains("version 7"), "{err}");
    }
}
