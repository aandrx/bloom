#!/usr/bin/env python3
"""Fake Bloom daemon serving fixed fixtures, for reviewing the TUI's look.

It speaks the real IPC framing so the TUI cannot tell the difference, and it serves
synthetic content only, so screenshots never contain real messages or handles. Set
BLOOM_FAKE_SESSION_STATE to a SessionState value (for example, renewal_due) to
exercise the status indicator; it defaults to active.

Usage: fake-daemon.py <socket path>
"""

from __future__ import annotations

import json
import re
import os
import socket
import struct
import sys
import threading
import time

PROTOCOL_VERSION = 4
MAX_FRAME = 1024 * 1024
SESSION_STATE = os.environ.get("BLOOM_FAKE_SESSION_STATE", "active")
LONG_TITLE = os.environ.get("BLOOM_FAKE_LONG_TITLE") == "1"
SUBSCRIBERS: set[socket.socket] = set()
SUBSCRIBERS_LOCK = threading.Lock()


def frame(payload: dict) -> bytes:
    """Serialize one frame, replacing an oversized response with an Internal error.

    The real dispatch serializes the whole payload before writing, so an oversized
    response leaves the stream untouched and a bounded replacement is safe. An
    oversized event has no valid replacement and raises before any bytes are sent.
    """
    body = json.dumps(payload, separators=(",", ":"), ensure_ascii=False).encode()
    if len(body) > MAX_FRAME:
        request_id = payload.get("request_id") if isinstance(payload, dict) else None
        is_response = (
            isinstance(payload, dict)
            and isinstance(payload.get("response"), dict)
            and isinstance(request_id, int)
            and not isinstance(request_id, bool)
        )
        if not is_response:
            raise ValueError("oversized non-response frame")
        body = json.dumps(
            {"request_id": request_id, "response": {"result": "error", "error": "internal"}},
            separators=(",", ":"),
            ensure_ascii=False,
        ).encode()
        if len(body) > MAX_FRAME:
            raise ValueError("oversized response fallback")
    return bytes([PROTOCOL_VERSION]) + struct.pack(">I", len(body)) + body


def read_frame(conn: socket.socket) -> dict | None:
    header = b""
    while len(header) < 5:
        chunk = conn.recv(5 - len(header))
        if not chunk:
            return None
        header += chunk
    (_version, length) = struct.unpack(">BI", header)
    body = b""
    while len(body) < length:
        chunk = conn.recv(length - len(body))
        if not chunk:
            return None
        body += chunk
    return json.loads(body)


def cid(value: str) -> dict:
    return {"transport": "openbubbles", "value": value}


def handle(value: str, kind: str = "email") -> dict:
    return {"kind": kind, "value": value}


def conversation(value: str, title: str, people: int, last: int) -> dict:
    participants = [{"handle": handle("me@example.invalid"), "display_name": None, "is_me": True}]
    for index in range(people - 1):
        participants.append(
            {
                "handle": handle(f"person{index}@example.invalid"),
                "display_name": None,
                "is_me": False,
            }
        )
    return {
        "id": cid(value),
        "remote_id": f"iMessage;-;{value}",
        "kind": "group" if people > 2 else "direct",
        "title": title,
        "participants": participants,
        "created_at": 1,
        "last_message_at": last,
        "preview": PREVIEWS.get(value),
    }


# The real daemon fills previews from its store; the fixture mirrors that.
PREVIEWS = {
    "weekend@example.invalid": "I'll check the tide times tonight and send the link here.",
    "dana@example.invalid": "Perfect, see you at the usual spot.",
    "jules@example.invalid": "Sending the revised deck over after lunch.",
    "design@example.invalid": "New type scale is in the shared library now.",
}

CONVERSATIONS = [
    conversation("weekend@example.invalid", "Weekend crew", 3, 1_700_000_400_000),
    # A direct chat the engine gave no title for: the UI must name it after the
    # other participant rather than showing `Conversation`.
    conversation(
        "dana@example.invalid",
        "Direct chat with a deliberately long synthetic title for narrow terminal review"
        if LONG_TITLE
        else None,
        2,
        1_700_000_300_000,
    ),
    conversation("jules@example.invalid", "Jules Lee", 2, 1_700_000_200_000),
    conversation("design@example.invalid", "Design systems", 4, 1_700_000_100_000),
]


def message(
    conversation_value: str,
    ident: str,
    at: int,
    text: str,
    mine: bool,
    sender: str = "maya@example.invalid",
    delivery: str | None = None,
) -> dict:
    return {
        "id": cid(ident),
        "conversation_id": cid(conversation_value),
        "remote_id": None,
        "sender": handle("me@example.invalid" if mine else sender),
        "parts": [{"type": "text", "value": text}],
        "created_at": at,
        "sent_at": None,
        "delivery": delivery or ("read" if mine else "delivered"),
        "from_me": mine,
        "edited_at": None,
        "reactions": [],
        "unsent_at": None,
    }


