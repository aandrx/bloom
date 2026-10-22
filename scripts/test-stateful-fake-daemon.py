#!/usr/bin/env python3
"""Black-box tests for stateful-fake-daemon.py over real Unix sockets."""
import json
import os
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest

DAEMON = os.path.join(os.path.dirname(__file__), "stateful-fake-daemon.py")
TIMEOUT = 2.0


def send(client, obj):
    payload = json.dumps(obj).encode()
    client.sendall(b"\x02" + struct.pack(">I", len(payload)) + payload)


def recv_exact(client, size):
    data = bytearray()
    while len(data) < size:
        chunk = client.recv(size - len(data))
        if not chunk:
            raise EOFError("socket closed before complete frame")
        data.extend(chunk)
    return bytes(data)


def recv(client):
    header = recv_exact(client, 5)
    version, size = struct.unpack(">BI", header)
    if version != 2 or not 0 < size <= 1048576:
        raise AssertionError("invalid response frame")
    return json.loads(recv_exact(client, size))


def request(path, op, **kwargs):
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(TIMEOUT)
        client.connect(path)
        send(client, {"request_id": 1, "request": {"op": op, **kwargs}})
        return recv(client)["response"]


class Tests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.sock = os.path.join(self.tmp.name, "ipc")
        self.store = os.path.join(self.tmp.name, "state.json")
        self.process = None
        self.addCleanup(self.tmp.cleanup)
        self.addCleanup(self.stop)
        self.start()

    def start(self, *extra):
        self.process = subprocess.Popen(
            [sys.executable, DAEMON, "--socket", self.sock, "--store", self.store, *extra],
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        )
        for _ in range(200):
            if self.process.poll() is not None:
                self.fail(self.process.stderr.read().decode())
            if os.path.exists(self.sock):
                try:
                    with socket.socket(socket.AF_UNIX) as probe:
                        probe.settimeout(TIMEOUT)
                        probe.connect(self.sock)
                    return
                except OSError:
                    pass
            time.sleep(0.01)
        self.fail("daemon did not start")

    def stop(self):
        if self.process and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=3)
        if self.process and self.process.stderr:
            self.process.stderr.close()

    def tearDown(self):
        self.stop()
        self.tmp.cleanup()

    def subscriber(self):
        client = socket.socket(socket.AF_UNIX)
        client.settimeout(TIMEOUT)
        client.connect(self.sock)
        send(client, {"request_id": 2, "request": {"op": "subscribe"}})
        self.assertEqual(recv(client)["response"]["result"], "subscribed")
        replay = recv(client)["event"]
        self.assertEqual(replay, {"event": "session", "state": "active"})
        return client

    def test_send_persists_exactly_and_emits_event(self):
        sub = self.subscriber()
        parts = [{"type": "text", "value": "precise body\nsecond line"}]
        response = request(self.sock, "send", conversation_id={"transport": "openbubbles", "value": "alice@example.invalid"}, body={"parts": parts})
        event = recv(sub)["event"]
        self.assertEqual(event["conversation_id"]["value"], "alice@example.invalid")
        self.assertEqual(event["message"]["parts"], parts)
        self.assertEqual(event["message"]["id"], response["message_id"])
        history = request(self.sock, "history", conversation_id={"transport": "openbubbles", "value": "alice@example.invalid"}, limit=50)
        self.assertEqual(history["messages"][0]["parts"], parts)
        self.assertEqual(request(self.sock, "list_conversations")["conversations"][0]["preview"], "precise body\nsecond line")
        sub.close()

    def test_reject_then_retry_and_unique_ids(self):
        self.stop()
        self.start("--reject-first-send")
        args = {"conversation_id": {"transport": "openbubbles", "value": "bob@example.invalid"}, "body": {"parts": [{"type": "text", "value": "retry me"}]}}
        self.assertEqual(request(self.sock, "send", **args)["result"], "error")
        self.assertEqual(request(self.sock, "history", conversation_id=args["conversation_id"], limit=50)["messages"], [])
        first = request(self.sock, "send", **args)["message_id"]["value"]
        second = request(self.sock, "send", **args)["message_id"]["value"]
        self.assertNotEqual(first, second)

    def test_start_reuses_validated_matching_direct_conversation(self):
        body = {"parts": [{"type": "text", "value": "hello"}]}
        sub = self.subscriber()
        response = request(self.sock, "start_conversation", address="mailto:carol@example.invalid", body=body)
        self.assertEqual(recv(sub)["event"]["event"], "conversations_changed")
        self.assertEqual(recv(sub)["event"]["event"], "message")
        request(self.sock, "start_conversation", address="carol@example.invalid", body=body)
        chats = request(self.sock, "list_conversations")["conversations"]
        self.assertEqual(sum(c["id"]["value"] == "carol@example.invalid" for c in chats), 1)
        self.assertEqual(request(self.sock, "start_conversation", address="bad@@example.invalid", body=body)["result"], "error")
        self.assertEqual(request(self.sock, "send", conversation_id={"transport": "other", "value": "carol@example.invalid"}, body=body)["result"], "error")
        sub.close()

    def test_rejected_first_start_is_non_mutating_and_retry_creates(self):
        self.stop()
        self.start("--reject-first-start")
        sub = self.subscriber()
        before = request(self.sock, "list_conversations")["conversations"]
        self.assertEqual(len(before), 2)
        body = {"parts": [{"type": "text", "value": "first to Carol"}]}
        rejected = request(self.sock, "start_conversation", address="carol@example.invalid", body=body)
        self.assertEqual(rejected, {"result": "error", "error": "transport_unavailable"})
        self.assertEqual(request(self.sock, "list_conversations")["conversations"], before)
        self.assertEqual(request(self.sock, "history", conversation_id={"transport": "openbubbles", "value": "carol@example.invalid"}, limit=20)["messages"], [])
        with self.assertRaises(socket.timeout):
            recv(sub)
        request(self.sock, "start_conversation", address="carol@example.invalid", body=body)
        self.assertEqual(recv(sub)["event"]["event"], "conversations_changed")
        self.assertEqual(recv(sub)["event"]["event"], "message")
        sub.close()

    def test_email_and_telephone_contact_forms(self):
        body = {"parts": [{"type": "text", "value": "contact message"}]}
        for raw, expected, kind in (("mailto:Carol@example.invalid", "carol@example.invalid", "email"), ("tel:+1 (555) 000-0003", "+15550000003", "tel")):
            response = request(self.sock, "start_conversation", address=raw, body=body)
            self.assertEqual(response["result"], "send")
            chats = request(self.sock, "list_conversations")["conversations"]
            chat = next(c for c in chats if c["id"]["value"] == expected)
            self.assertEqual(chat["participants"][1]["handle"]["kind"], kind)

    def test_delay_does_not_block_list_or_history(self):
        self.stop()
        self.start("--delay", "0.35")
        errors = []
        def delayed_send():
            try:
                request(self.sock, "send", conversation_id={"transport": "openbubbles", "value": "alice@example.invalid"}, body={"parts": [{"type": "text", "value": "delayed"}]})
            except Exception as error:
                errors.append(error)
        worker = threading.Thread(target=delayed_send)
        worker.start()
        time.sleep(0.06)
        started = time.monotonic()
        request(self.sock, "list_conversations")
        request(self.sock, "history", conversation_id={"transport": "openbubbles", "value": "alice@example.invalid"}, limit=5)
        self.assertLess(time.monotonic() - started, 0.25)
        worker.join(timeout=2)
        self.assertFalse(worker.is_alive())
        self.assertEqual(errors, [])

    def test_non_text_or_empty_bodies_are_rejected_without_mutation(self):
        before = request(self.sock, "list_conversations")["conversations"]
        for parts in ([], [{"type": "text", "value": ""}],
                      [{"type": "unsupported", "value": {"kind": "attachment"}}]):
            result = request(self.sock, "start_conversation", address="carol@example.invalid",
                             body={"parts": parts})
            self.assertEqual(result, {"result": "error", "error": "unsupported"})
        self.assertEqual(request(self.sock, "list_conversations")["conversations"], before)

    def test_restart_and_session_replay_on_reconnect(self):
        sub = self.subscriber()
        sub.close()
        args = {"conversation_id": {"transport": "openbubbles", "value": "alice@example.invalid"}, "body": {"parts": [{"type": "text", "value": "durable"}]}}
        request(self.sock, "send", **args)
        self.stop()
        self.start()
        self.assertEqual(len(request(self.sock, "history", conversation_id=args["conversation_id"], limit=50)["messages"]), 1)
        sub = self.subscriber()
        sub.close()

    def test_history_before_pages_contiguously_and_default_is_newest(self):
        conversation = {"transport": "openbubbles", "value": "alice@example.invalid"}
        for index in range(5):
            result = request(self.sock, "fixture_append_message",
                             conversation_id=conversation, text=f"page {index}")
            self.assertEqual(result["result"], "fixture_append_message")
        # Freeze the fixture timestamps and ids so ordering is deterministic even
        # when the sends above land in the same millisecond.
        self.stop()
        with open(self.store, encoding="utf-8") as stream:
            state = json.load(stream)
        for index, message in enumerate(state["messages"]):
            message["created_at"] = 1000 + index * 10
            message["id"]["value"] = f"page-{index}"
        with open(self.store, "w", encoding="utf-8") as stream:
            json.dump(state, stream)
        self.start()

        full = request(self.sock, "history", conversation_id=conversation, limit=50)["messages"]
        expected = [f"page {index}" for index in range(5)]
        self.assertEqual([m["parts"][0]["value"] for m in full], expected)

        newest = request(self.sock, "history", conversation_id=conversation, limit=2)["messages"]
        self.assertEqual(newest, full[-2:])

        pages = []
        cursor = None
        # Bound the loop so a cursor that repeats a page fails the coverage assertion
        # below instead of hanging the suite.
        for _ in range(len(expected) + 2):
            arguments = {"conversation_id": conversation, "limit": 2}
            if cursor is not None:
                arguments["before"] = cursor
            page = request(self.sock, "history", **arguments)["messages"]
            if not page:
                break
            pages.append(page)
            if len(page) < 2:
                break
            cursor = {"created_at": page[0]["created_at"], "id": page[0]["id"]}
        else:
            self.fail("paging never reached the oldest message")

        collected = [message["parts"][0]["value"] for page in reversed(pages) for message in page]
        self.assertEqual(collected, expected)
        self.assertEqual(len(collected), len(set(collected)))


if __name__ == "__main__":
    unittest.main(verbosity=2)
