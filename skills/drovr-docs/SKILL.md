---
name: drovr-docs
description: Show documents meant for the user in a drovr doc pane. Load whenever you write a plan, report, spec, review, research notes, release notes, or a summary the user asked for, so it opens as rendered Markdown next to the agent pane instead of scrolling past in the terminal.
---

# drovr docs

drovr shows Markdown files in a doc pane beside your agent pane. Use it for
documents the user will read, not for files you only edit.

## When to open a document

Open a document when its purpose is to be read by the user: a plan, report,
spec, review, research notes, release notes, or a summary they asked for.

Do not open files you merely create or edit as part of the work: code,
README, CHANGELOG, config, tests, docs that belong to the codebase. Open
them only when the user asks.

When you cannot tell whether the user wants to read it in a pane, give the
path and offer to open it in one line.

## How

1. Write the document as Markdown. Put it in the project (for example
   `docs/` or next to the work it describes); put plans in `~/.claude/plans/`.
2. Open it:

   ```bash
   drovr doc open <path> --title "<short title>"
   ```

   Use a title of two to five words.
3. On later revisions, edit the same file. The pane reloads on its own; do
   not run `drovr doc open` again and do not create a new file per revision.
4. In your reply, give the path and stop. Do not paste the document into the
   terminal.

## When drovr is unavailable

If `command -v drovr` fails or `HERDR_PANE_ID` is unset, you are not in a
drovr pane: write the file and give its path.
