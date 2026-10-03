//! drovr fork: durable task records (docs/design/tasks.md, sections 2 and 3).
//!
//! One SQLite file on the client machine holds projects (sidebar sections),
//! their tasks, acceptance criteria, the task thread, attempts, artifacts and
//! decisions. The client panel and `drovr task` both use [`TaskStore`];
//! remote agents queue [`TaskOp`]s in an outbox the client pulls.

// Part of the store API (reorder, set_criteria, pin_entry, artifacts,
// decisions requested from code) is contract surface with only test callers
// until the panel's drag-reorder and artifact review land.
#![allow(dead_code)]

pub(crate) mod cli;
pub(crate) mod import;
pub(crate) mod ops;
pub(crate) mod outbox;
pub(crate) mod schema;
pub(crate) mod store;
pub(crate) mod transitions;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)] // the contract re-exports them for the client
pub(crate) use ops::{OpContext, OpResult, OutboxOp, TaskOp, OUTBOX_V};
pub(crate) use store::{ImportReport, TaskStore};

pub(crate) type TaskId = i64;

/// A text enum stored as its snake_case name: `as_str`, `parse` and the
/// rusqlite conversions.
macro_rules! text_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl $name {
            pub(crate) const ALL: &'static [$name] = &[$(Self::$variant),+];

            pub(crate) fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }

            pub(crate) fn parse(text: &str) -> Option<$name> {
                let text = text.trim().to_ascii_lowercase();
                Self::ALL.iter().copied().find(|value| value.as_str() == text)
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(ToSqlOutput::from(self.as_str()))
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                let text = value.as_str()?;
                $name::parse(text).ok_or_else(|| {
                    FromSqlError::Other(format!("bad {} value {text:?}", stringify!($name)).into())
                })
            }
        }
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Triage,
    Ready,
    Working,
    Blocked,
    Review,
    Done,
    Cancelled,
}

impl Status {
    pub(crate) const LANES: [Status; 6] = [
        Self::Triage,
        Self::Ready,
        Self::Working,
        Self::Blocked,
        Self::Review,
        Self::Done,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Triage => "triage",
            Self::Ready => "ready",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Review => "review",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
        }
    }

    /// Also accepts "doing" for Working and "canceled".
    pub(crate) fn parse(text: &str) -> Option<Status> {
        match text.trim().to_ascii_lowercase().as_str() {
            "triage" => Some(Self::Triage),
            "ready" => Some(Self::Ready),
            "working" | "doing" => Some(Self::Working),
            "blocked" => Some(Self::Blocked),
            "review" => Some(Self::Review),
            "done" => Some(Self::Done),
            "cancelled" | "canceled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Triage => "Triage",
            Self::Ready => "Ready",
            Self::Working => "Working",
            Self::Blocked => "Blocked",
            Self::Review => "Review",
            Self::Done => "Done",
            Self::Cancelled => "Cancelled",
        }
    }

    pub(crate) fn is_closed(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled)
    }

    /// Index into [`Status::LANES`]; Cancelled shares the Done lane.
    pub(crate) fn lane(self) -> usize {
        match self {
            Self::Triage => 0,
            Self::Ready => 1,
            Self::Working => 2,
            Self::Blocked => 3,
            Self::Review => 4,
            Self::Done | Self::Cancelled => 5,
        }
    }
}

