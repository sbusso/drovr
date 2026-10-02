# Inbox pane and richer sidebar rows

Status: draft 3, 2026-10-02. Inputs: docs/reports/2026-10-02-mato-projects.md,
herdr 0.9.3 source, mato's Claude and Codex settings, and two hook experiments
with Claude Code 2.1.287 (section 11). Draft 3 records the review of draft 2;
see Decisions.

## 1. Goal and scope

drovr shows every agent on every machine, grouped by section (project) and
workspace (ongoing task). Two additions make that the daily workflow:

- **Inbox.** A pane that lists everything that needs you, on every machine,
  where you can answer without jumping to the agent pane.
- **Sidebar rows.** Each working agent shows what it is doing. Waiting agents
  show a glyph only; the detail lives in the inbox.

drovr owns these features. There is no separate server: live state stays in
herdr pane metadata, and durable task records come later, in a drovr-owned file
per project.

In scope for v1: items from herdr state plus hook tokens; permission answers
through a waiting PermissionRequest hook; question and plan answers by sending
keys; replies to finished agents; Claude Code and Codex through hooks; other
agents through screen state, jump-only.

Out of scope for v1: durable task records, digests, phone or web clients, a
separate task list.

mato runs Claude with `permissions.defaultMode = "auto"`, so permission items
are rare there; most mato items are asks, dialog, denied and finished.

## 2. Item kinds

One item per pane; a new state replaces the previous one. Sort order is the
order of this table, then oldest first.

| Kind | Glyph | Source | Answer keys |
|---|---|---|---|
| permission | `!` | PermissionRequest (Claude; Codex hook) | `y` once, `a` always (confirm), `n` no, `r` no with note |
| question | `?` | PermissionRequest AskUserQuestion, one question, single select | `1`-`4` option, `o` other (free text) |
| plan | `▤` | PermissionRequest ExitPlanMode; title and line count from `tool_input.plan` | `y` approve (manual edits), `r` keep planning with note, `p` read plan |
| asks | `?` | Stop whose last assistant message ends with `?` | `r` reply |
| dialog | `◆` | herdr blocked with no matching hook item: Codex without hook, multi-question or multi-select AskUserQuestion, MCP elicitation, trust prompts | jump only |
| stuck | `⧗` | herdr working, no hook event for the workspace's stuck threshold (section 7) | jump, `d` |
| limit | `◔` | `drovr_ctx` ≥ 85% of the model window; StopFailure `rate_limit` | jump, `d` |
| exited | `✗` | SessionEnd without Stop; pane process gone while tokens remain | `d` |
| denied | `⊘` | PermissionDenied (auto mode classifier) | `r` reply, `d` |
| finished | `✓` | herdr Done not yet seen; StopFailure shows as finished with error text | `r` reply, `d` |

Keys that send screen keys (question options, plan approval) map to on-screen
labels, never to positions. drovr reads the screen, finds the option whose
label matches the action (for `y` on a plan, the "manually approve edits"
option), and sends that option's number. A label or hook decision that grants a
session or persistent permission ("always", "don't ask again", "auto-accept")
needs a second press, whatever key reached it.

All kinds share `enter` (jump), `space` or `l` (expand), `d` (dismiss, not on
waiting kinds), `z` (snooze), `m` (mute workspace).

## 3. Placement

The inbox is a panel that the drovr client draws on the right side of the
screen, the same way it draws the sidebar. It looks and behaves like a pane,
but it is not a herdr pane and runs no process on any server. The client
already holds every endpoint's snapshot and SSH bridge, so the inbox works the
same whether the current workspace is local or on a remote machine.

- `prefix i` toggles it. When it is open, `prefix i` focuses it, or closes it
  when it already has focus.
- Clicking a sidebar glyph or a section count opens it, filtered to that
  workspace or project (section 4).
- Its width is a share of the screen: 40% by default, at least 48 columns.
  The user resizes it by dragging its left border; drovr saves the width in
  `sidebar.toml`. When the panes area would keep fewer than 32 columns, the
  inbox opens over the panes area instead of beside it, and closes on Esc.
