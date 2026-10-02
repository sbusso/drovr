<p align="center"><img src="drovr-logo.svg" width="112" alt="drovr logo"></p>

# drovr

**A herdr client for people who run many agents on more than one machine.**

drovr is a fork of [sheprd](https://github.com/andreconde21/sheprd) by André
Conde, which is itself a *client-side* fork of
[herdr](https://github.com/herdrdev/herdr) by herdrdev. The server is
unchanged, so the `drovr` client attaches to stock `herdr` servers of the same
version, locally and over SSH, and uses herdr's own config, state and socket
paths. Everything herdr does, drovr does; this page lists only the differences.

> Upstream does not accept outside pull requests, and drovr does not send any.
> Please don't report drovr behaviour to herdr.

## What drovr adds

### One sidebar instead of two
herdr splits the sidebar into *machines* (workspaces per machine) and *agents*.
With several machines that means the same work appears twice, in two different
orders. drovr replaces both with **one list**: your projects, each showing its
agents from **every** machine, then **Other** for everything not in a project.

```
 ○ all agents ● 2   detailed      ← filter · needs-you counter · view (click)
 ▾ ★ storefront
 ● Fix checkout rounding          ← agent topic
   gpu-box                        ← workspace (if ≠ project) · machine
 ▾ ★ billing
 ○ ⚑ Invoice PDF layout          ← ⚑ kept active
   billing-web
 ● Retry failed webhooks
   billing-web · gpu-box
 ▸ infra                   ○ 3    ← collapsed: worst status + count
 ▾ Other
 ○ Weekly notes               4
   notes
 new · Local              menu
```

Clicking the view label cycles *detailed*, *compact* (one line per workspace)
and *structured*: workspace headers (remote machine on the right) with one line
per agent, a vendor mark and a title coloured by state (working in the vendor's
colour behind radar's spinner, done green, blocked red, idle fading over 15
minutes and 2 hours). The mark is the vendor's logo from the
[herdr-radar](https://github.com/hhdebb/herdr-radar) icon font, in its brand
colour; the font must be installed. Without it, set `agent_icons = "letter"`
(the vendor's first letter) or `"none"` (no mark) under `[ui.sidebar]` in
herdr's `config.toml`.

```
 ▾ GTM ──────────────── ● 1 · 3   ← section: needs you · agents
   gtm-rd
     ✓ Fix auth flow in gateway
                                  ← agent_gap
     ? Review PR 42

   scratch                        ← no agents: dimmed
```

In the structured view a section header carries a thin rule and, on the
right, how many of its agents need you (`● n`, only when some do) and how
many agents it shows; a collapsed section keeps both counts. Agents of one
workspace are one blank row apart (`agent_gap = 1`, set `0` to stack them),
and workspaces without agents are dimmed (`show_empty_workspaces = false`
hides them). Both go under `[ui.sidebar]` in herdr's `config.toml`.

An agent row shows, in every view, the first of: the session's own name (the
one the Claude desktop app and `claude --resume` show, reported by the usage
hook below), the terminal title unless it is only the vendor's name ("Claude
Code") or the pane's folder, the pane's label, its tab's custom name or its
folder's name, then the generic title. The terminal title alone often reads
"Claude Code": Claude sets its topic there only for some sessions, while the
desktop app reads the name from the session transcript.

A two-row "drovr" banner with the version in small pixel digits sits above
the sidebar's toggles when the sidebar is at least 18 columns wide and tall
enough to keep 10 rows for the list; the version hides first when it is
narrow.
Turn it off with `banner = false` under `[ui.sidebar]`.

The sidebar stays quiet: status and topic only. **Peek** (`prefix+space`)
reveals, for ten seconds (press again to hide): idle age, context size
(`ctx 581k`) and jump number per agent, today's time and tokens per project,
and each machine's latency.

### Time and tokens per project
A small Claude Code hook (`scripts/drovr-usage-hook`, a Stop hook) reads each
session's transcript incrementally after every turn and attaches the session's
context size and per-day usage to its pane as herdr metadata, so it reaches the
sidebar from any machine without syncing files. It also reports the
session's name (`drovr_name`): the latest custom title (`/rename`), else the
latest AI-generated title, else the latest summary found in the transcript. drovr keeps the history in
`~/.local/state/herdr/drovr-usage.json` and attributes it to projects with the
same rules as the sidebar. Right-click a project for *Today* and *Last 7 days*
(active time · input + output + cache-write tokens; cache reads are excluded).

Install the hook on every machine where agents run:
`curl -fsSL https://raw.githubusercontent.com/sbusso/drovr/main/scripts/drovr-install-hooks | bash`

- **Two views** (click the right header label): *detailed*, one row per agent
  with its topic, and *compact*, one line per workspace.
- **Filter** (click the left header label): *all agents* or *active*. Active
  keeps agents that are working, need you, are kept, or went idle less than
  24 h ago (`recent_hours`), so something you just read doesn't vanish.
  Older idle agents are dimmed in *all agents*.

### Collapsed sidebar: a project rail
Collapsed, the sidebar becomes a 3-column rail: the needs-you counter, then one
row per project (worst status + a 2-letter tag, e.g. `●TC`), with the project
you're in spelled downwards beneath its row. Click a row to jump to that
project's most urgent agent. Peek (`prefix+space`) shows the full sidebar over
the panes for a moment. Tags are derived from the name; set `short = "OP"` on a
project to choose your own.

### Projects across machines
- **Drag** any row (a workspace header or one of its agents) to move its
  workspace. Drop it on a project header to add it at the end of that project,
  on a row or the blank line above it to place it just above that row's
  workspace, or on the lower half of a project's last workspace to place it
  last. Drop it on **Other** to take it out. An accent line shows where it will
  land; release outside the sidebar or press Esc to cancel. Right-click →
  `→ project` does the same without the mouse gesture.
- **Auto-assign**: a project's match rules catch workspaces whose *name or
  folder* contains the rule (`storefront` catches `~/code/storefront-api` on
  every machine), so new agents land in the right place with no clicks.
  Dragging something to Other overrides its rules.
- **Organise**: right-click a header → Collapse, Pin to top, Move up/down,
  Rename, Auto-match rules, Delete. Left-click a header to collapse it.
- **Hide** workspaces you rarely look at (right-click → Hide); `prefix+alt+h`
  shows them again, dimmed with ⊘.

### Attention you control
- `prefix+u` jumps to the **next agent that needs you**: blocked, finished and
  not looked at yet, or marked unread, in sidebar order. The `● 2` counter in
  the header shows how many there are; clicking it does the same.
- **Click a desktop notification** to raise the terminal and land on that agent.
- Right-click an agent → **Mark unread** (a yellow `●` status that counts as
  needing you until you visit it) or **Mark inactive** (drops a finished or
  blocked agent out of the queue until its state changes again).
- **Keep active** (right-click an agent): pins it to the active view (⚑) until
  you unpin it, for the thing you're still working on.
- **Jump numbers only when you want them**: `prefix+#` shows a number on every
  row; type it and drovr jumps as soon as the number is unambiguous.

### New workspaces on any machine
- `prefix+alt+c` (or clicking **new** in the footer) asks which machine, then a
  name. The workspace joins the project you're in and starts in that project's
  folder on the chosen machine.
- Right-click a project → **New agent here** does the same and starts `cc` in it.

### Document pane
A pane next to your agent that shows a Markdown file rendered: plans, reports,
specs. It reloads when the file changes and keeps your scroll position, follows
links between documents (Backspace goes back), searches with `/`, and `q`
closes the pane. Each
workspace has at most one doc pane; opening another document switches it.

Three ways to open a document:
- **From an agent or a shell**: `drovr doc open <path> [--title <title>]`.
  The first call splits a pane to the right of the caller (45% of its width)
  without taking focus; later calls switch that pane. Add `--focus` to move
  to it. `drovr doc open --recent` reopens the workspace's newest document.
- **Ctrl+click a Markdown link** in a pane, with the `drovr-docs` plugin
  installed (see below). Plain text is clickable only for `http(s)` URLs, so
  bare paths and `file://` links work when the program prints them as
  hyperlinks.
- **Right-click a workspace or an agent** in the sidebar → **Documents…**
  lists the workspace's last 10 documents. The item shows for workspaces on
  this machine that have opened documents.

`drovr doc view <path>` runs the viewer in the current pane. Each machine keeps
its recent documents in `~/.local/state/herdr/drovr/docs.json` (20 per
workspace); `drovr doc open` runs on the machine that hosts the workspace.

Install the plugin and the Claude Code skill on each machine that runs agents:

```bash
herdr plugin install sbusso/drovr/plugins/drovr-docs --ref drovr-main
mkdir -p ~/.claude/skills && cp -R skills/drovr-docs ~/.claude/skills/   # from a drovr checkout
```

The plugin adds the Ctrl+click handler and an **Open document…** workspace
action (the newest recent document). The skill tells agents to open the plans
and reports they write for you with `drovr doc open`.

### Small fixes
- Workspaces are tracked by id, so two with the same name are independent and a
  rename keeps a workspace in its project.
- **Go To** (`prefix+g`) opens ready to type; arrows and Enter still pick, Left/
  Right still jump between workspaces while the search is empty.

### Keys (defaults; no config needed)
| Key | Action |
|---|---|
| `prefix+space` | peek: idle age, context, numbers, usage, latency |
| `prefix+u` | next agent that needs you |
| `prefix+#` | show jump numbers, type one to jump |
| `prefix+alt+c` | new workspace on a machine you pick |
| `prefix+.` | project menu for the focused workspace |
| `prefix+alt+h` | show / conceal hidden workspaces |

## Configuration
Everything lives client-side in `~/.config/herdr/sidebar.toml`. The UI writes
it, and hand edits reload within a second:

```toml
compact = false                        # view: one row per agent / per workspace
structured = false                     # view: workspace headers + one line per agent
active_only = false                    # filter: all agents / active ones
recent_hours = 24                      # idle agents stay "active" this long
hidden = ["gpu-box/scratch"]           # machine/workspace

[[group]]
name = "storefront"
pinned = true
match = ["storefront"]                 # name or folder substring
members = ["local/notes"]              # explicit members, in display order
```

The combined sidebar appears when the client is connected to 2+ machines. With
a single machine drovr looks like herdr. Stock herdr ignores this file.

## Install
macOS and Linux, x86_64 and arm64 (Linux builds are static):

```bash
curl -fsSL https://raw.githubusercontent.com/sbusso/drovr/main/scripts/drovr-install | bash
drovr            # instead of `herdr`
```

It installs to `~/.local/share/drovr/drovr` and leaves your `herdr` install alone.
Update later with `drovr update`.
The server keeps running stock herdr. drovr follows herdr's `master` branch, so
use a herdr build from the same `master` range on each machine.

## Versioning
`drovr-v<version>-<n>`, where `<version>` is the herdr `Cargo.toml` version on
`master` at the time and `<n>` counts drovr releases on that version, e.g.
`drovr-v0.9.3-2`.

## For maintainers of this fork
- Fork code lives in `src/client/shell/projects.rs` (model),
  `src/client/shell/drovr_sidebar.rs` (the combined sidebar) and
  `src/client/shell/project_actions.rs` (menus, clicks, drag, keys). Small hooks in
  upstream files are tagged: `grep -rn "drovr fork" src`.
- **Following herdr is automatic**: drovr follows herdr's `master`, not its
  stable tags. *drovr rebase* runs daily: when herdr `master` has moved past
  the merge-base of `main` and herdr `master`, it rebases drovr's own commits
  onto the new `master` and tracks the result in one issue per herdr commit
  ("drovr rebase onto herdr <sha>", 12-character sha). If the rebase is clean
  and the tests pass it pushes `rebase/herdr-<sha>` and marks the issue ready
  to ship; on a conflict or a test failure it pushes nothing and the issue
  lists the conflicting files or links the failing run. Issues and branches
  for older herdr commits are closed or deleted once superseded. *drovr
  promote* (Actions → Run workflow, input: the sha) ships a ready rebase: it
  moves `main` with `--force-with-lease`, tags `drovr-v<version>-<n>` and can
  be rerun after a partial failure. Conflict resolutions are remembered via
  `git rerere` (`.github/rr-cache`).
- Both workflows need the secret `DROVR_PUSH_TOKEN`: a fine-grained PAT for
  this repository with *Contents* and *Workflows* read/write. `GITHUB_TOKEN`
  cannot push commits that change `.github/workflows`.
- Manual rebase: `git fetch herdr && git rebase --onto herdr/master $(git merge-base main herdr/master) main`.
- Release: `git tag drovr-v<ver>-<n> && git push origin drovr-v<ver>-<n>` → the
  "drovr release" workflow builds `drovr-{macos,linux}-{arm64,x86_64}` with
  SHA-256 checksums and publishes them. Run it by hand with a tag to rebuild.
- *drovr CI* builds and tests pushes and pull requests to `main` on macOS and
  Linux. Upstream workflows are kept but their jobs only run in
  `herdrdev/herdr` (`if: github.repository == 'herdrdev/herdr'`).
- Local build needs Zig 0.16.0 (`cargo build --release`).

## License
Apache-2.0, same as herdr (see `LICENSE`). herdr is © its
authors. The sheprd changes this fork is built on are © André Conde. drovr
changes are © their authors.
