---
name: drovr-docs
description: Show documents meant for the user in a drovr doc pane. Load whenever you write a report, spec, review, research notes, release notes, a plan outside plan mode, or a summary the user asked for, so it opens as rendered Markdown next to the agent pane instead of scrolling past in the terminal.
---

# drovr docs

drovr shows Markdown files in a doc pane beside your agent pane. Use it for
documents the user will read, not for files you only edit.

## When to open a document

Open a document when its purpose is to be read by the user: a report, spec,
review, research notes, release notes, a plan written outside plan mode, or a
summary they asked for.

Plan-mode plans open on their own: drovr's plan hook shows every plan file
Claude Code writes under `~/.claude/plans/`. Do not run commands for them.

Do not open files you merely create or edit as part of the work: code,
README, CHANGELOG, config, tests, docs that belong to the codebase. Open
them only when the user asks.

When you cannot tell whether the user wants to read it in a pane, give the
path and offer to open it in one line.

## How

1. Write the document as Markdown. Put it in the project (for example
   `docs/` or next to the work it describes).
2. Open it:

   ```bash
   drovr doc open <path> --title "<short title>"
   ```

   Use a title of two to five words.
3. On later revisions, edit the same file rather than creating a new one,
   then run the same `drovr doc open <path>` again. The doc pane is shared by
   the workspace and may show another document by then; reopening is
   idempotent and brings yours back.
4. In your reply, give the path and stop. Do not paste the document into the
   terminal.

## When drovr is unavailable

If `command -v drovr` fails or `HERDR_PANE_ID` is unset, you are not in a
drovr pane: write the file and give its path.
