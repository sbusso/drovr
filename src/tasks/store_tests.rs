use super::*;
use crate::tasks::ops::{OpContext, TaskOp};
use crate::tasks::test_support::TempDir;
use crate::tasks::{Kind, Priority};

fn store() -> TaskStore {
    TaskStore::open_in_memory().unwrap()
}

fn agent() -> Actor {
    Actor::Agent("claude@local".into())
}

fn add(store: &TaskStore, project: &str, title: &str) -> Task {
    store
        .create_task(
            &NewTask {
                project: project.into(),
                title: Some(title.into()),
                ..NewTask::default()
            },
            &Actor::Human,
        )
        .unwrap()
}

fn add_with(store: &TaskStore, title: &str, status: Status, criteria: &[&str]) -> Task {
    store
        .create_task(
            &NewTask {
                project: "Acme".into(),
                title: Some(title.into()),
                status: Some(status),
                criteria: criteria.iter().map(|c| c.to_string()).collect(),
                ..NewTask::default()
            },
            &Actor::Human,
        )
        .unwrap()
}

fn attempt(machine: &str, pane: &str) -> NewAttempt {
    NewAttempt {
        harness: "claude".into(),
        machine: machine.into(),
        workspace_key: Some(format!("{machine}/w1:AC-1 task")),
        pane_key: Some(format!("{machine}/{pane}")),
        session_id: None,
    }
}

fn choices(n: usize) -> Vec<Choice> {
    (0..n)
        .map(|i| Choice {
            id: format!("c{i}"),
            label: format!("Choice {i}"),
            consequence: None,
            recommended: false,
        })
        .collect()
}

fn decision(title: &str, choices: Vec<Choice>) -> NewDecision {
    NewDecision {
        title: title.into(),
        summary: String::new(),
        choices,
        allow_text: true,
        default_choice: None,
        expires_at: None,
        wait_until: None,
    }
}

