//! `drovr task`: the CLI agents and the user report through
//! (docs/design/tasks.md section 6.1). Hand-parsed like `drovr doc open`.
//!
//! Two modes. `db`: open the tasks db and apply the op. `outbox`: queue the
//! op for the client to pull over SSH (a remote machine has no db).

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::ops::{exit_code, OpContext, OpResult, TaskOp, OUTBOX_V};
use super::outbox::{self, Reply};
use super::{
    cut_text, time_text, Actor, ArtifactKind, CheckState, Choice, DecisionState, Kind, Outcome,
    Priority, Status, StoreError, TaskCard, TaskDetail, TaskFilter, TaskStore, MAX_TEXT,
};

const USAGE: &str = "usage:
  drovr task list [--project NAME] [--status S[,S...]] [--all] [--json]
  drovr task show [ID] [--json]
  drovr task add TITLE [--project NAME] [--body TEXT|-] [--kind K] [--priority P]
                 [--criterion TEXT]... [--json]
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
  drovr task proto";

const CLI_BUSY_MS: u32 = 5000;
const VERIFY_LIMIT: Duration = Duration::from_secs(300);
const DECIDE_WAIT_SECS: u32 = 600;
const WAIT_POLL: Duration = Duration::from_millis(500);

/// What the CLI reads from its environment. Tests build one by hand.
#[derive(Clone, Debug)]
pub(crate) struct Env {
    /// $DROVR_TASK
    pub task: Option<String>,
    /// $DROVR_TASK_MODE
    pub mode: Option<String>,
    /// $DROVR_TASKS_DB is set.
    pub db_env: bool,
    /// `TaskStore::default_path()`.
    pub db_path: PathBuf,
    /// $DROVR_AGENT
    pub agent: Option<String>,
    /// $HERDR_PANE_ID
    pub pane: Option<String>,
    /// $HERDR_WORKSPACE_ID
    pub workspace: Option<String>,
    /// `outbox::root()`.
    pub outbox: PathBuf,
    /// The herdr binary that rings the client; None skips the ring.
    pub herdr: Option<String>,
    pub cwd: PathBuf,
    /// How long an outbox op waits for the client's reply (3 s).
    pub reply_wait: Duration,
}

impl Env {
    fn from_process() -> Env {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        Env {
            task: var("DROVR_TASK"),
            mode: var("DROVR_TASK_MODE"),
            db_env: var("DROVR_TASKS_DB").is_some(),
            db_path: TaskStore::default_path(),
            agent: var("DROVR_AGENT"),
            pane: var("HERDR_PANE_ID"),
            workspace: var("HERDR_WORKSPACE_ID"),
            outbox: outbox::root(),
            herdr: Some(var("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into())),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            reply_wait: Duration::from_secs(3),
        }
    }

    fn mode(&self) -> Mode {
        match self.mode.as_deref() {
            Some("db") => Mode::Db,
            Some("outbox") => Mode::Outbox,
            _ if self.db_env || self.db_path.exists() => Mode::Db,
            _ => Mode::Outbox,
        }
    }

    fn actor(&self, mode: Mode) -> Actor {
        match &self.pane {
            Some(_) => {
                let harness = self.agent.as_deref().unwrap_or("agent");
                let machine = match mode {
                    Mode::Db => "local",
                    Mode::Outbox => "agent",
                };
                Actor::Agent(format!("{harness}@{machine}"))
            }
            None => Actor::Human,
        }
    }

    fn pane_key(&self) -> Option<String> {
        self.pane.as_ref().map(|pane| format!("local/{pane}"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Db,
    Outbox,
}

/// Entry point from `main`: `drovr task ARGS...`.
pub(crate) fn run(args: &[String]) -> io::Result<i32> {
    let env = Env::from_process();
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let stderr = io::stderr();
    let mut err = stderr.lock();
    Ok(run_with(args, &env, &mut input, &mut out, &mut err))
}

pub(crate) fn run_with(
    args: &[String],
    env: &Env,
    stdin: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let command = match parse(args, stdin) {
        Ok(command) => command,
        Err(Usage(message)) => {
            if !message.is_empty() {
                let _ = writeln!(err, "error: {message}");
            }
            let _ = writeln!(err, "{USAGE}");
            return 2;
        }
    };
    match execute(command, env, out) {
        Ok(code) => code,
        Err(Failure { code, message }) => {
            let _ = writeln!(err, "{message}");
            code
        }
    }
}

// Parsing

#[derive(Debug, PartialEq)]
struct Usage(String);

fn usage(message: impl Into<String>) -> Usage {
    Usage(message.into())
}

#[derive(Debug, PartialEq)]
enum Cmd {
    List {
        project: Option<String>,
        statuses: Vec<Status>,
        all: bool,
        json: bool,
    },
    Show {
        id: Option<String>,
        json: bool,
    },
    Add {
        title: String,
        project: Option<String>,
        body: String,
        kind: Option<Kind>,
        priority: Option<Priority>,
        criteria: Vec<String>,
        json: bool,
    },
    /// Any op built from its arguments; `json` prints the OpResult.
    Op {
        op: TaskOp,
        json: bool,
    },
    Verify {
        id: Option<String>,
        positions: Vec<i64>,
    },
    Artifact {
        id: Option<String>,
        target: String,
        title: Option<String>,
        kind: Option<ArtifactKind>,
        summary: Option<String>,
    },
    Decide {
        op: TaskOp,
        wait: Option<u32>,
    },
    Import {
        path: PathBuf,
        map: Vec<(String, String)>,
        dry_run: bool,
    },
    Proto,
}

/// Flags of one subcommand: value flags (repeatable), bool flags, and the
/// number of positionals it takes without an id (None = any, ids by shape).
struct Spec {
    values: &'static [&'static str],
    bools: &'static [&'static str],
}

#[derive(Default)]
struct Parsed {
    positionals: Vec<String>,
    values: Vec<(String, String)>,
    bools: Vec<String>,
    /// `--wait` with or without SECS.
    wait: Option<Option<u32>>,
}

impl Parsed {
    fn value(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(flag, _)| flag == name)
            .map(|(_, value)| value.as_str())
    }

    fn all(&self, name: &str) -> Vec<String> {
        self.values
            .iter()
            .filter(|(flag, _)| flag == name)
            .map(|(_, value)| value.clone())
            .collect()
    }

    fn flag(&self, name: &str) -> bool {
        self.bools.iter().any(|flag| flag == name)
    }
}

fn split_flags(args: &[String], spec: &Spec) -> Result<Parsed, Usage> {
    let mut parsed = Parsed::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        index += 1;
        let Some(name) = arg.strip_prefix("--") else {
            parsed.positionals.push(arg.clone());
            continue;
        };
        let (name, inline) = match name.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (name, None),
        };
        if name == "wait" && spec.bools.contains(&"wait") {
            let secs = match inline {
                Some(value) => Some(
                    value
                        .parse()
                        .map_err(|_| usage(format!("--wait takes seconds, not {value:?}")))?,
                ),
                None => match args.get(index).and_then(|next| next.parse::<u32>().ok()) {
                    Some(secs) => {
                        index += 1;
                        Some(secs)
                    }
                    None => None,
                },
            };
            parsed.wait = Some(secs);
        } else if spec.bools.contains(&name) {
            if inline.is_some() {
                return Err(usage(format!("--{name} takes no value")));
            }
            parsed.bools.push(name.to_owned());
        } else if spec.values.contains(&name) {
            let value = match inline {
                Some(value) => value,
                None => {
                    let value = args
                        .get(index)
                        .ok_or_else(|| usage(format!("missing value for --{name}")))?;
                    index += 1;
                    value.clone()
                }
            };
            parsed.values.push((name.to_owned(), value));
        } else {
            return Err(usage(format!("unknown option --{name}")));
        }
    }
    Ok(parsed)
}