impl ToSql for Status {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for Status {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        Status::parse(text)
            .ok_or_else(|| FromSqlError::Other(format!("bad status {text:?}").into()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    Fix,
    Feature,
    Chore,
    Research,
    Spec,
}
text_enum!(Kind { Fix => "fix", Feature => "feature", Chore => "chore", Research => "research", Spec => "spec" });

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Priority {
    Urgent,
    High,
    #[default]
    Normal,
    Low,
}
text_enum!(Priority { Urgent => "urgent", High => "high", Normal => "normal", Low => "low" });

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CheckState {
    Open,
    Passed,
    Failed,
}
text_enum!(CheckState { Open => "open", Passed => "passed", Failed => "failed" });

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Succeeded,
    Failed,
    Stopped,
    NeedsHuman,
}
text_enum!(Outcome { Succeeded => "succeeded", Failed => "failed", Stopped => "stopped", NeedsHuman => "needs_human" });

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EntryKind {
    Human,
    Agent,
    Event,
}
text_enum!(EntryKind { Human => "human", Agent => "agent", Event => "event" });

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ArtifactKind {
    Doc,
    Diff,
    Link,
    File,
    Report,
}
text_enum!(ArtifactKind { Doc => "doc", Diff => "diff", Link => "link", File => "file", Report => "report" });

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Review {
    Unreviewed,
    Accepted,
    Rejected,
}
text_enum!(Review { Unreviewed => "unreviewed", Accepted => "accepted", Rejected => "rejected" });

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DecisionState {
    Open,
    Ruled,
    Withdrawn,
    Expired,
}
text_enum!(DecisionState { Open => "open", Ruled => "ruled", Withdrawn => "withdrawn", Expired => "expired" });

/// Who acts. The string is the author written into entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Actor {
    /// The panel, or the CLI outside a herdr pane. Author "you".
    Human,
    /// The CLI inside a herdr pane. Author e.g. "claude@mato".
    Agent(String),
    /// The client's signal sync. Author "drovr".
    Auto,
}

impl Actor {
    pub(crate) fn author(&self) -> String {
        match self {
            Self::Human => "you".into(),
            Self::Agent(name) => name.clone(),
            Self::Auto => "drovr".into(),
        }
    }
}

/// RFC 3339 UTC, whole seconds (section 2.2).
pub(crate) fn now_text() -> String {
    time_text(time::OffsetDateTime::now_utc())
}

pub(crate) fn time_text(at: time::OffsetDateTime) -> String {
    let at = at.to_offset(time::UtcOffset::UTC);
    let at = at.replace_nanosecond(0).unwrap_or(at);
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Cuts `text` to at most `max` bytes on a char boundary, appending `…`
/// when it was cut.
pub(crate) fn cut_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Longest stored evidence or entry body, in bytes (section 2.5).
pub(crate) const MAX_TEXT: usize = 20_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Project {
    pub id: i64,
    pub key: String,
    pub name: String,
    pub next_number: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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
    pub(crate) fn name(&self) -> &str {
        self.title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .or_else(|| {
                self.body
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
            })
            .unwrap_or("Untitled")
    }
}

/// One board row. Counts are computed in the list query.
#[derive(Clone, Debug, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Criterion {
    pub id: i64,
    pub task_id: TaskId,
    /// 1-based, as shown to users.
    pub position: i64,
    pub text: String,
    pub check_cmd: Option<String>,
    pub state: CheckState,
    pub evidence: Option<String>,
    pub checked_by: Option<String>,
    pub checked_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Artifact {
    pub id: i64,
    pub task_id: TaskId,
    pub attempt_id: Option<i64>,
    pub kind: ArtifactKind,
    pub title: String,
    /// Absolute path on `machine`, or a URL.
    pub target: String,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
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
    /// "panel" | "cli" | "expiry"
    pub surface: Option<String>,
    pub expires_at: Option<String>,
    pub wait_until: Option<String>,
    pub created_at: String,
}

impl Decision {
    /// The ruling as one line: the chosen label, else the free text.
    pub(crate) fn ruling_line(&self) -> Option<String> {
        if let Some(choice) = &self.ruling_choice {
            let label = self
                .choices
                .iter()
                .find(|item| &item.id == choice)
                .map_or(choice.as_str(), |item| item.label.as_str());
            return Some(format!("ruled {choice}: {label}"));
        }
        self.ruling_text
            .as_ref()
            .map(|text| format!("ruled text: {text}"))
    }
}

/// An open decision with what the inbox row and the notice draw.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct OpenDecision {
    pub decision: Decision,
    pub display_id: String,
    pub task_name: String,
    pub project: String,
    /// Pane of the task's open attempt, if any ("machine/pane_id").
    pub pane_key: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TaskDetail {
    pub task: Task,
    pub project: Project,
    /// By position.
    pub criteria: Vec<Criterion>,
    /// By seq, last 200.
    pub entries: Vec<Entry>,
    /// Newest first.
    pub attempts: Vec<Attempt>,
    /// Newest first.
    pub artifacts: Vec<Artifact>,
    /// The open one, else the latest.
    pub decision: Option<Decision>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TaskFilter {
    /// Section name.
    pub project: Option<String>,
    /// A `machine/{workspace_id}:` prefix (the caller builds it from the
    /// endpoint label and the workspace id), so a renamed workspace still
    /// matches. The store compares `substr(workspace_key, 1, len) = prefix`,
    /// not LIKE (`_` in ids would be a wildcard).
    pub workspace_key: Option<String>,
    /// Empty = every status.
    pub statuses: Vec<Status>,
    /// False: `archived_at IS NULL` only.
    pub include_archived: bool,
    /// Newest N done/cancelled by `closed_at`.
    pub done_limit: Option<u32>,
    /// Case-insensitive, in name or display id.
    pub text: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct NewTask {
    pub project: String,
    pub title: Option<String>,
    pub body: String,
    pub kind: Option<Kind>,
    pub priority: Priority,
    /// Default Triage.
    pub status: Option<Status>,
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
pub(crate) enum Ruling {
    Choice(String),
    Text(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Refusal {
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug)]
pub(crate) enum StoreError {
    Sqlite(rusqlite::Error),
    /// SQLITE_BUSY after busy_timeout.
    Busy,
    /// Display id or row id as text.
    NotFound(String),
    Refused(Refusal),
    /// Bad input, message for the user.
    Invalid(String),
    /// Schema newer than this binary.
    TooNew {
        found: i64,
        known: i64,
    },
}

impl StoreError {
    /// Short machine-readable name, used as `OpResult.code`.
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Sqlite(_) => "error",
            Self::Busy => "busy",
            Self::NotFound(_) => "not_found",
            Self::Refused(refusal) => refusal.code,
            Self::Invalid(_) => "invalid",
            Self::TooNew { .. } => "too_new",
        }
    }

    pub(crate) fn refused(code: &'static str, message: impl Into<String>) -> StoreError {
        Self::Refused(Refusal {
            code,
            message: message.into(),
        })
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(err) => write!(f, "tasks db error: {err}"),
            Self::Busy => f.write_str("tasks db busy"),
            Self::NotFound(id) => write!(f, "no task {id}"),
            Self::Refused(refusal) => f.write_str(&refusal.message),
            Self::Invalid(message) => f.write_str(message),
            Self::TooNew { found, known } => write!(
                f,
                "tasks db is version {found}; this drovr knows {known}. Update drovr."
            ),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(err: rusqlite::Error) -> Self {
        match err.sqlite_error_code() {
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
                Self::Busy
            }
            _ => Self::Sqlite(err),
        }
    }
}

pub(crate) type StoreResult<T> = Result<T, StoreError>;

/// Writes. Runs `f` with the process-wide store, opening (and creating)
/// `default_path()` with busy_ms = 250 on first use, then `backup_daily()`.
/// Under cfg(test) the store is a thread-local `TaskStore::open_in_memory()`.
pub(crate) fn with_store<R>(f: impl FnOnce(&TaskStore) -> StoreResult<R>) -> StoreResult<R> {
    handle::with(true, f)?.ok_or_else(|| StoreError::Invalid("tasks db not open".into()))
}

/// Reads. Same handle, but when the file does not exist yet it returns
/// `Ok(R::default())` without creating it.
pub(crate) fn read_store<R: Default>(
    f: impl FnOnce(&TaskStore) -> StoreResult<R>,
) -> StoreResult<R> {
    handle::with(false, f).map(Option::unwrap_or_default)
}

#[cfg(test)]
mod handle {
    use super::{StoreResult, TaskStore};
    use std::cell::RefCell;

    thread_local! {
        static STORE: RefCell<Option<TaskStore>> = const { RefCell::new(None) };
    }

    pub(super) fn with<R>(
        _create: bool,
        f: impl FnOnce(&TaskStore) -> StoreResult<R>,
    ) -> StoreResult<Option<R>> {
        STORE.with(|cell| {
            let mut slot = cell.borrow_mut();
            if slot.is_none() {
                *slot = Some(TaskStore::open_in_memory()?);
            }
            let store = slot.as_ref().expect("store opened above");
            f(store).map(Some)
        })
    }
}

#[cfg(not(test))]
mod handle {
    use super::{StoreError, StoreResult, TaskStore};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    const RETRY: Duration = Duration::from_secs(10);

    #[derive(Default)]
    struct Handle {
        store: Option<TaskStore>,
        /// Last open failure: when, and the error to return until RETRY.
        failed: Option<(Instant, Failure)>,
        /// Day (yyyymmdd) of the last daily backup attempt.
        backed_up: Option<String>,
    }

    #[derive(Clone)]
    enum Failure {
        TooNew { found: i64, known: i64 },
        Other(String),
    }

    impl Failure {
        fn of(err: &StoreError) -> Failure {
            match err {
                StoreError::TooNew { found, known } => Failure::TooNew {
                    found: *found,
                    known: *known,
                },
                other => Failure::Other(other.to_string()),
            }
        }

        fn error(&self) -> StoreError {
            match self {
                Failure::TooNew { found, known } => StoreError::TooNew {
                    found: *found,
                    known: *known,
                },
                Failure::Other(message) => StoreError::Invalid(message.clone()),
            }
        }
    }

    fn handle() -> &'static Mutex<Handle> {
        static HANDLE: OnceLock<Mutex<Handle>> = OnceLock::new();
        HANDLE.get_or_init(Mutex::default)
    }

    pub(super) fn with<R>(
        create: bool,
        f: impl FnOnce(&TaskStore) -> StoreResult<R>,
    ) -> StoreResult<Option<R>> {
        let mut guard = handle().lock().unwrap_or_else(|err| err.into_inner());
        if guard.store.is_none() {
            if let Some((at, failure)) = &guard.failed {
                if at.elapsed() < RETRY {
                    return Err(failure.error());
                }
            }
            let path = TaskStore::default_path();
            let opened = if create {
                TaskStore::open(&path, 250).map(Some)
            } else {
                TaskStore::open_existing(&path, 250)
            };
            match opened {
                Ok(Some(store)) => {
                    guard.store = Some(store);
                    guard.failed = None;
                }
                Ok(None) => return Ok(None),
                Err(StoreError::Busy) => return Err(StoreError::Busy),
                Err(err) => {
                    guard.failed = Some((Instant::now(), Failure::of(&err)));
                    return Err(err);
                }
            }
        }
        let handle = &mut *guard;
        let Some(store) = handle.store.as_ref() else {
            return Ok(None);
        };
        if create {
            let today = super::now_text()
                .get(..10)
                .unwrap_or_default()
                .replace('-', "");
            if handle.backed_up.as_deref() != Some(today.as_str()) {
                handle.backed_up = Some(today);
                if let Err(err) = store.backup_daily() {
                    tracing::warn!(%err, "tasks db daily backup failed");
                }
            }
        }
        f(store).map(Some)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;

    /// A unique empty directory under the system temp dir, removed on drop.
    pub(crate) struct TempDir(pub(crate) PathBuf);

    impl TempDir {
        pub(crate) fn new(label: &str) -> TempDir {
            use std::sync::atomic::{AtomicU32, Ordering};
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "drovr-tasks-{label}-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }

        pub(crate) fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_parse_accepts_doing_and_every_name() {
        assert_eq!(Status::parse("doing"), Some(Status::Working));
        for status in Status::LANES.into_iter().chain([Status::Cancelled]) {
            assert_eq!(Status::parse(status.as_str()), Some(status));
        }
        assert_eq!(Status::parse("nope"), None);
        assert_eq!(Outcome::parse("needs_human"), Some(Outcome::NeedsHuman));
    }

    #[test]
    fn now_text_has_whole_seconds() {
        let now = now_text();
        assert_eq!(now.len(), 20, "{now}");
        assert!(now.ends_with('Z'));
    }

    #[test]
    fn cut_text_respects_char_boundaries() {
        assert_eq!(cut_text("abc", 5), "abc");
        assert_eq!(cut_text("ééé", 3), "é…");
    }

    #[test]
    fn task_name_falls_back_to_body_then_untitled() {
        let mut task = Task {
            id: 1,
            project_id: 1,
            number: 1,
            display_id: "AC-1".into(),
            title: None,
            body: "\n  first line\nsecond".into(),
            status: Status::Triage,
            kind: None,
            priority: Priority::Normal,
            executor: None,
            workspace_key: None,
            auto_status: true,
            position: 1024.0,
            version: 1,
            status_since: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            closed_at: None,
            archived_at: None,
        };
        assert_eq!(task.name(), "first line");
        task.body.clear();
        assert_eq!(task.name(), "Untitled");
        task.title = Some("Title".into());
        assert_eq!(task.name(), "Title");
    }

    #[test]
    fn with_store_and_read_store_share_the_test_store() {
        let task = with_store(|store| {
            store.create_task(
                &NewTask {
                    project: "Shared".into(),
                    title: Some("one".into()),
                    ..NewTask::default()
                },
                &Actor::Human,
            )
        })
        .unwrap();
        let found = read_store(|store| store.task(&task.display_id)).unwrap();
        assert_eq!(found.unwrap().id, task.id);
    }
}