fn code(err: StoreError) -> &'static str {
    match err {
        StoreError::Refused(refusal) => refusal.code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn entries(store: &TaskStore, id: &str) -> Vec<Entry> {
    store.task_detail(id).unwrap().unwrap().entries
}

#[test]
fn key_derivation() {
    assert_eq!(derive_key("Infrastructure"), "INF");
    assert_eq!(derive_key("Outsmartis ops"), "OO");
    assert_eq!(derive_key("a b c d e"), "ABCD");
    assert_eq!(derive_key("42 things"), "P4T");
    assert_eq!(derive_key("9lives"), "P9LI");
    assert_eq!(derive_key("x"), "XX");
    assert_eq!(derive_key("drovr-main"), "DM");
    assert_eq!(derive_key("Bob's app"), "BA");
    assert_eq!(derive_key("日本"), "XX");
    let store = store();
    assert_eq!(store.ensure_project("Outsmartis ops").unwrap().key, "OO");
    assert_eq!(store.ensure_project("Other Org").unwrap().key, "OO2");
    assert_eq!(store.ensure_project("Old Office").unwrap().key, "OO3");
    assert_eq!(store.ensure_project("Outsmartis ops").unwrap().key, "OO");
}

#[test]
fn ensure_project_refuses_empty_and_other() {
    let store = store();
    assert!(matches!(
        store.ensure_project(""),
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store.ensure_project("\0other"),
        Err(StoreError::Invalid(_))
    ));
    assert!(store.projects().unwrap().is_empty());
}

#[test]
fn create_numbers_and_positions() {
    let store = store();
    let one = add(&store, "Acme", "first");
    let two = add(&store, "Acme", "second");
    assert_eq!((one.number, two.number), (1, 2));
    assert_eq!(one.display_id, "ACM-1");
    assert_eq!(two.display_id, "ACM-2");
    assert!(two.position > one.position);
    assert_eq!(one.status, Status::Triage);
    assert_eq!(one.version, 1);
    assert_eq!(store.project("Acme").unwrap().unwrap().next_number, 3);
    assert!(
        store.task("acm-1").unwrap().is_some(),
        "ids match case-insensitively"
    );
    assert!(matches!(
        store.create_task(
            &NewTask {
                project: "Acme".into(),
                ..NewTask::default()
            },
            &Actor::Human
        ),
        Err(StoreError::Invalid(_))
    ));
}

#[test]
fn criteria_take_a_check_command() {
    let store = store();
    let task = add_with(
        &store,
        "t",
        Status::Ready,
        &["tests pass (check: `cargo test`)", "docs updated"],
    );
    let detail = store.task_detail(&task.display_id).unwrap().unwrap();
    assert_eq!(detail.criteria[0].text, "tests pass");
    assert_eq!(detail.criteria[0].check_cmd.as_deref(), Some("cargo test"));
    assert_eq!(detail.criteria[1].check_cmd, None);
    assert_eq!(detail.criteria[1].position, 2);
    let added = store
        .add_criterion(
            &task.display_id,
            "lint (check: cargo clippy)",
            &Actor::Human,
        )
        .unwrap();
    assert_eq!(added.position, 3);
    assert_eq!(added.check_cmd.as_deref(), Some("cargo clippy"));
    let set = store
        .set_criteria(&task.display_id, &["only".into()], &Actor::Human)
        .unwrap();
    assert_eq!(set.len(), 1);
    assert_eq!(set[0].position, 1);
}

#[test]
fn rename_keeps_display_ids_and_refuses_an_existing_name() {
    let store = store();
    let task = add(&store, "Acme", "t");
    add(&store, "Beta", "u");
    store.rename_project("Acme", "Acme Corp").unwrap();
    let renamed = store.project("Acme Corp").unwrap().unwrap();
    assert_eq!(renamed.key, "ACM");
    assert!(store.project("Acme").unwrap().is_none());
    assert_eq!(
        store.task(&task.display_id).unwrap().unwrap().display_id,
        "ACM-1"
    );
    let err = store.rename_project("Acme Corp", "Beta").unwrap_err();
    assert_eq!(err.to_string(), "a project named Beta already exists");
    assert_eq!(code(err), "project_exists");
    store.rename_project("Nope", "Gamma").unwrap();
}

#[test]
fn update_with_a_stale_version_is_refused() {
    let store = store();
    let task = add(&store, "Acme", "t");
    let patch = |body: &str, version| TaskPatch {
        body: Some(body.into()),
        expected_version: Some(version),
        ..TaskPatch::default()
    };
    let saved = store
        .update_task(
            &task.display_id,
            &patch("new body", task.version),
            &Actor::Human,
        )
        .unwrap();
    assert_eq!(saved.body, "new body");
    assert_eq!(saved.version, task.version + 1);
    let err = store
        .update_task(
            &task.display_id,
            &patch("lost", task.version),
            &Actor::Human,
        )
        .unwrap_err();
    assert_eq!(err.to_string(), "ACM-1 changed since you opened it");
    assert_eq!(code(err), "stale");
    assert_eq!(store.task("ACM-1").unwrap().unwrap().body, "new body");
    // The same patch again is a no-op.
    let same = store
        .update_task(
            &task.display_id,
            &patch("new body", saved.version),
            &Actor::Human,
        )
        .unwrap();
    assert_eq!(same.version, saved.version);
    let archived = store
        .update_task(
            &task.display_id,
            &TaskPatch {
                archived: Some(true),
                kind: Some(Some(Kind::Fix)),
                priority: Some(Priority::Urgent),
                title: Some(None),
                ..TaskPatch::default()
            },
            &Actor::Human,
        )
        .unwrap();
    assert!(archived.archived_at.is_some());
    assert_eq!(archived.title, None);
    assert_eq!(archived.name(), "new body");
}

#[test]
fn a_move_to_the_current_status_is_a_noop() {
    let store = store();
    let task = add(&store, "Acme", "t");
    let before = entries(&store, &task.display_id).len();
    let same = store
        .move_task(&task.display_id, Status::Triage, &Actor::Human, None)
        .unwrap();
    assert_eq!(same.version, task.version);
    assert_eq!(entries(&store, &task.display_id).len(), before);
}

#[test]
fn human_moves_write_events_and_turn_auto_off() {
    let store = store();
    let task = add(&store, "Acme", "t");
    let moved = store
        .move_task(&task.display_id, Status::Ready, &Actor::Human, Some("go"))
        .unwrap();
    assert_eq!(moved.status, Status::Ready);
    assert!(!moved.auto_status);
    assert_eq!(moved.version, task.version + 1);
    let last = entries(&store, &task.display_id).pop().unwrap();
    assert_eq!(last.body, "triage → ready: go");
    assert_eq!(last.event_type.as_deref(), Some("status"));
    assert_eq!(last.author, "you");
}

#[test]
fn human_close_ends_the_attempt_and_withdraws_the_decision() {
    let store = store();
    let working = add_with(&store, "working", Status::Ready, &[]);
    store
        .start_attempt(&working.display_id, &attempt("local", "p2"), &Actor::Human)
        .unwrap();
    store
        .request_decision(
            &working.display_id,
            &decision("Which?", choices(2)),
            &agent(),
        )
        .unwrap();
    store
        .move_task(&working.display_id, Status::Cancelled, &Actor::Human, None)
        .unwrap();
    let detail = store.task_detail(&working.display_id).unwrap().unwrap();
    assert_eq!(detail.task.status, Status::Cancelled);
    assert!(detail.task.closed_at.is_some());
    assert_eq!(detail.attempts[0].outcome, Some(Outcome::Stopped));
    assert_eq!(detail.attempts[0].note.as_deref(), Some("closed by you"));
    assert_eq!(detail.decision.unwrap().state, DecisionState::Withdrawn);
}

#[test]
fn closing_from_review_counts_as_succeeded() {
    let store = store();
    let task = add_with(&store, "t", Status::Ready, &[]);
    store
        .start_attempt(&task.display_id, &attempt("local", "p1"), &Actor::Human)
        .unwrap();
    store
        .move_task(&task.display_id, Status::Review, &Actor::Human, None)
        .unwrap();
    let done = store
        .move_task(&task.display_id, Status::Done, &Actor::Human, None)
        .unwrap();
    assert!(done.closed_at.is_some());
    let detail = store.task_detail(&task.display_id).unwrap().unwrap();
    assert_eq!(detail.attempts[0].outcome, Some(Outcome::Succeeded));
    let reopened = store
        .move_task(&task.display_id, Status::Ready, &Actor::Human, None)
        .unwrap();
    assert_eq!(reopened.closed_at, None);
}

#[test]
fn agent_and_auto_moves() {
    let store = store();
    let task = add_with(&store, "t", Status::Ready, &[]);
    let id = task.display_id.as_str();
    let moved = store
        .move_task(id, Status::Working, &agent(), None)
        .unwrap();
    assert_eq!(moved.status, Status::Working);
    assert!(moved.auto_status, "agent moves keep auto on");
    assert_eq!(
        entries(&store, id).pop().unwrap().body,
        "ready → working (claude@local)"
    );
    let err = store
        .move_task(id, Status::Done, &agent(), None)
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "a task in working does not move to done from an agent"
    );
    // Auto between working and blocked writes no entry.
    let count = entries(&store, id).len();
    store
        .move_task(id, Status::Blocked, &Actor::Auto, None)
        .unwrap();
    store
        .move_task(id, Status::Working, &Actor::Auto, None)
        .unwrap();
    assert_eq!(entries(&store, id).len(), count);
    // A human move turns auto off; auto moves are then skipped.
    store
        .move_task(id, Status::Blocked, &Actor::Human, None)
        .unwrap();
    let skipped = store
        .move_task(id, Status::Working, &Actor::Auto, None)
        .unwrap();
    assert_eq!(skipped.status, Status::Blocked);
    // Review -> ready from a human needs a note.
    store
        .move_task(id, Status::Review, &Actor::Human, None)
        .unwrap();
    assert_eq!(
        code(
            store
                .move_task(id, Status::Ready, &Actor::Human, None)
                .unwrap_err()
        ),
        "note_required"
    );
    store
        .move_task(id, Status::Ready, &Actor::Human, Some("tests fail"))
        .unwrap();
}

