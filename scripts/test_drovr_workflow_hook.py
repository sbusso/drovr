"""Tests for scripts/drovr-workflow-hook: script meta, journal progress, the
Markdown view, end detection, and its registration by drovr-install-hooks."""

import importlib.machinery
import importlib.util
import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
HOOK = REPO / "scripts" / "drovr-workflow-hook"
INSTALLER = REPO / "scripts" / "drovr-install-hooks"

_loader = importlib.machinery.SourceFileLoader("drovr_workflow_hook", str(HOOK))
_spec = importlib.util.spec_from_loader(_loader.name, _loader)
wf = importlib.util.module_from_spec(_spec)
_loader.exec_module(wf)

SCRIPT = """export const meta = {
  name: 'demo',
  description: 'Build the demo',
  phases: [
    { title: 'Plan', detail: 'outline' },
    { title: "Build" },
    { title: `Review`, detail: 'two lenses' },
  ],
}
await phase('Plan')
"""

JOURNAL = [
    {"type": "launched"},
    {"type": "started", "key": "a", "label": "planner", "phase": "Plan"},
    {"type": "result", "key": "a", "result": {"status": "done", "changes": "Wrote the plan."}},
    {"type": "started", "key": "b", "label": "builder", "phase": "Build"},
]


class WorkflowHookTests(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())

    def tearDown(self):
        shutil.rmtree(self.dir)

    def write(self, name, text):
        path = self.dir / name
        path.write_text(text)
        return str(path)

    def test_script_meta_reads_description_and_phases(self):
        desc, phases = wf.script_meta(self.write("demo.js", SCRIPT))
        self.assertEqual(desc, "Build the demo")
        self.assertEqual(phases, [("Plan", "outline"), ("Build", ""), ("Review", "two lenses")])
        self.assertEqual(wf.script_meta(str(self.dir / "missing.js")), ("", []))

    def test_progress_counts_phases_before_the_current_one(self):
        _, phases = wf.script_meta(self.write("demo.js", SCRIPT))
        journal = self.write("journal.jsonl", "".join(json.dumps(e) + "\n" for e in JOURNAL) + "{broken\n")
        agents = wf.read_journal(journal)
        self.assertEqual([a["label"] for a in agents.values()], ["planner", "builder"])
        self.assertEqual(wf.progress(phases, agents, "running"), (1, 3, 1))
        self.assertEqual(wf.progress(phases, agents, "done"), (3, 3, None))
        self.assertEqual(wf.progress(phases, {}, "running"), (0, 3, None))

    def test_render_marks_phases_and_lists_results(self):
        desc, phases = wf.script_meta(self.write("demo.js", SCRIPT))
        agents = wf.read_journal(self.write("journal.jsonl", "".join(json.dumps(e) + "\n" for e in JOURNAL)))
        text, cue = wf.render({"name": "demo"}, desc, phases, agents, "running", 0)
        self.assertEqual(cue, ("running 1/3", "1/2 agents"))
        self.assertIn("- ✓ **Plan** — outline", text)
        self.assertIn("- ▸ **Build**", text)
        self.assertIn("- · **Review** — two lenses", text)
        self.assertIn("| builder | Build | running |", text)
        self.assertIn("### planner\n\nWrote the plan.", text)
        _, cue = wf.render({"name": "demo"}, desc, phases, agents, "failed", 0)
        self.assertEqual(cue, ("failed 1/3", "failed · 1/2 agents"))
        _, cue = wf.render({"name": "demo"}, desc, phases, agents, "done", 0)
        self.assertEqual(cue, ("done 3/3", "1/2 agents"))
        _, cue = wf.render({"name": "demo"}, desc, phases, {}, "running", 0)
        self.assertEqual(cue, ("running 0/3", "starting"))
        planning = {"a": {"label": "planner", "phase": "Plan", "result": None}}
        _, cue = wf.render({"name": "demo"}, desc, phases, planning, "running", 0)
        self.assertEqual(cue, ("running 0/3", "0/1 agents"))

    def test_scan_reads_only_this_task_after_the_offset(self):
        note = lambda task, status: json.dumps({"type": "attachment", "attachment": {
            "prompt": f"<task-notification>\n<task-id>{task}</task-id>\n<status>{status}</status>"}}) + "\n"
        transcript = self.write("t.jsonl", note("w1", "completed"))
        offset = os.path.getsize(transcript)
        with open(transcript, "a") as handle:
            handle.write(note("other", "completed"))
        self.assertEqual(wf.scan_transcript(transcript, "r1", "w1", offset)[0], None)
        self.assertEqual(wf.scan_transcript(transcript, "r1", "w1", 0)[0], "completed")
        with open(transcript, "a") as handle:
            handle.write(note("w1", "failed"))
            handle.write('{"partial": ')
        status, task, new_offset, _ = wf.scan_transcript(transcript, "r1", "w1", offset)
        self.assertEqual((status, task), ("failed", "w1"))
        self.assertLess(new_offset, os.path.getsize(transcript), "partial line is read next time")

    def test_scan_follows_a_resume_of_the_same_run(self):
        note = lambda task, status: json.dumps({"attachment": {
            "prompt": f"<task-id>{task}</task-id><status>{status}</status>"}}) + "\n"
        launch = lambda run, task: json.dumps({"toolUseResult": {
            "status": "async_launched", "runId": run, "taskId": task}}) + "\n"
        transcript = self.write("t.jsonl", note("w1", "killed") + launch("r1", "w2"))
        self.assertEqual(wf.scan_transcript(transcript, "r1", "w1", 0)[:2], (None, "w2"))
        with open(transcript, "a") as handle:
            handle.write(launch("r9", "w9"))
            handle.write(note("w2", "completed"))
        self.assertEqual(wf.scan_transcript(transcript, "r1", "w1", 0)[:2], ("completed", "w2"))

    def test_scan_sees_the_next_prompt(self):
        line = lambda kind: json.dumps({"type": "user", "origin": {"kind": kind},
                                        "message": {"content": "next task"}}) + "\n"
        transcript = self.write("t.jsonl", line("task-notification"))
        self.assertFalse(wf.scan_transcript(transcript, "r1", "w1", 0)[3])
        with open(transcript, "a") as handle:
            handle.write(line("human"))
        self.assertTrue(wf.scan_transcript(transcript, "r1", "w1", 0)[3])

    def test_launch_ignores_other_tool_results(self):
        # No pane, or a result that is not an async launch: nothing runs.
        os.environ.pop("HERDR_PANE_ID", None)
        wf.launch({"tool_response": {"status": "async_launched", "transcriptDir": "/x"}})
        os.environ["HERDR_PANE_ID"] = "w1:p1"
        try:
            wf.launch({"tool_response": "text result"})
            wf.launch({"tool_response": {"status": "completed"}})
        finally:
            os.environ.pop("HERDR_PANE_ID")


class InstallerTests(unittest.TestCase):
    def test_installer_registers_the_workflow_hook(self):
        home = Path(tempfile.mkdtemp())
        try:
            (home / ".claude").mkdir()
            env = {"HOME": str(home), "PATH": os.environ["PATH"], "DROVR_RAW": f"file://{REPO}"}
            result = subprocess.run(["bash", str(INSTALLER)], capture_output=True, text=True, env=env, timeout=60)
            self.assertEqual(result.returncode, 0, result.stderr)
            hook = home / ".local/bin/drovr-workflow-hook"
            self.assertTrue(os.access(hook, os.X_OK))
            settings = json.loads((home / ".claude/settings.json").read_text())
            groups = [g for g in settings["hooks"]["PostToolUse"] if g.get("matcher") == "Workflow"]
            self.assertEqual([g["hooks"][0]["command"] for g in groups], [f"python3 {hook}"])
        finally:
            shutil.rmtree(home)


if __name__ == "__main__":
    unittest.main()
