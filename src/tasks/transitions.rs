//! Who may move a task where, the criteria gate, and the refusal texts
//! (docs/design/tasks.md section 2.6).

use super::{Actor, CheckState, Criterion, Status, StoreError};

/// Moves an agent (the CLI in a pane) or the client's signal sync may make.
/// Humans may make any move.
pub(crate) const AGENT_MOVES: &[(Status, Status)] = &[
    (Status::Triage, Status::Working),
    (Status::Ready, Status::Working),
    (Status::Working, Status::Blocked),
    (Status::Blocked, Status::Working),
    (Status::Working, Status::Review),
    (Status::Working, Status::Ready),
];

/// Checks a move for `actor`. Does not check the gate (see [`gate`]) and
/// does not treat a move to the current status (a no-op for the caller).
pub(crate) fn check_move(
    from: Status,
    to: Status,
    actor: &Actor,
    note: Option<&str>,
) -> Result<(), StoreError> {
    match actor {
        Actor::Human => {
            let has_note = note.is_some_and(|note| !note.trim().is_empty());
            if from == Status::Review && to == Status::Ready && !has_note {
                return Err(StoreError::refused(
                    "note_required",
                    "sending a task back needs a note",
                ));
            }
            Ok(())
        }
        Actor::Agent(_) | Actor::Auto => {
            if AGENT_MOVES.contains(&(from, to)) {
                Ok(())
            } else {
                Err(not_allowed(from, to))
            }
        }
    }
}

pub(crate) fn not_allowed(from: Status, to: Status) -> StoreError {
    StoreError::refused(
        "not_allowed",
        format!(
            "a task in {} does not move to {} from an agent",
            from.as_str(),
            to.as_str()
        ),
    )
}

/// Result of the criteria gate: failed and open criterion positions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Gate {
    pub failed: Vec<i64>,
    pub open: Vec<i64>,
}

impl Gate {
    pub(crate) fn passes(&self) -> bool {
        self.failed.is_empty() && self.open.is_empty()
    }

    /// The refusal for a closed gate; failed criteria are reported first.
    pub(crate) fn refusal(&self) -> Option<StoreError> {
        if !self.failed.is_empty() {
            return Some(StoreError::refused(
                "criteria_failed",
                format!("criteria {} failed", list(&self.failed)),
            ));
        }
        if !self.open.is_empty() {
            return Some(StoreError::refused(
                "criteria_open",
                format!("criteria {} have no verdict", list(&self.open)),
            ));
        }
        None
    }
}

/// Passes when every criterion is passed. A task with no criteria passes.
pub(crate) fn gate(criteria: &[Criterion]) -> Gate {
    let mut gate = Gate::default();
    for criterion in criteria {
        match criterion.state {
            CheckState::Passed => {}
            CheckState::Failed => gate.failed.push(criterion.position),
            CheckState::Open => gate.open.push(criterion.position),
        }
    }
    gate
}

/// "2, 4"
pub(crate) fn list(positions: &[i64]) -> String {
    positions
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn decision_open(display_id: &str) -> StoreError {
    StoreError::refused(
        "decision_open",
        format!("{display_id} already has an open decision"),
    )
}

pub(crate) fn no_attempt(display_id: &str) -> StoreError {
    StoreError::refused("no_attempt", format!("{display_id} has no open attempt"))
}

pub(crate) fn project_exists(name: &str) -> StoreError {
    StoreError::refused(
        "project_exists",
        format!("a project named {name} already exists"),
    )
}

pub(crate) fn stale(display_id: &str) -> StoreError {
    StoreError::refused("stale", format!("{display_id} changed since you opened it"))
}

pub(crate) fn decision_ruled() -> StoreError {
    StoreError::refused("decision_ruled", "that decision was already answered")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn criterion(position: i64, state: CheckState) -> Criterion {
        Criterion {
            id: position,
            task_id: 1,
            position,
            text: format!("c{position}"),
            check_cmd: None,
            state,
            evidence: None,
            checked_by: None,
            checked_at: None,
        }
    }

    #[test]
    fn agent_and_auto_moves_follow_the_table() {
        let agent = Actor::Agent("claude@local".into());
        for &(from, to) in AGENT_MOVES {
            assert!(
                check_move(from, to, &agent, None).is_ok(),
                "{from:?}->{to:?}"
            );
            assert!(check_move(from, to, &Actor::Auto, None).is_ok());
        }
        for to in [Status::Done, Status::Cancelled] {
            let err = check_move(Status::Review, to, &agent, None).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!(
                    "a task in review does not move to {} from an agent",
                    to.as_str()
                )
            );
        }
        assert!(check_move(Status::Triage, Status::Done, &Actor::Human, None).is_ok());
    }

    #[test]
    fn send_back_needs_a_note() {
        let err = check_move(Status::Review, Status::Ready, &Actor::Human, None).unwrap_err();
        assert!(matches!(err, StoreError::Refused(ref r) if r.code == "note_required"));
        assert!(check_move(Status::Review, Status::Ready, &Actor::Human, Some(" ")).is_err());
        assert!(check_move(Status::Review, Status::Ready, &Actor::Human, Some("fix")).is_ok());
    }

    #[test]
    fn gate_reports_failed_before_open() {
        assert!(gate(&[]).passes());
        let criteria = [
            criterion(1, CheckState::Passed),
            criterion(2, CheckState::Open),
            criterion(3, CheckState::Failed),
            criterion(4, CheckState::Open),
        ];
        let result = gate(&criteria);
        assert_eq!(result.failed, vec![3]);
        assert_eq!(result.open, vec![2, 4]);
        assert_eq!(result.refusal().unwrap().to_string(), "criteria 3 failed");
        let open_only = gate(&criteria[..2]);
        assert_eq!(
            open_only.refusal().unwrap().to_string(),
            "criteria 2 have no verdict"
        );
    }
}