#[test]
fn auto_leaves_a_decision_block_alone() {
    let store = store();
    let task = add_with(&store, "t", Status::Ready, &[]);
    let id = task.display_id.as_str();
    store
        .start_attempt(id, &attempt("local", "p1"), &Actor::Human)
        .unwrap();
    store
        .request_decision(id, &decision("Which?", choices(2)), &agent())
        .unwrap();
    assert_eq!(store.task(id).unwrap().unwrap().status, Status::Blocked);
    let still = store
        .move_task(id, Status::Working, &Actor::Auto, None)
        .unwrap();
    assert_eq!(still.status, Status::Blocked);
}

#[test]
fn agent_review_goes_through_the_gate() {
    let store = store();
    let task = add_with(&store, "t", Status::Working, &["a", "b"]);
    let id = task.display_id.as_str();
    assert_eq!(
        code(
            store
                .move_task(id, Status::Review, &agent(), None)
                .unwrap_err()
        ),
        "criteria_open"
    );
    store
        .check_criterion(id, 1, CheckState::Passed, Some("ok"), &agent())
        .unwrap();
    store
        .check_criterion(id, 2, CheckState::Passed, None, &agent())
        .unwrap();
    store.move_task(id, Status::Review, &agent(), None).unwrap();
}

#[test]
fn finish_succeeded_checks_the_gate() {
    let store = store();
    let task = add_with(&store, "t", Status::Ready, &["one", "two", "three"]);
    let id = task.display_id.as_str();
    assert_eq!(
        code(
            store
                .finish_attempt(id, Outcome::Succeeded, None, &agent())
                .unwrap_err()
        ),
        "no_attempt"
    );
    store
        .start_attempt(id, &attempt("local", "p1"), &agent())
        .unwrap();
    store
        .check_criterion(id, 1, CheckState::Passed, Some("ok"), &agent())
        .unwrap();
    store
        .check_criterion(id, 3, CheckState::Failed, Some("no"), &agent())
        .unwrap();
    let err = store
        .finish_attempt(id, Outcome::Succeeded, None, &agent())
        .unwrap_err();
    assert_eq!(err.to_string(), "criteria 3 failed");
    store
        .check_criterion(id, 3, CheckState::Passed, Some("ok"), &agent())
        .unwrap();
    let err = store
        .finish_attempt(id, Outcome::Succeeded, None, &agent())
        .unwrap_err();
    assert_eq!(err.to_string(), "criteria 2 have no verdict");
    let detail = store.task_detail(id).unwrap().unwrap();
    assert!(
        detail.attempts[0].ended_at.is_none(),
        "the attempt stays open"
    );
    store
        .check_criterion(id, 2, CheckState::Passed, Some("ok"), &agent())
        .unwrap();
    let done = store
        .finish_attempt(id, Outcome::Succeeded, Some("all green"), &agent())
        .unwrap();
    assert_eq!(done.status, Status::Review);
    let detail = store.task_detail(id).unwrap().unwrap();
    assert_eq!(detail.attempts[0].outcome, Some(Outcome::Succeeded));
    assert_eq!(detail.attempts[0].note.as_deref(), Some("all green"));
    assert_eq!(
        detail.criteria[0].checked_by.as_deref(),
        Some("claude@local")
    );
}