/// `^[A-Za-z][A-Za-z0-9]*-[0-9]+$`
pub(crate) fn looks_like_id(text: &str) -> bool {
    let Some((key, number)) = text.rsplit_once('-') else {
        return false;
    };
    key.starts_with(|ch: char| ch.is_ascii_alphabetic())
        && key.bytes().all(|b| b.is_ascii_alphanumeric())
        && !number.is_empty()
        && number.bytes().all(|b| b.is_ascii_digit())
}

/// Takes the id off the front when the first positional looks like one and
/// the command got more positionals than it takes without an id.
fn take_id(positionals: &mut Vec<String>, takes: usize) -> Option<String> {
    if positionals.len() > takes && positionals.first().is_some_and(|p| looks_like_id(p)) {
        return Some(positionals.remove(0).to_ascii_uppercase());
    }
    None
}

/// `-` reads the value from stdin (up to 20 000 bytes).
fn text_arg(value: &str, stdin: &mut dyn Read) -> Result<String, Usage> {
    if value != "-" {
        return Ok(value.to_owned());
    }
    let mut bytes = Vec::new();
    stdin
        .take(MAX_TEXT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| usage(format!("cannot read stdin: {err}")))?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(cut_text(text.trim_end_matches('\n'), MAX_TEXT))
}

fn no_extra(positionals: &[String]) -> Result<(), Usage> {
    match positionals.first() {
        Some(extra) => Err(usage(format!("unexpected argument {extra:?}"))),
        None => Ok(()),
    }
}

