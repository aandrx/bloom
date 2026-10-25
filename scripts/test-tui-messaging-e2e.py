#!/usr/bin/env python3
"""Drive the real Bloom TUI through a PTY against isolated synthetic IPC state.

Run after `cargo build -p bloom-tui` with a Python interpreter containing pyte:
  ~/.cache/bloom-tools/bin/python scripts/test-tui-messaging-e2e.py
No real engine, account, socket or configuration is accessed.
Set BLOOM_E2E_ARTIFACTS to retain synthetic ANSI, text and HTML screen captures.
"""
from __future__ import annotations

import contextlib
import json
import os
from pathlib import Path
import runpy
import socket
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
SHOT = runpy.run_path(str(ROOT / "scripts/tui-shot.py"), run_name="capture_helpers")
DAEMON = ROOT / "scripts/stateful-fake-daemon.py"
BINARY = ROOT / "target/debug/bloom-tui"


@contextlib.contextmanager
def isolated_environment(values):
    previous = {name: os.environ.get(name) for name in values}
    os.environ.update(values)
    try:
        yield
    finally:
        for name, value in previous.items():
            if value is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = value


class MessagingFlows(unittest.TestCase):
    def setUp(self):
        self.assertTrue(BINARY.is_file(), "Build bloom-tui before running this suite")
        self.directory = tempfile.TemporaryDirectory(prefix="bloom-tui-functional-")
        self.root = Path(self.directory.name)
        self.socket_path = self.root / "ipc.sock"
        self.store_path = self.root / "fixture.json"
        self.daemon = None
        self.capture_count = 0

    def tearDown(self):
        self.stop_daemon()
        self.directory.cleanup()

    def start_daemon(self, *options):
        self.daemon = subprocess.Popen(
            [sys.executable, str(DAEMON), "--socket", str(self.socket_path),
             "--store", str(self.store_path), *options],
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        )
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if self.daemon.poll() is not None:
                self.fail("Synthetic daemon exited during startup")
            try:
                with socket.socket(socket.AF_UNIX) as connection:
                    connection.connect(str(self.socket_path))
                return
            except OSError:
                time.sleep(0.01)
        self.fail("Synthetic daemon did not become ready")

    def stop_daemon(self):
        if self.daemon is None:
            return
        if self.daemon.poll() is None:
            self.daemon.terminate()
            try:
                self.daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.daemon.kill()
                self.daemon.wait(timeout=5)
        if self.daemon.stderr is not None:
            self.daemon.stderr.close()
        self.daemon = None

    def capture(self, keys):
        environment = {
            "BLOOM_SOCKET_PATH": str(self.socket_path),
            "BLOOM_SOCKET": str(self.socket_path),
            "XDG_CONFIG_HOME": str(self.root / "config"),
            "XDG_STATE_HOME": str(self.root / "logs"),
            "XDG_DATA_HOME": str(self.root / "data"),
            "NO_COLOR": "1",
        }
        with isolated_environment(environment):
            raw = SHOT["run_tui"](
                [str(BINARY)], 100, 30, SHOT["parse_keys"](keys), 1.5,
            )
        screen = SHOT["render"](raw, 100, 30)
        text = "\n".join(screen.display)
        self.capture_count += 1
        artifact_directory = os.environ.get("BLOOM_E2E_ARTIFACTS")
        if artifact_directory:
            destination = Path(artifact_directory) / self._testMethodName / str(self.capture_count)
            destination.mkdir(parents=True, exist_ok=True)
            (destination / "screen.ansi").write_bytes(raw)
            (destination / "screen.txt").write_text(text + "\n")
            (destination / "screen.html").write_text(SHOT["to_html"](screen, 100, 30))
        return text

    def persisted(self):
        return json.loads(self.store_path.read_text())

    def test_send_routes_exact_body_and_survives_restart(self):
        self.start_daemon()
        text = self.capture("text:ExactAliceBody enter")
        state = self.persisted()
        self.assertEqual(len(state["messages"]), 1)
        message = state["messages"][0]
        self.assertEqual(message["conversation_id"]["value"], "alice@example.invalid")
        self.assertEqual(message["parts"], [{"type": "text", "value": "ExactAliceBody"}])
        self.assertTrue(any("ExactAliceBody" in line.partition("│")[2] for line in text.splitlines()),
                        "Live message must appear in the transcript, not just a sidebar preview")
        self.stop_daemon()
        self.start_daemon()
        self.assertIn("ExactAliceBody", self.capture(""), "History must return after relaunch")

    def test_rejected_send_restores_draft_and_explicit_retry_succeeds(self):
        self.start_daemon("--reject-first-send")
        text = self.capture("text:RecoverAliceDraft enter")
        self.assertEqual(self.persisted()["messages"], [], "Rejection must not persist an echo")
        self.assertIn("not sent", text.lower(), "Fixture rejection must reach the TUI")
        self.assertIn("RecoverAliceDraft", text, "Rejected draft must remain editable")

        # Use a fresh isolated fixture for a continuous rejection -> retry user flow.
        self.stop_daemon()
        self.store_path.unlink(missing_ok=True)
        self.start_daemon("--reject-first-send")
        self.capture("text:RecoverAliceDraft enter enter")
        messages = self.persisted()["messages"]
        self.assertEqual(len(messages), 1, "Only explicit retry should persist a message")
        self.assertEqual(messages[0]["parts"], [{"type": "text", "value": "RecoverAliceDraft"}])

    def test_new_conversation_uses_recipient_and_creates_real_fixture_history(self):
        self.start_daemon()
        text = self.capture(
            "text:/new enter text:carol@example.invalid enter text:ExactCarolBody enter"
        )
        state = self.persisted()
        self.assertEqual(len(state["messages"]), 1, text)
        message = state["messages"][0]
        conversation = next(
            chat for chat in state["conversations"]
            if chat["id"] == message["conversation_id"]
        )
        peers = [p["handle"]["value"] for p in conversation["participants"] if not p["is_me"]]
        self.assertEqual(peers, ["carol@example.invalid"])
        self.assertEqual(message["parts"], [{"type": "text", "value": "ExactCarolBody"}])
        self.assertIn("carol@example.invalid", text.splitlines()[0], "New-chat completion must open the recipient's thread")
        self.assertTrue(any("ExactCarolBody" in line.partition("│")[2] for line in text.splitlines()),
                        "New-chat body must appear in the transcript, not just a sidebar preview")
        self.capture(
            "text:/new enter text:carol@example.invalid enter text:SecondCarolBody enter"
        )
        state = self.persisted()
        self.assertEqual(len(state["conversations"]), 3, "Same recipient must reuse its chat")
        self.assertEqual(len(state["messages"]), 2)
        self.assertEqual(state["messages"][0]["conversation_id"], state["messages"][1]["conversation_id"])
        self.assertNotEqual(state["messages"][0]["id"], state["messages"][1]["id"])
        self.assertEqual(state["messages"][1]["parts"], [{"type": "text", "value": "SecondCarolBody"}])

    def test_draft_is_restored_only_to_its_own_chat_after_restart(self):
        self.start_daemon()
        self.capture("text:AliceDurableDraft")
        drafts = [entry for entry in self.persisted().get("drafts", [])
                  if not entry["deleted"]]
        self.assertTrue(any(entry["draft"]["body"] == "AliceDurableDraft"
                            for entry in drafts),
                        "a debounced draft must remain in the daemon store")
        text = self.capture("")
        self.assertIn("AliceDurableDraft", text,
                      "Restart must restore the draft to the selected chat")
        switched = self.capture("ctrl-b n")
        self.assertNotIn("AliceDurableDraft", switched,
                         "Another chat must not inherit the draft")

    def test_send_does_not_resurrect_a_draft_after_restart(self):
        self.start_daemon()
        self.capture("text:SendOnceDraft enter ctrl-c")
        self.assertEqual(len(self.persisted()["messages"]), 1)
        self.capture("")
        self.capture("enter")
        self.assertEqual(len(self.persisted()["messages"]), 1,
                         "A sent draft must not return and send twice")

    def test_cancel_removes_the_stored_draft(self):
        self.start_daemon()
        self.capture("text:CancelMeDraft esc ctrl-c")
        self.capture("enter")
        self.assertEqual(self.persisted()["messages"], [])
        drafts = [entry for entry in self.persisted().get("drafts", [])
                  if not entry["deleted"]]
        self.assertFalse(any(entry["draft"]["body"] == "CancelMeDraft"
                             for entry in drafts),
                         "Explicit cancel must leave a delete intent")

    def test_failed_draft_save_keeps_text_and_shows_generic_error(self):
        self.start_daemon("--fail-draft-saves")
        text = self.capture("text:RetainAfterDraftError")
        self.assertIn("RetainAfterDraftError", text,
                      "A failed save must not remove the composer text")
        self.assertIn("Draft sync failed", text,
                      "A failed save must surface the generic draft error")

    def test_rejected_new_conversation_recovers_recipient_and_body(self):
        self.start_daemon("--reject-first-start")
        text = self.capture(
            "text:/new enter text:carol@example.invalid enter text:RecoverCarolDraft enter"
        )
        state = self.persisted()
        self.assertEqual(state["messages"], [])
        self.assertEqual(len(state["conversations"]), 2, "Rejected start must not create a chat")
        self.assertIn("not sent", text.lower(), "Start rejection must reach the TUI")
        self.assertIn("RecoverCarolDraft", text)
        self.assertNotIn("Message sent.", text)
        self.stop_daemon()
        self.store_path.unlink(missing_ok=True)
        self.start_daemon("--reject-first-start")
        self.capture(
            "text:/new enter text:carol@example.invalid enter text:RecoverCarolDraft enter enter"
        )
        state = self.persisted()
        self.assertEqual(len(state["messages"]), 1, "Explicit retry must send restored new-chat draft")
        self.assertEqual(state["messages"][0]["parts"], [{"type": "text", "value": "RecoverCarolDraft"}])
        conversation = next(
            chat for chat in state["conversations"]
            if chat["id"] == state["messages"][0]["conversation_id"]
        )
        self.assertIn("carol@example.invalid", [p["handle"]["value"] for p in conversation["participants"]])


if __name__ == "__main__":
    unittest.main(verbosity=2)