#[test]
fn finish_without_criteria_passes_and_other_outcomes_route() {
    let store = store();
    for (outcome, to) in [
        (Outcome::Succeeded, Status::Review),
        (Outcome::Failed, Status::Ready),
        (Outcome::Stopped, Status::Ready),
        (Outcome::NeedsHuman, Status::Blocked),
    ] {
        let task = add_with(&store, "t", Status::Ready, &[]);
        store
            .start_attempt(&task.display_id, &attempt("local", "p"), &agent())
            .unwrap();
        let done = store
            .finish_attempt(&task.display_id, outcome, None, &agent())
            .unwrap();
        assert_eq!(done.status, to, "{outcome:?}");
    }
}

#[test]
fn one_open_attempt_per_task() {
    let store = store();
    let task = add_with(&store, "t", Status::Triage, &[]);
    let id = task.display_id.as_str();
    store
        .update_task(
            id,
            &TaskPatch {
                auto_status: Some(false),
                ..Default::default()
            },
            &Actor::Human,
        )
        .unwrap();
    let first = store
        .start_attempt(id, &attempt("local", "p1"), &Actor::Human)
        .unwrap();
    let after = store.task(id).unwrap().unwrap();
    assert_eq!(after.status, Status::Working);
    assert!(after.auto_status, "a start turns auto back on");
    assert_eq!(after.executor.as_deref(), Some("claude"));
    assert_eq!(after.workspace_key.as_deref(), Some("local/w1:AC-1 task"));
    assert_eq!(
        store.task_for_pane("local/p1").unwrap().unwrap().id,
        task.id
    );
    let second = store
        .start_attempt(id, &attempt("mato", "p9"), &Actor::Human)
        .unwrap();
    let detail = store.task_detail(id).unwrap().unwrap();
    assert_eq!(detail.attempts.len(), 2);
    assert_eq!(detail.attempts[0].id, second.id);
    assert_eq!(detail.attempts[1].id, first.id);
    assert_eq!(detail.attempts[1].outcome, Some(Outcome::Stopped));
    assert!(store.task_for_pane("local/p1").unwrap().is_none());
    assert_eq!(store.task_for_pane("mato/p9").unwrap().unwrap().id, task.id);
    let cards = store.list(&TaskFilter::default()).unwrap();
    assert_eq!(
        cards[0].live,
        Some(("claude".into(), "mato".into(), Some("mato/p9".into())))
    );
    assert_eq!(cards[0].last_outcome, Some(Outcome::Stopped));
}

#[test]
fn release_ends_the_attempt_and_readies_the_task() {
    let store = store();
    let task = add_with(&store, "t", Status::Ready, &[]);
    let id = task.display_id.as_str();
    store
        .start_attempt(id, &attempt("local", "p1"), &agent())
        .unwrap();
    assert!(matches!(
        store.release(id, " ", &agent()),
        Err(StoreError::Invalid(_))
    ));
    let released = store.release(id, "stuck on auth", &agent()).unwrap();
    assert_eq!(released.status, Status::Ready);
    let detail = store.task_detail(id).unwrap().unwrap();
    assert_eq!(detail.attempts[0].note.as_deref(), Some("stuck on auth"));
    assert_eq!(
        code(store.release(id, "again", &agent()).unwrap_err()),
        "no_attempt"
    );
}

