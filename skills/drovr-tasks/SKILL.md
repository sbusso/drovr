---
name: drovr-tasks
description: Report progress on a drovr task with the drovr task command. Load when DROVR_TASK is set in your environment or your first prompt names a drovr task (an id like AC-12), so notes, criterion verdicts, documents, decisions and the final outcome reach the task board instead of only the terminal.
---

# drovr tasks

drovr keeps a board of tasks per project. When drovr starts you on a task, it
sets `DROVR_TASK` to the task id (for example `AC-12`) and writes a context
file with the task, its acceptance criteria, pinned notes and recent messages.
You report back with `drovr task`. Commands without an id use `DROVR_TASK`.

## Start

1. Read the context file named in your first prompt. It is
   `~/.local/state/herdr/drovr/tasks/<id>.md` when the prompt does not say.
2. Run `drovr task show` to see the current state, including criteria and
   any open decision.

You do not need to run `drovr task start`; drovr records the attempt when it
starts you. Run it only when you pick up a task by hand.

## Report progress

Add a note at milestones, not at every step:

```bash
drovr task note "Schema and store done; starting the CLI."
```

Use `-` to pass longer text on stdin:

```bash
drovr task note - <<'EOF'
Found two callers of the old API; both moved to the new one.
EOF
```

## Acceptance criteria

Every criterion needs a verdict with evidence before you finish.

- For criteria that have a check command, run them:

  ```bash
  drovr task verify          # every criterion with a check command
  drovr task verify 2 3      # only criteria 2 and 3
  ```

  `verify` runs each command in the current directory and records pass or
  fail with the command output as evidence.
- For every other criterion, record the verdict yourself with the command
  output or a one-line reason as evidence:

  ```bash
  drovr task check 1 pass --evidence "cargo test: 41 passed"
  drovr task check 4 fail --evidence "docs/api.md still describes v1"
  ```

Do not mark a criterion passed without evidence.

To add a criterion, with an optional check command:

```bash
drovr task criteria --add "lint is clean (check: cargo clippy -- -D warnings)"
```

## Documents

Attach documents you write for the user, and open them with the drovr-docs
skill as before:

```bash
drovr task artifact docs/design/decisions.md --summary "storage options compared"
```

`--kind` defaults to `doc` for `.md` files, `link` for URLs and `diff` for
`.diff` or `.patch` files.

## Ask for a decision

Ask only when you are blocked and cannot choose yourself. Keep the title under
120 characters, give 2 to 4 choices, recommend one, and wait for the answer:

```bash
drovr task decide --title "Which table holds decisions?" \
  --summary "One open decision per task; the board shows it." \
  --choice new:"New decisions table":"one more migration" \
  --choice reuse:"Reuse entries":"no migration, harder queries" \
  --recommend new --wait
```

`--wait` prints the ruling, for example `ruled new: New decisions table`, or
`waiting` when nobody answered in time. After `waiting`, continue with other
work; the answer arrives in your pane as a message.

## Finish

When every criterion passed:

```bash
drovr task done --outcome succeeded --note "All criteria pass."
```

A refusal lists the criteria that are still open or failed; fix them and run
`done` again. Otherwise finish with `failed`, `stopped` or `needs_human` and a
note that says why:

```bash
drovr task done --outcome needs_human --note "Needs a decision on the API shape."
```

To hand the task back without finishing it, use
`drovr task release --note "<why>"`.

Never move a task to done or cancelled. The user closes tasks.

## When drovr task is missing

If `drovr task` answers `unknown command` or is not installed, say so once in
your reply and continue the work. Do not retry.

## Reference

| Command | Purpose |
|---------|---------|
| `drovr task show [ID] [--json]` | Task, criteria, decision, recent notes |
| `drovr task list [--project NAME] [--status S,S] [--all]` | Tasks of a project |
| `drovr task note [ID] TEXT\|-` | Add a note |
| `drovr task check [ID] N pass\|fail --evidence TEXT\|-` | Record a verdict |
| `drovr task verify [ID] [N]...` | Run check commands and record verdicts |
| `drovr task criteria [ID] --add TEXT` | Add a criterion |
| `drovr task artifact [ID] PATH\|URL [--title T] [--summary S]` | Attach a document or link |
| `drovr task decide [ID] --title T --choice ID:LABEL[:CONSEQUENCE]... [--recommend ID] [--wait]` | Ask the user to decide |
| `drovr task done [ID] --outcome O [--note TEXT]` | Finish the attempt |
| `drovr task release [ID] --note TEXT` | Hand the task back |

Exit codes: 0 done, 1 error, 2 usage, 3 refused, 4 not found. On a remote
machine an op that the client has not answered yet prints `<id> queued` and
exits 0; drovr applies it on its next pull.