- The panes area shrinks while the inbox is open; herdr's pane layout is
  recomputed for the narrower area, as it is when the sidebar width changes.

## 4. Layout

One line per item. The selected item expands to show its options and, after
`space`, the detail. The agent name appears only when it is not claude. The
project and machine form a dim chip at the right.

```
 Inbox  3 waiting · 4 done     [Waiting] Done All     ≡ group
 ! fix-ctrl-click  git push origin fix/ctrl-click  drovr·mac  4m
 ╭ ? inbox-design  Which layout for the inbox?      drovr·mac 12m
 │   1 one pane   2 two panes   o other
 ╰   enter jump  space more  ↗
 ◆ api-tests codex  allow command? cargo test      gtm·mato   2m
 ? spec-rewrite  Should I also update §39?          gtm·mato  20m
 ⧗ migrate-db  no activity for 14m                  gtm·mato  14m
 ✓ pane-layout  Layout tests pass, 3 files changed  drovr·mac  1h
 y yes  n no  r note  d dismiss  z snooze  ? keys
```

Below 60 columns the chip and age move to a second dim line under each item,
and the tab row shortens to `W D A`.

Expanded detail (`space`), rendered inside the item and scrollable:

- Bash: the full command from `pane.read`.
- Edit, Write: diffstat from the hook (`+12 −3 src/x.rs`), then the diff as
  Claude draws it on screen, read with `pane.read`.
- Question, dialog: the screen excerpt around the prompt.
- Plan: the first 20 lines; `p` opens the full plan in the local doc pane.

Screen text is shown, never stored.

Ordering: one flat list sorted by kind (section 2) then age. `≡ group`
toggles grouping by project, with the same order inside each group.

Filters: `tab` or a click on the tab cycles Waiting, Done, All. A filter set by
a sidebar click shows as a removable chip.

## 5. Focus, keys and mouse

- Focus is herdr's normal pane focus. `prefix i` and clicks move it in and out
  of the inbox pane like any other pane; herdr's focus border shows which pane
  has it.
- Keys go to the focused pane only. The inbox drops keys that arrive within
  250 ms of gaining focus (terminal focus-in event), so a digit aimed at an
  agent pane cannot answer an item.
- After an answer, focus stays in the inbox and the cursor moves to the next
  waiting item. The item comes back if the request fails.
- `prefix a`, from anywhere: open or focus the inbox on the oldest waiting
  item. After the answer, focus returns to the previous pane.
- Mouse: click selects and expands; click on the workspace name or `↗` jumps;
  click on an option label answers with the same checks as the key; right-click
  opens a menu (jump, dismiss, dismiss all done in project, snooze, mute); a
  hover `✕` dismisses done rows; the wheel scrolls.

Replies (`r`, `o`, note on `n`/plan) open a multi-line editor in the item. It
grows to 10 lines; `ctrl+s` or `alt+enter` sends, `enter` adds a line,
`ctrl+e` opens `$EDITOR` on a temporary file, `esc` cancels and keeps the
draft for that item.

## 6. Sidebar split

The sidebar shows activity; the inbox shows waiting and done detail.

```
 drovr                  ●3
  fix-ctrl-click        !
  inbox-design          ?
  pane-layout
   ▸ Bash cargo test   30s
   codex · idle         1h
```

- Working: `▸` and `drovr_doing` ("Bash cargo test", "Edit src/x.rs").
- Waiting or done: the kind glyph on the workspace row, nothing else.
- Section header: one count of inbox items. Glyph and count are click targets
  that open the inbox pane (section 3). Idle agents and agents without hook
  tokens show the herdr state only.

## 7. Dismiss, snooze, mute, seen, stuck

- Done items leave when any client views the pane, because herdr then moves
  Done to Idle. No drovr mark is needed for that path.
- `d` dismisses one item; `D` dismisses every done item in the current filter.
- `z` snoozes for 1 h; pressing again cycles 4 h, until tomorrow 09:00. A
  snoozed waiting item returns early if its prompt changes.