#[test]
fn decisions() {
    let store = store();
    let task = add_with(&store, "t", Status::Ready, &[]);
    let id = task.display_id.as_str();
    store
        .start_attempt(id, &attempt("local", "p1"), &agent())
        .unwrap();
    for bad in [choices(0), choices(9)] {
        assert!(matches!(
            store.request_decision(id, &decision("Which?", bad), &agent()),
            Err(StoreError::Invalid(_))
        ));
    }
    let mut two_rec = choices(2);
    two_rec[0].recommended = true;
    two_rec[1].recommended = true;
    assert!(store
        .request_decision(id, &decision("Which?", two_rec), &agent())
        .is_err());
    let mut dup = choices(2);
    dup[1].id = "c0".into();
    assert!(store
        .request_decision(id, &decision("Which?", dup), &agent())
        .is_err());
    let long = "x".repeat(121);
    assert!(store
        .request_decision(id, &decision(&long, choices(2)), &agent())
        .is_err());

    let asked = store
        .request_decision(id, &decision("Which table?", choices(3)), &agent())
        .unwrap();
    assert_eq!(asked.state, DecisionState::Open);
    assert_eq!(store.task(id).unwrap().unwrap().status, Status::Blocked);
    assert_eq!(
        code(
            store
                .request_decision(id, &decision("Again?", choices(2)), &agent())
                .unwrap_err()
        ),
        "decision_open"
    );
    let open = store.open_decisions(Some("Acme")).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].display_id, id);
    assert_eq!(open[0].pane_key.as_deref(), Some("local/p1"));
    assert!(store.open_decisions(Some("Other")).unwrap().is_empty());
    let cards = store.list(&TaskFilter::default()).unwrap();
    assert!(cards[0].open_decision);

    assert!(store
        .rule_decision(
            asked.id,
            &Ruling::Choice("zz".into()),
            "panel",
            &Actor::Human
        )
        .is_err());
    let ruled = store
        .rule_decision(
            asked.id,
            &Ruling::Choice("c1".into()),
            "panel",
            &Actor::Human,
        )
        .unwrap();
    assert_eq!(ruled.state, DecisionState::Ruled);
    assert_eq!(ruled.ruling_line().as_deref(), Some("ruled c1: Choice 1"));
    assert_eq!(ruled.ruled_by.as_deref(), Some("you"));
    assert_eq!(store.task(id).unwrap().unwrap().status, Status::Working);
    let err = store
        .rule_decision(asked.id, &Ruling::Text("late".into()), "cli", &Actor::Human)
        .unwrap_err();
    assert_eq!(err.to_string(), "that decision was already answered");
    let bodies: Vec<String> = entries(&store, id).into_iter().map(|e| e.body).collect();
    assert!(bodies.contains(&"asked: Which table?".to_owned()));
    assert!(bodies.contains(&"answered: Choice 1 (panel)".to_owned()));

    // A ruling with no open attempt moves blocked -> ready.
    store
        .request_decision(id, &decision("Next?", choices(2)), &agent())
        .unwrap();
    let pending = store.task_detail(id).unwrap().unwrap().decision.unwrap();
    store.release(id, "handing over", &agent()).unwrap();
    assert_eq!(
        store.decision(pending.id).unwrap().unwrap().state,
        DecisionState::Withdrawn
    );
    store
        .move_task(id, Status::Blocked, &Actor::Human, None)
        .unwrap();
    let late = store
        .request_decision(id, &decision("Third?", choices(2)), &Actor::Human)
        .unwrap();
    store
        .rule_decision(
            late.id,
            &Ruling::Text("do both".into()),
            "cli",
            &Actor::Human,
        )
        .unwrap();
    assert_eq!(store.task(id).unwrap().unwrap().status, Status::Ready);
}

#[test]
fn expiry_with_and_without_a_default() {
    let store = store();
    let with = add_with(&store, "with", Status::Ready, &[]);
    let without = add_with(&store, "without", Status::Ready, &[]);
    let later = add_with(&store, "later", Status::Ready, &[]);
    let mut d = decision("Default?", choices(2));
    d.default_choice = Some("c1".into());
    d.expires_at = Some("2026-01-01T00:00:00Z".into());
    let a = store
        .request_decision(&with.display_id, &d, &agent())
        .unwrap();
    d.default_choice = None;
    let b = store
        .request_decision(&without.display_id, &d, &agent())
        .unwrap();
    d.expires_at = Some("2099-01-01T00:00:00Z".into());
    store
        .request_decision(&later.display_id, &d, &agent())
        .unwrap();
    let changed = store.expire_decisions("2026-06-01T00:00:00Z").unwrap();
    assert_eq!(changed, vec![a.id, b.id]);
    let a = store.decision(a.id).unwrap().unwrap();
    assert_eq!(a.state, DecisionState::Ruled);
    assert_eq!(a.ruling_choice.as_deref(), Some("c1"));
    assert_eq!(a.surface.as_deref(), Some("expiry"));
    assert_eq!(
        store.decision(b.id).unwrap().unwrap().state,
        DecisionState::Expired
    );
    assert_eq!(store.open_decisions(None).unwrap().len(), 1);
}

#[test]
fn decision_wait_until_is_set_and_cleared() {
    let store = store();
    let task = add_with(&store, "t", Status::Working, &[]);
    let d = store
        .request_decision(&task.display_id, &decision("W?", choices(2)), &agent())
        .unwrap();
    store
        .set_decision_wait(d.id, Some("2099-01-01T00:00:00Z"))
        .unwrap();
    assert!(store.decision(d.id).unwrap().unwrap().wait_until.is_some());
    store.set_decision_wait(d.id, None).unwrap();
    assert!(store.decision(d.id).unwrap().unwrap().wait_until.is_none());
}

