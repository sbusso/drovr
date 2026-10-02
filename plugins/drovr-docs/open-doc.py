#!/usr/bin/env python3
"""Run `drovr doc open` for a clicked Markdown link, or with --recent."""
import json
import os
import shutil
import subprocess
import sys
from urllib.parse import unquote, urlsplit


def drovr_bin():
    found = shutil.which("drovr")
    if found:
        return found
    # The herdr server's PATH often lacks ~/.local/bin.
    fallback = os.path.expanduser("~/.local/bin/drovr")
    return fallback if os.access(fallback, os.X_OK) else None


def clicked_path(context):
    url = os.environ.get("HERDR_PLUGIN_CLICKED_URL") or context.get("clicked_url")
    if not url:
        return None
    if url.lower().startswith("file://"):
        parts = urlsplit(url)
        if parts.netloc not in ("", "localhost"):
            # file://host/path: only local files can be shown.
            hostname = os.uname().nodename.split(".")[0]
            if parts.netloc.split(".")[0] != hostname:
                return None
        return unquote(parts.path)
    path = url.split("#", 1)[0].split("?", 1)[0]
    path = os.path.expanduser(path)
    if not os.path.isabs(path):
        cwd = context.get("focused_pane_cwd") or context.get("workspace_cwd") or os.getcwd()
        path = os.path.join(cwd, path)
    return os.path.normpath(path)


def main():
    try:
        context = json.loads(os.environ.get("HERDR_PLUGIN_CONTEXT_JSON") or "{}")
    except json.JSONDecodeError:
        context = {}
    drovr = drovr_bin()
    if not drovr:
        sys.exit("drovr-docs: drovr is not installed on this machine")
    if "--recent" in sys.argv[1:]:
        args = ["--recent"]
    else:
        path = clicked_path(context)
        if not path:
            sys.exit("drovr-docs: no local file in clicked link")
        if not os.path.isfile(path):
            sys.exit(f"drovr-docs: not a file: {path}")
        args = [path]
    sys.exit(subprocess.call([drovr, "doc", "open", *args]))


if __name__ == "__main__":
    main()
