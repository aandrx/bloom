#!/usr/bin/env python3
"""Black-box tests for stateful-fake-daemon.py over real Unix sockets."""
import json
import copy
import os
import runpy
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest

DAEMON = os.path.join(os.path.dirname(__file__), "stateful-fake-daemon.py")
FAKE = os.path.join(os.path.dirname(__file__), "fake-daemon.py")
TIMEOUT = 2.0


def identity(value, transport="openbubbles"):
    return {"transport": transport, "value": value}


def parse_frame(data):
    version, size = struct.unpack(">BI", data[:5])
    return version, json.loads(data[5:5 + size]), len(data) - 5


def oversized_response(limit):
    return {
        "request_id": 41,
        "response": {
            "result": "message_context",
            "messages": [{"parts": [{"type": "text", "value": "x" * (limit + 16)}]}],
        },
    }


def send(client, obj):
    payload = json.dumps(obj).encode()
    client.sendall(b"\x09" + struct.pack(">I", len(payload)) + payload)


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
    if version != 9 or not 0 < size <= 1048576:
        raise AssertionError("invalid response frame")
    return json.loads(recv_exact(client, size))


def request(path, op, **kwargs):
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(TIMEOUT)
        client.connect(path)
        send(client, {"request_id": 1, "request": {"op": op, **kwargs}})
        return recv(client)["response"]


def typed_parts():
    """Synthetic mixed parts: text, two attachments, unsupported, text."""
    return [
        {"type": "text", "value": "typed line one\n  indented\ttail  "},
        {"type": "attachment", "value": {
            "id": "synthetic-att-0001",
            "mime_type": "application/octet-stream",
            "file_name": "r\u00e9sum\u00e9 synth\u00e9tique.txt",
            "byte_len": 0,
        }},
        {"type": "unsupported", "value": {"kind": "attachment"}},
        {"type": "attachment", "value": {
            "id": "synthetic-name-only.bin",
            "mime_type": "application/x-synthetic",
            "file_name": None,
            "byte_len": 0,
        }},
        {"type": "text", "value": "typed line two\n"},
    ]