#[test]
fn reorder_places_between_neighbours_and_renumbers() {
    let store = store();
    let a = add_with(&store, "a", Status::Ready, &[]);
    let b = add_with(&store, "b", Status::Ready, &[]);
    let c = add_with(&store, "c", Status::Ready, &[]);
    store
        .reorder(
            &c.display_id,
            Status::Ready,
            Some(&a.display_id),
            Some(&b.display_id),
        )
        .unwrap();
    let order = |store: &TaskStore| -> Vec<String> {
        store
            .list(&TaskFilter::default())
            .unwrap()
            .into_iter()
            .map(|card| card.task.name().to_owned())
            .collect()
    };
    assert_eq!(order(&store), vec!["a", "c", "b"]);
    // Squeeze a and b together so the gap is below 1e-6.
    store
        .conn()
        .execute_batch(&format!(
            "UPDATE tasks SET position = 5000.0 WHERE id = {};
             UPDATE tasks SET position = 5000.0000000001 WHERE id = {};
             UPDATE tasks SET position = 9000.0 WHERE id = {};",
            a.id, b.id, c.id
        ))
        .unwrap();
    store
        .reorder(
            &c.display_id,
            Status::Ready,
            Some(&a.display_id),
            Some(&b.display_id),
        )
        .unwrap();
    let positions: Vec<f64> = store
        .list(&TaskFilter::default())
        .unwrap()
        .into_iter()
        .map(|card| card.task.position)
        .collect();
    assert_eq!(order(&store), vec!["a", "c", "b"]);
    assert_eq!(positions, vec![1024.0, 1536.0, 2048.0]);
    // A neighbour from another lane is refused.
    let other = add_with(&store, "x", Status::Triage, &[]);
    assert!(store
        .reorder(&c.display_id, Status::Ready, Some(&other.display_id), None)
        .is_err());
}

#[test]
fn list_filters() {
    let store = store();
    let a = add_with(&store, "Alpha one", Status::Ready, &["x"]);
    let b = add_with(&store, "Beta", Status::Working, &[]);
    add(&store, "Other", "elsewhere");
    store
        .link_workspace(&a.display_id, Some("local/w_1:AC-1 alpha"))
        .unwrap();
    store
        .link_workspace(&b.display_id, Some("local/wx1:beta"))
        .unwrap();
    let by_ws = |prefix: &str| {
        store
            .list(&TaskFilter {
                workspace_key: Some(prefix.into()),
                ..TaskFilter::default()
            })
            .unwrap()
            .len()
    };
    assert_eq!(by_ws("local/w_1:"), 1);
    assert_eq!(by_ws("local/w%1:"), 0, "no LIKE wildcards");
    let text = store
        .list(&TaskFilter {
            text: Some("ALPHA".into()),
            ..TaskFilter::default()
        })
        .unwrap();
    assert_eq!(text.len(), 1);
    assert_eq!(text[0].criteria_total, 1);
    let acme = store
        .list(&TaskFilter {
            project: Some("Acme".into()),
            statuses: vec![Status::Working],
            ..TaskFilter::default()
        })
        .unwrap();
    assert_eq!(acme.len(), 1);
    assert_eq!(acme[0].task.id, b.id);

    // Done lane: newest closed first, limited, archived hidden.
    let mut closed = Vec::new();
    for i in 0..3 {
        let t = add_with(&store, &format!("done {i}"), Status::Ready, &[]);
        store
            .move_task(&t.display_id, Status::Done, &Actor::Human, None)
            .unwrap();
        store
            .conn()
            .execute(
                "UPDATE tasks SET closed_at = ?2 WHERE id = ?1",
                rusqlite::params![t.id, format!("2026-01-0{}T00:00:00Z", i + 1)],
            )
            .unwrap();
        closed.push(t);
    }
    let limited = store
        .list(&TaskFilter {
            project: Some("Acme".into()),
            done_limit: Some(2),
            ..TaskFilter::default()
        })
        .unwrap();
    let names: Vec<&str> = limited.iter().map(|c| c.task.name()).collect();
    assert_eq!(names, vec!["Alpha one", "Beta", "done 2", "done 1"]);
    store
        .update_task(
            &closed[2].display_id,
            &TaskPatch {
                archived: Some(true),
                ..Default::default()
            },
            &Actor::Human,
        )
        .unwrap();
    assert_eq!(store.lane_counts("Acme").unwrap(), [0, 1, 1, 0, 0, 2]);
    let all = store
        .list(&TaskFilter {
            project: Some("Acme".into()),
            include_archived: true,
            ..TaskFilter::default()
        })
        .unwrap();
    assert_eq!(all.len(), 5);
}

#[test]
fn lane_counts_put_cancelled_under_done() {
    let store = store();
    let t = add(&store, "Acme", "t");
    add(&store, "Acme", "u");
    store
        .move_task(&t.display_id, Status::Cancelled, &Actor::Human, None)
        .unwrap();
    assert_eq!(store.lane_counts("Acme").unwrap(), [1, 0, 0, 0, 0, 1]);
    assert_eq!(store.lane_counts("None").unwrap(), [0; 6]);
}