def history_key(item: dict) -> tuple[int, str]:
    """Match the store ordering (created_at, id) with the id's stable JSON form."""
    return item["created_at"], json.dumps(item["id"], separators=(",", ":"), ensure_ascii=False)


def canonical_conversation(conversation_id: dict) -> dict:
    """Resolve an alias to its canonical conversation identity."""
    for conversation in CONVERSATIONS:
        if conversation_id == conversation["id"] or conversation_id in conversation.get("aliases", []):
            return conversation["id"]
    return conversation_id


def scoped_id(item: dict) -> str:
    """Compare conversation identities including their transport."""
    return json.dumps(item, separators=(",", ":"), ensure_ascii=False)


def message_context(conversation_id: dict, message_id: dict, limit: int) -> list[dict] | None:
    """Return the bounded window centred on an exact stored message, or None.

    Mirrors the real store: resolve the canonical conversation and its aliases,
    search every history row by full scoped identity, always include the exact
    target, and remap returned identities to the canonical conversation.
    """
    target = next((item for thread in HISTORY.values() for item in thread
                   if item["id"] == message_id), None)
    if target is None:
        return None
    canonical = canonical_conversation(conversation_id)
    conversation = next((item for item in CONVERSATIONS if item["id"] == canonical), None)
    aliases = conversation.get("aliases", []) if conversation is not None else []
    if target["conversation_id"] != canonical and target["conversation_id"] not in aliases:
        return None
    identities = {scoped_id(item) for item in [canonical, *aliases]}
    rows = [item for thread in HISTORY.values() for item in thread]
    bound = history_key(target)
    older_quota = (limit - 1) // 2
    newer_quota = limit - 1 - older_quota
    older = [item for item in rows
             if scoped_id(item["conversation_id"]) in identities and history_key(item) < bound]
    older.sort(key=history_key)
    older = older[-older_quota:] if older_quota else []
    newer = [item for item in rows
             if scoped_id(item["conversation_id"]) in identities and history_key(item) > bound]
    newer.sort(key=history_key)
    newer = newer[:newer_quota]

    def remap(item: dict) -> dict:
        result = dict(item)
        result["conversation_id"] = canonical
        return result

    return [remap(item) for item in older] + [remap(target)] + [remap(item) for item in newer]


def search_snippet(text, query):
    """Return a 30-character context window around the match, or None.

    The match is located with a case-insensitive regex so the offsets index the
    original text: lowercasing can change length, which would slice the wrong run.
    """
    match = re.search(re.escape(query), text, re.IGNORECASE)
    if match is None:
        return None
    start = max(0, match.start() - 30)
    stop = min(len(text), match.end() + 30)
    body = text[start:stop].replace("\r\n", " ").replace("\r", " ").replace("\n", " ")
    return ("\u2026" if start > 0 else "") + body + ("\u2026" if stop < len(text) else "")


HISTORY = {
    "weekend@example.invalid": [
        message("weekend@example.invalid", "m1", 1_700_000_100_000, "Saturday looks perfect for the coast. Should we take the early train?", False),
        message(
            "weekend@example.invalid",
            "m2",
            1_700_000_200_000,
            "I can bring coffee and the little thermos. Meet at the station?",
            False,
            sender="jules@example.invalid",
        ),
        message(
            "weekend@example.invalid",
            "m3",
            1_700_000_300_000,
            "Yes, 8:40 works. I'll grab pastries on the way.",
            True,
            delivery="delivered",
        ),
        message("weekend@example.invalid", "m4", 1_700_000_400_000, "I'll check the tide times tonight and send the link here.", False),
        # A follow-up from the same sender a minute later: grouped under one byline.
        message("weekend@example.invalid", "m5", 1_700_000_460_000, "Low tide is around 9, so that should work out.", False),
    ],
    "dana@example.invalid": [
        message("dana@example.invalid", "n1", 1_700_000_300_000, "Are we still on for the walk on Sunday?", False),
        message("dana@example.invalid", "n2", 1_700_000_310_000, "Yes, morning works best for me.", True, delivery="delivered"),
        message("dana@example.invalid", "n3", 1_700_000_320_000, "Perfect, see you at the usual spot.", False),
    ],
    "jules@example.invalid": [
        message("jules@example.invalid", "j1", 1_700_000_200_000, "Sending the revised deck over after lunch.", False),
    ],
    "design@example.invalid": [
        message("design@example.invalid", "d1", 1_700_000_100_000, "New type scale is in the shared library now.", False),
    ],
}