fn parse(args: &[String], stdin: &mut dyn Read) -> Result<Cmd, Usage> {
    let Some((sub, rest)) = args.split_first() else {
        return Err(usage(""));
    };
    let spec = |values, bools| Spec { values, bools };
    match sub.as_str() {
        "list" => {
            let mut p = split_flags(rest, &spec(&["project", "status"], &["all", "json"]))?;
            no_extra(&p.positionals)?;
            let mut statuses = Vec::new();
            for value in p.all("status") {
                for part in value.split(',').filter(|s| !s.trim().is_empty()) {
                    statuses.push(
                        Status::parse(part)
                            .ok_or_else(|| usage(format!("unknown status {part:?}")))?,
                    );
                }
            }
            p.positionals.clear();
            Ok(Cmd::List {
                project: p.value("project").map(str::to_owned),
                statuses,
                all: p.flag("all"),
                json: p.flag("json"),
            })
        }
        "show" => {
            let mut p = split_flags(rest, &spec(&[], &["json"]))?;
            let id = take_id(&mut p.positionals, 0);
            no_extra(&p.positionals)?;
            Ok(Cmd::Show {
                id,
                json: p.flag("json"),
            })
        }
        "add" => {
            let p = split_flags(
                rest,
                &spec(
                    &["project", "body", "kind", "priority", "criterion"],
                    &["json"],
                ),
            )?;
            let [title] = p.positionals.as_slice() else {
                return Err(usage("add takes one TITLE"));
            };
            Ok(Cmd::Add {
                title: title.clone(),
                project: p.value("project").map(str::to_owned),
                body: p
                    .value("body")
                    .map(|body| text_arg(body, stdin))
                    .transpose()?
                    .unwrap_or_default(),
                kind: p.value("kind").map(parse_kind).transpose()?,
                priority: p.value("priority").map(parse_priority).transpose()?,
                criteria: p.all("criterion"),
                json: p.flag("json"),
            })
        }
        "status" => {
            let mut p = split_flags(rest, &spec(&["note"], &["json"]))?;
            let task = take_id(&mut p.positionals, 1);
            let [status] = p.positionals.as_slice() else {
                return Err(usage("status takes STATUS"));
            };
            let to =
                Status::parse(status).ok_or_else(|| usage(format!("unknown status {status:?}")))?;
            Ok(Cmd::Op {
                op: TaskOp::Status {
                    task,
                    to,
                    note: p.value("note").map(str::to_owned),
                },
                json: p.flag("json"),
            })
        }
        "start" => {
            let mut p = split_flags(rest, &spec(&["harness", "session"], &["json"]))?;
            let task = take_id(&mut p.positionals, 0);
            no_extra(&p.positionals)?;
            Ok(Cmd::Op {
                op: TaskOp::Start {
                    task,
                    // Filled from $DROVR_AGENT at execution when empty.
                    harness: p.value("harness").unwrap_or_default().to_owned(),
                    session_id: p.value("session").map(str::to_owned),
                },
                json: p.flag("json"),
            })
        }
        "note" => {
            let mut p = split_flags(rest, &spec(&[], &["json"]))?;
            let task = take_id(&mut p.positionals, 1);
            let [text] = p.positionals.as_slice() else {
                return Err(usage("note takes one TEXT (quote it, or - for stdin)"));
            };
            Ok(Cmd::Op {
                op: TaskOp::Note {
                    task,
                    body: text_arg(text, stdin)?,
                },
                json: p.flag("json"),
            })
        }
        "criteria" => {
            let p = split_flags(rest, &spec(&["add", "set"], &["json"]))?;
            let mut positionals = p.positionals.clone();
            let task = take_id(&mut positionals, 0);
            let (mut add, mut set) = (p.all("add"), p.all("set"));
            // `--add a b c`: later positionals join the list that was given.
            match (add.is_empty(), set.is_empty()) {
                (false, true) => add.append(&mut positionals),
                (true, false) => set.append(&mut positionals),
                (false, false) => return Err(usage("criteria takes --add or --set, not both")),
                (true, true) => return Err(usage("criteria takes --add TEXT or --set TEXT")),
            }
            Ok(Cmd::Op {
                op: TaskOp::Criteria { task, set, add },
                json: p.flag("json"),
            })
        }
        "check" => {
            let mut p = split_flags(rest, &spec(&["evidence"], &["json"]))?;
            let task = take_id(&mut p.positionals, 2);
            let [position, verdict] = p.positionals.as_slice() else {
                return Err(usage("check takes N pass|fail"));
            };
            Ok(Cmd::Op {
                op: TaskOp::Check {
                    task,
                    position: parse_position(position)?,
                    state: parse_verdict(verdict)?,
                    evidence: p
                        .value("evidence")
                        .map(|text| text_arg(text, stdin))
                        .transpose()?,
                },
                json: p.flag("json"),
            })
        }
        "verify" => {
            let mut p = split_flags(rest, &spec(&[], &[]))?;
            let id = p
                .positionals
                .first()
                .filter(|first| looks_like_id(first))
                .map(|first| first.to_ascii_uppercase());
            if id.is_some() {
                p.positionals.remove(0);
            }
            let positions = p
                .positionals
                .iter()
                .map(|n| parse_position(n))
                .collect::<Result<_, _>>()?;
            Ok(Cmd::Verify { id, positions })
        }
        "artifact" => {
            let mut p = split_flags(rest, &spec(&["title", "kind", "summary"], &["json"]))?;
            let id = take_id(&mut p.positionals, 1);
            let [target] = p.positionals.as_slice() else {
                return Err(usage("artifact takes one PATH or URL"));
            };
            Ok(Cmd::Artifact {
                id,
                target: target.clone(),
                title: p.value("title").map(str::to_owned),
                kind: p
                    .value("kind")
                    .map(|kind| {
                        ArtifactKind::parse(kind)
                            .ok_or_else(|| usage(format!("unknown artifact kind {kind:?}")))
                    })
                    .transpose()?,
                summary: p.value("summary").map(str::to_owned),
            })
        }
        "done" => {
            let mut p = split_flags(rest, &spec(&["outcome", "note"], &["json"]))?;
            let task = take_id(&mut p.positionals, 0);
            no_extra(&p.positionals)?;
            let outcome = match p.value("outcome") {
                Some(value) => Outcome::parse(value)
                    .ok_or_else(|| usage(format!("unknown outcome {value:?}")))?,
                None => Outcome::Succeeded,
            };
            Ok(Cmd::Op {
                op: TaskOp::Done {
                    task,
                    outcome,
                    note: p.value("note").map(str::to_owned),
                },
                json: p.flag("json"),
            })
        }
        "release" => {
            let mut p = split_flags(rest, &spec(&["note"], &["json"]))?;
            let task = take_id(&mut p.positionals, 0);
            no_extra(&p.positionals)?;
            let note = p
                .value("note")
                .filter(|note| !note.trim().is_empty())
                .ok_or_else(|| usage("release needs --note TEXT"))?;
            Ok(Cmd::Op {
                op: TaskOp::Release {
                    task,
                    note: note.to_owned(),
                },
                json: p.flag("json"),
            })
        }
        "decide" => parse_decide(rest),
        "import" => {
            let p = split_flags(rest, &spec(&["map"], &["dry-run"]))?;
            let [path] = p.positionals.as_slice() else {
                return Err(usage("import takes one PATH"));
            };
            let map = p
                .all("map")
                .into_iter()
                .map(|pair| match pair.split_once('=') {
                    Some((key, section)) if !key.is_empty() && !section.is_empty() => {
                        Ok((key.to_owned(), section.to_owned()))
                    }
                    _ => Err(usage(format!("--map takes KEY=SECTION, not {pair:?}"))),
                })
                .collect::<Result<_, _>>()?;
            Ok(Cmd::Import {
                path: PathBuf::from(path),
                map,
                dry_run: p.flag("dry-run"),
            })
        }
        "proto" => {
            no_extra(rest)?;
            Ok(Cmd::Proto)
        }
        "help" | "--help" | "-h" => Err(usage("")),
        other => Err(usage(format!("unknown command {other:?}"))),
    }
}