#[test]
fn entries_artifacts_and_usage() {
    let store = store();
    let task = add_with(&store, "t", Status::Ready, &[]);
    let id = task.display_id.as_str();
    let started = store
        .start_attempt(id, &attempt("local", "p1"), &agent())
        .unwrap();
    let note = store
        .add_entry(id, EntryKind::Agent, "half way", &agent())
        .unwrap();
    assert_eq!(note.attempt_id, Some(started.id));
    assert!(store
        .add_entry(id, EntryKind::Human, "  ", &Actor::Human)
        .is_err());
    let long = "é".repeat(15_000);
    let cut = store
        .add_entry(id, EntryKind::Human, &long, &Actor::Human)
        .unwrap();
    assert!(cut.body.len() <= MAX_TEXT + '…'.len_utf8());
    assert!(cut.body.ends_with('…'));
    store.pin_entry(note.id, true).unwrap();
    let artifact = store
        .attach_artifact(
            id,
            &NewArtifact {
                kind: crate::tasks::ArtifactKind::Doc,
                title: "Design".into(),
                target: "/tmp/design.md".into(),
                machine: Some("local".into()),
                summary: Some("the plan".into()),
            },
            &agent(),
        )
        .unwrap();
    assert_eq!(artifact.attempt_id, Some(started.id));
    store
        .review_artifact(artifact.id, Review::Accepted)
        .unwrap();
    store
        .set_attempt_usage(started.id, Some(10), Some(20), Some(42), Some("sess"))
        .unwrap();
    let before = store.task(id).unwrap().unwrap().version;
    store
        .set_attempt_usage(started.id, Some(10), None, None, None)
        .unwrap();
    assert_eq!(
        store.task(id).unwrap().unwrap().version,
        before,
        "unchanged usage is a no-op"
    );
    let detail = store.task_detail(id).unwrap().unwrap();
    assert!(detail.entries.iter().any(|e| e.pinned && e.id == note.id));
    assert_eq!(detail.artifacts[0].review, Review::Accepted);
    assert_eq!(detail.attempts[0].cost_cents, Some(42));
    assert_eq!(detail.attempts[0].session_id.as_deref(), Some("sess"));
    let seqs: Vec<i64> = detail.entries.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=seqs.len() as i64).collect::<Vec<_>>());
}

#[test]
fn apply_once_applies_each_seq_once_per_source() {
    let store = store();
    let task = add_with(&store, "t", Status::Ready, &[]);
    let ctx = OpContext {
        actor: Actor::Agent("claude@mato".into()),
        machine: "mato".into(),
        pane_key: Some("mato/p1".into()),
    };
    let op = TaskOp::Note {
        task: Some(task.display_id.clone()),
        body: "from mato".into(),
    };
    let count = || entries(&store, &task.display_id).len();
    let before = count();
    assert!(
        store
            .apply_once("mato/p1/abc", 1, &op, &ctx)
            .unwrap()
            .unwrap()
            .ok
    );
    assert!(store
        .apply_once("mato/p1/abc", 1, &op, &ctx)
        .unwrap()
        .is_none());
    assert_eq!(count(), before + 1);
    assert_eq!(store.applied_seq("mato/p1/abc").unwrap(), 1);
    // A new epoch is another source.
    assert!(store
        .apply_once("mato/p1/xyz", 1, &op, &ctx)
        .unwrap()
        .is_some());
    assert_eq!(count(), before + 2);
    // A refused op is recorded, answered, and leaves no writes.
    let refused = TaskOp::Status {
        task: Some(task.display_id.clone()),
        to: Status::Done,
        note: None,
    };
    let result = store
        .apply_once("mato/p1/abc", 2, &refused, &ctx)
        .unwrap()
        .unwrap();
    assert!(!result.ok);
    assert_eq!(result.code.as_deref(), Some("not_allowed"));
    assert_eq!(result.exit(), 3);
    assert_eq!(store.applied_seq("mato/p1/abc").unwrap(), 2);
    assert_eq!(store.applied_seq("unknown").unwrap(), 0);
}

#[test]
fn apply_resolves_the_task_from_the_pane() {
    let store = store();
    let task = add_with(&store, "t", Status::Ready, &[]);
    let ctx = OpContext {
        actor: agent(),
        machine: "local".into(),
        pane_key: Some("local/p7".into()),
    };
    let start = TaskOp::Start {
        task: Some(task.display_id.clone()),
        harness: "claude".into(),
        session_id: None,
    };
    let started = store.apply(&start, &ctx).unwrap();
    assert_eq!(started.message, "ACM-1 working  attempt 1 started");
    let note = TaskOp::Note {
        task: None,
        body: "no id".into(),
    };
    let noted = store.apply(&note, &ctx).unwrap();
    assert_eq!(noted.task.as_deref(), Some("ACM-1"));
    let lost = OpContext {
        pane_key: None,
        ..ctx
    };
    let err = store.apply(&note, &lost).unwrap_err();
    assert_eq!(err.to_string(), "no task id: pass ID or set DROVR_TASK");
}

#[test]
fn open_existing_on_a_missing_path_creates_nothing() {
    let dir = TempDir::new("missing");
    let path = dir.path().join("sub").join("tasks.db");
    assert!(TaskStore::open_existing(&path, 250).unwrap().is_none());
    assert!(!path.exists());
    assert!(!path.parent().unwrap().exists());
}