def serve_client(conn: socket.socket) -> None:
    while True:
        request = read_frame(conn)
        if request is None:
            break
        request_id = request.get("request_id", 0)
        kind = request.get("request", {})
        op = kind.get("op")

        if op == "sync_conversations":
            if os.environ.get("BLOOM_FAKE_MISSED_MESSAGE") == "1":
                # A stored phone message with no live Message event: the sidebar
                # updates, and the open transcript must catch up from History.
                ident = "missed-inbound-1"
                thread = HISTORY["weekend@example.invalid"]
                if not any(item["id"]["value"] == ident for item in thread):
                    text = "Incoming after a missed live event"
                    thread.append(message("weekend@example.invalid", ident, 1_700_000_520_000, text, False))
                    CONVERSATIONS[0]["preview"] = text
                    CONVERSATIONS[0]["last_message_at"] = 1_700_000_520_000
                # Deliberately do not publish a Message or ConversationsChanged
                # event: the /sync response alone must refresh the open thread.
            else:
                push_all({"event": "conversations_changed"})
            conn.sendall(frame({
                "request_id": request_id,
                "response": {
                    "result": "conversation_sync",
                    "status": "complete",
                    "conversations": len(CONVERSATIONS),
                },
            }))
        elif op == "list_conversations":
            conn.sendall(frame({"request_id": request_id, "response": {"result": "conversations", "conversations": CONVERSATIONS}}))
        elif op == "message_context":
            limit = kind.get("limit")
            if not isinstance(limit, int) or isinstance(limit, bool) or not 1 <= limit <= 200:
                conn.sendall(frame({"request_id": request_id, "response": {"result": "error", "error": "invalid_request"}}))
                continue
            requested = kind.get("conversation_id", {})
            selected = message_context(requested, kind.get("message_id", {}), limit)
            if selected is None:
                conn.sendall(frame({"request_id": request_id, "response": {"result": "error", "error": "not_found"}}))
                continue
            conn.sendall(frame({"request_id": request_id, "response": {"result": "message_context", "messages": selected}}))
        elif op == "search_messages":
            if not isinstance(kind.get("limit", 0), int) or kind.get("limit", 0) <= 0:
                conn.sendall(frame({"request_id": request_id, "response": {"result": "error", "error": "invalid_request"}}))
                continue
            query = kind.get("query")
            limit = kind.get("limit", 0)
            hits = []
            if isinstance(query, str) and query.strip() and isinstance(limit, int) and limit > 0:
                messages = sorted(
                    (item for thread in HISTORY.values() for item in thread),
                    key=history_key,
                    reverse=True,
                )
                for item in messages:
                    snippet = None
                    for part in item.get("parts", []):
                        if part.get("type") == "text" and isinstance(part.get("value"), str):
                            snippet = search_snippet(part["value"], query.strip())
                            if snippet is not None:
                                break
                    if snippet is None:
                        continue
                    hits.append({
                        "conversation_id": item["conversation_id"],
                        "message_id": item["id"],
                        "created_at": item["created_at"],
                        "snippet": snippet,
                    })
                    if len(hits) >= limit:
                        break
            conn.sendall(frame({"request_id": request_id, "response": {"result": "search_results", "hits": hits}}))
        elif op == "history":
            key = kind.get("conversation_id", {}).get("value", "")
            messages = HISTORY.get(key, [])
            limit = kind.get("limit")
            cursor = kind.get("before")
            if limit == 0:
                messages = []
            elif cursor is not None:
                bound = history_key(cursor)
                older = [item for item in messages if history_key(item) < bound]
                older.sort(key=history_key)
                if isinstance(limit, int):
                    older = older[-limit:]
                messages = older
            elif isinstance(limit, int):
                messages = messages[-limit:]
            conn.sendall(frame({"request_id": request_id, "response": {"result": "history", "messages": messages}}))
        elif op == "send" or op == "start_conversation":
            conn.sendall(frame({"request_id": request_id, "response": {"result": "send", "message_id": cid("sent-1")}}))
            push(conn, {
                "event": "message",
                "conversation_id": cid("weekend@example.invalid"),
                "message": message("weekend@example.invalid", "sent-1", int(time.time() * 1000), "Yes, see you then.", True),
            })
        elif op == "subscribe":
            conn.sendall(frame({"request_id": request_id, "response": {"result": "subscribed"}}))
            conn.sendall(frame({"event": {"event": "session", "state": SESSION_STATE}}))
            with SUBSCRIBERS_LOCK:
                SUBSCRIBERS.add(conn)
            while True:
                time.sleep(1)


def push(conn: socket.socket, event: dict) -> None:
    try:
        conn.sendall(frame({"event": event}))
    except OSError:
        pass


def push_all(event: dict) -> None:
    with SUBSCRIBERS_LOCK:
        subscribers = tuple(SUBSCRIBERS)
    for subscriber in subscribers:
        push(subscriber, event)


def main() -> int:
    path = sys.argv[1]
    if os.path.exists(path):
        os.unlink(path)
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(path)
    server.listen(16)
    print(f"fake daemon listening on {path}", flush=True)
    while True:
        conn, _ = server.accept()
        threading.Thread(target=serve_client, args=(conn,), daemon=True).start()


if __name__ == "__main__":
    sys.exit(main())