def typed_message(builder, conversation_id, ident="typed-0001", at=1_650_000_000_000):
    """Build the canonical incoming shape, then replace its parts explicitly."""
    item = builder("placeholder@example.invalid", ident, at, "unused", False)
    item["conversation_id"] = copy.deepcopy(conversation_id)
    item["parts"] = typed_parts()
    return item


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


    def test_message_context_is_centred_bounded_and_validated(self):
        conversation = {"transport": "openbubbles", "value": "alice@example.invalid"}
        for index in range(7):
            request(self.sock, "fixture_append_message",
                    conversation_id=conversation, text=f"context {index}")
        self.stop()
        with open(self.store, encoding="utf-8") as stream:
            state = json.load(stream)
        for index, message in enumerate(state["messages"]):
            message["created_at"] = 1000 + index * 10
            message["id"]["value"] = f"ctx-{index}"
        with open(self.store, "w", encoding="utf-8") as stream:
            json.dump(state, stream)
        self.start()

        def context(message_id, limit, cid=conversation):
            return request(self.sock, "message_context", conversation_id=cid,
                           message_id={"transport": "openbubbles", "value": message_id},
                           limit=limit)

        def ids(response):
            return [message["id"]["value"] for message in response["messages"]]

        self.assertEqual(ids(context("ctx-3", 3)), ["ctx-2", "ctx-3", "ctx-4"])
        self.assertEqual(ids(context("ctx-3", 4)), ["ctx-2", "ctx-3", "ctx-4", "ctx-5"])
        self.assertEqual(ids(context("ctx-3", 1)), ["ctx-3"])
        self.assertEqual(ids(context("ctx-0", 5)), ["ctx-0", "ctx-1", "ctx-2"])
        self.assertEqual(ids(context("ctx-6", 5)), ["ctx-4", "ctx-5", "ctx-6"])
        for invalid in (0, 201):
            self.assertEqual(context("ctx-3", invalid),
                             {"result": "error", "error": "invalid_request"})
        self.assertEqual(context("ctx-3", 5, cid={"transport": "openbubbles", "value": "bob@example.invalid"}),
                         {"result": "error", "error": "not_found"})
        self.assertEqual(context("missing", 5),
                         {"result": "error", "error": "not_found"})

    def test_message_context_reaches_old_target_across_aliases(self):
        self.stop()
        with open(self.store, encoding="utf-8") as stream:
            state = json.load(stream)
        canonical = state["conversations"][0]
        alias = {"transport": "openbubbles", "value": "alice-legacy"}
        canonical["aliases"] = [alias]
        for index in range(2000):
            conversation = alias if index == 7 else canonical["id"]
            state["messages"].append({
                "id": {"transport": "openbubbles", "value": f"row-{index:04d}"},
                "conversation_id": conversation, "remote_id": None,
                "sender": canonical["participants"][1]["handle"],
                "parts": [{"type": "text", "value": f"row {index}"}],
                "created_at": 1000 + index, "sent_at": None, "delivery": "delivered",
                "from_me": False, "edited_at": None, "reactions": [], "unsent_at": None,
            })
        with open(self.store, "w", encoding="utf-8") as stream:
            json.dump(state, stream)
        self.start()

        expected = ["row-0005", "row-0006", "row-0007", "row-0008", "row-0009"]
        for requested in (canonical["id"], alias):
            response = request(self.sock, "message_context", conversation_id=requested,
                               message_id={"transport": "openbubbles", "value": "row-0007"},
                               limit=5)
            self.assertEqual([message["id"]["value"] for message in response["messages"]], expected)
            self.assertTrue(all(message["conversation_id"] == canonical["id"]
                                for message in response["messages"]))
        self.assertEqual(request(self.sock, "message_context", conversation_id=canonical["id"],
                                 message_id={"transport": "openbubbles", "value": "row-0007"},
                                 limit=1)["messages"][0]["id"]["value"], "row-0007")

    def test_stateful_typed_parts_survive_history_and_context_restart(self):
        self.stop()
        canonical = identity("alice@example.invalid")
        alias = identity("alice-typed-legacy")
        with open(self.store, encoding="utf-8") as stream:
            state = json.load(stream)
        conversation = next(c for c in state["conversations"] if c["id"] == canonical)
        conversation["aliases"] = [alias]
        expected = typed_message(runpy.run_path(FAKE)["message"], canonical)
        state["messages"].append(copy.deepcopy(expected))
        with open(self.store, "w", encoding="utf-8") as stream:
            json.dump(state, stream, ensure_ascii=False)
        self.start()

        for restart in (False, True):
            if restart:
                self.stop()
                self.start()
            history = request(self.sock, "history", conversation_id=canonical, limit=5)
            self.assertEqual([m["id"] for m in history["messages"]], [expected["id"]])
            self.assertEqual(history["messages"][0]["conversation_id"], canonical)
            self.assertEqual(history["messages"][0]["parts"], expected["parts"])
            for requested in (canonical, alias):
                response = request(self.sock, "message_context", conversation_id=requested,
                                   message_id=expected["id"], limit=5)
                self.assertEqual([m["id"] for m in response["messages"]], [expected["id"]])
                self.assertEqual(response["messages"][0]["conversation_id"], canonical)
                self.assertEqual(response["messages"][0]["parts"], expected["parts"])
            if not restart:
                # Force the daemon's real save path, not just a second load of
                # pre-seeded JSON. A supported fake text send in another chat
                # leaves Alice's history unchanged while persisting all parts.
                saved = request(self.sock, "send", conversation_id=identity("bob@example.invalid"),
                                body={"parts": [{"type": "text", "value": "synthetic save checkpoint"}]})
                self.assertEqual(saved["result"], "send")

    def test_seeded_reactions_survive_history_and_context_unchanged(self):
        self.stop()
        canonical = identity("alice@example.invalid")
        with open(self.store, encoding="utf-8") as stream:
            state = json.load(stream)
        builder = runpy.run_path(FAKE)["message"]
        expected = builder("placeholder@example.invalid", "reaction-0001",
                            1_650_000_000_000, "reaction fixture body", False)
        expected["conversation_id"] = copy.deepcopy(canonical)
        expected["reactions"] = [
            {"sender": {"kind": "email", "value": "alice@example.invalid"},
             "kind": "heart", "remote_id": "reaction-remote-1", "part": 2,
             "timestamp": 1_650_000_000_100},
            {"sender": {"kind": "email", "value": "bob@example.invalid"},
             "kind": "like", "remote_id": None, "part": None, "timestamp": None},
            {"sender": {"kind": "email", "value": "alice@example.invalid"},
             "kind": "heart", "remote_id": "reaction-remote-3", "part": None,
             "timestamp": 1_650_000_000_300},
        ]
        state["messages"].append(copy.deepcopy(expected))
        with open(self.store, "w", encoding="utf-8") as stream:
            json.dump(state, stream, ensure_ascii=False)
        self.start()

        def observed():
            history = request(self.sock, "history", conversation_id=canonical, limit=5)
            self.assertEqual([m["id"] for m in history["messages"]], [expected["id"]])
            self.assertEqual(history["messages"][0]["reactions"], expected["reactions"])
            context = request(self.sock, "message_context", conversation_id=canonical,
                              message_id=expected["id"], limit=5)
            self.assertEqual([m["id"] for m in context["messages"]], [expected["id"]])
            self.assertEqual(context["messages"][0]["reactions"], expected["reactions"])

        observed()
        # Force the daemon's own save path, then prove a fresh load keeps every
        # reaction field, including the part-bearing and partless entries.
        saved = request(self.sock, "send", conversation_id=identity("bob@example.invalid"),
                        body={"parts": [{"type": "text", "value": "reaction save checkpoint"}]})
        self.assertEqual(saved["result"], "send")
        self.stop()
        self.start()
        observed()

    def test_seeded_edit_and_unsend_state_survives_history_context_and_restart(self):
        self.stop()
        canonical = identity("alice@example.invalid")
        with open(self.store, encoding="utf-8") as stream:
            state = json.load(stream)
        builder = runpy.run_path(FAKE)["message"]
        edited = builder("placeholder@example.invalid", "edited-0001",
                         1_650_000_000_000, "revised fixture body", False)
        edited["conversation_id"] = copy.deepcopy(canonical)
        edited["edited_at"] = 1_650_000_000_500
        unsent = builder("placeholder@example.invalid", "unsent-0001",
                         1_650_000_000_100, "retained fixture body", False)
        unsent["conversation_id"] = copy.deepcopy(canonical)
        unsent["unsent_at"] = 1_650_000_000_600
        state["messages"].extend([copy.deepcopy(edited), copy.deepcopy(unsent)])
        with open(self.store, "w", encoding="utf-8") as stream:
            json.dump(state, stream, ensure_ascii=False)
        self.start()

        def observed():
            history = request(self.sock, "history", conversation_id=canonical, limit=5)
            rows = {row["id"]["value"]: row for row in history["messages"]}
            self.assertEqual(rows["edited-0001"]["edited_at"], edited["edited_at"])
            self.assertIsNone(rows["edited-0001"]["unsent_at"])
            self.assertEqual(rows["unsent-0001"]["unsent_at"], unsent["unsent_at"])
            self.assertEqual(rows["unsent-0001"]["parts"], unsent["parts"])
            context = request(self.sock, "message_context", conversation_id=canonical,
                              message_id=edited["id"], limit=5)
            target = next(row for row in context["messages"]
                          if row["id"]["value"] == "edited-0001")
            self.assertEqual(target["edited_at"], edited["edited_at"])
            self.assertEqual(target["parts"], edited["parts"])

        observed()
        saved = request(self.sock, "send", conversation_id=identity("bob@example.invalid"),
                        body={"parts": [{"type": "text", "value": "mutation save checkpoint"}]})
        self.assertEqual(saved["result"], "send")
        self.stop()
        self.start()
        observed()

    def test_search_messages_returns_snippet_and_ignores_empty_query(self):
        conversation = {"transport": "openbubbles", "value": "alice@example.invalid"}
        prefix = "0123456789" * 4
        suffix = "abcdefghij" * 4
        text = prefix + "NEEDLE" + suffix
        request(self.sock, "fixture_append_message", conversation_id=conversation, text=text)

        response = request(self.sock, "search_messages", query="  needle  ", limit=10)
        self.assertEqual(response["result"], "search_results")
        self.assertEqual(len(response["hits"]), 1)
        hit = response["hits"][0]
        self.assertEqual(hit["conversation_id"], conversation)
        with open(self.store, encoding="utf-8") as stream:
            stored = json.load(stream)["messages"][0]
        self.assertEqual(hit["created_at"], stored["created_at"])
        self.assertEqual(hit["message_id"], stored["id"])
        self.assertEqual(hit["snippet"], "\u2026" + prefix[-30:] + "NEEDLE" + suffix[:30] + "\u2026")

        self.assertEqual(request(self.sock, "search_messages", query="", limit=10)["hits"], [])
        self.assertEqual(request(self.sock, "search_messages", query="   ", limit=10)["hits"], [])
        # The daemon rejects an invalid limit rather than answering with an empty set.
        self.assertEqual(
            request(self.sock, "search_messages", query="needle", limit=0),
            {"result": "error", "error": "invalid_request"},
        )

    def test_search_messages_matches_typed_filename_parts_over_rpc(self):
        self.stop()
        conversation = identity("alice@example.invalid")
        with open(self.store, encoding="utf-8") as stream:
            state = json.load(stream)
        state["messages"].append(synthetic_message(
            conversation, "typed-filename", 1000,
            [synthetic_text("quiet body"), synthetic_attachment("r\u00e9sum\u00e9-needle.pdf")]))
        with open(self.store, "w", encoding="utf-8") as stream:
            json.dump(state, stream, ensure_ascii=False)
        self.start()
        response = request(self.sock, "search_messages", query="NEEDLE", limit=5)
        self.assertEqual(response["result"], "search_results")
        self.assertEqual([hit["message_id"]["value"] for hit in response["hits"]],
                         ["typed-filename"])
        self.assertEqual(response["hits"][0]["snippet"],
                         "Filename: r\u00e9sum\u00e9-needle.pdf")


    def test_drafts_round_trip_and_survive_restart(self):
        conversation = {"kind": "conversation",
                        "owner": identity("draft-conversation")}
        chat = {"kind": "new_chat", "transport": "openbubbles",
                "address": "Exact@Example.invalid"}
        other = {"kind": "new_chat", "transport": "openbubbles",
                 "address": "exact@example.invalid"}
        body = "a\U0001f469\u200d\U0001f469\u200d\U0001f467\u200d\U0001f466e\u0301"
        draft = {"body": body, "cursor": 3, "pasted_body": True, "uncertain": True}
        response = request(self.sock, "draft_save", key=conversation, draft=draft, revision=1)
        self.assertEqual(response, {"result": "draft_save", "outcome": "applied"})
        self.assertEqual(
            request(self.sock, "draft_save", key=chat, draft=draft, revision=1),
            {"result": "draft_save", "outcome": "applied"},
        )
        self.assertEqual(
            request(self.sock, "draft_save", key=other,
                    draft={"body": "other", "cursor": 0, "pasted_body": False,
                           "uncertain": False}, revision=1),
            {"result": "draft_save", "outcome": "applied"},
        )
        self.assertEqual(
            request(self.sock, "draft_save", key=conversation, draft=draft, revision=1),
            {"result": "draft_save", "outcome": "stale"},
        )
        invalid = request(self.sock, "draft_save", key=conversation,
                          draft={**draft, "cursor": 4}, revision=2)
        self.assertEqual(invalid, {"result": "error", "error": "invalid_request"})
        leading = {"body": "\u0301", "cursor": 1,
                   "pasted_body": False, "uncertain": False}
        self.assertEqual(
            request(self.sock, "draft_save",
                    key={"kind": "new_chat", "transport": "openbubbles",
                         "address": "leading@example.invalid"},
                    draft=leading, revision=1),
            {"result": "draft_save", "outcome": "applied"},
        )
        too_large = request(self.sock, "draft_save", key=conversation,
                            draft={**draft, "body": "x" * 65537, "cursor": 0}, revision=2)
        self.assertEqual(too_large, {"result": "error", "error": "invalid_request"})
        blank = request(self.sock, "draft_save",
                        key={"kind": "new_chat", "transport": "openbubbles", "address": "   "},
                        draft=draft, revision=1)
        self.assertEqual(blank, {"result": "error", "error": "invalid_request"})
        self.assertEqual(
            request(self.sock, "draft_delete", key=conversation, revision=2),
            {"result": "draft_delete", "outcome": "applied"},
        )
        self.assertEqual(
            request(self.sock, "draft_save", key=conversation, draft=draft, revision=2),
            {"result": "draft_save", "outcome": "stale"},
        )

        self.stop()
        self.start()
        loaded = request(self.sock, "draft_load", key=conversation)
        self.assertEqual(loaded["draft"], None)
        self.assertEqual(loaded["revision_floor"], 2)
        self.assertFalse(loaded["corrupt_deleted"])
        loaded_chat = request(self.sock, "draft_load", key=chat)
        self.assertEqual(loaded_chat["draft"], draft)
        self.assertEqual(loaded_chat["revision_floor"], 1)
        self.assertEqual(request(self.sock, "draft_load", key=other)["draft"]["body"], "other")
        self.assertEqual(
            request(self.sock, "draft_load",
                    key={"kind": "new_chat", "transport": "openbubbles",
                         "address": "exact@example.invalid"})["draft"]["body"],
            "other",
        )


