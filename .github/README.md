# sheprd

**A herdr fork for people who run many agents on more than one machine.**
A shepherd tends the herd: same runtime, smarter pasture.

sheprd is a *client-side* fork of [herdr](https://github.com/herdrdev/herdr) by
herdrdev. The server is unchanged, so the `sheprd` client attaches to stock
`herdr` servers of the same version, locally and over SSH. Everything herdr does,
sheprd does; this page lists only the differences.

> Upstream does not accept outside pull requests, and sheprd does not send any.
> Please don't report sheprd behaviour to herdr.

## What sheprd adds

### Projects across machines
Group workspaces from **any** machine under one collapsible project header,
instead of one list per machine. Remote members carry a dim machine tag.

```
 ▾ ★ TheCalendar
   ● TheCalendar       dev
 ▾ ★ LF
   ○ lf-seguros-web
   ○ LF Contracts
   ● lf-seguros-web    dev
 ▸ Infrastructure       ○ 3     ← collapsed: worst status + member count
 ▾ dev               74ms ●     ← ungrouped leftovers, with live latency
   ○ DTech
```

- **Assign**: drag a workspace onto a project header, or right-click → `→ Project`.
- **Auto-assign**: a project's match rules catch workspaces whose *name or folder*
  contains the rule (`calendar` catches `~/Projects/TheCalendar` on every machine),
  so new agents land in the right place with no clicks.
- **Organise**: right-click a header → Collapse, Pin to top, Move up/down, Rename,
  Auto-match rules, Delete. Left-click a header to collapse it.
- **Hide** workspaces you rarely look at (right-click → Hide). `prefix+alt+h`
  shows them again (dimmed with ⊘). Their agents leave the agents list too.

### A calmer agents list
- Follows **project order across machines** (no more "all Local, then all dev").
- **Jump numbers** on the right; `prefix+#` jumps to any number, not just 1–9.
- Click the **`agents`** header to show **only the current project's agents**.
- **Mark unread** (right-click an agent) → yellow `●` until you next focus it.
- Right-click an agent → Go to, Mark unread/read, Rename pane, Move workspace to
  project, Hide workspace.

### Remote
- Smoothed **round-trip time** beside each saved machine (`dev 74ms`), measured
  from herdr's existing heartbeat, so you can tell a slow link from a slow herdr.

### Keys (defaults; no config needed)
| Key | Action |
|---|---|
| `prefix+.` | project menu for the focused workspace |
| `prefix+#` | jump to agent by number |
| `prefix+alt+h` | show / conceal hidden workspaces |

## Configuration
Project layout lives client-side in `~/.config/herdr/sidebar.toml`. The UI writes
it, and hand edits reload within a second:

```toml
show_hidden = false
agents_project_only = false
hidden = ["dev/Vaultwarden"]           # machine/workspace

[[group]]
name = "TheCalendar"
pinned = true
match = ["calendar"]                   # name or folder substring
members = ["local/Finance"]            # explicit members, in display order
```

Projects appear when the client is connected to 2+ machines (the federated
sidebar). Stock herdr ignores this file.

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
- Fork code lives in `src/client/shell/projects.rs` (model) and
  `src/client/shell/project_actions.rs` (menus, clicks, keys). Small hooks in
  upstream files are tagged: `grep -rn "andreconde fork" src`.
- Rebase on a new herdr release: `git fetch origin --tags && git rebase --onto v<new> v<old> main`.
- Release: `git tag sheprd-v<ver>-<n> && git push fork sheprd-v<ver>-<n>` → the
  "sheprd release" workflow builds and publishes. Upstream workflows are disabled here.
- Local build needs Zig 0.16.0 (`cargo build --release`).

## License
Apache-2.0, same as herdr. herdr is © its authors; sheprd changes are © André Conde.
