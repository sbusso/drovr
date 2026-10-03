//! Task operations shared by the local CLI, the outbox and the client
//! (docs/design/tasks.md section 3.5).
//!
//! `TaskOp` is append-only once the outbox format ships: a new variant or
//! field is added with `#[serde(default)]`; nothing is renamed or removed. A
//! new variant needs `OUTBOX_V` to go up.

use serde::{Deserialize, Serialize};

use super::{
    Actor, ArtifactKind, CheckState, Choice, Kind, Outcome, Priority, Status, StoreError,
    StoreResult, Task,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum TaskOp {
    Add {
        project: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        body: String,
        #[serde(default)]
        kind: Option<Kind>,
        #[serde(default)]
        priority: Option<Priority>,
        #[serde(default)]
        criteria: Vec<String>,
    },
    Update {
        #[serde(default)]
        task: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        kind: Option<Kind>,
        #[serde(default)]
        priority: Option<Priority>,
    },
    Status {
        #[serde(default)]
        task: Option<String>,
        to: Status,
        #[serde(default)]
        note: Option<String>,
    },
    /// CLI default harness: $DROVR_AGENT, else "claude".
    Start {
        #[serde(default)]
        task: Option<String>,
        harness: String,
        #[serde(default)]
        session_id: Option<String>,
    },
    Note {
        #[serde(default)]
        task: Option<String>,
        body: String,
    },
    Criteria {
        #[serde(default)]
        task: Option<String>,
        #[serde(default)]
        set: Vec<String>,
        #[serde(default)]
        add: Vec<String>,
    },
    Check {
        #[serde(default)]
        task: Option<String>,
        position: i64,
        state: CheckState,
        #[serde(default)]
        evidence: Option<String>,
    },
    Artifact {
        #[serde(default)]
        task: Option<String>,
        kind: ArtifactKind,
        title: String,
        target: String,
        #[serde(default)]
        summary: Option<String>,
    },
    Done {
        #[serde(default)]
        task: Option<String>,
        outcome: Outcome,
        #[serde(default)]
        note: Option<String>,
    },
    Release {
        #[serde(default)]
        task: Option<String>,
        note: String,
    },
    Decide {
        #[serde(default)]
        task: Option<String>,
        title: String,
        #[serde(default)]
        summary: String,
        choices: Vec<Choice>,
        #[serde(default)]
        default_choice: Option<String>,
        #[serde(default = "default_true")]
        allow_text: bool,
        #[serde(default)]
        expires_at: Option<String>,
        /// `--wait`: seconds the CLI waits; the store sets wait_until.
        #[serde(default)]
        wait_secs: Option<u32>,
    },
    Withdraw {
        #[serde(default)]
        task: Option<String>,
    },
}

fn default_true() -> bool {
    true
}

impl TaskOp {
    /// The task id the op names, if any.
    pub(crate) fn task(&self) -> Option<&str> {
        match self {
            Self::Add { .. } => None,
            Self::Update { task, .. }
            | Self::Status { task, .. }
            | Self::Start { task, .. }
            | Self::Note { task, .. }
            | Self::Criteria { task, .. }
            | Self::Check { task, .. }
            | Self::Artifact { task, .. }
            | Self::Done { task, .. }
            | Self::Release { task, .. }
            | Self::Decide { task, .. }
            | Self::Withdraw { task } => task.as_deref(),
        }
    }

    /// Fills the task id when the op has none.
    pub(crate) fn set_task(&mut self, id: String) {
        match self {
            Self::Add { .. } => {}
            Self::Update { task, .. }
            | Self::Status { task, .. }
            | Self::Start { task, .. }
            | Self::Note { task, .. }
            | Self::Criteria { task, .. }
            | Self::Check { task, .. }
            | Self::Artifact { task, .. }
            | Self::Done { task, .. }
            | Self::Release { task, .. }
            | Self::Decide { task, .. }
            | Self::Withdraw { task } => {
                if task.is_none() {
                    *task = Some(id);
                }
            }
        }
    }
}

/// Outbox format version; the client accepts ops with `v <= OUTBOX_V`.
pub(crate) const OUTBOX_V: u32 = 1;

pub(crate) struct OpContext {
    pub actor: Actor,
    /// "local", "mato"
    pub machine: String,
    /// "machine/pane_id"
    pub pane_key: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct OpResult {
    pub ok: bool,
    /// Display id.
    pub task: Option<String>,
    pub status: Option<Status>,
    /// One line for the CLI: `{id} {status}  {what}`, or the error.
    pub message: String,
    /// Refusal code when ok = false ("not_found", "invalid", a refusal code,
    /// "busy", "error", "too_new").
    pub code: Option<String>,
    pub decision_id: Option<i64>,
}

impl OpResult {
    pub(crate) fn ok(task: &Task, what: &str) -> OpResult {
        OpResult {
            ok: true,
            task: Some(task.display_id.clone()),
            status: Some(task.status),
            message: format!("{} {}  {what}", task.display_id, task.status.as_str()),
            code: None,
            decision_id: None,
        }
    }

    pub(crate) fn failed(err: &StoreError, task: Option<&str>) -> OpResult {
        OpResult {
            ok: false,
            task: task.map(str::to_owned),
            status: None,
            message: err.to_string(),
            code: Some(err.code().to_owned()),
            decision_id: None,
        }
    }

    /// The CLI exit code of this result (see [`exit_code`]).
    pub(crate) fn exit(&self) -> i32 {
        if self.ok {
            return 0;
        }
        match self.code.as_deref() {
            Some("not_found") => 4,
            Some("invalid") => 2,
            Some("busy" | "error" | "too_new") | None => 1,
            Some(_) => 3,
        }
    }

    /// Ok results as they are; errors as `ok = false` results.
    pub(crate) fn of(result: &StoreResult<OpResult>, task: Option<&str>) -> OpResult {
        match result {
            Ok(result) => result.clone(),
            Err(err) => OpResult::failed(err, task),
        }
    }
}

/// One outbox file (section 6.3). `epoch` names the outbox directory's
/// counter; `source` for `apply_once` is "{machine}/{pane}/{epoch}".
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct OutboxOp {
    pub v: u32,
    pub epoch: String,
    pub seq: u64,
    pub ts: u64,
    pub pane: String,
    pub op: TaskOp,
}

/// Why an outbox line was not parsed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OutboxParse {
    /// Written by a newer drovr (`v` above `OUTBOX_V`): leave the file.
    Newer(u32),
    /// Not an op this format knows: move the file aside.
    Bad(String),
}

/// The two-step parse of section 6.3: `{"v": u32}` first, then the op.
pub(crate) fn parse_outbox_line(line: &str) -> Result<OutboxOp, OutboxParse> {
    #[derive(Deserialize)]
    struct Version {
        v: u32,
    }
    let version: Version =
        serde_json::from_str(line).map_err(|err| OutboxParse::Bad(err.to_string()))?;
    if version.v > OUTBOX_V {
        return Err(OutboxParse::Newer(version.v));
    }
    serde_json::from_str(line).map_err(|err| OutboxParse::Bad(err.to_string()))
}

/// `apply` result to CLI exit code: ok 0; Refused 3; NotFound 4; Invalid 2;
/// Busy, Sqlite, TooNew 1. The outbox reply carries the code.
pub(crate) fn exit_code(result: &StoreResult<OpResult>) -> i32 {
    match result {
        Ok(result) => result.exit(),
        Err(StoreError::Refused(_)) => 3,
        Err(StoreError::NotFound(_)) => 4,
        Err(StoreError::Invalid(_)) => 2,
        Err(StoreError::Busy | StoreError::Sqlite(_) | StoreError::TooNew { .. }) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXED: &str = r#"{"v":1,"epoch":"k3f9q2","seq":7,"ts":1791100000,"pane":"p12","op":{"op":"check","task":"AC-12","position":2,"state":"passed","evidence":"cargo test: 41 passed"}}"#;

    fn every_op() -> Vec<TaskOp> {
        let task = Some("AC-1".to_owned());
        vec![
            TaskOp::Add {
                project: "Acme".into(),
                title: Some("t".into()),
                body: "b".into(),
                kind: Some(Kind::Fix),
                priority: Some(Priority::High),
                criteria: vec!["c".into()],
            },
            TaskOp::Update {
                task: task.clone(),
                title: Some("t".into()),
                body: None,
                kind: Some(Kind::Spec),
                priority: None,
            },
            TaskOp::Status {
                task: task.clone(),
                to: Status::Review,
                note: Some("n".into()),
            },
            TaskOp::Start {
                task: task.clone(),
                harness: "claude".into(),
                session_id: Some("s".into()),
            },
            TaskOp::Note {
                task: task.clone(),
                body: "hi".into(),
            },
            TaskOp::Criteria {
                task: task.clone(),
                set: vec!["a".into()],
                add: vec!["b".into()],
            },
            TaskOp::Check {
                task: task.clone(),
                position: 1,
                state: CheckState::Failed,
                evidence: Some("e".into()),
            },
            TaskOp::Artifact {
                task: task.clone(),
                kind: ArtifactKind::Doc,
                title: "t".into(),
                target: "/x.md".into(),
                summary: None,
            },
            TaskOp::Done {
                task: task.clone(),
                outcome: Outcome::NeedsHuman,
                note: None,
            },
            TaskOp::Release {
                task: task.clone(),
                note: "n".into(),
            },
            TaskOp::Decide {
                task: task.clone(),
                title: "t".into(),
                summary: "s".into(),
                choices: vec![Choice {
                    id: "a".into(),
                    label: "A".into(),
                    consequence: Some("c".into()),
                    recommended: true,
                }],
                default_choice: Some("a".into()),
                allow_text: false,
                expires_at: None,
                wait_secs: Some(30),
            },
            TaskOp::Withdraw { task },
        ]
    }

    #[test]
    fn every_op_round_trips() {
        for op in every_op() {
            let json = serde_json::to_string(&op).unwrap();
            let back: TaskOp = serde_json::from_str(&json).unwrap();
            assert_eq!(back, op, "{json}");
        }
    }

    #[test]
    fn the_fixed_line_parses() {
        let parsed = parse_outbox_line(FIXED).unwrap();
        assert_eq!(parsed.epoch, "k3f9q2");
        assert_eq!(parsed.seq, 7);
        assert_eq!(parsed.pane, "p12");
        assert_eq!(
            parsed.op,
            TaskOp::Check {
                task: Some("AC-12".into()),
                position: 2,
                state: CheckState::Passed,
                evidence: Some("cargo test: 41 passed".into()),
            }
        );
    }

    #[test]
    fn a_newer_version_is_rejected_before_the_op_is_read() {
        let line = FIXED.replace(r#""v":1"#, &format!(r#""v":{}"#, OUTBOX_V + 1));
        assert_eq!(parse_outbox_line(&line).unwrap_err(), OutboxParse::Newer(2));
        // An op this binary does not know, under a newer v, is still "newer".
        let unknown = r#"{"v":2,"epoch":"e","seq":1,"ts":1,"pane":"p","op":{"op":"teleport"}}"#;
        assert_eq!(
            parse_outbox_line(unknown).unwrap_err(),
            OutboxParse::Newer(2)
        );
        let bad = r#"{"v":1,"epoch":"e","seq":1,"ts":1,"pane":"p","op":{"op":"teleport"}}"#;
        assert!(matches!(parse_outbox_line(bad), Err(OutboxParse::Bad(_))));
        assert!(matches!(
            parse_outbox_line("nope"),
            Err(OutboxParse::Bad(_))
        ));
    }

    #[test]
    fn exit_codes_follow_the_error_kind() {
        let refused: StoreResult<OpResult> = Err(StoreError::refused("stale", "x"));
        assert_eq!(exit_code(&refused), 3);
        assert_eq!(exit_code(&Err(StoreError::NotFound("AC-1".into()))), 4);
        assert_eq!(exit_code(&Err(StoreError::Invalid("x".into()))), 2);
        assert_eq!(exit_code(&Err(StoreError::Busy)), 1);
        assert_eq!(
            exit_code(&Err(StoreError::TooNew { found: 2, known: 1 })),
            1
        );
        for err in [
            StoreError::refused("stale", "x"),
            StoreError::NotFound("AC-1".into()),
            StoreError::Invalid("x".into()),
            StoreError::Busy,
        ] {
            let result = OpResult::failed(&err, None);
            assert_eq!(result.exit(), exit_code(&Err(err)));
        }
    }
}