class FakeDraftRPC(unittest.TestCase):
    def setUp(self):
        self.module = runpy.run_path(FAKE)
        self.module["DRAFTS"].clear()
        self.client, self.server = socket.socketpair()
        self.client.settimeout(TIMEOUT)
        self.thread = threading.Thread(
            target=self.module["serve_client"], args=(self.server,), daemon=True
        )
        self.thread.start()

    def tearDown(self):
        self.client.close()
        self.thread.join(timeout=2)
        if self.thread.is_alive():
            self.server.close()
            self.thread.join(timeout=2)
        self.assertFalse(self.thread.is_alive())
        self.server.close()

    def rpc(self, op, **kwargs):
        send(self.client, {"request_id": 1, "request": {"op": op, **kwargs}})
        return recv(self.client)["response"]

    def test_save_load_delete_and_validation(self):
        conversation = {"kind": "conversation", "owner": identity("draft-conversation")}
        chat = {"kind": "new_chat", "transport": "openbubbles",
                "address": "Exact@Example.invalid"}
        body = "a\U0001f469\u200d\U0001f469\u200d\U0001f467\u200d\U0001f466e\u0301"
        draft = {"body": body, "cursor": 3, "pasted_body": True, "uncertain": False}
        self.assertEqual(
            self.rpc("draft_save", key=conversation, draft=draft, revision=1),
            {"result": "draft_save", "outcome": "applied"},
        )
        self.assertEqual(
            self.rpc("draft_save", key=chat, draft=draft, revision=1),
            {"result": "draft_save", "outcome": "applied"},
        )
        self.assertEqual(
            self.rpc("draft_save", key=conversation, draft=draft, revision=1),
            {"result": "draft_save", "outcome": "stale"},
        )
        self.assertEqual(
            self.rpc("draft_save", key=conversation,
                     draft={**draft, "cursor": 4}, revision=2),
            {"result": "error", "error": "invalid_request"},
        )
        self.assertEqual(
            self.rpc(
                "draft_save",
                key={"kind": "new_chat", "transport": "openbubbles",
                     "address": "leading@example.invalid"},
                draft={"body": "\u0301", "cursor": 1,
                       "pasted_body": False, "uncertain": False},
                revision=1,
            ),
            {"result": "draft_save", "outcome": "applied"},
        )
        self.assertEqual(
            self.rpc("draft_save", key=conversation,
                     draft={**draft, "body": "x" * 65537, "cursor": 0}, revision=2),
            {"result": "error", "error": "invalid_request"},
        )
        self.assertEqual(
            self.rpc("draft_save",
                     key={"kind": "new_chat", "transport": "openbubbles",
                          "address": "  "}, draft=draft, revision=1),
            {"result": "error", "error": "invalid_request"},
        )
        loaded = self.rpc("draft_load", key=conversation)
        self.assertEqual(loaded, {"result": "draft_load", "draft": draft,
                                  "revision_floor": 1, "corrupt_deleted": False})
        self.assertEqual(
            self.rpc("draft_delete", key=conversation, revision=2),
            {"result": "draft_delete", "outcome": "applied"},
        )
        self.assertEqual(
            self.rpc("draft_load", key=conversation),
            {"result": "draft_load", "draft": None, "revision_floor": 2,
             "corrupt_deleted": False},
        )
        self.assertEqual(
            self.rpc("draft_save", key=conversation, draft=draft, revision=2),
            {"result": "draft_save", "outcome": "stale"},
        )
        self.assertEqual(self.rpc("draft_load", key=chat)["draft"], draft)