fn parse_decide(rest: &[String]) -> Result<Cmd, Usage> {
    let spec = Spec {
        values: &[
            "title",
            "summary",
            "choice",
            "recommend",
            "default",
            "expires",
        ],
        bools: &["no-text", "wait"],
    };
    let mut p = split_flags(rest, &spec)?;
    let task = take_id(&mut p.positionals, 0);
    no_extra(&p.positionals)?;
    let title = p
        .value("title")
        .filter(|title| !title.trim().is_empty())
        .ok_or_else(|| usage("decide needs --title"))?
        .to_owned();
    let recommend = p.value("recommend");
    let mut choices = Vec::new();
    for value in p.all("choice") {
        let mut parts = value.splitn(3, ':');
        let id = parts.next().unwrap_or_default().trim().to_owned();
        let label = parts.next().map(str::trim).unwrap_or_default().to_owned();
        if id.is_empty() || label.is_empty() {
            return Err(usage(format!(
                "--choice takes ID:LABEL[:CONSEQUENCE], not {value:?}"
            )));
        }
        let consequence = parts
            .next()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_owned);
        choices.push(Choice {
            recommended: recommend == Some(id.as_str()),
            id,
            label,
            consequence,
        });
    }
    if choices.is_empty() {
        return Err(usage("decide needs at least one --choice ID:LABEL"));
    }
    if let Some(recommend) = recommend {
        if !choices.iter().any(|c| c.id == recommend) {
            return Err(usage(format!("--recommend {recommend} is not a choice")));
        }
    }
    let expires_at =
        p.value("expires")
            .map(|minutes| {
                let minutes: i64 =
                    minutes.parse().ok().filter(|m| *m > 0).ok_or_else(|| {
                        usage(format!("--expires takes minutes, not {minutes:?}"))
                    })?;
                Ok(time_text(
                    time::OffsetDateTime::now_utc() + time::Duration::minutes(minutes),
                ))
            })
            .transpose()?;
    let wait = p.wait.map(|secs| secs.unwrap_or(DECIDE_WAIT_SECS));
    Ok(Cmd::Decide {
        op: TaskOp::Decide {
            task,
            title,
            summary: p.value("summary").unwrap_or_default().to_owned(),
            choices,
            default_choice: p.value("default").map(str::to_owned),
            allow_text: !p.flag("no-text"),
            expires_at,
            wait_secs: wait,
        },
        wait,
    })
}

fn parse_kind(text: &str) -> Result<Kind, Usage> {
    Kind::parse(text).ok_or_else(|| usage(format!("unknown kind {text:?}")))
}

fn parse_priority(text: &str) -> Result<Priority, Usage> {
    Priority::parse(text).ok_or_else(|| usage(format!("unknown priority {text:?}")))
}

fn parse_position(text: &str) -> Result<i64, Usage> {
    text.parse()
        .ok()
        .filter(|n| *n >= 1)
        .ok_or_else(|| usage(format!("criterion number expected, not {text:?}")))
}

fn parse_verdict(text: &str) -> Result<CheckState, Usage> {
    match text.to_ascii_lowercase().as_str() {
        "pass" | "passed" => Ok(CheckState::Passed),
        "fail" | "failed" => Ok(CheckState::Failed),
        "open" => Ok(CheckState::Open),
        _ => Err(usage(format!("verdict is pass or fail, not {text:?}"))),
    }
}

// Execution

struct Failure {
    code: i32,
    message: String,
}

fn fail(code: i32, message: impl Into<String>) -> Failure {
    Failure {
        code,
        message: message.into(),
    }
}

impl From<StoreError> for Failure {
    fn from(err: StoreError) -> Self {
        let message = err.to_string();
        fail(exit_code(&Err(err)), message)
    }
}

type Exec = Result<i32, Failure>;

fn execute(command: Cmd, env: &Env, out: &mut dyn Write) -> Exec {
    if command == Cmd::Proto {
        let _ = writeln!(out, "drovr-task {OUTBOX_V}");
        return Ok(0);
    }
    let mode = env.mode();
    match mode {
        Mode::Db => execute_db(command, env, out),
        Mode::Outbox => execute_outbox(command, env, out),
    }
}

