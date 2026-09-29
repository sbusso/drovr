# andreconde fork of herdr

Private, client-side fork of [herdrdev/herdr](https://github.com/herdrdev/herdr).
Not for upstream PRs (upstream does not accept them). Based on the stable tag in
the release name; the server side is unchanged, so this client talks to a stock
server of the same version.

## What it adds (federated sidebar, i.e. with 2+ machines)

- **Projects**: named groups of workspaces from *any* machine, shown above the
  per-machine lists. Remote members carry a dim machine tag.
  - Right-click a workspace → `→ <project>` / `→ New project…` / Remove / Move up/down.
  - Right-click a project header → Collapse, Pin to top, Move up/down, Rename,
    Auto-match rules, Delete. Left-click the header toggles collapse.
  - Collapsed projects show their worst member status and member count.
  - Auto-match rules: a workspace whose name contains a rule joins that project.
- **Hide/show** workspaces (right-click → Hide; `prefix+alt+h` shows them again, dimmed with ⊘).
  Agents of hidden workspaces leave the agents list too.
- **Agents list** follows project order across machines (in "grouped" sort), has
  jump numbers on the right, and a yellow `●` for manually marked unread agents.
  - Right-click an agent → Go to, Mark unread/read, Rename pane (active machine),
    Move workspace to project, Hide workspace.
  - `prefix+#` → jump to agent N (any number, not just 1–9).
- `prefix+.` → project menu for the focused workspace (keyboard access).

Layout is stored client-side in `~/.config/herdr/sidebar.toml` (hand-editable,
reloaded within a second of saving). See `src/client/shell/projects.rs`.

## Code map (keep rebases cheap)

New files: `src/client/shell/projects.rs`, `src/client/shell/project_actions.rs`,
`.github/workflows/fork-release.yml`, this file. Upstream files carry small,
commented hooks tagged `andreconde fork`:
`grep -rn "andreconde fork" src`.

## Release

```bash
git tag ac-v<upstream>-<n> && git push fork andreconde --tags
```

CI builds `herdr-linux-x86_64` (static musl) and attaches it to the release.
Install with `~/.local/bin/herdr-fork-install` (on each machine).

## Rebase on a new upstream release

```bash
git fetch origin --tags
git rebase --onto v<new> v<old> andreconde
```