def context_state():
    """Same-timestamp, escaped-id rows split across a canonical id and an alias."""
    canonical = identity("weekend")
    alias = identity("weekend-legacy")
    rows = []
    for value, conversation in [
        ("a1", canonical),
        ('a2"', alias),
        ("a3}", canonical),
        ("a4\\", alias),
    ]:
        rows.append({
            "id": identity(value),
            "conversation_id": conversation,
            "created_at": 1000,
        })
    return {
        "conversations": [{"id": canonical, "aliases": [alias]}],
        "messages": rows,
    }


class StatefulContextHelper(unittest.TestCase):
    def setUp(self):
        self.module = runpy.run_path(DAEMON)
        self.state = context_state()
        self.canonical = identity("weekend")
        self.alias = identity("weekend-legacy")

    def context(self, conversation_id, value, limit):
        return self.module["message_context"](
            self.state, conversation_id, identity(value), limit
        )

    def test_alias_target_and_neighbours_canonicalize_for_both_requests(self):
        for requested in (self.canonical, self.alias):
            selected = self.context(requested, 'a2"', 3)
            self.assertEqual([item["id"]["value"] for item in selected],
                             ["a1", 'a2"', "a3}"])
            self.assertTrue(all(item["conversation_id"] == self.canonical
                                for item in selected))
            self.assertEqual(
                [item["id"]["value"] for item in self.context(requested, 'a2"', 2)],
                ['a2"', "a3}"],
            )

    def test_missing_and_wrong_chat_are_not_found(self):
        self.assertIsNone(self.context(self.canonical, "missing", 3))
        self.assertIsNone(self.context(identity("bob"), 'a2"', 3))


class FakeContextHelper(unittest.TestCase):
    def setUp(self):
        self.module = runpy.run_path(FAKE)
        state = context_state()
        self.module["CONVERSATIONS"][:] = state["conversations"]
        self.module["HISTORY"].clear()
        self.module["HISTORY"].update({"weekend": state["messages"]})
        self.canonical = identity("weekend")
        self.alias = identity("weekend-legacy")

    def context(self, conversation_id, value, limit):
        return self.module["message_context"](
            conversation_id, identity(value), limit
        )

    def test_alias_target_and_neighbours_canonicalize_for_both_requests(self):
        for requested in (self.canonical, self.alias):
            selected = self.context(requested, 'a2"', 3)
            self.assertEqual([item["id"]["value"] for item in selected],
                             ["a1", 'a2"', "a3}"])
            self.assertTrue(all(item["conversation_id"] == self.canonical
                                for item in selected))
            self.assertEqual(
                [item["id"]["value"] for item in self.context(requested, 'a2"', 2)],
                ['a2"', "a3}"],
            )

    def test_missing_and_wrong_chat_are_not_found(self):
        self.assertIsNone(self.context(self.canonical, "missing", 3))
        self.assertIsNone(self.context(identity("bob"), 'a2"', 3))


def participant_scenario():
    """Shared membership fixture: direct, group, transport, kind, alias, no metadata."""
    direct = identity("direct@example.invalid")
    group = identity("group@example.invalid")
    parallel = identity("parallel@example.invalid", transport="other")
    phone = identity("phone@example.invalid")
    archive = identity("archive@example.invalid")
    archive_old = identity("archive-legacy")
    ghost = identity("ghost@example.invalid")
    me = {"kind": "email", "value": "me@example.invalid"}

    def person(kind, value):
        return {"handle": {"kind": kind, "value": value}, "display_name": None, "is_me": False}

    conversations = [
        {"id": direct, "kind": "direct", "aliases": [],
         "participants": [{"handle": me, "is_me": True}, person("email", "alice@example.invalid")]},
        {"id": group, "kind": "group", "aliases": [],
         "participants": [{"handle": me, "is_me": True}, person("email", "bob@example.invalid"),
                           person("email", "carol@example.invalid")]},
        {"id": parallel, "kind": "direct", "aliases": [],
         "participants": [{"handle": me, "is_me": True}, person("email", "alice@example.invalid")]},
        {"id": phone, "kind": "direct", "aliases": [],
         "participants": [{"handle": me, "is_me": True}, person("tel", "alice@example.invalid")]},
        {"id": archive, "kind": "direct", "aliases": [archive_old],
         "participants": [{"handle": me, "is_me": True}, person("email", "erin@example.invalid")]},
    ]
    rows = []

    def row(value, conversation, text, at, sender):
        rows.append({"id": identity(value), "conversation_id": conversation, "created_at": at,
                     "sender": {"kind": "email", "value": sender},
                     "parts": [{"type": "text", "value": text}]})

    row("direct-1", direct, "Needle direct", 100, "bob@example.invalid")
    row("group-1", group, "Needle group", 110, "carol@example.invalid")
    row("parallel-1", parallel, "Needle parallel", 120, "alice@example.invalid")
    row("phone-1", phone, "Needle phone", 130, "alice@example.invalid")
    row("archive-1", archive_old, "Needle archived", 140, "retired@example.invalid")
    row("ghost-1", ghost, "Needle ghost", 150, "ghost@example.invalid")
    row("haystack-1", direct, "No match here", 160, "alice@example.invalid")
    for index in range(60):
        row(f"newer-{index:02d}", ghost, f"Needle newer {index}", 1000 + index, "ghost@example.invalid")
    return conversations, rows, archive


def scope(transport, kind, value):
    return {"transport": transport, "handle": {"kind": kind, "value": value}}