- `m` mutes a workspace: its done, asks, stuck and limit items are hidden and
  raise no toast. Waiting items still show.
- Dismiss and snooze marks live on the server as pane tokens from source
  `drovr-inbox`: `drovr_dis` = the `state_change_seq` dismissed (while Done it
  equals the completion seq), `drovr_snz` = snooze end. Every client sees the
  same inbox. Mutes are a client preference.
- The stuck threshold is set per workspace in drovr's `sidebar.toml`, with a
  global default of 10 minutes. Keys are `<machine>/<workspace>`, as in
  `hidden`:

  ```toml
  [inbox]
  stuck_minutes = 10

  [inbox.stuck_minutes_by_workspace]
  "mato/migrate-db" = 45
  ```

## 8. Data flow

Hooks report tokens to herdr on the agent's machine; herdr pushes snapshots
to every client. The `drovr inbox` process subscribes to each endpoint (local
socket; SSH API bridge for remotes) and answers through the same connections.
Items are built from `ClientShellAgent` (`agent_status`, `state_change_seq`,
`tokens`) in one function shared by the sidebar and the inbox process. The
snapshot has no rule labels and no `completion_seq`, and every Codex wait is
plain `blocked`; drovr does not try to classify them beyond hook tokens. The
detail of a dialog comes from `pane.read` on selection.

**drovr-state-hook.** Runs on SessionStart, UserPromptSubmit, PreToolUse,
PostToolUse, PostToolUseFailure, PermissionRequest, PermissionDenied,
Notification, Stop, StopFailure and SessionEnd, for Claude and for Codex
(`~/.codex/hooks.json` PermissionRequest).

- `--seq` is `time.time_ns()` taken under the pane lock when the report is
  sent; herdr drops reports whose seq is not newer. (Draft 3 said "at hook
  start", as herdr's own hook does. Each report carries the merged pane state,
  so a seq from hook start would let herdr drop a newer state, and a waiting
  PermissionRequest hook's exit report would always be dropped.)
- Per-pane state lives in `<state_dir>/drovr/state-hook/<pane>.json` under
  `flock`.
- Claude's PermissionRequest input has no `tool_use_id` (section 11). The hook
  records each PreToolUse as (`tool_use_id`, hash of `tool_name` and
  `tool_input`); a PermissionRequest takes the oldest unclaimed PreToolUse with
  the same hash. If none matches, the request has no tool id and is cleared
  only by a turn boundary or its own hook exit.
- Each PermissionRequest gets a random 8-character request id. Pending requests
  are a set: PostToolUse and PostToolUseFailure with the claimed tool id,
  PermissionDenied, and the request's own hook exit remove one;
  UserPromptSubmit, Stop, SessionStart, SessionEnd and an idle Notification
  (`notification_type` `idle_prompt`) clear the set. The idle Notification is
  the only event after "No" or Esc in the terminal (section 11).
- AskUserQuestion with several questions, multi-select or more than 4
  options does not enter the set; the hook exits at once and herdr's blocked
  state makes it a dialog item.
  `drovr_wait` always reflects the oldest pending request, so parallel tools do
  not wipe each other.
- Subagent events (`agent_id` set): PreToolUse is ignored for `drovr_doing`;
  PermissionRequest is kept and the item is labelled "subagent".
- The hook skips a report when no value changed, and a PreToolUse report
  follows the previous PreToolUse report by at least 2 s; a held-back value
  goes out with the next report. Ceiling: a long tool that starts within 2 s
  of the previous one shows the previous tool in `drovr_doing` until the next
  event; a trailing report would need a background process. No TTL: a TTL makes every report count as changed and pushes a
  snapshot to every client.
- On failure (for example `metadata_token_limit`) the hook appends stderr to
  `<state_dir>/drovr/state-hook/errors.log` and exits 0.

| Key | Value (≤ 80 chars) | Cleared by |
|---|---|---|
| `drovr_state` | `<kind>\|<unix s>`: idle, working, asks, finished, limit, denied, exited; the time is the last kind change or reported PreToolUse | SessionEnd (set to `exited` when the session ends while working) |
| `drovr_doing` | `Bash cargo test` | PostToolUse, Stop |
| `drovr_wait` | `<kind>\|<req8>\|<sub?>\|<summary>` | request end, prompt, Stop |
| `drovr_o1`-`drovr_o4` | option labels, single-select question only | same as `drovr_wait` |
| `drovr_diff` | diffstat for Edit, Write; `<n> lines` for a plan | same as `drovr_wait` |
| `drovr_last` | first line of the last message; `?` suffix kept | UserPromptSubmit |

Token budget: the 32-key limit is per terminal across all sources, and going
over rejects the whole report. Usage hook: up to 10 stored, 11 per report.
drovr-state: 9. drovr-inbox: 2. Total 21, leaving 11 for other sources
(herdr-radar is not installed on mato).

Summaries are cut at 80 characters, so no key-based answer is sent from a token
alone: the full command or question always comes from `pane.read` first.

**Plan text.** ExitPlanMode's `tool_input` carries `planFilePath` (for example
`~/.claude/plans/<name>.md`); the hook stores that path in the pane state file.
`p` reads the state file and then the plan through the SSH API bridge from
build step 1, and opens the plan in the doc pane on the client machine. The y
and r keys stay in the inbox item beside it. No drovr binary or drovr-docs
plugin is needed on the server.