fn open_db(env: &Env, create: bool) -> Result<TaskStore, Failure> {
    if create {
        return Ok(TaskStore::open(&env.db_path, CLI_BUSY_MS)?);
    }
    TaskStore::open_existing(&env.db_path, CLI_BUSY_MS)?
        .ok_or_else(|| fail(1, format!("no tasks db at {}", env.db_path.display())))
}

/// The task id: the argument, else $DROVR_TASK, else the open attempt on
/// this pane (db mode).
fn resolve_id(id: Option<String>, env: &Env, store: Option<&TaskStore>) -> Result<String, Failure> {
    if let Some(id) = id.or_else(|| env.task.clone()) {
        return Ok(id.trim().to_ascii_uppercase());
    }
    if let (Some(store), Some(pane)) = (store, env.pane_key()) {
        if let Some(task) = store.task_for_pane(&pane)? {
            return Ok(task.display_id);
        }
    }
    Err(fail(2, "no task id: pass ID or set DROVR_TASK"))
}

fn print_result(out: &mut dyn Write, result: &OpResult, json: bool) -> i32 {
    if json {
        let _ = writeln!(out, "{}", serde_json::to_string(result).unwrap_or_default());
    } else {
        let _ = writeln!(out, "{}", result.message);
    }
    result.exit()
}

fn execute_db(command: Cmd, env: &Env, out: &mut dyn Write) -> Exec {
    let actor = env.actor(Mode::Db);
    let creates = matches!(command, Cmd::Add { .. } | Cmd::Import { .. }) && actor == Actor::Human;
    let store = open_db(env, creates)?;
    let ctx = OpContext {
        actor: actor.clone(),
        machine: "local".into(),
        pane_key: env.pane_key(),
    };
    match command {
        Cmd::List {
            project,
            statuses,
            all,
            json,
        } => {
            let cards = store.list(&list_filter(project, statuses, all))?;
            print_cards(out, &cards, json);
            Ok(0)
        }
        Cmd::Show { id, json } => {
            let id = resolve_id(id, env, Some(&store))?;
            let detail = store
                .task_detail(&id)?
                .ok_or_else(|| fail(4, format!("no task {id}")))?;
            print_detail(out, &detail, json);
            Ok(0)
        }
        Cmd::Add {
            title,
            project,
            body,
            kind,
            priority,
            criteria,
            json,
        } => {
            let project = match project {
                Some(project) => project,
                None => default_project(env, &store)?,
            };
            let op = TaskOp::Add {
                project,
                title: Some(title),
                body,
                kind,
                priority,
                criteria,
            };
            apply_db(&store, op, &ctx, out, json)
        }
        Cmd::Op { mut op, json } => {
            let id = resolve_id(op.task().map(str::to_owned), env, Some(&store))?;
            op.set_task(id);
            fill_harness(&mut op, env);
            apply_db(&store, op, &ctx, out, json)
        }
        Cmd::Artifact {
            id,
            target,
            title,
            kind,
            summary,
        } => {
            let id = resolve_id(id, env, Some(&store))?;
            let op = artifact_op(id, &target, title, kind, summary, &env.cwd);
            apply_db(&store, op, &ctx, out, false)
        }
        Cmd::Verify { id, positions } => {
            let id = resolve_id(id, env, Some(&store))?;
            let detail = store
                .task_detail(&id)?
                .ok_or_else(|| fail(4, format!("no task {id}")))?;
            let mut worst = 0;
            for (position, cmd) in verify_targets(&detail, &positions)? {
                let op = verify_one(&id, position, &cmd, &env.cwd);
                let code = apply_db(&store, op, &ctx, out, false)?;
                worst = worst.max(code);
            }
            Ok(worst)
        }
        Cmd::Decide { mut op, wait } => {
            let id = resolve_id(op.task().map(str::to_owned), env, Some(&store))?;
            op.set_task(id);
            let result = OpResult::of(&store.apply(&op, &ctx), op.task());
            let code = print_result(out, &result, false);
            match (wait, result.decision_id) {
                (Some(secs), Some(decision_id)) if result.ok => {
                    wait_db(&store, decision_id, secs, out)?;
                    Ok(0)
                }
                _ => Ok(code),
            }
        }
        Cmd::Import { path, map, dry_run } => {
            let report = store.import_workspace(&path, &map, dry_run)?;
            let _ = writeln!(
                out,
                "imported {} tasks into {} projects ({} skipped)",
                report.tasks, report.projects, report.skipped
            );
            Ok(0)
        }
        Cmd::Proto => Ok(0),
    }
}

fn apply_db(
    store: &TaskStore,
    op: TaskOp,
    ctx: &OpContext,
    out: &mut dyn Write,
    json: bool,
) -> Exec {
    let result = store.apply(&op, ctx);
    if let Err(err @ (StoreError::Busy | StoreError::Sqlite(_) | StoreError::TooNew { .. })) =
        result
    {
        return Err(err.into());
    }
    let result = OpResult::of(&result, op.task());
    Ok(print_result(out, &result, json))
}