class ParticipantSearchMixin:
    def search(self, query, limit, participant=None):
        raise NotImplementedError

    def ids(self, hits):
        return [hit["message_id"]["value"] for hit in hits]

    def test_direct_and_group_membership_ignores_sender(self):
        self.assertEqual(
            self.ids(self.search("needle", 10, scope("openbubbles", "email", "alice@example.invalid"))),
            ["direct-1"],
        )
        self.assertEqual(
            self.ids(self.search("needle", 10, scope("openbubbles", "email", "bob@example.invalid"))),
            ["group-1"],
        )

    def test_alias_membership_and_retired_only_participant(self):
        hits = self.search("needle", 1, scope("openbubbles", "email", "erin@example.invalid"))
        self.assertEqual(self.ids(hits), ["archive-1"])
        self.assertEqual(hits[0]["conversation_id"], self.archive)
        self.assertEqual(self.search("needle", 10, scope("openbubbles", "email", "retired@example.invalid")), [])

    def test_transport_and_handle_kind_are_exact(self):
        self.assertEqual(
            self.ids(self.search("needle", 10, scope("openbubbles", "tel", "alice@example.invalid"))),
            ["phone-1"],
        )
        self.assertEqual(
            self.ids(self.search("needle", 10, scope("other", "email", "alice@example.invalid"))),
            ["parallel-1"],
        )
        self.assertEqual(self.search("needle", 10, scope("openbubbles", "opaque", "alice@example.invalid")), [])

    def test_missing_membership_and_missing_body_match_are_empty(self):
        self.assertEqual(self.search("needle", 10, scope("openbubbles", "email", "ghost@example.invalid")), [])
        self.assertEqual(self.search("absent", 10, scope("openbubbles", "email", "alice@example.invalid")), [])

    def test_unfiltered_results_are_unchanged(self):
        hits = self.search("needle", 500)
        self.assertEqual(len(hits), 66)
        self.assertEqual(self.ids(hits)[0], "newer-59")
        self.assertEqual(self.ids(hits)[-1], "direct-1")
        self.assertEqual(self.search("needle", 500, None), hits)
        archived = next(hit for hit in hits if hit["message_id"]["value"] == "archive-1")
        self.assertEqual(archived["conversation_id"], self.archive,
                         "unfiltered results must retain canonical alias identity too")


class StatefulParticipantSearch(ParticipantSearchMixin, unittest.TestCase):
    def setUp(self):
        self.module = runpy.run_path(DAEMON)
        conversations, rows, self.archive = participant_scenario()
        self.state = {"conversations": copy.deepcopy(conversations), "messages": copy.deepcopy(rows)}

    def search(self, query, limit, participant=None):
        return self.module["search_messages"](self.state, query, limit, participant)


class FakeParticipantSearch(ParticipantSearchMixin, unittest.TestCase):
    def setUp(self):
        self.module = runpy.run_path(FAKE)
        conversations, rows, self.archive = participant_scenario()
        self.module["CONVERSATIONS"][:] = copy.deepcopy(conversations)
        self.module["HISTORY"].clear()
        for row in rows:
            self.module["HISTORY"].setdefault(row["conversation_id"]["value"], []).append(copy.deepcopy(row))

    def search(self, query, limit, participant=None):
        return self.module["search_messages"](query, limit, participant)


# Expected windows from the Rust `str::escape_debug` oracle; embedded so the tests
# do not depend on /tmp artifacts or a Rust build.
RUST_ESCAPE_VECTORS = [
    ("accents", "r\u00e9sum\u00e9-needle.pdf", "r\u00e9sum\u00e9-needle.pdf",
     "r\u00e9sum\u00e9-needle.pdf"),
    ("controls", "x\0\t\x1b\x85needle.pdf", "x\0\t\x1b\x85needle.pdf",
     "x\\0\\t\\u{1b}\\u{85}needle.pdf"),
    ("bidi", "\u061c\u200e\u200f\u202e\u2066needle.pdf",
     "\u061c\u200e\u200f\u202e\u2066needle.pdf",
     "\\u{61c}\\u{200e}\\u{200f}\\u{202e}\\u{2066}needle.pdf"),
    ("quotes", 'say "needle"\\file.txt', 'say "needle"\\file.txt',
     'say \\"needle\\"\\\\file.txt'),
    ("apostrophe", "director's-needle.pdf", "director's-needle.pdf", r"director\'s-needle.pdf"),
    ("leading-mark", "\u0301needle.pdf", "\u0301needle.pdf", "\\u{301}needle.pdf"),
    ("following-mark", "a\u0301needle.pdf", "a\u0301needle.pdf", "a\u0301needle.pdf"),
    ("spacing-mark", "\u093eneedle.pdf", "\u093eneedle.pdf", "\u093eneedle.pdf"),
    ("joiner", "\U0001f468\u200d\U0001f469needle.pdf",
     "\U0001f468\u200d\U0001f469needle.pdf",
     "\U0001f468\\u{200d}\U0001f469needle.pdf"),
    ("nbsp", "needle\xa0.pdf", "needle\xa0.pdf", "needle\\u{a0}.pdf"),
    ("crlf", "first\r\nneedle.pdf", "first needle.pdf", "first needle.pdf"),
]


# Additional leading-Mc outputs captured from the pinned Rust escape oracle.
RUST_LEADING_MC_ESCAPES = (
    0x09BE, 0x09D7, 0x0B3E, 0x0B57, 0x0BBE, 0x0BD7, 0x0CC0, 0x0CC2,
    0x0CC7, 0x0CC8, 0x0CCA, 0x0CCB, 0x0CD5, 0x0CD6, 0x0D3E, 0x0D57,
    0x0DCF, 0x0DDF, 0x1715, 0x1734, 0x1B35, 0x1B3B, 0x1B3D, 0x1B43,
    0x1B44, 0x1BAA, 0x1BF2, 0x1BF3, 0xA953, 0xA9C0, 0x111C0, 0x11235,
    0x1133E, 0x1134D, 0x11357, 0x113B8, 0x113C2, 0x113C5, 0x113C7,
    0x113C8, 0x113C9, 0x113CF, 0x114B0, 0x114BD, 0x115AF, 0x116B6,
    0x11930, 0x1193D, 0x11F41, 0x16FF0, 0x16FF1, 0x1D166, 0x1D16D,
)
RUST_ESCAPE_VECTORS += [
    (f"leading-mc-{cp:x}", chr(cp) + "needle.pdf", chr(cp) + "needle.pdf",
     f"\\u{{{cp:x}}}needle.pdf")
    for cp in RUST_LEADING_MC_ESCAPES
]


def synthetic_text(value):
    return {"type": "text", "value": value}


def synthetic_attachment(name, ident="synthetic-att", mime="application/octet-stream"):
    return {"type": "attachment", "value": {
        "id": ident, "mime_type": mime, "file_name": name, "byte_len": 0}}


def synthetic_message(conversation_id, ident, at, parts):
    return {
        "id": identity(ident, transport=conversation_id["transport"]),
        "conversation_id": copy.deepcopy(conversation_id),
        "remote_id": None,
        "sender": {"kind": "email", "value": "synthetic@example.invalid"},
        "parts": copy.deepcopy(parts),
        "created_at": at,
        "sent_at": None,
        "delivery": "delivered",
        "from_me": False,
        "edited_at": None,
        "reactions": [],
        "unsent_at": None,
    }


