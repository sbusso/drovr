//! drovr fork: answers sent from the inbox (docs/design/inbox-pane.md,
//! section 9).
//!
//! Three routes, all run on a background thread against the agent's machine
//! (the local socket and shell, or the endpoint's SSH bridge):
//!
//! - Hook decision ([`Action::Decide`]): Claude's drovr-state-hook waits on
//!   `<state>/decide/<req>.json`. After `agent.get` shows the agent blocked on
//!   the same request, a shell script writes the file (temporary name, then
//!   rename) when the hook's pane state still lists the request as pending.
//!   A decision is bound to one hook invocation, so it never answers a later
//!   prompt.
//! - Option keys ([`Action::Keys`]): `agent.get` (blocked, same
//!   `state_change_seq`, same request), two `pane.read`s that both show the
//!   item's text and the chosen label, then `agent.send_keys` with that
//!   option's on-screen number. Keys map to labels, never to positions.
//! - Replies ([`Action::Prompt`]): `agent.prompt`, unless the agent is
//!   working or blocked.
//!
//! A failed check sends nothing and reports [`CHANGED`].

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};

use crate::api::client::ApiClient;
use crate::api::schema::{
    AgentPromptParams, AgentSendKeysParams, AgentTarget, Method, PaneReadParams, ReadFormat,
    ReadIntent, ReadSource, Request,
};

use super::inbox::{ApiRoute, InboxReply};

/// What a failed check reports; the inbox shows it on the item.
pub(crate) const CHANGED: &str = "Changed in the terminal. Jump to see it.";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Exit status of the scripts when the request is no longer pending.
const NOT_PENDING: i32 = 3;
/// Longest note the hook accepts as a deny message.
pub(crate) const NOTE_MAX_BYTES: usize = 4000;
/// Largest plan file read.
const PLAN_MAX_BYTES: usize = 1024 * 1024;

/// A hook decision (section 9, step 3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Allow,
    /// Allow with the request's `permission_suggestions`.
    Always,
    /// Deny, with a note for the agent (the hook's default when empty).
    Deny(String),
}

impl Decision {
    fn json(&self) -> String {
        match self {
            Self::Allow => json!({ "behavior": "allow" }),
            Self::Always => json!({ "behavior": "allow", "always": true }),
            Self::Deny(note) if note.trim().is_empty() => json!({ "behavior": "deny" }),
            Self::Deny(note) => json!({ "behavior": "deny", "message": note }),
        }
        .to_string()
    }
}

/// The on-screen option an answer key picks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Choice {
    /// A question option, by its label (`drovr_oN`).
    Label(String),
    /// Plan `y`: the "manually approve edits" option.
    ApprovePlan,
    /// Permission keys sent as keys (Codex): `y`, `a`, `n`.
    Yes,
    Always,
    No,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Decide {
        decision: Decision,
    },
    Keys {
        /// Text that must be on screen: the question or plan title, or the
        /// command of a permission.
        text: String,
        choice: Choice,
    },
    Prompt(String),
}

/// One answer for the item of `pane_id` at `seq` and request `wait_id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Answer {
    pub(crate) pane_id: String,
    pub(crate) seq: u64,
    pub(crate) wait_id: String,
    pub(crate) action: Action,
}

/// Work for a machine, run by [`spawn`].
#[derive(Clone, Debug)]
pub(crate) enum Task {
    Answer(Answer),
    /// Read the plan file of a pending ExitPlanMode request.
    FetchPlan {
        pane_id: String,
        req: String,
    },
}

/// Output of a shell script on the agent's machine.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ShOutput {
    pub(crate) code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

/// The agent's machine: herdr API calls and POSIX shell scripts.
pub(crate) trait Machine {
    /// The `result` of a successful API call, or the error message.
    fn call(&self, method: Method) -> Result<Value, String>;
    fn sh(&self, script: &str) -> Result<ShOutput, String>;
}