/// Polls the decision until it leaves `open` or `secs` pass; prints the
/// ruling or `waiting`. On timeout `wait_until` is cleared so the client
/// relays the ruling instead.
fn wait_db(
    store: &TaskStore,
    decision_id: i64,
    secs: u32,
    out: &mut dyn Write,
) -> Result<(), Failure> {
    let deadline = Instant::now() + Duration::from_secs(u64::from(secs));
    loop {
        let decision = store
            .decision(decision_id)?
            .ok_or_else(|| fail(4, format!("no decision {decision_id}")))?;
        if decision.state != DecisionState::Open {
            let _ = writeln!(out, "{}", ruling_text(&decision));
            return Ok(());
        }
        if Instant::now() >= deadline {
            store.set_decision_wait(decision_id, None)?;
            let _ = writeln!(out, "waiting");
            return Ok(());
        }
        std::thread::sleep(WAIT_POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

fn ruling_text(decision: &super::Decision) -> String {
    decision
        .ruling_line()
        .unwrap_or_else(|| decision.state.as_str().to_owned())
}

fn fill_harness(op: &mut TaskOp, env: &Env) {
    if let TaskOp::Start { harness, .. } = op {
        if harness.trim().is_empty() {
            *harness = env.agent.clone().unwrap_or_else(|| "claude".into());
        }
    }
}

/// The section of the task that runs in this workspace, else the project
/// of $DROVR_TASK.
fn default_project(env: &Env, store: &TaskStore) -> Result<String, Failure> {
    if let Some(workspace) = &env.workspace {
        let prefix = format!("local/{workspace}:");
        let cards = store.list(&TaskFilter {
            workspace_key: Some(prefix),
            ..TaskFilter::default()
        })?;
        if let Some(card) = cards
            .iter()
            .find(|card| card.live.is_some())
            .or(cards.first())
        {
            if let Some(detail) = store.task_detail(&card.task.display_id)? {
                return Ok(detail.project.name);
            }
        }
    }
    if let Some(id) = &env.task {
        if let Some(detail) = store.task_detail(id)? {
            return Ok(detail.project.name);
        }
    }
    Err(fail(2, "no project: pass --project NAME"))
}

fn list_filter(project: Option<String>, statuses: Vec<Status>, all: bool) -> TaskFilter {
    let statuses = if statuses.is_empty() && !all {
        Status::LANES
            .into_iter()
            .filter(|status| !status.is_closed())
            .collect()
    } else {
        statuses
    };
    TaskFilter {
        project,
        statuses,
        include_archived: all,
        ..TaskFilter::default()
    }
}

fn print_cards(out: &mut dyn Write, cards: &[TaskCard], json: bool) {
    if json {
        let _ = writeln!(out, "{}", serde_json::to_string(cards).unwrap_or_default());
        return;
    }
    for card in cards {
        let mut line = format!(
            "{} {}  {}",
            card.task.display_id,
            card.task.status.as_str(),
            card.task.name()
        );
        if card.criteria_total > 0 {
            line.push_str(&format!(
                "  ✓{}/{}",
                card.criteria_passed, card.criteria_total
            ));
        }
        if card.open_decision {
            line.push_str("  ?");
        }
        if let Some((harness, machine, _)) = &card.live {
            line.push_str(&format!("  ● {harness}@{machine}"));
        }
        let _ = writeln!(out, "{line}");
    }
}

fn print_detail(out: &mut dyn Write, detail: &TaskDetail, json: bool) {
    if json {
        let _ = writeln!(out, "{}", serde_json::to_string(detail).unwrap_or_default());
        return;
    }
    let task = &detail.task;
    let _ = writeln!(
        out,
        "{} {}  {}",
        task.display_id,
        task.status.as_str(),
        task.name()
    );
    let mut meta = vec![format!("project {}", detail.project.name)];
    if let Some(kind) = task.kind {
        meta.push(format!("kind {}", kind.as_str()));
    }
    meta.push(format!("priority {}", task.priority.as_str()));
    if let Some(attempt) = detail.attempts.iter().find(|a| a.ended_at.is_none()) {
        meta.push(format!("live {}@{}", attempt.harness, attempt.machine));
    }
    let _ = writeln!(out, "{}", meta.join("  "));
    if !task.body.trim().is_empty() {
        let _ = writeln!(out, "\n{}", task.body.trim_end());
    }
    if !detail.criteria.is_empty() {
        let _ = writeln!(out, "\nCriteria");
        for criterion in &detail.criteria {
            let mark = match criterion.state {
                CheckState::Passed => "✓",
                CheckState::Failed => "✗",
                CheckState::Open => "○",
            };
            let check = criterion
                .check_cmd
                .as_deref()
                .map(|cmd| format!("  (check: `{cmd}`)"))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "  {mark} {}. {}{check}",
                criterion.position, criterion.text
            );
        }
    }
    if let Some(decision) = detail
        .decision
        .as_ref()
        .filter(|d| d.state == DecisionState::Open)
    {
        let _ = writeln!(out, "\nDecision {} (open): {}", decision.id, decision.title);
        for choice in &decision.choices {
            let rec = if choice.recommended { "  (rec)" } else { "" };
            let _ = writeln!(out, "  {} {}{rec}", choice.id, choice.label);
        }
    }
    let notes: Vec<_> = detail
        .entries
        .iter()
        .filter(|entry| entry.kind != super::EntryKind::Event)
        .collect();
    if !notes.is_empty() {
        let _ = writeln!(out, "\nNotes");
        for entry in notes.iter().rev().take(10).rev() {
            let _ = writeln!(
                out,
                "  {}: {}",
                entry.author,
                entry.body.replace('\n', "\n    ")
            );
        }
    }
}

fn artifact_op(
    id: String,
    target: &str,
    title: Option<String>,
    kind: Option<ArtifactKind>,
    summary: Option<String>,
    cwd: &Path,
) -> TaskOp {
    let is_url = target.starts_with("http://") || target.starts_with("https://");
    let target = if is_url || Path::new(target).is_absolute() {
        target.to_owned()
    } else {
        cwd.join(target).to_string_lossy().into_owned()
    };
    let lower = target.to_ascii_lowercase();
    let kind = kind.unwrap_or(if is_url {
        ArtifactKind::Link
    } else if lower.ends_with(".md") {
        ArtifactKind::Doc
    } else if lower.ends_with(".diff") || lower.ends_with(".patch") {
        ArtifactKind::Diff
    } else {
        ArtifactKind::File
    });
    let title = title.unwrap_or_else(|| {
        if is_url {
            target.clone()
        } else {
            Path::new(&target)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| target.clone())
        }
    });
    TaskOp::Artifact {
        task: Some(id),
        kind,
        title,
        target,
        summary,
    }
}

/// (position, command) of the criteria to verify: the given ones, else
/// every criterion with a check command.
fn verify_targets(detail: &TaskDetail, positions: &[i64]) -> Result<Vec<(i64, String)>, Failure> {
    if positions.is_empty() {
        let targets: Vec<_> = detail
            .criteria
            .iter()
            .filter_map(|c| c.check_cmd.clone().map(|cmd| (c.position, cmd)))
            .collect();
        if targets.is_empty() {
            return Err(fail(
                2,
                format!(
                    "{} has no criteria with a check command",
                    detail.task.display_id
                ),
            ));
        }
        return Ok(targets);
    }
    positions
        .iter()
        .map(|&position| {
            let criterion = detail
                .criteria
                .iter()
                .find(|c| c.position == position)
                .ok_or_else(|| {
                    fail(
                        2,
                        format!("{} has no criterion {position}", detail.task.display_id),
                    )
                })?;
            let cmd = criterion
                .check_cmd
                .clone()
                .ok_or_else(|| fail(2, format!("criterion {position} has no check command")))?;
            Ok((position, cmd))
        })
        .collect()
}

/// Runs one check command and returns the `Check` op with its verdict.
fn verify_one(id: &str, position: i64, cmd: &str, cwd: &Path) -> TaskOp {
    let (code, output) = run_check(cmd, cwd, VERIFY_LIMIT);
    let output = tail(&output, MAX_TEXT - cmd.len().min(MAX_TEXT / 2) - 64);
    let state = if code.as_deref() == Some("0") {
        CheckState::Passed
    } else {
        CheckState::Failed
    };
    let code = code.unwrap_or_else(|| "timeout".into());
    TaskOp::Check {
        task: Some(id.to_owned()),
        position,
        state,
        evidence: Some(format!("$ {cmd}\n{}\nexit {code}", output.trim_end())),
    }
}

/// `sh -c CMD` with a time limit; (exit code text, stdout + stderr).
/// None for the code when the limit killed it.
fn run_check(cmd: &str, cwd: &Path, limit: Duration) -> (Option<String>, String) {
    let child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(err) => return (Some("127".into()), format!("cannot run sh: {err}")),
    };
    let readers: Vec<_> = [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|mut pipe| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            bytes
        })
    })
    .collect();
    let start = Instant::now();
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                break Some(
                    status
                        .code()
                        .map_or_else(|| "signal".to_owned(), |c| c.to_string()),
                )
            }
            Ok(None) if start.elapsed() < limit => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let mut output = String::new();
    for reader in readers {
        let bytes = reader.join().unwrap_or_default();
        output.push_str(&String::from_utf8_lossy(&bytes));
    }
    (code, output)
}

