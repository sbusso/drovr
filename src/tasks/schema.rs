//! Schema migrations of the tasks store (docs/design/tasks.md section 2.5).
//!
//! `MIGRATIONS` is append-only: index = version - 1. A published migration
//! is never edited; a change is a new entry.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use super::{now_text, StoreError, StoreResult};

const MIGRATIONS_TABLE: &str = "CREATE TABLE IF NOT EXISTS migrations (
  version INTEGER PRIMARY KEY,
  applied_at TEXT NOT NULL
);";

pub(crate) const MIGRATIONS: &[&str] = &[
    // v1
    r#"
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
"#,
];

/// The schema version this binary writes.
pub(crate) fn known_version() -> i64 {
    MIGRATIONS.len() as i64
}

/// Brings the database to `MIGRATIONS.len()` under one immediate
/// transaction. Refuses a newer database (`TooNew`) and backs up an older
/// one to `{path}.v{found}.bak` first (skipped for in-memory stores).
pub(crate) fn migrate(conn: &mut Connection, path: Option<&Path>) -> StoreResult<()> {
    migrate_with(conn, path, MIGRATIONS)
}

fn migrate_with(
    conn: &mut Connection,
    path: Option<&Path>,
    migrations: &[&str],
) -> StoreResult<()> {
    let known = migrations.len() as i64;
    loop {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(MIGRATIONS_TABLE)?;
        let found: i64 = tx.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM migrations",
            [],
            |row| row.get(0),
        )?;
        if found > known {
            drop(tx);
            return Err(StoreError::TooNew { found, known });
        }
        if found == known {
            tx.commit()?;
            return Ok(());
        }
        if found >= 1 {
            if let Some(path) = path {
                let backup = backup_path(path, &format!("v{found}"));
                if !backup.exists() {
                    drop(tx);
                    vacuum_into(conn, &backup)?;
                    continue;
                }
            }
        }
        for (index, sql) in migrations.iter().enumerate().skip(found as usize) {
            tx.execute_batch(sql)?;
            tx.execute(
                "INSERT INTO migrations (version, applied_at) VALUES (?1, ?2)",
                rusqlite::params![index as i64 + 1, now_text()],
            )?;
        }
        tx.commit()?;
        return Ok(());
    }
}

/// `{path}.{suffix}.bak`.
pub(crate) fn backup_path(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{suffix}.bak"));
    name.into()
}

pub(crate) fn vacuum_into(conn: &Connection, target: &Path) -> StoreResult<()> {
    let target = target
        .to_str()
        .ok_or_else(|| StoreError::Invalid(format!("backup path is not UTF-8: {target:?}")))?;
    conn.execute("VACUUM INTO ?1", [target])?;
    Ok(())
}

/// The highest applied version, 0 for a database without migrations.
pub(crate) fn applied_version(conn: &Connection) -> StoreResult<i64> {
    let exists: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'migrations'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Ok(0);
    }
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM migrations",
        [],
        |row| row.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::test_support::TempDir;
    use crate::tasks::TaskStore;

    fn versions(path: &Path) -> Vec<i64> {
        let conn = Connection::open(path).unwrap();
        let mut stmt = conn
            .prepare("SELECT version FROM migrations ORDER BY version")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn migrate_on_empty_file_then_again_is_a_noop() {
        let dir = TempDir::new("migrate");
        let path = dir.path().join("tasks.db");
        drop(TaskStore::open(&path, 5000).unwrap());
        assert_eq!(versions(&path), vec![1]);
        drop(TaskStore::open(&path, 5000).unwrap());
        assert_eq!(versions(&path), vec![1]);
        // No version backup for a fresh file.
        assert!(!backup_path(&path, "v0").exists());
        assert!(!backup_path(&path, "v1").exists());
    }

    #[test]
    fn migrate_race_applies_once() {
        let dir = TempDir::new("race");
        let path = dir.path().join("tasks.db");
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || TaskStore::open(&path, 5000).map(drop))
            })
            .collect();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        assert_eq!(versions(&path), vec![1]);
    }

    #[test]
    fn newer_file_is_refused_and_left_unchanged() {
        let dir = TempDir::new("toonew");
        let path = dir.path().join("tasks.db");
        drop(TaskStore::open(&path, 5000).unwrap());
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "INSERT INTO migrations (version, applied_at) VALUES (99, 'x')",
                [],
            )
            .unwrap();
        }
        let err = TaskStore::open(&path, 5000).err().unwrap();
        assert!(matches!(
            err,
            StoreError::TooNew {
                found: 99,
                known: 1
            }
        ));
        assert_eq!(
            err.to_string(),
            "tasks db is version 99; this drovr knows 1. Update drovr."
        );
        assert_eq!(versions(&path), vec![1, 99]);
        assert!(!backup_path(&path, "v99").exists());
    }

    #[test]
    fn older_file_is_backed_up_before_migrating() {
        let dir = TempDir::new("older");
        let path = dir.path().join("tasks.db");
        let v2: &[&str] = &[
            MIGRATIONS[0],
            "CREATE TABLE extra (id INTEGER PRIMARY KEY);",
        ];
        {
            let mut conn = Connection::open(&path).unwrap();
            migrate_with(&mut conn, Some(&path), &v2[..1]).unwrap();
            migrate_with(&mut conn, Some(&path), v2).unwrap();
        }
        let backup = backup_path(&path, "v1");
        assert!(backup.exists());
        assert_eq!(versions(&backup), vec![1]);
        assert_eq!(versions(&path), vec![1, 2]);
    }

    #[test]
    fn a_failed_migration_rolls_back_every_version_of_the_run() {
        let mut conn = Connection::open_in_memory().unwrap();
        let broken: &[&str] = &[MIGRATIONS[0], "CREATE TABLE broken (;"];
        assert!(migrate_with(&mut conn, None, broken).is_err());
        assert_eq!(applied_version(&conn).unwrap(), 0);
    }
}