/// A machine reached through an [`ApiRoute`]. The API client is made once,
/// so the SSH bridge's status probe runs once per task.
pub(crate) struct RouteMachine {
    route: ApiRoute,
    client: OnceLock<Result<ApiClient, String>>,
}

impl RouteMachine {
    pub(crate) fn new(route: ApiRoute) -> Self {
        Self {
            route,
            client: OnceLock::new(),
        }
    }
}

impl Machine for RouteMachine {
    fn call(&self, method: Method) -> Result<Value, String> {
        let client = self
            .client
            .get_or_init(|| match &self.route {
                ApiRoute::Local => Ok(ApiClient::local()),
                ApiRoute::Remote(bridge) => bridge.api_client().map_err(|e| e.to_string()),
            })
            .clone()?;
        let request = Request {
            id: "drovr:inbox:answer".into(),
            method,
        };
        let value = client
            .request_value_with_timeout(&request, REQUEST_TIMEOUT)
            .map_err(|error| error.to_string())?;
        match value.get("error") {
            Some(error) => Err(error["message"]
                .as_str()
                .unwrap_or("request failed")
                .to_owned()),
            None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
        }
    }

    fn sh(&self, script: &str) -> Result<ShOutput, String> {
        let output = match &self.route {
            ApiRoute::Local => std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .stdin(std::process::Stdio::null())
                .output(),
            ApiRoute::Remote(bridge) => bridge.run_sh(script),
        }
        .map_err(|error| error.to_string())?;
        Ok(ShOutput {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

type LoopEvents = tokio::sync::mpsc::Sender<crate::client::events::ClientLoopEvent>;

/// Runs `task` on a background thread and posts the result to the client
/// loop as an inbox reply. An answer's result is `Ok(Null)`, or `Err` with
/// [`CHANGED`] when a check failed; a plan's is `{"path", "text"}`.
pub(crate) fn spawn(route: ApiRoute, task: Task, reply: InboxReply, events: Option<LoopEvents>) {
    std::thread::spawn(move || {
        let machine = RouteMachine::new(route);
        let result = match task {
            Task::Answer(answer) => run_answer(&machine, &answer).map(|()| Value::Null),
            Task::FetchPlan { pane_id, req } => fetch_plan(&machine, &pane_id, &req),
        };
        if let Err(error) = &result {
            // The reply may carry a draft: it is not logged.
            tracing::info!(%error, "inbox task did not complete");
        }
        if let Some(events) = events {
            let _ = events.blocking_send(crate::client::events::ClientLoopEvent::InboxReply {
                reply,
                result,
            });
        }
    });
}

/// Runs the checks of section 9 and sends the answer when they pass.
pub(crate) fn run_answer(machine: &dyn Machine, answer: &Answer) -> Result<(), String> {
    let changed = || CHANGED.to_owned();
    let agent = machine.call(Method::AgentGet(AgentTarget {
        target: answer.pane_id.clone(),
    }))?;
    let agent = &agent["agent"];
    let status = agent["agent_status"].as_str().unwrap_or_default();
    let wait_id = agent["tokens"]["drovr_wait"]
        .as_str()
        .and_then(|wait| wait.split('|').nth(1))
        .unwrap_or_default();
    let same_request =
        status == "blocked" && !answer.wait_id.is_empty() && wait_id == answer.wait_id;
    match &answer.action {
        Action::Decide { decision } => {
            if !same_request || !valid_request_id(&answer.wait_id) {
                return Err(changed());
            }
            let output = machine.sh(&decide_script(
                &answer.pane_id,
                &answer.wait_id,
                &decision.json(),
            ))?;
            match output.code {
                Some(0) => Ok(()),
                Some(NOT_PENDING) => Err(changed()),
                _ => Err(format!("decision not written: {}", output.stderr)),
            }
        }
        Action::Keys { text, choice } => {
            if !same_request || agent["state_change_seq"].as_u64() != Some(answer.seq) {
                return Err(changed());
            }
            let first = read_screen(machine, &answer.pane_id)?;
            let number = confirm(&first, text, choice).ok_or_else(changed)?;
            // Read again immediately before the send: the screen must still
            // show the same prompt and the same option number.
            let second = read_screen(machine, &answer.pane_id)?;
            if confirm(&second, text, choice) != Some(number) {
                return Err(changed());
            }
            machine
                .call(Method::AgentSendKeys(AgentSendKeysParams {
                    target: answer.pane_id.clone(),
                    keys: vec![number.to_string()],
                }))
                .map(drop)
        }
        Action::Prompt(text) => {
            if matches!(status, "blocked" | "working") {
                return Err(changed());
            }
            machine
                .call(Method::AgentPrompt(AgentPromptParams {
                    target: answer.pane_id.clone(),
                    text: text.clone(),
                    wait: None,
                }))
                .map(drop)
        }
    }
}

/// The bottom of the pane's screen, as detection sees it: a scrolled
/// viewport does not move it.
pub(crate) fn read_screen(machine: &dyn Machine, pane_id: &str) -> Result<String, String> {
    let read = machine.call(Method::PaneRead(screen_read_params(pane_id)))?;
    Ok(read["read"]["text"].as_str().unwrap_or_default().to_owned())
}

pub(crate) fn screen_read_params(pane_id: &str) -> PaneReadParams {
    PaneReadParams {
        pane_id: pane_id.to_owned(),
        source: ReadSource::Detection,
        lines: None,
        format: ReadFormat::Text,
        strip_ansi: true,
        intent: ReadIntent::Passive,
    }
}

/// `{"path", "text"}` of the plan file of request `req`, read on the agent's
/// machine from the hook's pane state.
fn fetch_plan(machine: &dyn Machine, pane_id: &str, req: &str) -> Result<Value, String> {
    if !valid_request_id(req) {
        return Err(CHANGED.to_owned());
    }
    let output = machine.sh(&plan_script(pane_id, req))?;
    match output.code {
        Some(0) => {
            let (path, text) = output
                .stdout
                .split_once('\n')
                .unwrap_or((&output.stdout, ""));
            Ok(json!({ "path": path, "text": text }))
        }
        Some(NOT_PENDING) => Err("the plan request has ended".to_owned()),
        Some(127) => Err("python3 not found on the agent's machine".to_owned()),
        _ => Err(format!("plan not read: {}", output.stderr)),
    }
}

// ------------------------------------------------------------------ scripts

/// Request ids come from pane tokens, which any client can set: only short
/// alphanumeric ids reach a file name.
fn valid_request_id(req: &str) -> bool {
    (1..=32).contains(&req.len()) && req.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The hook's per-pane file name (`re.sub(r"[^A-Za-z0-9_.-]", "_", pane)`).
fn pane_file(pane_id: &str) -> String {
    pane_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The hook's state directory, as drovr-state-hook computes it.
const STATE_ROOT: &str =
    "root=${DROVR_STATE_HOOK_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/herdr/drovr/state-hook}\n";

/// Writes the decision file for `req` when the pane state still lists it as
/// pending; exits [`NOT_PENDING`] otherwise. Ceiling: the pending check reads
/// the hook's JSON with grep (`"req": "<id>"`, json.dump's separators).
fn decide_script(pane_id: &str, req: &str, decision: &str) -> String {
    let quote = crate::remote::shell_quote;
    format!(
        "{STATE_ROOT}grep -q {needle} \"$root\"/{state} 2>/dev/null || exit {NOT_PENDING}\nmkdir -p \"$root/decide\" || exit 1\ntmp=\"$root/decide/.{req}.$$\"\nprintf '%s' {decision} > \"$tmp\" && mv -f \"$tmp\" \"$root/decide/{req}.json\"\n",
        needle = quote(&format!("\"req\": \"{req}\"")),
        state = quote(&format!("{}.json", pane_file(pane_id))),
        decision = quote(decision),
    )
}

/// Prints the plan file path of pending request `req`, a newline, then the
/// file (at most [`PLAN_MAX_BYTES`]); exits [`NOT_PENDING`] when the request
/// or its path is gone.
fn plan_script(pane_id: &str, req: &str) -> String {
    let quote = crate::remote::shell_quote;
    let program = format!(
        "import json, os, sys\ntry:\n    state = json.load(open(sys.argv[1]))\nexcept (OSError, ValueError):\n    sys.exit({NOT_PENDING})\nreqs = [r for r in state.get('pending') or [] if isinstance(r, dict) and r.get('req') == sys.argv[2]]\npath = reqs[0].get('plan_path') if reqs else None\nif not isinstance(path, str) or not path:\n    sys.exit({NOT_PENDING})\npath = os.path.expanduser(path)\nwith open(path, 'rb') as handle:\n    data = handle.read({PLAN_MAX_BYTES})\nsys.stdout.write(path + '\\n')\nsys.stdout.write(data.decode('utf-8', 'replace'))\n"
    );
    format!(
        "{STATE_ROOT}exec python3 -c {program} \"$root\"/{state} {req}\n",
        program = quote(&program),
        state = quote(&format!("{}.json", pane_file(pane_id))),
        req = quote(req),
    )
}

// ------------------------------------------------------------------ screen

/// Box drawing and selection marks around option lines.
fn frame_char(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '│' | '┃' | '|' | '❯' | '›' | '>' | '▶' | '●' | '○' | '◯' | '◉'
        )
}

/// Whitespace collapsed, frame characters dropped at line ends.
fn normalize(text: &str) -> String {
    text.lines()
        .map(|line| line.trim_matches(frame_char))
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The numbered options of the prompt at the bottom of `screen`: the last run
/// of lines `1. …`, `2. …` with consecutive numbers. Numbered lists above it
/// (a plan's steps) are not options.
pub(crate) fn screen_options(screen: &str) -> Vec<(u8, String)> {
    let mut runs: Vec<Vec<(u8, String)>> = Vec::new();
    for line in screen.lines() {
        let line = line.trim_matches(frame_char);
        let Some((number, label)) = line.split_once(". ") else {
            continue;
        };
        let Ok(number) = number.parse::<u8>() else {
            continue;
        };
        let label = normalize(label);
        if number == 1 {
            runs.push(vec![(1, label)]);
        } else if let Some(run) = runs
            .last_mut()
            .filter(|run| run.last().is_some_and(|(last, _)| *last + 1 == number))
        {
            run.push((number, label));
        }
    }
    runs.pop().unwrap_or_default()
}

/// A label that grants a session or persistent permission; the key that
/// picks it needs a second press.
pub(crate) fn grants(label: &str) -> bool {
    let label = label.to_lowercase().replace('’', "'");
    ["always", "don't ask again", "auto-accept", "auto accept"]
        .iter()
        .any(|word| label.contains(word))
}

fn chooses(label: &str, choice: &Choice) -> bool {
    let lower = label.to_lowercase();
    match choice {
        Choice::Label(want) => {
            let cut = want.ends_with('…');
            let want = normalize(want.trim_end_matches('…'));
            !want.is_empty()
                && if cut {
                    label.starts_with(&want)
                } else {
                    label == want
                }
        }
        Choice::ApprovePlan => lower.contains("manually approve"),
        Choice::Yes => first_word(&lower) == "yes" && !grants(label),
        Choice::Always => first_word(&lower) == "yes" && grants(label),
        Choice::No => first_word(&lower) == "no",
    }
}

fn first_word(label: &str) -> &str {
    label
        .split(|c: char| !c.is_alphanumeric())
        .next()
        .unwrap_or_default()
}

/// The on-screen number of the one option `choice` picks, if exactly one
/// matches.
pub(crate) fn pick(options: &[(u8, String)], choice: &Choice) -> Option<u8> {
    let mut found = options.iter().filter(|(_, label)| chooses(label, choice));
    match (found.next(), found.next()) {
        (Some((number, _)), None) => Some(*number),
        _ => None,
    }
}

/// Whether `screen` shows the item's `text` (an 80-character summary, cut
/// with `…`), whitespace and frames aside.
pub(crate) fn shows(screen: &str, text: &str) -> bool {
    let text = normalize(text.trim_end_matches('…'));
    !text.is_empty() && normalize(screen).contains(&text)
}

/// The number to send: `screen` shows `text` and exactly one option that
/// `choice` picks.
pub(crate) fn confirm(screen: &str, text: &str, choice: &Choice) -> Option<u8> {
    shows(screen, text)
        .then(|| pick(&screen_options(screen), choice))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const QUESTION: &str = "\
 ☐ Layout

Which layout for the inbox?

❯ 1. two panes
     Two panes side by side
  2. one pane
     A single pane
  3. Type something.
";

    /// A machine whose answers are set per test; it records every call.
    #[derive(Default)]
    struct Fake {
        status: &'static str,
        seq: u64,
        wait: String,
        screens: RefCell<Vec<String>>,
        sh_code: i32,
        calls: RefCell<Vec<String>>,
    }

    impl Fake {
        fn blocked(screens: &[&str]) -> Self {
            Self {
                status: "blocked",
                seq: 7,
                wait: "question|ab12cd34||Which layout for the inbox?".into(),
                screens: RefCell::new(screens.iter().rev().map(|s| (*s).to_owned()).collect()),
                ..Self::default()
            }
        }

        fn sent(&self) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .filter(|call| {
                    call.starts_with("send") || call.starts_with("prompt") || call.starts_with("sh")
                })
                .cloned()
                .collect()
        }
    }

    impl Machine for Fake {
        fn call(&self, method: Method) -> Result<Value, String> {
            match method {
                Method::AgentGet(_) => {
                    self.calls.borrow_mut().push("get".into());
                    Ok(json!({ "agent": {
                        "agent_status": self.status,
                        "state_change_seq": self.seq,
                        "tokens": { "drovr_wait": self.wait },
                    }}))
                }
                Method::PaneRead(params) => {
                    assert_eq!(params.source, ReadSource::Detection);
                    self.calls.borrow_mut().push("read".into());
                    let text = self.screens.borrow_mut().pop().unwrap_or_default();
                    Ok(json!({ "read": { "text": text } }))
                }
                Method::AgentSendKeys(params) => {
                    self.calls
                        .borrow_mut()
                        .push(format!("send {}", params.keys.join(" ")));
                    Ok(Value::Null)
                }
                Method::AgentPrompt(params) => {
                    self.calls
                        .borrow_mut()
                        .push(format!("prompt {}", params.text));
                    Ok(Value::Null)
                }
                other => panic!("unexpected {other:?}"),
            }
        }

        fn sh(&self, script: &str) -> Result<ShOutput, String> {
            self.calls.borrow_mut().push(format!("sh {script}"));
            Ok(ShOutput {
                code: Some(self.sh_code),
                ..ShOutput::default()
            })
        }
    }

    fn answer(action: Action) -> Answer {
        Answer {
            pane_id: "w1:p2".into(),
            seq: 7,
            wait_id: "ab12cd34".into(),
            action,
        }
    }

    fn option(label: &str) -> Action {
        Action::Keys {
            text: "Which layout for the inbox?".into(),
            choice: Choice::Label(label.into()),
        }
    }

    #[test]
    fn options_map_to_on_screen_labels_not_positions() {
        assert_eq!(
            screen_options(QUESTION),
            [
                (1, "two panes".to_owned()),
                (2, "one pane".to_owned()),
                (3, "Type something.".to_owned())
            ]
        );
        // The token order says "one pane" is option 1; the screen says 2.
        let fake = Fake::blocked(&[QUESTION, QUESTION]);
        assert_eq!(run_answer(&fake, &answer(option("one pane"))), Ok(()));
        assert_eq!(fake.sent(), ["send 2"]);
        // A cut label matches as a prefix; a full one must match exactly.
        assert_eq!(
            pick(&screen_options(QUESTION), &Choice::Label("two pa…".into())),
            Some(1)
        );
        assert_eq!(
            pick(&screen_options(QUESTION), &Choice::Label("two".into())),
            None
        );
    }

    #[test]
    fn a_plan_numbered_list_is_not_the_options() {
        let screen = "\
Ready to code?
 # Inbox answers
 1. Map labels
 2. Add checks

 Would you like to proceed?
 ❯ 1. Yes, and auto-accept edits
   2. Yes, and manually approve edits
   3. No, keep planning
";
        let options = screen_options(screen);
        assert_eq!(options.len(), 3);
        assert_eq!(pick(&options, &Choice::ApprovePlan), Some(2));
        assert!(grants(&options[0].1) && !grants(&options[1].1));
        assert_eq!(
            confirm(screen, "Inbox answers", &Choice::ApprovePlan),
            Some(2)
        );
        let codex = "\
 Allow command?
 $ cargo test --workspace
 › 1. Yes, proceed
   2. Yes, and don't ask again for this command
   3. No, and tell Codex what to do differently
";
        assert_eq!(
            confirm(codex, "cargo test --workspace", &Choice::Yes),
            Some(1)
        );
        assert_eq!(
            confirm(codex, "cargo test --workspace", &Choice::Always),
            Some(2)
        );
        assert_eq!(
            confirm(codex, "cargo test --workspace", &Choice::No),
            Some(3)
        );
        assert_eq!(confirm(codex, "cargo build", &Choice::Yes), None);
    }

    #[test]
    fn each_failed_check_sends_nothing() {
        let other_question = QUESTION.replace("Which layout", "Which colour");
        let moved = QUESTION
            .replace("1. two panes", "1. one pane")
            .replace("2. one pane", "2. two panes");
        let cases: Vec<(&str, Fake)> = vec![
            (
                "not blocked",
                Fake {
                    status: "working",
                    ..Fake::blocked(&[QUESTION, QUESTION])
                },
            ),
            (
                "new state",
                Fake {
                    seq: 8,
                    ..Fake::blocked(&[QUESTION, QUESTION])
                },
            ),
            (
                "new request",
                Fake {
                    wait: "question|ffff0000||Which layout for the inbox?".into(),
                    ..Fake::blocked(&[QUESTION, QUESTION])
                },
            ),
            (
                "other prompt on screen",
                Fake::blocked(&[&other_question, QUESTION]),
            ),
            (
                "label not on screen",
                Fake::blocked(&["Which layout for the inbox?\n1. three panes\n"]),
            ),
            (
                "changed before the send",
                Fake::blocked(&[QUESTION, &moved]),
            ),
            ("gone before the send", Fake::blocked(&[QUESTION, ""])),
        ];
        for (case, fake) in cases {
            assert_eq!(
                run_answer(&fake, &answer(option("one pane"))),
                Err(CHANGED.to_owned()),
                "{case}"
            );
            assert!(fake.sent().is_empty(), "{case}: {:?}", fake.sent());
        }
        // An empty item text (DROVR_STATE_TEXT=0) never answers by keys.
        let fake = Fake::blocked(&[QUESTION, QUESTION]);
        let blind = Action::Keys {
            text: String::new(),
            choice: Choice::Label("one pane".into()),
        };
        assert_eq!(run_answer(&fake, &answer(blind)), Err(CHANGED.to_owned()));
        assert!(fake.sent().is_empty());
    }

    #[test]
    fn decisions_and_replies_check_the_agent_first() {
        let deny = || Action::Decide {
            decision: Decision::Deny("use two panes".into()),
        };
        // Blocked on the same request: the script writes the file.
        let fake = Fake::blocked(&[]);
        assert_eq!(run_answer(&fake, &answer(deny())), Ok(()));
        let sent = fake.sent();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("decide/ab12cd34.json") && sent[0].contains("use two panes"));
        // Another request, or not blocked: nothing is written.
        for fake in [
            Fake {
                wait: "permission|ffff0000||Bash git push".into(),
                ..Fake::blocked(&[])
            },
            Fake {
                status: "idle",
                ..Fake::blocked(&[])
            },
        ] {
            assert_eq!(run_answer(&fake, &answer(deny())), Err(CHANGED.to_owned()));
            assert!(fake.sent().is_empty());
        }
        // The hook's state no longer lists the request.
        let fake = Fake {
            sh_code: NOT_PENDING,
            ..Fake::blocked(&[])
        };
        assert_eq!(run_answer(&fake, &answer(deny())), Err(CHANGED.to_owned()));
        // A request id that is not a plain id never reaches a file name.
        let fake = Fake {
            wait: "permission|../x||Bash".into(),
            ..Fake::blocked(&[])
        };
        let mut odd = answer(deny());
        odd.wait_id = "../x".into();
        assert_eq!(run_answer(&fake, &odd), Err(CHANGED.to_owned()));
        assert!(fake.sent().is_empty());

        // Replies: never into a working or blocked agent.
        let reply = || Action::Prompt("also update the docs".into());
        let fake = Fake::blocked(&[]);
        assert_eq!(run_answer(&fake, &answer(reply())), Err(CHANGED.to_owned()));
        assert!(fake.sent().is_empty());
        let fake = Fake {
            status: "done",
            ..Fake::blocked(&[])
        };
        assert_eq!(run_answer(&fake, &answer(reply())), Ok(()));
        assert_eq!(fake.sent(), ["prompt also update the docs"]);
    }

    #[cfg(unix)]
    #[test]
    fn scripts_write_decisions_for_pending_requests_only_and_read_plans() {
        let root = std::env::temp_dir().join(format!(
            "drovr-inbox-answer-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        let state = root.join("state");
        std::fs::create_dir_all(&state).expect("state dir");
        let run = |script: &str| {
            let output = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .env("DROVR_STATE_HOOK_DIR", &state)
                .output()
                .expect("run script");
            (
                output.status.code(),
                String::from_utf8_lossy(&output.stdout).into_owned(),
            )
        };
        let plan = root.join("plan.md");
        std::fs::write(&plan, "# Inbox answers\n\n1. Map labels\n").expect("plan");
        let pending = json!({ "pending": [{
            "req": "ab12cd34", "kind": "plan", "plan_path": plan.to_string_lossy(),
        }]});
        // json.dump's separators, as the hook writes them.
        let text = serde_json::to_string(&pending)
            .expect("json")
            .replace("\":", "\": ")
            .replace(",\"", ", \"");
        std::fs::write(state.join("w1_p2.json"), text).expect("pane state");

        let decision = Decision::Deny("it's \"fine\"".into()).json();
        assert_eq!(
            run(&decide_script("w1:p2", "ab12cd34", &decision)).0,
            Some(0)
        );
        let written = std::fs::read_to_string(state.join("decide/ab12cd34.json")).expect("file");
        assert_eq!(
            serde_json::from_str::<Value>(&written).expect("json"),
            json!({ "behavior": "deny", "message": "it's \"fine\"" })
        );
        // A finished request (not in the pending set) gets no file.
        assert_eq!(
            run(&decide_script("w1:p2", "deadbeef", &decision)).0,
            Some(NOT_PENDING)
        );
        assert!(!state.join("decide/deadbeef.json").exists());

        if std::process::Command::new("python3")
            .arg("-V")
            .output()
            .is_ok()
        {
            let (code, out) = run(&plan_script("w1:p2", "ab12cd34"));
            assert_eq!(code, Some(0));
            assert_eq!(
                out,
                format!("{}\n# Inbox answers\n\n1. Map labels\n", plan.display())
            );
            assert_eq!(run(&plan_script("w1:p2", "deadbeef")).0, Some(NOT_PENDING));
        }
        std::fs::remove_dir_all(&root).expect("cleanup");
    }
}