**Install.** drovr-install-hooks appends the drovr hook to each event without
reordering existing entries (on mato, PermissionRequest already runs orca with
a 10 s timeout and rosterd-hook). Claude runs all hooks of an event in
parallel, so the waiting drovr hook does not delay the others; a decision from
another hook (for example orca) closes the dialog first and the drovr hook's
later output is ignored. The drovr PermissionRequest entry sets `timeout` to the
wait limit plus 5 s. `--dry-run` prints the settings diff; run it against mato
before installing.

## 9. Answers

### Permission items: hook decision

The drovr-state-hook's PermissionRequest handler publishes the item, then
waits for a decision. Claude shows its dialog while the hook waits, so the user
can still answer in the terminal (section 11).

1. The hook polls `<state_dir>/drovr/state-hook/decide/<req8>.json` every
   250 ms.
2. To answer, the inbox writes that file through the bridge (temporary name,
   then rename), after `agent.get` shows the agent still blocked.
3. The hook prints the PermissionRequest decision and exits:
   - `y`: `{"behavior": "allow"}`
   - `n`, `r`: `{"behavior": "deny", "message": "<note or 'Denied in drovr'>"}`
   - `a`: `allow` with `updatedPermissions` from the request's
     `permission_suggestions` (documented, to verify in build step 2).
   The decision file is `{"behavior": "allow"}`, `{"behavior": "allow",
   "always": true}` or `{"behavior": "deny", "message": "..."}`; any other
   content is logged and removed. A file whose request is no longer pending is
   ignored.
4. The hook exits with no output when its request leaves the pending set
   (terminal answer, turn end) or after the wait limit (default 10 min,
   `DROVR_DECIDE_WAIT_S`). The dialog then stays, and the item falls back to
   jump-only.

This path is race-free. A decision is bound to one hook invocation, so it can
never answer a later prompt. If the user answers in the terminal first, Claude
ignores the hook's later output (verified). If the hook answers first, the
dialog closes and a late keypress lands in the prompt input, not in a dialog.

Codex: its PermissionRequest hook decision format is not verified yet; Codex
permissions use the key path below until it is. The hook publishes the Codex request and exits
at once; Codex has no PostToolUseFailure, PermissionDenied, Notification,
StopFailure or SessionEnd hooks, so its request is removed by the claimed
tool's PostToolUse or a turn boundary.

### Question and plan items: keys

A hook `allow`, with or without `updatedInput.answers`, does not close the
AskUserQuestion or ExitPlanMode dialog in Claude 2.1.287 (section 11), so
option choices and plan approval send keys. Before sending, the inbox runs, in
order, through the bridge:

1. `agent.get`: `agent_status` is blocked and `state_change_seq` equals the
   item's.
2. `pane.read`: the screen shows the item's question or plan title (the 80
   character summary as a prefix of the on-screen text) and the chosen label.
3. A second `pane.read` immediately before the send repeats check 2.
4. `agent.send_keys` with the option number.

Any failed check removes the answer keys and shows "Changed in the terminal.
Jump to see it." Options are shown only after the screen confirms them, so
stale tokens never produce an answerable item.

Residual race on this path: herdr bumps `state_change_seq` only when the state
changes, and stock herdr has no compare-and-send. Between the last read and the
send, one SSH round trip, a new prompt with the same text can appear and be
answered.

Notes without keys: a hook `deny` with a `message` is honoured for both tools,
and Claude receives the message (verified). So `r` on a plan ("keep planning
with note") and `o` on a question (free-text answer) use the hook decision
path and are race-free.

### Replies

Replies to Done, asks and denied items use `agent.prompt`.

One request at a time per pane, on a background thread. The hook redacts
secrets; `DROVR_STATE_TEXT=0` reports kinds only.

## 10. Build plan

1. **SSH API bridge** per remote endpoint; route remote Ctrl+click doc opens
   and plan reads through it. Test: the remote Ctrl+click test fails today,
   then passes.

   Built as `crate::remote::EndpointBridge`, one per saved SSH endpoint,
   held by the client and started on first use. The herdr API has no file
   access, so the bridge has two channels: herdr API calls through
   `herdr remote-api-bridge`, and POSIX shell scripts over the endpoint's
   managed SSH transport. Reading the plan file (`p`) and writing decision
   files (section 9) use the shell channel; they land with their callers in
   step 5. A remote Ctrl+click runs `drovr doc open` on the remote machine
   through the shell channel. The doc pane there still needs the drovr
   binary on that machine, because the viewer runs in a pane of that
   server. When the SSH shell does not find drovr on its PATH or in
   `~/.local/bin`, the bridge invokes the `drovr.docs` plugin through the API
   channel; the plugin looks for drovr on the herdr server's PATH.
2. **drovr-state-hook** for Claude and Codex, with PreToolUse matching,
   pending-request set, decision wait, redaction, install `--dry-run`. Tests:
   recorded payloads with a stub `herdr` (parallel tools, subagent, reject in
   terminal, Esc interrupt, decision file, wait limit); a live check of `a`
   with `updatedPermissions`.
3. **Sidebar split**: `AgentSignal` parsing and glyph rows. Unit and render
   tests.
4. **Inbox pane, read and jump**: `drovr inbox` process, `drovr_inbox` token,
   toggle and reuse, flat order, filters, server marks, per-workspace stuck
   threshold, focus-drop window. Tests: ordering, focus-drop window, narrow
   layout.
5. **Answers**: hook decisions, label mapping, checks in section 9, editor,
   plan fetch. Tests: a fake endpoint where each check fails and nothing is
   sent; a stale decision file for a finished request is ignored.

## 11. Experiments (Claude Code 2.1.287, macOS)

Setup: a temporary git project with a project-local `.claude/settings.json`
whose hooks append their stdin JSON to a log, and a PermissionRequest hook that
waits for a decision file. Claude ran interactively in a private tmux server
with `--setting-sources project,local --permission-mode default --model haiku`
and no `HERDR_*` variables, so user hooks and settings were not loaded.

Verified:

- PreToolUse and PermissionRequest both fire for AskUserQuestion and for
  ExitPlanMode (in plan mode), as they do for Bash.
- Claude's PermissionRequest input has `tool_name`, `tool_input`,
  `permission_mode` and, for Bash, `permission_suggestions`; it has no
  `tool_use_id`, although the hooks reference lists one. PreToolUse has it.
- ExitPlanMode's `tool_input` has `plan` and `planFilePath`.
- While the PermissionRequest hook sleeps, the permission dialog is on screen
  (Bash, AskUserQuestion, ExitPlanMode). The hooks reference says the hook runs
  before the dialog is shown; in this version the two overlap.
- Bash: hook `allow` closes the dialog and the command runs ("Allowed by
  PermissionRequest hook"); hook `deny` with a message closes it and Claude
  sees the message.
- Answering "Yes" in the terminal while the hook waits runs the tool at once;
  the hook keeps running, and its later `deny` is ignored.
- Answering "No" in the terminal fires no PostToolUse, PostToolUseFailure,
  PermissionDenied or Stop; only Notification follows.
- AskUserQuestion and ExitPlanMode: hook `deny` with a message closes the
  dialog and Claude receives the message (it revised the plan from it). Hook
  `allow`, including `updatedInput.answers` for the question, leaves the
  dialog open.

Verified in build step 2 (same setup, hooks from drovr-state-hook):

- PreToolUse matching: the PermissionRequest claimed the PreToolUse
  `tool_use_id`, and the terminal answer's PostToolUse removed the request.
- A decision file `deny` with a message closed the dialog.
- `allow` with `updatedPermissions` from `permission_suggestions` writes the
  suggestion: for `curl -sI https://example.com` the suggestion was
  `addRules` `Bash(curl -sI https://example.com)` to `localSettings`; the
  hook wrote that rule to `.claude/settings.local.json`, and the same command
  then ran without a dialog. The dialog's own option offered the broader
  `curl *`, so `a` grants the exact command only. For a write redirect the
  suggestion was `addDirectories` for the session, and neither the hook nor
  the dialog's option 2 stopped the next prompt.

Documented, not verified: the 600 s default hook timeout; PermissionRequest
does not fire in `-p` mode.

## Decisions

- The inbox is a client-drawn right panel, toggled by `prefix i` and opened by
  sidebar clicks (user decision, 2026-10-02): a herdr pane would run on the
  workspace's server and could not see every machine. The sidebar-swap and
  popup placements are dropped.
- No workspace (gtm) server: drovr owns the inbox. Live state stays in herdr
  pane metadata; durable task records come later in a drovr-owned per-project
  file.
- The stuck threshold is per workspace in `sidebar.toml`, with a global
  default.
- Plan text for remote agents is fetched over the SSH API bridge.
- Permission answers go through a waiting PermissionRequest hook (section 9);
  key sending remains for question options and plan approval.
- Draft 1's rule "clear on PreToolUse with a different id" is not used: with
  parallel tools it clears a pending permission. Requests are cleared only by
  their own end events, their hook exit or a turn boundary.
- No `failed` state: herdr has Idle, Working, Blocked, Done, Unknown. StopFailure
  is a finished item with error text and leaves when the pane is viewed.
- Codex screen rules are not mapped to answerable items: the fork's manifest
  fixes do not reach stock servers, which load manifests from herdr.dev. Codex
  answers come from its PermissionRequest hook; without it, Codex is jump-only.

## Open questions

1. Answered: the inbox is drawn by the client (section 3), so it is the same
   on local and remote workspaces.
2. What is Codex's PermissionRequest decision format, and does Codex keep its
   dialog on screen while the hook waits?

Answered:

- Does `allow` with `updatedPermissions` behave like the dialog's "always"
  option? It applies the request's suggestion, which can be narrower than the
  dialog's option (exact command rather than `curl *`); see section 11.
- Does Claude keep its dialog on screen while a PermissionRequest hook waits,
  and can the hook return the decision? Yes to both for Bash (section 11).
  Permission answers use the hook; for AskUserQuestion and ExitPlanMode only
  `deny` with a message works.
- Does PermissionRequest fire for AskUserQuestion and ExitPlanMode? Yes, and
  PreToolUse fires too. PermissionRequest is the source for both items.
- Will the workspace (gtm) server run next to the agents? No; drovr owns the
  features.
- Stuck threshold per workspace or global? Per workspace, with a global default.
- Plan text over SSH or scrollback? Over the SSH API bridge.