/// The last `max` bytes of `text`, on a char boundary, with `…` in front
/// when cut.
fn tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

// Outbox mode

fn execute_outbox(command: Cmd, env: &Env, out: &mut dyn Write) -> Exec {
    let pane = env
        .pane
        .clone()
        .ok_or_else(|| fail(1, format!("no tasks db at {}", env.db_path.display())))?;
    let task_env = env.task.clone();
    let id_or_env = |id: Option<String>| {
        id.or_else(|| task_env.clone())
            .map(|id| id.to_ascii_uppercase())
    };
    match command {
        Cmd::List {
            project,
            statuses,
            all,
            json,
        } => {
            let details = outbox::snapshots(&env.outbox);
            if details.is_empty() {
                return Err(fail(4, "no task data on this machine yet"));
            }
            let filter = list_filter(project, statuses, all);
            let cards: Vec<TaskCard> = details
                .into_iter()
                .filter(|d| filter.project.as_ref().is_none_or(|p| &d.project.name == p))
                .filter(|d| filter.statuses.is_empty() || filter.statuses.contains(&d.task.status))
                .filter(|d| filter.include_archived || d.task.archived_at.is_none())
                .map(card_of)
                .collect();
            print_cards(out, &cards, json);
            Ok(0)
        }
        Cmd::Show { id, json } => {
            let id =
                id_or_env(id).ok_or_else(|| fail(2, "no task id: pass ID or set DROVR_TASK"))?;
            let detail = outbox::snapshot(&env.outbox, &id)
                .ok_or_else(|| fail(4, "no task data on this machine yet"))?;
            print_detail(out, &detail, json);
            Ok(0)
        }
        Cmd::Add {
            title,
            project,
            body,
            kind,
            priority,
            criteria,
            json,
        } => {
            let project = project
                .or_else(|| {
                    task_env
                        .as_deref()
                        .and_then(|id| outbox::snapshot(&env.outbox, id))
                        .map(|detail| detail.project.name)
                })
                .ok_or_else(|| fail(2, "no project: pass --project NAME"))?;
            let op = TaskOp::Add {
                project,
                title: Some(title),
                body,
                kind,
                priority,
                criteria,
            };
            queue_and_wait(env, &pane, op, out, json).map(|(code, _)| code)
        }
        Cmd::Op { mut op, json } => {
            if let Some(id) = id_or_env(op.task().map(str::to_owned)) {
                op.set_task(id);
            }
            fill_harness(&mut op, env);
            queue_and_wait(env, &pane, op, out, json).map(|(code, _)| code)
        }
        Cmd::Artifact {
            id,
            target,
            title,
            kind,
            summary,
        } => {
            let id = id_or_env(id).unwrap_or_default();
            let mut op = artifact_op(id, &target, title, kind, summary, &env.cwd);
            if let TaskOp::Artifact { task, .. } = &mut op {
                if task.as_deref() == Some("") {
                    *task = None;
                }
            }
            queue_and_wait(env, &pane, op, out, false).map(|(code, _)| code)
        }
        Cmd::Verify { id, positions } => {
            let id =
                id_or_env(id).ok_or_else(|| fail(2, "no task id: pass ID or set DROVR_TASK"))?;
            let detail = outbox::snapshot(&env.outbox, &id)
                .ok_or_else(|| fail(4, "no task data on this machine yet"))?;
            let mut worst = 0;
            for (position, cmd) in verify_targets(&detail, &positions)? {
                let op = verify_one(&id, position, &cmd, &env.cwd);
                let (code, _) = queue_and_wait(env, &pane, op, out, false)?;
                worst = worst.max(code);
            }
            Ok(worst)
        }
        Cmd::Decide { mut op, wait } => {
            if let Some(id) = id_or_env(op.task().map(str::to_owned)) {
                op.set_task(id);
            }
            let title = match &op {
                TaskOp::Decide { title, .. } => title.clone(),
                _ => String::new(),
            };
            let task = op.task().map(str::to_owned);
            let (code, reply) = queue_and_wait(env, &pane, op, out, false)?;
            let Some(secs) = wait else {
                return Ok(code);
            };
            if reply.as_ref().is_some_and(|reply| !reply.result.ok) {
                return Ok(code);
            }
            wait_outbox(env, &pane, reply, task.as_deref(), &title, secs, out);
            Ok(0)
        }
        Cmd::Import { .. } => Err(fail(1, format!("no tasks db at {}", env.db_path.display()))),
        Cmd::Proto => Ok(0),
    }
}

