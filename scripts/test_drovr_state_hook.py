"""Tests for scripts/drovr-state-hook and the installer, with recorded hook
payloads and a stub `herdr` that logs each report. Nothing here reads or
writes the real ~/.claude, ~/.codex or herdr state: HOME and every state path
point into a temporary directory.

Run: python3 -m unittest scripts.test_drovr_state_hook
"""
import importlib.machinery
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
HOOK = REPO / "scripts" / "drovr-state-hook"
INSTALLER = REPO / "scripts" / "drovr-install-hooks"

STUB_HERDR = """#!/bin/sh
printf '%s\\0' "$@" >> "$STUB_LOG"
printf '\\n' >> "$STUB_LOG"
"""


def load_hook():
    loader = importlib.machinery.SourceFileLoader("drovr_state_hook", str(HOOK))
    module = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
    loader.exec_module(module)
    return module


def clean_env(home):
    env = {k: v for k, v in os.environ.items() if not k.startswith(("HERDR_", "DROVR_", "XDG_", "CLAUDE", "CODEX_"))}
    env["HOME"] = str(home)
    return env


class StateHookTests(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.state = self.tmp / "state"
        stub = self.tmp / "herdr"
        stub.write_text(STUB_HERDR)
        stub.chmod(0o755)
        self.log = self.tmp / "herdr.log"
        self.env = clean_env(self.tmp)
        self.env.update(
            HERDR_PANE_ID="w1:p1",
            HERDR_BIN_PATH=str(stub),
            STUB_LOG=str(self.log),
            DROVR_STATE_HOOK_DIR=str(self.state),
            DROVR_DECIDE_WAIT_S="20",
        )
        self.procs = []

    def tearDown(self):
        for proc in self.procs:
            if proc.poll() is None:
                proc.kill()
            proc.communicate()
        shutil.rmtree(self.tmp)

    # helpers

    def run_hook(self, event, agent="claude", **env):
        result = subprocess.run(
            [sys.executable, str(HOOK), agent],
            input=json.dumps(event),
            capture_output=True,
            text=True,
            env={**self.env, **env},
            timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def start_hook(self, event, agent="claude", **env):
        """A PermissionRequest hook that waits; returns once its request is
        in the pane state (or the hook has exited)."""
        before = len(self.pending())
        proc = subprocess.Popen(
            [sys.executable, str(HOOK), agent],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env={**self.env, **env},
        )
        proc.stdin.write(json.dumps(event))
        proc.stdin.close()
        self.procs.append(proc)
        self.wait_until(lambda: len(self.pending()) > before or proc.poll() is not None)
        return proc

    def finish(self, proc):
        out, _ = proc.communicate(timeout=10)
        self.assertEqual(proc.returncode, 0)
        return out

    def wait_until(self, predicate, timeout=10):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if predicate():
                return
            time.sleep(0.05)
        self.fail("condition not reached")

    def pending(self):
        try:
            return json.loads((self.state / "w1_p1.json").read_text())["pending"]
        except (OSError, ValueError):
            return []

    def reports(self):
        if not self.log.exists():
            return []
        out = []
        for line in self.log.read_bytes().split(b"\n"):
            if not line:
                continue
            args = line.decode().split("\0")[:-1]
            self.assertEqual(args[:5], ["pane", "report-metadata", "w1:p1", "--source", "drovr-state"])
            self.assertEqual(args[5], "--seq")
            tokens = {}
            rest = args[7:]
            for flag, value in zip(rest[::2], rest[1::2]):
                if flag == "--token":
                    key, _, val = value.partition("=")
                    tokens[key] = val
                else:
                    tokens[value] = None
            out.append((int(args[6]), tokens))
        return out

    def tokens(self):
        current = {}
        last_seq = 0
        for seq, report in self.reports():
            self.assertGreater(seq, last_seq, "seq must increase")
            last_seq = seq
            for key, value in report.items():
                if value is None:
                    current.pop(key, None)
                else:
                    current[key] = value
        return current

    def wait_req(self):
        wait = self.tokens().get("drovr_wait")
        return wait.split("|")[1] if wait else None

    def decide(self, req, decision):
        path = self.state / "decide" / f"{req}.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_suffix(".tmp")
        tmp.write_text(json.dumps(decision))
        tmp.rename(path)

    # recorded payloads (Claude Code 2.1.287 shapes)

    @staticmethod
    def pre(tool_id, command, **extra):
        return {
            "hook_event_name": "PreToolUse",
            "session_id": "s1",
            "cwd": "/work",
            "tool_name": "Bash",
            "tool_input": {"command": command, "description": "run"},
            "tool_use_id": tool_id,
            **extra,
        }

    @staticmethod
    def permission(command, **extra):
        return {
            "hook_event_name": "PermissionRequest",
            "session_id": "s1",
            "cwd": "/work",
            "permission_mode": "default",
            "tool_name": "Bash",
            "tool_input": {"command": command, "description": "run"},
            "permission_suggestions": [
                {"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": command}],
                 "behavior": "allow", "destination": "localSettings"}
            ],
            **extra,
        }

    @staticmethod
    def post(tool_id, command, event="PostToolUse", **extra):
        return {
            "hook_event_name": event,
            "session_id": "s1",
            "tool_name": "Bash",
            "tool_input": {"command": command, "description": "run"},
            "tool_use_id": tool_id,
            **extra,
        }

    def prompt(self):
        self.run_hook({"hook_event_name": "UserPromptSubmit", "session_id": "s1", "prompt": "go"})

    # tests

    def test_parallel_tools_keep_each_pending_request(self):
        self.prompt()
        self.run_hook(self.pre("t1", "git push"))
        self.run_hook(self.pre("t2", "rm -rf build"))
        first = self.start_hook(self.permission("git push"))
        req1 = self.wait_req()
        second = self.start_hook(self.permission("rm -rf build"))
        self.assertEqual(self.wait_req(), req1, "the oldest request stays shown")
        self.assertEqual(self.tokens()["drovr_wait"], f"permission|{req1}||Bash git push")
        # The second tool finishes: only its request leaves.
        self.run_hook(self.post("t2", "rm -rf build"))
        self.assertEqual(self.finish(second), "")
        self.assertEqual(self.wait_req(), req1)
        self.run_hook(self.post("t1", "git push"))
        self.assertEqual(self.finish(first), "")
        self.assertNotIn("drovr_wait", self.tokens())

    def test_subagent_request_is_labelled_and_does_not_set_doing(self):
        self.prompt()
        self.run_hook(self.pre("t0", "cargo test"))
        self.run_hook(self.pre("t1", "ls", agent_id="a1", agent_type="Explore"))
        self.assertEqual(self.tokens()["drovr_doing"], "Bash cargo test")
        proc = self.start_hook(self.permission("ls", agent_id="a1", agent_type="Explore"))
        req = self.wait_req()
        self.assertEqual(self.tokens()["drovr_wait"], f"permission|{req}|sub|Bash ls")
        self.run_hook(self.post("t1", "ls", agent_id="a1"))
        self.assertEqual(self.finish(proc), "")
        self.assertEqual(self.tokens()["drovr_doing"], "Bash cargo test")

    def test_reject_in_terminal_clears_on_idle_notification(self):
        # "No" in the terminal fires no PostToolUse, PermissionDenied or Stop.
        self.prompt()
        self.run_hook(self.pre("t1", "git push"))
        proc = self.start_hook(self.permission("git push"))
        self.assertIsNotNone(self.wait_req())
        self.run_hook({
            "hook_event_name": "Notification",
            "notification_type": "permission_prompt",
            "message": "Claude needs your permission to use Bash",
        })
        self.assertIsNotNone(self.wait_req(), "a permission notification keeps the item")
        self.run_hook({
            "hook_event_name": "Notification",
            "notification_type": "idle_prompt",
            "message": "Claude is waiting for your input",
        })
        self.assertEqual(self.finish(proc), "")
        self.assertNotIn("drovr_wait", self.tokens())
        self.assertNotIn("drovr_doing", self.tokens())

    def test_esc_interrupt_then_new_prompt_clears(self):
        self.prompt()
        self.run_hook(self.pre("t1", "git push"))
        proc = self.start_hook(self.permission("git push"))
        self.assertIsNotNone(self.wait_req())
        self.prompt()
        self.assertEqual(self.finish(proc), "")
        tokens = self.tokens()
        self.assertNotIn("drovr_wait", tokens)
        self.assertNotIn("drovr_doing", tokens)
        self.assertTrue(tokens["drovr_state"].startswith("working|"))

    def test_decision_file_answers_the_request(self):
        cases = [
            ({"behavior": "allow"}, {"behavior": "allow"}),
            ({"behavior": "deny", "message": "use --dry-run"}, {"behavior": "deny", "message": "use --dry-run"}),
            ({"behavior": "deny"}, {"behavior": "deny", "message": "Denied in drovr"}),
        ]
        for decision, expected in cases:
            with self.subTest(decision=decision):
                self.prompt()
                self.run_hook(self.pre("t1", "git push"))
                proc = self.start_hook(self.permission("git push"))
                self.decide(self.wait_req(), decision)
                out = json.loads(self.finish(proc))
                self.assertEqual(out["hookSpecificOutput"]["hookEventName"], "PermissionRequest")
                self.assertEqual(out["hookSpecificOutput"]["decision"], expected)
                self.assertNotIn("drovr_wait", self.tokens())

    def test_always_sends_permission_suggestions(self):
        self.prompt()
        event = self.permission("git push")
        proc = self.start_hook(event)
        self.decide(self.wait_req(), {"behavior": "allow", "always": True})
        decision = json.loads(self.finish(proc))["hookSpecificOutput"]["decision"]
        self.assertEqual(decision, {"behavior": "allow", "updatedPermissions": event["permission_suggestions"]})

    def test_invalid_and_stale_decision_files_are_ignored(self):
        self.prompt()
        proc = self.start_hook(self.permission("git push"))
        req = self.wait_req()
        self.decide(req, {"behavior": "maybe"})
        self.decide("deadbeef", {"behavior": "allow"})  # no such pending request
        time.sleep(0.6)
        self.assertIsNone(proc.poll(), "an invalid decision does not end the wait")
        self.assertIn("invalid decision file", (self.state / "errors.log").read_text())
        self.decide(req, {"behavior": "allow"})
        self.assertEqual(json.loads(self.finish(proc))["hookSpecificOutput"]["decision"], {"behavior": "allow"})
        self.assertTrue((self.state / "decide" / "deadbeef.json").exists(), "only the own file is read")

    def test_wait_limit_exits_silently_and_drops_the_item(self):
        self.prompt()
        proc = self.start_hook(self.permission("git push"), DROVR_DECIDE_WAIT_S="0.5")
        self.assertIsNotNone(self.wait_req())
        self.assertEqual(self.finish(proc), "")
        self.assertNotIn("drovr_wait", self.tokens())

    def test_unmatched_request_is_cleared_by_turn_boundary(self):
        self.prompt()
        proc = self.start_hook(self.permission("never pre-announced"))
        self.run_hook(self.post("t9", "other"))
        self.assertIsNotNone(self.wait_req(), "an unrelated PostToolUse keeps it")
        self.run_hook({"hook_event_name": "Stop", "last_assistant_message": "Done."})
        self.assertEqual(self.finish(proc), "")
        self.assertNotIn("drovr_wait", self.tokens())

    def test_question_and_plan_items(self):
        self.prompt()
        question = {
            "hook_event_name": "PermissionRequest",
            "tool_name": "AskUserQuestion",
            "tool_input": {"questions": [{
                "question": "Which layout for the inbox?",
                "header": "Layout",
                "multiSelect": False,
                "options": [{"label": "one pane", "description": ""}, {"label": "two panes", "description": ""}],
            }]},
        }
        proc = self.start_hook(question)
        req = self.wait_req()
        tokens = self.tokens()
        self.assertEqual(tokens["drovr_wait"], f"question|{req}||Which layout for the inbox?")
        self.assertEqual((tokens["drovr_o1"], tokens["drovr_o2"]), ("one pane", "two panes"))
        self.assertNotIn("drovr_o3", tokens)
        self.decide(req, {"behavior": "deny", "message": "other: a popup"})
        self.finish(proc)
        self.assertNotIn("drovr_o1", self.tokens())

        multi = json.loads(json.dumps(question))
        multi["tool_input"]["questions"][0]["multiSelect"] = True
        self.assertEqual(self.run_hook(multi), "", "multi-select is a dialog item: no wait")
        self.assertNotIn("drovr_wait", self.tokens())

        plan = {
            "hook_event_name": "PermissionRequest",
            "tool_name": "ExitPlanMode",
            "tool_input": {"plan": "# Inbox pane\n\n1. step\n2. step\n", "planFilePath": "/home/u/.claude/plans/inbox.md"},
        }
        proc = self.start_hook(plan)
        req = self.wait_req()
        tokens = self.tokens()
        self.assertEqual(tokens["drovr_wait"], f"plan|{req}||Inbox pane")
        self.assertEqual(tokens["drovr_diff"], "4 lines")
        state = json.loads((self.state / "w1_p1.json").read_text())
        self.assertEqual(state["pending"][0]["plan_path"], "/home/u/.claude/plans/inbox.md")
        self.decide(req, {"behavior": "allow"})
        self.finish(proc)

    def test_edit_diffstat(self):
        self.prompt()
        proc = self.start_hook({
            "hook_event_name": "PermissionRequest",
            "cwd": "/work",
            "tool_name": "Edit",
            "tool_input": {"file_path": "/work/src/x.rs", "old_string": "a\nb\nc", "new_string": "a\n" * 12},
        })
        self.assertEqual(self.tokens()["drovr_diff"], "+12 −3 src/x.rs")
        self.assertTrue(self.tokens()["drovr_wait"].endswith("|Edit src/x.rs"))
        self.decide(self.wait_req(), {"behavior": "allow"})
        self.finish(proc)

    def test_stop_states(self):
        self.prompt()
        self.run_hook({"hook_event_name": "Stop", "last_assistant_message": "I updated the spec.\nShould I also update §39?"})
        tokens = self.tokens()
        self.assertTrue(tokens["drovr_state"].startswith("asks|"))
        self.assertEqual(tokens["drovr_last"], "I updated the spec.?")
        self.prompt()
        self.assertNotIn("drovr_last", self.tokens())
        self.run_hook({"hook_event_name": "StopFailure", "error": "rate_limit"})
        self.assertTrue(self.tokens()["drovr_state"].startswith("limit|"))
        self.prompt()
        self.run_hook({"hook_event_name": "SessionEnd", "reason": "other"})
        self.assertTrue(self.tokens()["drovr_state"].startswith("exited|"))
        self.run_hook({"hook_event_name": "SessionStart", "source": "startup"})
        self.run_hook({"hook_event_name": "SessionEnd", "reason": "prompt_input_exit"})
        self.assertEqual(self.tokens(), {})

    def test_last_message_from_transcript(self):
        transcript = self.tmp / "t.jsonl"
        transcript.write_text(
            json.dumps({"type": "assistant", "message": {"content": [{"type": "text", "text": "Layout tests pass"}]}}) + "\n"
        )
        self.prompt()
        self.run_hook({"hook_event_name": "Stop", "transcript_path": str(transcript)})
        self.assertEqual(self.tokens()["drovr_last"], "Layout tests pass")
        self.assertTrue(self.tokens()["drovr_state"].startswith("finished|"))

    def test_redaction_and_kinds_only(self):
        self.prompt()
        command = (
            "curl -H 'Authorization: Bearer abcdefghijklmnop' https://user:hunter2@example.com "
            "API_KEY=s3cr3tvalue ghp_0123456789abcdefghijABCDEFGHIJ"
        )
        self.run_hook(self.pre("t1", command))
        doing = self.tokens()["drovr_doing"]
        for secret in ("abcdefghijklmnop", "hunter2", "s3cr3tvalue", "ghp_0123"):
            self.assertNotIn(secret, doing)
        self.assertLessEqual(len(doing), 80)
        hook = load_hook()
        path = "cargo test --manifest-path /Users/someone/Code/project/crates/very_long_crate_name/Cargo.toml"
        self.assertEqual(hook.redact(path), path, "paths are kept")
        name = "negotiated_remote_health_probe_expires_the_connection"
        self.assertEqual(hook.redact(name), name, "long identifiers without digits are kept")
        self.assertEqual(hook.redact("mysql -psecretpw db"), "mysql -p*** db")
        self.assertEqual(hook.redact("curl -u admin:hunter2 https://api"), "curl -u admin:*** https://api")
        self.assertEqual(hook.redact("curl --user=admin:hunter2 x"), "curl --user=admin:*** x")
        self.assertEqual(hook.redact("X-Auth-Token: abc123"), "X-Auth-Token: ***")
        self.assertEqual(hook.redact("export GITHUB_TOKEN=abc"), "export GITHUB_TOKEN=***")
        sha = "0123456789abcdef0123456789abcdef01234567"
        for kept in (
            "cargo nextest run client::auth::login",
            "git log --author=bob",
            "find . -print0",
            "max_tokens=100",
            f"git show {sha}",
        ):
            self.assertEqual(hook.redact(kept), kept)
        self.assertEqual(hook.redact(f"echo {sha}"), "echo ***", "40-hex outside git is redacted")

        self.prompt()
        time.sleep(2.1)  # past the PreToolUse report gap
        self.run_hook(self.pre("t3", "git push origin main"), DROVR_STATE_TEXT="0")
        self.assertEqual(self.tokens()["drovr_doing"], "Bash")
        proc = self.start_hook(self.permission("git push origin main"), DROVR_STATE_TEXT="0")
        self.assertEqual(self.tokens()["drovr_wait"], f"permission|{self.wait_req()}||Bash", "tool name only")
        self.decide(self.wait_req(), {"behavior": "allow"})
        self.finish(proc)
        self.run_hook({"hook_event_name": "Stop", "last_assistant_message": "secret plans?"}, DROVR_STATE_TEXT="0")
        self.assertNotIn("drovr_last", self.tokens())
        self.assertTrue(self.tokens()["drovr_state"].startswith("asks|"))

    def test_pre_tool_use_is_throttled_and_unchanged_reports_skipped(self):
        self.prompt()
        count = len(self.reports())
        self.run_hook({"hook_event_name": "Notification", "notification_type": "permission_prompt", "message": "x"})
        self.assertEqual(len(self.reports()), count, "no change, no report")
        self.run_hook(self.pre("t1", "ls"))
        self.assertEqual(len(self.reports()), count + 1)
        self.run_hook(self.pre("t2", "pwd"))
        self.assertEqual(len(self.reports()), count + 1, "a PreToolUse within 2 s of the last one is held back")
        # It is still recorded for permission matching, and sent with the next report.
        state = json.loads((self.state / "w1_p1.json").read_text())
        self.assertEqual([p[0] for p in state["pre"]], ["t1", "t2"])
        self.run_hook({"hook_event_name": "Stop", "last_assistant_message": "Done."})
        self.assertNotIn("drovr_doing", self.tokens())

    def test_quick_tool_then_long_tool_keeps_doing(self):
        self.prompt()
        self.run_hook(self.pre("t1", "cat a"))
        self.run_hook(self.post("t1", "cat a"))
        self.run_hook(self.pre("t2", "cargo test"))
        self.assertEqual(self.tokens()["drovr_doing"], "Bash cat a", "the clear is held back with the next tool")
        time.sleep(2.1)
        self.run_hook(self.post("t2", "cargo test"))
        self.assertNotIn("drovr_doing", self.tokens())
        self.run_hook(self.pre("t3", "cargo build"))
        self.assertEqual(self.tokens()["drovr_doing"], "Bash cargo build", "a clear does not start the gap")

    def test_request_without_tool_id_ends_with_its_tool(self):
        # Another PreToolUse hook changed the input: no PreToolUse hash matches.
        self.prompt()
        self.run_hook(self.pre("t1", "git push"))
        proc = self.start_hook(self.permission("git push --dry-run"))
        self.assertIsNotNone(self.wait_req())
        self.run_hook(self.post("t1", "git push --dry-run"))
        self.assertEqual(self.finish(proc), "")
        self.assertNotIn("drovr_wait", self.tokens())

    def test_request_after_its_tool_finished_is_not_added(self):
        # Another PermissionRequest hook allowed a fast tool before this hook
        # took the pane lock.
        self.prompt()
        self.run_hook(self.pre("t1", "git push"))
        self.run_hook(self.post("t1", "git push"))
        start = time.monotonic()
        self.assertEqual(self.run_hook(self.permission("git push")), "")
        self.assertLess(time.monotonic() - start, 5)
        self.assertEqual(self.pending(), [])
        # The next real request for the same command is published.
        self.run_hook(self.pre("t2", "git push"))
        proc = self.start_hook(self.permission("git push"))
        self.assertIsNotNone(self.wait_req())
        self.run_hook(self.post("t2", "git push"))
        self.finish(proc)

    def test_nested_claude_session_is_ignored(self):
        owner = subprocess.Popen(["sleep", "60"])
        self.procs.append(owner)
        child = subprocess.Popen(["sleep", "60"])
        self.procs.append(child)
        parent_env = {"CLAUDE_PID": str(owner.pid)}
        child_env = {"CLAUDE_PID": str(child.pid)}
        self.run_hook({"hook_event_name": "UserPromptSubmit", "session_id": "s1", "prompt": "go"}, **parent_env)
        self.run_hook(self.pre("t1", "git push"), **parent_env)
        proc = self.start_hook(self.permission("git push"), **parent_env)
        req = self.wait_req()
        for event in (
            {"hook_event_name": "SessionStart", "session_id": "c1", "source": "startup"},
            {"hook_event_name": "UserPromptSubmit", "session_id": "c1", "prompt": "child"},
            {"hook_event_name": "Stop", "session_id": "c1", "last_assistant_message": "child done?"},
            {"hook_event_name": "SessionEnd", "session_id": "c1", "reason": "other"},
        ):
            self.run_hook(event, **child_env)
        # Codex started from the owner's Bash tool inherits its CLAUDE_PID.
        self.run_hook({"hook_event_name": "Stop", "session_id": "x1"}, agent="codex", **parent_env)
        self.assertIsNone(proc.poll(), "the parent's hook still waits")
        tokens = self.tokens()
        self.assertEqual(self.wait_req(), req)
        self.assertTrue(tokens["drovr_state"].startswith("working|"))
        self.assertNotIn("drovr_last", tokens)
        self.decide(req, {"behavior": "allow"})
        self.assertEqual(json.loads(self.finish(proc))["hookSpecificOutput"]["decision"], {"behavior": "allow"})
        # Once the owner is gone, a new Claude in the pane takes over.
        owner.kill()
        owner.wait()
        self.run_hook({"hook_event_name": "SessionStart", "session_id": "c2", "source": "startup"}, **child_env)
        self.assertTrue(self.tokens()["drovr_state"].startswith("idle|"))

    def test_nested_codex_thread_is_ignored(self):
        self.run_hook({"hook_event_name": "UserPromptSubmit", "session_id": "th1"}, agent="codex", CODEX_THREAD_ID="th1")
        self.run_hook({"hook_event_name": "Stop", "session_id": "th2"}, agent="codex", CODEX_THREAD_ID="th1")
        self.assertTrue(self.tokens()["drovr_state"].startswith("working|"))

    def test_codex_interrupt_ends_the_turn(self):
        self.run_hook({"hook_event_name": "UserPromptSubmit", "session_id": "th1"}, agent="codex")
        self.run_hook(self.pre("c1", "cargo test"), agent="codex")
        self.run_hook(self.permission("cargo test", tool_use_id="c1"), agent="codex")
        self.assertIsNotNone(self.wait_req())
        self.run_hook({"hook_event_name": "Interrupt", "session_id": "th1"}, agent="codex")
        tokens = self.tokens()
        self.assertNotIn("drovr_wait", tokens)
        self.assertNotIn("drovr_doing", tokens)
        self.assertTrue(tokens["drovr_state"].startswith("idle|"))

    def test_subagent_events_move_the_beat_not_the_tool_time(self):
        self.prompt()
        self.run_hook(self.pre("t1", "explore", tool_name="Agent"))
        working = self.tokens()["drovr_state"]
        self.assertEqual(working.count("|"), 1, "no beat while it equals the tool time")
        # Ten minutes later the subagent is still running tools.
        path = self.state / "w1_p1.json"
        data = json.loads(path.read_text())
        data["ts"] -= 600
        data["beat"] -= 600
        path.write_text(json.dumps(data))
        self.run_hook(self.pre("s1", "ls", agent_id="sub1"))
        kind, ts, beat = self.tokens()["drovr_state"].split("|")
        self.assertEqual((kind, int(ts)), ("working", data["ts"]))
        self.assertGreaterEqual(int(beat), data["ts"] + 600)
        # A kind change moves the time to the beat again.
        self.run_hook({"hook_event_name": "Stop", "session_id": "s1", "last_assistant_message": "done"})
        self.assertEqual(self.tokens()["drovr_state"].split("|")[0::2], ["finished"])

    def test_codex_request_is_published_without_waiting(self):
        self.prompt()
        start = time.monotonic()
        self.assertEqual(self.run_hook(self.permission("cargo test", tool_use_id="c1"), agent="codex"), "")
        self.assertLess(time.monotonic() - start, 5)
        self.assertIsNotNone(self.wait_req())
        self.run_hook(self.post("c1", "cargo test"), agent="codex")
        self.assertNotIn("drovr_wait", self.tokens())

    def test_report_failure_is_logged_and_retried(self):
        failing = self.tmp / "failing-herdr"
        failing.write_text("#!/bin/sh\necho 'metadata_token_limit' >&2\nexit 1\n")
        failing.chmod(0o755)
        self.run_hook({"hook_event_name": "UserPromptSubmit"}, HERDR_BIN_PATH=str(failing))
        self.assertIn("metadata_token_limit", (self.state / "errors.log").read_text())
        self.run_hook({"hook_event_name": "Notification", "notification_type": "permission_prompt", "message": "x"})
        self.assertTrue(self.tokens()["drovr_state"].startswith("working|"), "unsent values go out with the next report")

    def test_without_pane_does_nothing(self):
        env = dict(self.env)
        del env["HERDR_PANE_ID"]
        result = subprocess.run([sys.executable, str(HOOK), "claude"], input=json.dumps(self.permission("x")),
                                capture_output=True, text=True, env=env, timeout=10)
        self.assertEqual((result.returncode, result.stdout), (0, ""))
        self.assertFalse(self.state.exists())


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.home = Path(tempfile.mkdtemp())
        (self.home / ".claude").mkdir()
        (self.home / ".codex").mkdir()
        self.orca = {"type": "command", "command": "orca-hook", "timeout": 10}
        self.settings = self.home / ".claude" / "settings.json"
        self.settings.write_text(json.dumps({
            "permissions": {"defaultMode": "auto"},
            "hooks": {"PermissionRequest": [{"hooks": [self.orca]}, {"hooks": [{"type": "command", "command": "rosterd-hook"}]}]},
        }))
        self.codex = self.home / ".codex" / "hooks.json"
        self.codex.write_text(json.dumps({"hooks": {"PermissionRequest": [{"hooks": [self.orca]}]}}))
        self.env = clean_env(self.home)
        self.env["DROVR_RAW"] = f"file://{REPO}"

    def tearDown(self):
        shutil.rmtree(self.home)

    def install(self, *args):
        result = subprocess.run(["bash", str(INSTALLER), *args], capture_output=True, text=True, env=self.env, timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def test_dry_run_prints_diff_and_changes_nothing(self):
        before = (self.settings.read_text(), self.codex.read_text())
        out = self.install("--dry-run")
        self.assertIn(f"+++ {self.settings} (drovr)", out)
        self.assertIn(f"+++ {self.codex} (drovr)", out)
        self.assertIn("drovr-state-hook claude", out)
        self.assertIn("drovr-state-hook codex", out)
        self.assertEqual((self.settings.read_text(), self.codex.read_text()), before)
        self.assertFalse((self.home / ".local").exists())
        self.assertFalse(list((self.home / ".claude").glob("*.bak*")))

    def test_install_appends_after_existing_hooks(self):
        self.install()
        state = str(self.home / ".local/bin/drovr-state-hook")
        self.assertTrue(os.access(state, os.X_OK))
        settings = json.loads(self.settings.read_text())
        groups = settings["hooks"]["PermissionRequest"]
        self.assertEqual([g["hooks"][0]["command"] for g in groups], ["orca-hook", "rosterd-hook", f"python3 {state} claude"])
        self.assertEqual(groups[2]["hooks"][0]["timeout"], 605)
        for event in ("PreToolUse", "PostToolUseFailure", "PermissionDenied", "Notification", "StopFailure", "SessionEnd"):
            self.assertEqual(settings["hooks"][event][-1]["hooks"][0]["command"], f"python3 {state} claude")
        self.assertEqual(settings["permissions"], {"defaultMode": "auto"})
        codex = json.loads(self.codex.read_text())
        self.assertEqual([g["hooks"][0]["command"] for g in codex["hooks"]["PermissionRequest"]],
                         ["orca-hook", f"python3 {state} codex"])
        self.assertNotIn("Notification", codex["hooks"])
        self.assertEqual(codex["hooks"]["Interrupt"][-1]["hooks"][0]["command"], f"python3 {state} codex")
        self.assertTrue((self.home / ".claude/settings.json.bak-pre-drovr").exists())
        # A second run changes nothing.
        self.assertIn("no change", self.install("--dry-run"))
        before = self.settings.read_text()
        self.install()
        self.assertEqual(self.settings.read_text(), before)


if __name__ == "__main__":
    unittest.main()
