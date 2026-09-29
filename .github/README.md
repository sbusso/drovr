<p align="center"><img src="sheprd-logo.svg" width="112" alt="sheprd logo"></p>

# sheprd

**A herdr fork for people who run many agents on more than one machine.**
A shepherd tends the herd: same runtime, smarter pasture.

sheprd is a *client-side* fork of [herdr](https://github.com/herdrdev/herdr) by
herdrdev, with its own logo and name so the two are never confused. The server is unchanged, so the `sheprd` client attaches to stock
`herdr` servers of the same version, locally and over SSH. Everything herdr does,
sheprd does; this page lists only the differences.

> Upstream does not accept outside pull requests, and sheprd does not send any.
> Please don't report sheprd behaviour to herdr.

## What sheprd adds

### One sidebar instead of two
herdr splits the sidebar into *machines* (workspaces per machine) and *agents*.
With several machines that means the same work appears twice, in two different
orders. sheprd replaces both with **one list**: your projects, each showing its
agents from **every** machine, then **Other** for everything not in a project.

```
 ○ all agents       detailed      ← filter toggle · view toggle (click)
 ▾ ★ storefront
 ● Fix checkout rounding      1   ← agent topic + jump number
   gpu-box                        ← workspace (if ≠ project) · machine
 ▾ ★ billing
 ○ Invoice PDF layout         2
   billing-web
 ● Retry failed webhooks      3
   billing-web · gpu-box
 ▸ infra                   ○ 3    ← collapsed: worst status + count
 ▾ Other
 ○ Weekly notes               4
   notes
 new · Local   gpu-box 38ms menu  ← live latency per remote machine
```

- **Two views** (click the right header label): *detailed*, one row per agent
  with its topic, and *compact*, one line per workspace.
- **Filter** (click the left header label): *all agents* or *active* (only
  agents that are working or need you).

### Projects across machines
- **Drag** any row onto a project header to move its workspace there. Drop it on
  **Other** to take it out, or on another row to place it just above that row.
  Right-click → `→ project` does the same without the mouse gesture.
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
  not looked at yet, or marked unread, in sidebar order.
- Right-click an agent → **Mark unread** (a yellow `●` status that counts as
  needing you until you visit it) or **Mark inactive** (drops a finished or
  blocked agent out of the queue until its state changes again).
- **Jump numbers** on every agent; `prefix+#` jumps to any number, not just 1–9.

### Keys (defaults; no config needed)
| Key | Action |
|---|---|
| `prefix+u` | next agent that needs you |
| `prefix+#` | jump to agent by number |
| `prefix+.` | project menu for the focused workspace |
| `prefix+alt+h` | show / conceal hidden workspaces |

## Configuration
Everything lives client-side in `~/.config/herdr/sidebar.toml`. The UI writes
it, and hand edits reload within a second:

```toml
compact = false                        # view: one row per agent / per workspace
active_only = false                    # filter: all agents / active ones
hidden = ["gpu-box/scratch"]           # machine/workspace

[[group]]
name = "storefront"
pinned = true
match = ["storefront"]                 # name or folder substring
members = ["local/notes"]              # explicit members, in display order
```

The combined sidebar appears when the client is connected to 2+ machines. With
a single machine sheprd looks like herdr. Stock herdr ignores this file.

## Install
Linux x86_64, static binary:

```bash
curl -fsSL https://raw.githubusercontent.com/andreconde21/sheprd/main/scripts/sheprd-install | bash
sheprd            # instead of `herdr`
```

It installs to `~/.local/share/sheprd/sheprd` and leaves your `herdr` install alone.
The server keeps running stock herdr; use the matching herdr version on each machine.

## Versioning
`sheprd-v<herdr version>-<n>`, e.g. `sheprd-v0.9.3-1` = herdr 0.9.3 + sheprd patch set 1.

## For maintainers of this fork
- Fork code lives in `src/client/shell/projects.rs` (model),
  `src/client/shell/sheprd_sidebar.rs` (the combined sidebar) and
  `src/client/shell/project_actions.rs` (menus, clicks, drag, keys). Small hooks in
  upstream files are tagged: `grep -rn "andreconde fork" src`.
- Rebase on a new herdr release: `git fetch origin --tags && git rebase --onto v<new> v<old> main`.
- Release: `git tag sheprd-v<ver>-<n> && git push fork sheprd-v<ver>-<n>` → the
  "sheprd release" workflow builds and publishes. Upstream workflows are disabled here.
- Local build needs Zig 0.16.0 (`cargo build --release`).

## License
Apache-2.0, same as herdr. herdr is © its authors; sheprd changes are © André Conde.