def filename_scope_scenario():
    """Filename hits across direct, group, alias, and exact transport scopes."""
    direct = identity("direct@example.invalid")
    group = identity("group@example.invalid")
    parallel = identity("parallel@example.invalid", transport="other")
    phone = identity("phone@example.invalid")
    archive = identity("archive@example.invalid")
    archive_old = identity("archive-legacy")
    ghost = identity("ghost@example.invalid")
    me = {"kind": "email", "value": "me@example.invalid"}

    def person(kind, value):
        return {"handle": {"kind": kind, "value": value}, "display_name": None, "is_me": False}

    conversations = [
        {"id": direct, "kind": "direct", "aliases": [],
         "participants": [{"handle": me, "is_me": True}, person("email", "alice@example.invalid")]},
        {"id": group, "kind": "group", "aliases": [],
         "participants": [{"handle": me, "is_me": True}, person("email", "bob@example.invalid"),
                           person("email", "carol@example.invalid")]},
        {"id": parallel, "kind": "direct", "aliases": [],
         "participants": [{"handle": me, "is_me": True}, person("email", "alice@example.invalid")]},
        {"id": phone, "kind": "direct", "aliases": [],
         "participants": [{"handle": me, "is_me": True}, person("tel", "alice@example.invalid")]},
        {"id": archive, "kind": "direct", "aliases": [archive_old],
         "participants": [{"handle": me, "is_me": True}, person("email", "erin@example.invalid")]},
    ]
    conversations.append({
        "id": ghost, "kind": "direct", "aliases": [],
        "participants": [{"handle": me, "is_me": True}, person("email", "bob@example.invalid")],
    })
    rows = [
        synthetic_message(direct, "direct-name", 90,
                          [synthetic_text("quiet"), synthetic_attachment("report-needle.pdf")]),
        synthetic_message(direct, "direct-body", 100, [synthetic_text("Needle direct")]),
        synthetic_message(group, "group-name", 110, [synthetic_attachment("group-needle.pdf")]),
        synthetic_message(parallel, "parallel-name", 120, [synthetic_attachment("parallel-needle.pdf")]),
        synthetic_message(phone, "phone-name", 130, [synthetic_attachment("phone-needle.pdf")]),
        synthetic_message(archive_old, "archive-name", 140, [synthetic_attachment("alias-needle.pdf")]),
    ]
    # These 60 newer rows are outside Alice's direct-chat membership.
    for index in range(60):
        rows.append(synthetic_message(
            ghost, f"ghost-{index:02d}", 1000 + index,
            [synthetic_text("report-needle newer out of scope")]))
    return conversations, rows, archive


class FixtureFilenameParityMixin:
    def add(self, ident, at, parts):
        message = synthetic_message(self.conversation, ident, at, parts)
        self.add_message(message)
        return message

    def ids(self, hits):
        return [hit["message_id"]["value"] for hit in hits]

    def test_filename_only_prefix_and_body_first_priority(self):
        self.add("filename-only", 100, [synthetic_text("quiet body"),
                                        synthetic_attachment("r\u00e9sum\u00e9-needle.pdf")])
        hits = self.search("NEEDLE", 10)
        self.assertEqual(self.ids(hits), ["filename-only"])
        self.assertEqual(hits[0]["snippet"], "Filename: r\u00e9sum\u00e9-needle.pdf")

        self.add("body-wins", 90, [synthetic_attachment("earlier-needle.pdf"),
                                   synthetic_text("needle body")])
        hits = self.search("needle", 10)
        self.assertEqual(self.ids(hits), ["filename-only", "body-wins"])
        self.assertEqual(hits[1]["snippet"], "needle body")

    def test_first_matching_filename_yields_one_hit(self):
        self.add("multi", 100, [synthetic_attachment("first-needle.pdf", "att-1"),
                                synthetic_attachment("second-needle.pdf", "att-2")])
        hits = self.search("needle", 10)
        self.assertEqual(self.ids(hits), ["multi"])
        self.assertEqual(hits[0]["snippet"], "Filename: first-needle.pdf")

    def test_punctuation_query_is_literal(self):
        body = "from: bob? [needle]\\d+"
        self.add("literal", 100, [synthetic_text(body)])
        self.assertEqual(self.search("from: bob?", 10)[0]["snippet"], body)
        self.assertEqual(self.search("needle]\\d+", 10)[0]["snippet"], body)

    def test_no_cross_part_joining(self):
        self.add("split-text", 100, [synthetic_text("nee"), synthetic_text("dle")])
        self.add("split-attachment", 90, [synthetic_text("nee"),
                                          synthetic_attachment("dle.pdf")])
        self.assertEqual(self.search("needle", 10), [])

    def test_missing_names_and_metadata_never_infer_a_filename(self):
        self.add("empty-name", 100, [synthetic_attachment("", "needle-id", "needle/mime"),
                                     synthetic_attachment(None, "needle-id-2", "needle/mime")])
        self.add("absent-name", 90, [{"type": "attachment", "value": {
            "id": "needle-id-3", "mime_type": "needle/mime", "byte_len": 0}}])
        self.add("unsupported", 80, [{"type": "unsupported", "value": {
            "kind": "needle", "file_name": "needle.pdf"}}])
        self.add("clean-name", 70, [synthetic_attachment("clean.pdf", "needle-id-4", "needle/mime")])
        self.assertEqual(self.search("needle", 10), [])

    def test_escape_debug_oracle_vectors_and_snippets(self):
        for index, (case, name, window, escaped) in enumerate(RUST_ESCAPE_VECTORS):
            self.assertEqual(self.module["escape_debug"](window), escaped, case)
            self.add("unicode-" + case, 1000 + index,
                     [synthetic_text("quiet"), synthetic_attachment(name)])
        snippets = {hit["message_id"]["value"]: hit["snippet"]
                    for hit in self.search("needle", len(RUST_ESCAPE_VECTORS))}
        for case, _, _, escaped in RUST_ESCAPE_VECTORS:
            self.assertEqual(snippets["unicode-" + case], "Filename: " + escaped, case)

    def test_dotless_and_dotted_i_use_original_spans(self):
        # The old regex fixture falsely matched the dotless body and missed this name.
        self.add("dotless-body", 100, [synthetic_text("d\u0131r\u0131m"),
                                       synthetic_attachment("i.pdf")])
        hits = self.search("i", 10)
        self.assertEqual(self.ids(hits), ["dotless-body"])
        self.assertEqual(hits[0]["snippet"], "Filename: i.pdf")

        self.add("dotted-name", 200, [synthetic_text("quiet"),
                                      synthetic_attachment("\u0130stanbul-needle.pdf")])
        hits = self.search("i\u0307", 10)
        self.assertEqual(self.ids(hits), ["dotted-name"])
        self.assertEqual(hits[0]["snippet"], "Filename: \u0130stanbul-needle.pdf")

    def test_filename_snippets_are_safe_and_preserve_literal_ellipses(self):
        self.add("escaped", 100, [synthetic_text("quiet"),
                                  synthetic_attachment("line\nneedle\u202e\\end")])
        self.add("literal-ellipsis", 90, [synthetic_attachment("\u2026needle\u2026")])
        self.add("truncated", 80, [synthetic_attachment("p" * 40 + "needle" + "s" * 40)])
        snippets = {hit["message_id"]["value"]: hit["snippet"]
                    for hit in self.search("needle", 10)}
        self.assertEqual(snippets["escaped"], "Filename: line needle\\u{202e}\\\\end")
        self.assertEqual(snippets["literal-ellipsis"], "Filename: \u2026needle\u2026")
        self.assertEqual(snippets["truncated"],
                         "Filename: \u2026" + "p" * 30 + "needle" + "s" * 30 + "\u2026")

    def test_mixed_chronology_at_restrictive_limit(self):
        self.add("old-body", 100, [synthetic_text("needle old")])
        self.add("mid-name", 200, [synthetic_text("quiet"),
                                   synthetic_attachment("middle-needle.pdf")])
        self.add("new-body", 300, [synthetic_text("needle new")])
        hits = self.search("needle", 2)
        self.assertEqual(self.ids(hits), ["new-body", "mid-name"])
        self.assertEqual(hits[0]["snippet"], "needle new")
        self.assertEqual(hits[1]["snippet"], "Filename: middle-needle.pdf")

    def test_global_scan_keeps_only_the_newest_twenty_thousand_rows(self):
        for index in range(20001):
            if index == 0:
                value = "outside-only-marker"
            elif index == 1:
                value = "inside-boundary-needle"
            else:
                value = f"quiet {index}"
            parts = [synthetic_text(value)]
            if index <= 1:
                parts.append(synthetic_attachment(value + "-filename"))
            self.add(f"scan-{index:05d}", 1000 + index, parts)
        self.assertEqual(self.search("outside-only-marker", 10), [])
        self.assertEqual(self.ids(self.search("inside-boundary-needle", 10)), ["scan-00001"])
        self.assertEqual(self.search("outside-only-marker-filename", 10), [])
        named = self.search("inside-boundary-needle-filename", 10)
        self.assertEqual(self.ids(named), ["scan-00001"])
        self.assertEqual(named[0]["snippet"], "Filename: inside-boundary-needle-filename")