fn card_of(detail: TaskDetail) -> TaskCard {
    let count = |state| detail.criteria.iter().filter(|c| c.state == state).count() as u32;
    TaskCard {
        criteria_total: detail.criteria.len() as u32,
        criteria_passed: count(CheckState::Passed),
        criteria_failed: count(CheckState::Failed),
        open_decision: detail
            .decision
            .as_ref()
            .is_some_and(|d| d.state == DecisionState::Open),
        last_outcome: detail
            .attempts
            .iter()
            .find(|a| a.ended_at.is_some())
            .and_then(|a| a.outcome),
        live: detail
            .attempts
            .iter()
            .find(|a| a.ended_at.is_none())
            .map(|a| (a.harness.clone(), a.machine.clone(), a.pane_key.clone())),
        task: detail.task,
    }
}

/// Queues the op, rings the client and waits for its reply. Prints the
/// reply, else `{id} queued`.
fn queue_and_wait(
    env: &Env,
    pane: &str,
    op: TaskOp,
    out: &mut dyn Write,
    json: bool,
) -> Result<(i32, Option<Reply>), Failure> {
    let queued = outbox::queue(&env.outbox, pane, &op)
        .map_err(|err| fail(1, format!("cannot queue the task op: {err}")))?;
    if let Some(herdr) = &env.herdr {
        outbox::ring(herdr, pane, &queued);
    }
    let reply: Option<Reply> = outbox::wait_json(
        &outbox::reply_path(&env.outbox, pane, &queued),
        env.reply_wait,
    );
    match reply {
        Some(reply) => {
            print_result(out, &reply.result, json);
            Ok((reply.exit, Some(reply)))
        }
        None => {
            let _ = writeln!(out, "{} queued", op.task().unwrap_or("task"));
            Ok((0, None))
        }
    }
}

fn wait_outbox(
    env: &Env,
    pane: &str,
    reply: Option<Reply>,
    task: Option<&str>,
    title: &str,
    secs: u32,
    out: &mut dyn Write,
) {
    let deadline = Instant::now() + Duration::from_secs(u64::from(secs));
    let mut decision_id = reply.and_then(|reply| reply.result.decision_id);
    loop {
        if decision_id.is_none() {
            // No first reply: learn the id from the snapshot.
            decision_id = task
                .and_then(|id| outbox::snapshot(&env.outbox, id))
                .and_then(|detail| detail.decision)
                .filter(|d| d.title == title.trim())
                .map(|d| d.id);
        }
        if let Some(id) = decision_id {
            let path = outbox::decision_reply_path(&env.outbox, pane, id);
            if let Some(ruling) = outbox::wait_json::<Reply>(&path, Duration::ZERO) {
                let _ = writeln!(out, "{}", ruling.result.message);
                return;
            }
        }
        if Instant::now() >= deadline {
            let _ = writeln!(out, "waiting");
            return;
        }
        std::thread::sleep(WAIT_POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