#[test]
fn concurrent_writers_allocate_without_gaps() {
    let dir = TempDir::new("concurrent");
    let path = dir.path().join("tasks.db");
    let first = TaskStore::open(&path, 5000).unwrap();
    let task = add(&first, "Acme", "shared");
    let before = entries(&first, &task.display_id).len() as i64;
    drop(first);
    let threads: Vec<_> = (0..2)
        .map(|n| {
            let path = path.clone();
            let id = task.display_id.clone();
            std::thread::spawn(move || {
                let store = TaskStore::open(&path, 5000).unwrap();
                for i in 0..200 {
                    store
                        .add_entry(&id, EntryKind::Human, &format!("{n}:{i}"), &Actor::Human)
                        .unwrap();
                    store
                        .create_task(
                            &NewTask {
                                project: "Load".into(),
                                title: Some(format!("{n}:{i}")),
                                ..NewTask::default()
                            },
                            &Actor::Human,
                        )
                        .unwrap();
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let store = TaskStore::open(&path, 5000).unwrap();
    let seqs: Vec<i64> = entries(&store, &task.display_id)
        .into_iter()
        .map(|e| e.seq)
        .collect();
    // task_detail returns the last 200; count with SQL instead.
    let (count, max): (i64, i64) = store
        .conn()
        .query_row(
            "SELECT COUNT(*), MAX(seq) FROM entries WHERE task_id = ?1",
            [task.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(count, before + 400);
    assert_eq!(max, before + 400);
    assert_eq!(seqs.len(), 200);
    let mut numbers: Vec<i64> = store
        .list(&TaskFilter {
            project: Some("Load".into()),
            ..TaskFilter::default()
        })
        .unwrap()
        .into_iter()
        .map(|card| card.task.number)
        .collect();
    numbers.sort_unstable();
    assert_eq!(numbers, (1..=400).collect::<Vec<_>>());
}

#[test]
fn data_version_changes_when_another_connection_commits() {
    let dir = TempDir::new("dataversion");
    let path = dir.path().join("tasks.db");
    let reader = TaskStore::open(&path, 5000).unwrap();
    let writer = TaskStore::open(&path, 5000).unwrap();
    let before = reader.data_version().unwrap();
    add(&reader, "Acme", "own write");
    assert_eq!(
        reader.data_version().unwrap(),
        before,
        "own commits do not count"
    );
    add(&writer, "Acme", "other write");
    assert_ne!(reader.data_version().unwrap(), before);
}

#[test]
fn daily_backup_is_written_once_and_pruned_to_seven() {
    let dir = TempDir::new("backup");
    let path = dir.path().join("tasks.db");
    let store = TaskStore::open(&path, 5000).unwrap();
    for day in 1..=9 {
        std::fs::write(
            dir.path().join(format!("tasks.db.202501{day:02}.bak")),
            b"old",
        )
        .unwrap();
    }
    std::fs::write(dir.path().join("tasks.db.v1.bak"), b"keep").unwrap();
    store.backup_daily().unwrap();
    let today: String = now_text()
        .chars()
        .take(10)
        .filter(char::is_ascii_digit)
        .collect();
    let todays = dir.path().join(format!("tasks.db.{today}.bak"));
    assert!(todays.exists());
    let daily: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.len() == "tasks.db.20250101.bak".len() && name.ends_with(".bak"))
        .collect();
    assert_eq!(daily.len(), 7, "{daily:?}");
    assert!(dir.path().join("tasks.db.v1.bak").exists());
    // A second call the same day is a no-op.
    let modified = std::fs::metadata(&todays).unwrap().modified().unwrap();
    store.backup_daily().unwrap();
    assert_eq!(
        std::fs::metadata(&todays).unwrap().modified().unwrap(),
        modified
    );
    TaskStore::open_in_memory().unwrap().backup_daily().unwrap();
}

#[test]
fn a_send_back_with_an_open_attempt_keeps_auto_on() {
    let store = store();
    let task = add_with(&store, "t", Status::Ready, &[]);
    let id = task.display_id.as_str();
    store
        .start_attempt(id, &attempt("local", "p1"), &agent())
        .unwrap();
    store.move_task(id, Status::Review, &agent(), None).unwrap();
    let sent = store
        .move_task(id, Status::Ready, &Actor::Human, Some("tests fail"))
        .unwrap();
    assert!(sent.auto_status, "the agent's signals drive it again");
    let back = store
        .move_task(id, Status::Working, &Actor::Auto, None)
        .unwrap();
    assert_eq!(back.status, Status::Working);
    // Without an open attempt a send back is a plain human move.
    let other = add_with(&store, "u", Status::Review, &[]);
    let sent = store
        .move_task(
            &other.display_id,
            Status::Ready,
            &Actor::Human,
            Some("redo"),
        )
        .unwrap();
    assert!(!sent.auto_status);
}