class StatefulFilenameParity(FixtureFilenameParityMixin, unittest.TestCase):
    def setUp(self):
        self.module = runpy.run_path(DAEMON)
        self.conversation = identity("alice@example.invalid")
        self.state = {"conversations": [{
            "id": self.conversation, "aliases": [], "participants": []}], "messages": []}

    def add_message(self, message):
        self.state["messages"].append(message)

    def search(self, query, limit, participant=None):
        return self.module["search_messages"](self.state, query, limit, participant)


class FakeFilenameParity(FixtureFilenameParityMixin, unittest.TestCase):
    def setUp(self):
        self.module = runpy.run_path(FAKE)
        self.conversation = identity("alice@example.invalid")
        self.module["CONVERSATIONS"][:] = [{
            "id": self.conversation, "aliases": [], "participants": []}]
        self.module["HISTORY"].clear()

    def add_message(self, message):
        self.module["HISTORY"].setdefault(
            message["conversation_id"]["value"], []).append(message)

    def search(self, query, limit, participant=None):
        return self.module["search_messages"](query, limit, participant)


class FilenameScopeSearchMixin:
    def search(self, query, limit, participant=None):
        raise NotImplementedError

    def ids(self, hits):
        return [hit["message_id"]["value"] for hit in hits]

    def test_synthetic_message_ids_keep_the_owning_transport(self):
        _, rows, _ = filename_scope_scenario()
        for row in rows:
            self.assertEqual(row["id"]["transport"], row["conversation_id"]["transport"],
                             row["id"]["value"])

    def test_scoped_older_filename_behind_newer_out_of_scope_rows(self):
        hits = self.search("report-needle", 1,
                           scope("openbubbles", "email", "alice@example.invalid"))
        self.assertEqual(self.ids(hits), ["direct-name"])
        self.assertEqual(hits[0]["snippet"], "Filename: report-needle.pdf")

    def test_alias_filename_hit_remaps_to_canonical_conversation(self):
        hits = self.search("alias-needle", 1,
                           scope("openbubbles", "email", "erin@example.invalid"))
        self.assertEqual(self.ids(hits), ["archive-name"])
        self.assertEqual(hits[0]["conversation_id"], self.archive)
        self.assertEqual(hits[0]["snippet"], "Filename: alias-needle.pdf")

    def test_group_filename_membership(self):
        hits = self.search("group-needle", 1,
                           scope("openbubbles", "email", "bob@example.invalid"))
        self.assertEqual(self.ids(hits), ["group-name"])

    def test_filename_transport_and_handle_kind_are_exact(self):
        self.assertEqual(
            self.ids(self.search("parallel-needle", 1,
                                 scope("other", "email", "alice@example.invalid"))),
            ["parallel-name"],
        )
        self.assertEqual(
            self.ids(self.search("phone-needle", 1,
                                 scope("openbubbles", "tel", "alice@example.invalid"))),
            ["phone-name"],
        )
        self.assertEqual(
            self.search("phone-needle", 10,
                        scope("openbubbles", "opaque", "alice@example.invalid")),
            [],
        )


class StatefulFilenameScopeSearch(FilenameScopeSearchMixin, unittest.TestCase):
    def setUp(self):
        self.module = runpy.run_path(DAEMON)
        conversations, rows, self.archive = filename_scope_scenario()
        self.state = {"conversations": copy.deepcopy(conversations),
                      "messages": copy.deepcopy(rows)}

    def search(self, query, limit, participant=None):
        return self.module["search_messages"](self.state, query, limit, participant)


class FakeFilenameScopeSearch(FilenameScopeSearchMixin, unittest.TestCase):
    def setUp(self):
        self.module = runpy.run_path(FAKE)
        conversations, rows, self.archive = filename_scope_scenario()
        self.module["CONVERSATIONS"][:] = copy.deepcopy(conversations)
        self.module["HISTORY"].clear()
        for row in rows:
            self.module["HISTORY"].setdefault(
                row["conversation_id"]["value"], []).append(copy.deepcopy(row))

    def search(self, query, limit, participant=None):
        return self.module["search_messages"](query, limit, participant)


