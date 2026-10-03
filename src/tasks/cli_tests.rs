use super::*;
use crate::tasks::test_support::TempDir;
use crate::tasks::{Actor, NewTask};

fn env(dir: &Path) -> Env {
    Env {
        task: None,
        mode: Some("db".into()),
        db_env: true,
        db_path: dir.join("state").join("tasks.db"),
        agent: None,
        pane: None,
        workspace: None,
        outbox: dir.join("outbox"),
        herdr: None,
        cwd: dir.to_owned(),
        reply_wait: Duration::ZERO,
    }
}

fn run(env: &Env, args: &[&str], stdin: &str) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let mut input = stdin.as_bytes();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_with(&args, env, &mut input, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

fn parsed(args: &[&str], stdin: &str) -> Result<Cmd, Usage> {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    parse(&args, &mut stdin.as_bytes())
}

fn op(args: &[&str]) -> TaskOp {
    match parsed(args, "").unwrap() {
        Cmd::Op { op, .. } => op,
        other => panic!("not an op: {other:?}"),
    }
}

#[test]
fn ids_are_detected_by_shape_and_count() {
    assert_eq!(
        op(&["status", "ac-12", "review"]),
        TaskOp::Status {
            task: Some("AC-12".into()),
            to: Status::Review,
            note: None
        }
    );
    assert_eq!(
        op(&["status", "doing"]),
        TaskOp::Status {
            task: None,
            to: Status::Working,
            note: None
        }
    );
    // `note AC-12` is a note whose text is "AC-12".
    assert_eq!(
        op(&["note", "AC-12"]),
        TaskOp::Note {
            task: None,
            body: "AC-12".into()
        }
    );
    assert_eq!(
        op(&["note", "AC-12", "half way"]),
        TaskOp::Note {
            task: Some("AC-12".into()),
            body: "half way".into()
        }
    );
    assert_eq!(
        op(&["check", "2", "pass"]),
        TaskOp::Check {
            task: None,
            position: 2,
            state: CheckState::Passed,
            evidence: None
        }
    );
    assert_eq!(
        op(&["check", "OO2-3", "1", "fail", "--evidence", "boom"]),
        TaskOp::Check {
            task: Some("OO2-3".into()),
            position: 1,
            state: CheckState::Failed,
            evidence: Some("boom".into())
        }
    );
    assert!(looks_like_id("AC-12"));
    assert!(looks_like_id("oo2-1"));
    assert!(!looks_like_id("2-1"));
    assert!(!looks_like_id("AC-"));
    assert!(!looks_like_id("A_C-1"));
    match parsed(&["verify", "ac-1", "2", "3"], "").unwrap() {
        Cmd::Verify { id, positions } => {
            assert_eq!(id.as_deref(), Some("AC-1"));
            assert_eq!(positions, vec![2, 3]);
        }
        other => panic!("{other:?}"),
    }
    match parsed(&["show", "AC-4", "--json"], "").unwrap() {
        Cmd::Show { id, json } => assert_eq!((id.as_deref(), json), (Some("AC-4"), true)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn stdin_and_defaults() {
    assert_eq!(
        op(&["note", "-"]),
        TaskOp::Note {
            task: None,
            body: String::new()
        }
    );
    match parsed(&["note", "-"], "from stdin\n").unwrap() {
        Cmd::Op {
            op: TaskOp::Note { body, .. },
            ..
        } => assert_eq!(body, "from stdin"),
        other => panic!("{other:?}"),
    }
    match parsed(&["check", "1", "pass", "--evidence", "-"], "log\n").unwrap() {
        Cmd::Op {
            op: TaskOp::Check { evidence, .. },
            ..
        } => assert_eq!(evidence.as_deref(), Some("log")),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        op(&["done"]),
        TaskOp::Done {
            task: None,
            outcome: Outcome::Succeeded,
            note: None
        }
    );
    assert_eq!(
        op(&["criteria", "--add", "a", "b", "--add=c"]),
        TaskOp::Criteria {
            task: None,
            set: vec![],
            add: vec!["a".into(), "c".into(), "b".into()],
        }
    );
    match parsed(
        &[
            "add",
            "Fix it",
            "--kind",
            "fix",
            "--priority",
            "high",
            "--criterion",
            "x",
        ],
        "",
    )
    .unwrap()
    {
        Cmd::Add {
            title,
            kind,
            priority,
            criteria,
            ..
        } => {
            assert_eq!(title, "Fix it");
            assert_eq!(kind, Some(Kind::Fix));
            assert_eq!(priority, Some(Priority::High));
            assert_eq!(criteria, vec!["x".to_owned()]);
        }
        other => panic!("{other:?}"),
    }
    let mut start = op(&["start"]);
    let mut env = env(Path::new("/nowhere"));
    fill_harness(&mut start, &env);
    assert!(matches!(&start, TaskOp::Start { harness, .. } if harness == "claude"));
    let mut start = op(&["start"]);
    env.agent = Some("codex".into());
    fill_harness(&mut start, &env);
    assert!(matches!(&start, TaskOp::Start { harness, .. } if harness == "codex"));
}

#[test]
fn decide_parses_choices_and_wait() {
    match parsed(
        &[
            "decide",
            "--title",
            "Which table?",
            "--choice",
            "new:New table:one more migration",
            "--choice",
            "reuse:Reuse entries",
            "--recommend",
            "new",
            "--default",
            "reuse",
            "--no-text",
            "--wait",
        ],
        "",
    )
    .unwrap()
    {
        Cmd::Decide { op, wait } => {
            assert_eq!(wait, Some(DECIDE_WAIT_SECS));
            let TaskOp::Decide {
                choices,
                allow_text,
                default_choice,
                wait_secs,
                ..
            } = op
            else {
                panic!("not decide");
            };
            assert_eq!(choices.len(), 2);
            assert!(choices[0].recommended);
            assert_eq!(
                choices[0].consequence.as_deref(),
                Some("one more migration")
            );
            assert!(!choices[1].recommended);
            assert!(!allow_text);
            assert_eq!(default_choice.as_deref(), Some("reuse"));
            assert_eq!(wait_secs, Some(DECIDE_WAIT_SECS));
        }
        other => panic!("{other:?}"),
    }
    match parsed(
        &["decide", "--title", "t", "--choice", "a:A", "--wait", "5"],
        "",
    )
    .unwrap()
    {
        Cmd::Decide { wait, .. } => assert_eq!(wait, Some(5)),
        other => panic!("{other:?}"),
    }
    assert!(parsed(&["decide", "--title", "t"], "").is_err());
    assert!(parsed(&["decide", "--choice", "a:A"], "").is_err());
    assert!(parsed(&["decide", "--title", "t", "--choice", "a"], "").is_err());
}

#[test]
fn usage_errors_exit_2() {
    let dir = TempDir::new("cli-usage");
    let env = env(dir.path());
    for args in [
        vec![],
        vec!["bogus"],
        vec!["status"],
        vec!["status", "nope"],
        vec!["check", "x", "pass"],
        vec!["release"],
        vec!["criteria"],
        vec!["list", "--wat"],
    ] {
        let (code, _, err) = run(&env, &args, "");
        assert_eq!(code, 2, "{args:?}");
        assert!(err.contains("usage:"), "{err}");
    }
}

#[test]
fn proto_prints_the_outbox_version() {
    let dir = TempDir::new("cli-proto");
    let (code, out, _) = run(&env(dir.path()), &["proto"], "");
    assert_eq!((code, out.as_str()), (0, "drovr-task 1\n"));
}

#[test]
fn db_mode_with_a_missing_file_exits_1_and_creates_nothing() {
    let dir = TempDir::new("cli-missing");
    let env = env(dir.path());
    let (code, _, err) = run(&env, &["note", "AC-1", "hi"], "");
    assert_eq!(code, 1);
    assert!(err.contains("no tasks db at"), "{err}");
    assert!(!env.db_path.exists());
    assert!(!env.db_path.parent().unwrap().exists());
    // An agent's add does not create the db either.
    let mut agent = env.clone();
    agent.pane = Some("p1".into());
    let (code, _, _) = run(&agent, &["add", "x", "--project", "Acme"], "");
    assert_eq!(code, 1);
    assert!(!env.db_path.exists());
}

#[test]
fn db_mode_end_to_end() {
    let dir = TempDir::new("cli-db");
    let human = env(dir.path());
    let (code, out, err) = run(
        &human,
        &[
            "add",
            "Spec decisions",
            "--project",
            "Acme",
            "--body",
            "-",
            "--criterion",
            "schema added",
            "--criterion",
            "docs",
        ],
        "Add decision requests.\n",
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "ACM-1 triage  added\n");
    assert!(human.db_path.exists());

    let mut agent = human.clone();
    agent.pane = Some("p3".into());
    agent.agent = Some("claude".into());
    agent.task = Some("ACM-1".into());
    let (code, out, _) = run(&agent, &["start"], "");
    assert_eq!(
        (code, out.as_str()),
        (0, "ACM-1 working  attempt 1 started\n")
    );

    // Without DROVR_TASK the pane's open attempt names the task.
    let mut pane_only = agent.clone();
    pane_only.task = None;
    let (code, out, _) = run(&pane_only, &["note", "-"], "half way\n");
    assert_eq!((code, out.as_str()), (0, "ACM-1 working  noted\n"));

    let (code, _, err) = run(&agent, &["done"], "");
    assert_eq!(code, 3);
    assert_eq!(err, "");
    let (code, out, _) = run(&agent, &["done", "--json"], "");
    assert_eq!(code, 3);
    let result: OpResult = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(result.code.as_deref(), Some("criteria_open"));
    assert_eq!(result.message, "criteria 1, 2 have no verdict");

    run(
        &agent,
        &["check", "1", "pass", "--evidence", "migrated"],
        "",
    );
    run(&agent, &["check", "2", "pass", "--evidence", "written"], "");
    let (code, out, _) = run(&agent, &["status", "done"], "");
    assert_eq!(code, 3);
    assert_eq!(
        out,
        "a task in working does not move to done from an agent\n"
    );
    let (code, out, _) = run(&agent, &["done", "--note", "all green"], "");
    assert_eq!(
        (code, out.as_str()),
        (0, "ACM-1 review  attempt succeeded\n")
    );

    let (code, out, _) = run(&human, &["show", "ACM-1"], "");
    assert_eq!(code, 0);
    assert!(out.starts_with("ACM-1 review  Spec decisions\n"), "{out}");
    assert!(out.contains("✓ 1. schema added"), "{out}");
    assert!(out.contains("claude@local: half way"), "{out}");

    let (code, out, _) = run(&human, &["show", "ACM-1", "--json"], "");
    assert_eq!(code, 0);
    let detail: TaskDetail = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(detail.criteria.len(), 2);

    let (code, out, _) = run(&human, &["list"], "");
    assert_eq!(
        (code, out.as_str()),
        (0, "ACM-1 review  Spec decisions  ✓2/2\n")
    );
    let (code, out, _) = run(&human, &["list", "--status", "triage,ready"], "");
    assert_eq!((code, out.as_str()), (0, ""));

    let (code, _, err) = run(&human, &["show", "ACM-9"], "");
    assert_eq!((code, err.as_str()), (4, "no task ACM-9\n"));
    let (code, out, _) = run(&human, &["note", "ACM-9", "x"], "");
    assert_eq!((code, out.as_str()), (4, "no task ACM-9\n"));
    let (code, _, err) = run(&human, &["note", "hi"], "");
    assert_eq!(
        (code, err.as_str()),
        (2, "no task id: pass ID or set DROVR_TASK\n")
    );

    let (code, out, _) = run(&human, &["status", "ACM-1", "ready"], "");
    assert_eq!(
        (code, out.as_str()),
        (3, "sending a task back needs a note\n")
    );
    let (code, _, _) = run(&human, &["status", "ACM-1", "done"], "");
    assert_eq!(code, 0);

    // An agent's add defaults to the project of the workspace its task runs in.
    let mut ws = agent.clone();
    ws.task = None;
    ws.workspace = Some("w1".into());
    let (code, _, err) = run(&ws, &["add", "x"], "");
    assert_eq!(code, 2, "{err}");
    let store = TaskStore::open(&human.db_path, 5000).unwrap();
    let second = store
        .create_task(
            &NewTask {
                project: "Beta".into(),
                title: Some("b".into()),
                ..NewTask::default()
            },
            &Actor::Human,
        )
        .unwrap();
    store
        .start_attempt(
            &second.display_id,
            &crate::tasks::NewAttempt {
                harness: "claude".into(),
                machine: "local".into(),
                workspace_key: Some("local/w1:BET-1 b".into()),
                pane_key: Some("local/p3".into()),
                session_id: None,
            },
            &Actor::Human,
        )
        .unwrap();
    let (code, out, _) = run(&ws, &["add", "follow up"], "");
    assert_eq!((code, out.as_str()), (0, "BET-2 triage  added\n"));
}

#[test]
fn artifact_defaults() {
    let cwd = Path::new("/work/repo");
    let op = artifact_op("A-1".into(), "docs/report.md", None, None, None, cwd);
    assert_eq!(
        op,
        TaskOp::Artifact {
            task: Some("A-1".into()),
            kind: ArtifactKind::Doc,
            title: "report.md".into(),
            target: "/work/repo/docs/report.md".into(),
            summary: None,
        }
    );
    let link = artifact_op("A-1".into(), "https://x.test/pr/1", None, None, None, cwd);
    assert!(
        matches!(link, TaskOp::Artifact { kind: ArtifactKind::Link, ref title, .. } if title == "https://x.test/pr/1")
    );
    let diff = artifact_op(
        "A-1".into(),
        "/tmp/fix.patch",
        Some("Fix".into()),
        None,
        None,
        cwd,
    );
    assert!(
        matches!(diff, TaskOp::Artifact { kind: ArtifactKind::Diff, ref title, .. } if title == "Fix")
    );
    let file = artifact_op("A-1".into(), "out.png", None, None, None, cwd);
    assert!(matches!(
        file,
        TaskOp::Artifact {
            kind: ArtifactKind::File,
            ..
        }
    ));
}

#[test]
fn verify_records_verdicts_with_output_and_exit_code() {
    let dir = TempDir::new("cli-verify");
    let human = env(dir.path());
    let store = TaskStore::open(&human.db_path, 5000).unwrap();
    store
        .create_task(
            &NewTask {
                project: "Acme".into(),
                title: Some("v".into()),
                criteria: vec![
                    "passes (check: true)".into(),
                    "fails (check: `sh -c 'echo no; exit 3'`)".into(),
                    "manual".into(),
                ],
                ..NewTask::default()
            },
            &Actor::Human,
        )
        .unwrap();
    let mut agent = human.clone();
    agent.pane = Some("p1".into());
    agent.task = Some("ACM-1".into());
    let (code, out, err) = run(&agent, &["verify"], "");
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        out,
        "ACM-1 triage  criterion 1 passed\nACM-1 triage  criterion 2 failed\n"
    );
    let detail = store.task_detail("ACM-1").unwrap().unwrap();
    assert_eq!(detail.criteria[0].state, CheckState::Passed);
    assert_eq!(
        detail.criteria[0].evidence.as_deref(),
        Some("$ true\n\nexit 0")
    );
    assert_eq!(detail.criteria[1].state, CheckState::Failed);
    assert_eq!(
        detail.criteria[1].evidence.as_deref(),
        Some("$ sh -c 'echo no; exit 3'\nno\nexit 3")
    );
    assert_eq!(detail.criteria[2].state, CheckState::Open);
    let (code, _, err) = run(&agent, &["verify", "3"], "");
    assert_eq!(
        (code, err.as_str()),
        (2, "criterion 3 has no check command\n")
    );
}

#[test]
fn run_check_times_out() {
    let (code, _) = run_check("sleep 5", Path::new("/"), Duration::from_millis(100));
    assert_eq!(code, None);
    assert_eq!(tail("abcdef", 3), "…def");
}

#[test]
fn decide_wait_in_db_mode() {
    let dir = TempDir::new("cli-decide");
    let human = env(dir.path());
    run(&human, &["add", "t", "--project", "Acme"], "");
    let mut agent = human.clone();
    agent.pane = Some("p1".into());
    agent.task = Some("ACM-1".into());
    run(&agent, &["start"], "");
    let (code, out, _) = run(
        &agent,
        &[
            "decide", "--title", "Which?", "--choice", "a:Alpha", "--wait", "0",
        ],
        "",
    );
    assert_eq!(code, 0);
    assert_eq!(out, "ACM-1 blocked  decision 1 asked\nwaiting\n");
    let store = TaskStore::open(&human.db_path, 5000).unwrap();
    assert_eq!(store.decision(1).unwrap().unwrap().wait_until, None);
    run(&agent, &["status", "ACM-1", "working"], "");

    store.withdraw_decision("ACM-1", &Actor::Human).unwrap();
    let path = human.db_path.clone();
    let ruler = std::thread::spawn(move || {
        let store = TaskStore::open(&path, 5000).unwrap();
        for _ in 0..100 {
            if let Some(open) = store.open_decisions(None).unwrap().first() {
                assert!(open.decision.wait_until.is_some());
                store
                    .rule_decision(
                        open.decision.id,
                        &crate::tasks::Ruling::Choice("b".into()),
                        "panel",
                        &Actor::Human,
                    )
                    .unwrap();
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("no decision appeared");
    });
    let (code, out, _) = run(
        &agent,
        &[
            "decide", "--title", "Second?", "--choice", "a:Alpha", "--choice", "b:Beta", "--wait",
            "20",
        ],
        "",
    );
    ruler.join().unwrap();
    assert_eq!(code, 0);
    assert_eq!(out, "ACM-1 blocked  decision 2 asked\nruled b: Beta\n");
}

#[test]
fn outbox_mode_queues_ops() {
    let dir = TempDir::new("cli-outbox");
    let mut remote = env(dir.path());
    remote.mode = Some("outbox".into());
    remote.db_env = false;
    remote.pane = Some("p4".into());
    remote.task = Some("AC-2".into());
    let (code, out, _) = run(&remote, &["note", "from mato"], "");
    assert_eq!((code, out.as_str()), (0, "AC-2 queued\n"));
    let pane = outbox::pane_dir(&remote.outbox, "p4");
    let line = std::fs::read_to_string(pane.join("1.json")).unwrap();
    let queued = crate::tasks::ops::parse_outbox_line(line.trim()).unwrap();
    assert_eq!(queued.pane, "p4");
    assert_eq!(
        queued.op,
        TaskOp::Note {
            task: Some("AC-2".into()),
            body: "from mato".into()
        }
    );
    assert!(!remote.db_path.exists());

    // A reply from the client sets the output and the exit code.
    let epoch = std::fs::read_to_string(pane.join("epoch")).unwrap();
    let reply = outbox::reply_path(
        &remote.outbox,
        "p4",
        &outbox::Queued {
            epoch: epoch.clone(),
            seq: 2,
        },
    );
    std::fs::create_dir_all(reply.parent().unwrap()).unwrap();
    std::fs::write(
        &reply,
        r#"{"ok":false,"task":"AC-2","status":null,"message":"criteria 2 failed","code":"criteria_failed","decision_id":null,"exit":3}"#,
    )
    .unwrap();
    let (code, out, _) = run(&remote, &["done"], "");
    assert_eq!((code, out.as_str()), (3, "criteria 2 failed\n"));

    // Without snapshots there is nothing to list.
    let (code, _, err) = run(&remote, &["list"], "");
    assert_eq!(
        (code, err.as_str()),
        (4, "no task data on this machine yet\n")
    );
    // Outside a pane there is no outbox and no db.
    let mut shell = remote.clone();
    shell.pane = None;
    let (code, _, err) = run(&shell, &["note", "x"], "");
    assert_eq!(code, 1);
    assert!(err.starts_with("no tasks db at"), "{err}");
}

#[test]
fn outbox_mode_reads_snapshots_for_show_list_and_verify() {
    let dir = TempDir::new("cli-snapshot");
    // Build a snapshot the way the client would: TaskDetail as JSON.
    let store = TaskStore::open_in_memory().unwrap();
    store
        .create_task(
            &NewTask {
                project: "Acme".into(),
                title: Some("remote".into()),
                criteria: vec!["ok (check: true)".into()],
                ..NewTask::default()
            },
            &Actor::Human,
        )
        .unwrap();
    let detail = store.task_detail("ACM-1").unwrap().unwrap();
    let mut remote = env(dir.path());
    remote.mode = Some("outbox".into());
    remote.pane = Some("p4".into());
    remote.task = Some("ACM-1".into());
    let snapshot = outbox::snapshot_path(&remote.outbox, "ACM-1");
    std::fs::create_dir_all(snapshot.parent().unwrap()).unwrap();
    std::fs::write(&snapshot, serde_json::to_string(&detail).unwrap()).unwrap();
    let (code, out, _) = run(&remote, &["list"], "");
    assert_eq!((code, out.as_str()), (0, "ACM-1 triage  remote  ✓0/1\n"));
    let (code, out, _) = run(&remote, &["show"], "");
    assert_eq!(code, 0);
    assert!(out.starts_with("ACM-1 triage  remote"), "{out}");
    let (code, out, _) = run(&remote, &["verify"], "");
    assert_eq!((code, out.as_str()), (0, "ACM-1 queued\n"));
    let line =
        std::fs::read_to_string(outbox::pane_dir(&remote.outbox, "p4").join("1.json")).unwrap();
    let queued = crate::tasks::ops::parse_outbox_line(line.trim()).unwrap();
    assert!(matches!(
        queued.op,
        TaskOp::Check {
            position: 1,
            state: CheckState::Passed,
            ..
        }
    ));
}

#[test]
fn import_through_the_cli() {
    let dir = TempDir::new("cli-import");
    let source = crate::tasks::import::tests::fixture(dir.path());
    let human = env(dir.path());
    let source = source.to_string_lossy().into_owned();
    let (code, out, err) = run(&human, &["import", &source, "--dry-run"], "");
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "imported 4 tasks into 2 projects (0 skipped)\n");
    let (code, out, _) = run(&human, &["import", &source, "--map", "OPS=Infra"], "");
    assert_eq!(
        (code, out.as_str()),
        (0, "imported 4 tasks into 2 projects (0 skipped)\n")
    );
    let (code, out, _) = run(&human, &["import", &source], "");
    assert_eq!(
        (code, out.as_str()),
        (0, "imported 0 tasks into 0 projects (4 skipped)\n")
    );
    let (code, out, _) = run(&human, &["list", "--project", "Infra"], "");
    assert_eq!((code, out.as_str()), (0, "OPS-1 ready  Rotate keys\n"));
}