class FakeTypedPartsRPC(unittest.TestCase):
    def setUp(self):
        self.module = runpy.run_path(FAKE)
        self.canonical = identity("weekend@example.invalid")
        self.alias = identity("weekend-typed-legacy")
        conversation = next(c for c in self.module["CONVERSATIONS"] if c["id"] == self.canonical)
        conversation["aliases"] = [self.alias]
        self.message = typed_message(self.module["message"], self.alias)
        self.module["HISTORY"].setdefault(self.alias["value"], []).append(self.message)

    def test_typed_parts_survive_history_and_context_rpc(self):
        client, server = socket.socketpair()
        client.settimeout(TIMEOUT)
        # serve_client blocks on reads, so closing the client gives it EOF.
        thread = threading.Thread(target=self.module["serve_client"], args=(server,), daemon=True)
        thread.start()
        snapshot = copy.deepcopy(self.message)
        try:
            send(client, {"request_id": 1, "request": {
                "op": "history", "conversation_id": self.alias, "limit": 5}})
            history = recv(client)["response"]
            self.assertEqual([m["id"] for m in history["messages"]], [self.message["id"]])
            self.assertEqual(history["messages"][0]["conversation_id"], self.alias)
            self.assertEqual(history["messages"][0]["parts"], self.message["parts"])
            for requested in (self.alias, self.canonical):
                send(client, {"request_id": 2, "request": {
                    "op": "message_context", "conversation_id": requested,
                    "message_id": self.message["id"], "limit": 1}})
                context = recv(client)["response"]
                self.assertEqual([m["id"] for m in context["messages"]], [self.message["id"]])
                self.assertEqual(context["messages"][0]["conversation_id"], self.canonical)
                self.assertEqual(context["messages"][0]["parts"], self.message["parts"])
            self.assertEqual(self.message, snapshot)
            self.assertEqual(self.module["HISTORY"][self.alias["value"]][-1], snapshot)
        finally:
            client.close()
            thread.join(timeout=2)
            if thread.is_alive():
                server.close()
                thread.join(timeout=2)
            self.assertFalse(thread.is_alive())
            server.close()


class FrameSerialization(unittest.TestCase):
    def check_fixture(self, path, version):
        module = runpy.run_path(path)
        limit = module.get("MAX_FRAME", 1024 * 1024)
        small = {"request_id": 7, "response": {"result": "conversations", "conversations": []}}
        frame_version, body, size = parse_frame(module["frame"](small))
        self.assertEqual(frame_version, version)
        self.assertEqual(body, small)
        self.assertEqual(size, len(json.dumps(body, separators=(",", ":"), ensure_ascii=False).encode()))

        oversized = oversized_response(limit)
        frame_version, body, size = parse_frame(module["frame"](oversized))
        self.assertEqual(frame_version, version)
        self.assertLessEqual(size, limit)
        self.assertEqual(body, {
            "request_id": 41,
            "response": {"result": "error", "error": "internal"},
        })

        event = {"event": {"event": "message", "body": "x" * (limit + 16)}}
        with self.assertRaises(ValueError):
            module["frame"](event)

    def test_fake_daemon_frames_stay_within_limit(self):
        self.check_fixture(FAKE, 9)

    def test_stateful_fake_daemon_frames_stay_within_limit(self):
        self.check_fixture(DAEMON, 9)


class FrameHeaderParity(unittest.TestCase):
    """Both fixtures must accept and reject the same frame headers."""

    def call_read_frame(self, module, data):
        client, server = socket.socketpair()
        outcome = {}

        def worker():
            try:
                outcome["value"] = module["read_frame"](server)
            except BaseException as error:  # noqa: BLE001 - recorded for assertion
                outcome["error"] = error

        thread = threading.Thread(target=worker, daemon=True)
        try:
            client.sendall(data)
            thread.start()
            thread.join(TIMEOUT)
            if thread.is_alive():
                server.close()
                client.close()
                thread.join(TIMEOUT)
            self.assertFalse(thread.is_alive(), "read_frame did not return within the timeout")
        finally:
            client.close()
            server.close()
        return outcome

    def test_valid_v9_frame_parses_in_both_fixtures(self):
        payload = {"request_id": 3, "request": {"op": "echo", "who": identity("me@example.invalid")}}
        for path in (DAEMON, FAKE):
            with self.subTest(fixture=os.path.basename(path)):
                module = runpy.run_path(path)
                frame_version, body, size = parse_frame(module["frame"](payload))
                self.assertEqual(frame_version, 9)
                self.assertEqual(size, len(json.dumps(body, separators=(",", ":"), ensure_ascii=False).encode()))
                outcome = self.call_read_frame(module, module["frame"](payload))
                self.assertNotIn("error", outcome)
                self.assertEqual(outcome.get("value"), body)

    def test_malformed_headers_rejected_identically_by_both_fixtures(self):
        limit = runpy.run_path(FAKE)["MAX_FRAME"]
        cases = {
            "version 5": (5, 2),
            "version 6": (6, 2),
            "version 7": (7, 2),
            "version 99": (99, 2),
            "zero length": (8, 0),
            "oversized length": (8, limit + 1),
        }
        for label, (version, length) in cases.items():
            outcomes = []
            for path in (DAEMON, FAKE):
                with self.subTest(fixture=os.path.basename(path), case=label):
                    outcome = self.call_read_frame(
                        runpy.run_path(path), struct.pack(">BI", version, length)
                    )
                    self.assertIn("error", outcome, "malformed header was accepted")
                    self.assertIsInstance(outcome["error"], ValueError)
                    outcomes.append((type(outcome["error"]), str(outcome["error"])))
            self.assertEqual(outcomes[0], outcomes[1], "fixtures disagree on the rejection")


class StaticServeClientInvalidHeader(unittest.TestCase):
    def test_v6_request_closes_without_response(self):
        module = runpy.run_path(FAKE)
        client, server = socket.socketpair()
        thread = None
        try:
            request = {"request_id": 9, "request": {"op": "list_conversations"}}
            payload = json.dumps(request).encode()
            client.settimeout(TIMEOUT)
            thread = threading.Thread(target=module["serve_client"], args=(server,), daemon=True)
            thread.start()
            client.sendall(b"\x06" + struct.pack(">I", len(payload)) + payload)
            received = bytearray()
            while True:
                try:
                    chunk = client.recv(4096)
                except socket.timeout:
                    self.fail("serve_client did not close the invalid connection")
                except ConnectionResetError:
                    break
                if not chunk:
                    break
                received.extend(chunk)
            self.assertEqual(bytes(received), b"", "serve_client wrote a response to an invalid header")
            thread.join(TIMEOUT)
            self.assertFalse(thread.is_alive(), "serve_client thread did not exit")
        finally:
            client.close()
            server.close()
            if thread is not None:
                thread.join(TIMEOUT)


if __name__ == "__main__":
    unittest.main(verbosity=2)
