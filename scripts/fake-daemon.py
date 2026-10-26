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
import os
import socket
import struct
import sys
import threading
import time
import unicodedata

PROTOCOL_VERSION = 8
MAX_FRAME = 1024 * 1024
MAX_DRAFT_BYTES = 65_536
SEARCH_SCAN_LIMIT = 20_000
SNIPPET_CONTEXT_CHARS = 30
ELLIPSIS = "\u2026"
_NONPRINTABLE_CATEGORIES = frozenset(("Cc", "Cf", "Cs", "Co", "Cn", "Zl", "Zp", "Zs"))
# Mn/Me covers most Grapheme_Extend characters; add common non-mark extenders.
_OTHER_GRAPHEME_EXTEND = frozenset({0x200C, 0x200D, 0x302E, 0x302F, 0xFF9E, 0xFF9F,
                                    0x1D165, *range(0x1D16E, 0x1D173)})
# Additional Mc leading escapes verified against the pinned Rust standard library.
_OTHER_GRAPHEME_EXTEND |= frozenset({
    0x09BE, 0x09D7, 0x0B3E, 0x0B57, 0x0BBE, 0x0BD7, 0x0CC0, 0x0CC2,
    0x0CC7, 0x0CC8, 0x0CCA, 0x0CCB, 0x0CD5, 0x0CD6, 0x0D3E, 0x0D57,
    0x0DCF, 0x0DDF, 0x1715, 0x1734, 0x1B35, 0x1B3B, 0x1B3D, 0x1B43,
    0x1B44, 0x1BAA, 0x1BF2, 0x1BF3, 0xA953, 0xA9C0, 0x111C0, 0x11235,
    0x1133E, 0x1134D, 0x11357, 0x113B8, 0x113C2, 0x113C5, 0x113C7,
    0x113C8, 0x113C9, 0x113CF, 0x114B0, 0x114BD, 0x115AF, 0x116B6,
    0x11930, 0x1193D, 0x11F41, 0x16FF0, 0x16FF1, 0x1D166, 0x1D16D,
})
SESSION_STATE = os.environ.get("BLOOM_FAKE_SESSION_STATE", "active")
LONG_TITLE = os.environ.get("BLOOM_FAKE_LONG_TITLE") == "1"
SUBSCRIBERS: set[socket.socket] = set()
SUBSCRIBERS_LOCK = threading.Lock()
DRAFTS: dict[tuple, dict] = {}
DRAFTS_LOCK = threading.Lock()


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
    (version, length) = struct.unpack(">BI", header)
    if version != PROTOCOL_VERSION or length < 1 or length > MAX_FRAME:
        raise ValueError("invalid frame header")
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


def _is_grapheme_extended(character):
    """Approximate Unicode Grapheme_Extend for leading-character escaping."""
    return (unicodedata.category(character) in ("Mn", "Me")
            or ord(character) in _OTHER_GRAPHEME_EXTEND)


def _is_printable(character):
    if character == " ":
        return True
    return unicodedata.category(character) not in _NONPRINTABLE_CATEGORIES


def escape_debug(text):
    """Rust-style escaping with tested vectors, not exhaustive Unicode parity.

    Leading Grapheme_Extend and Unicode table versions are approximated here.
    """
    pieces = []
    for index, character in enumerate(text):
        if character == "\0":
            piece = "\\0"
        elif character == "\t":
            piece = "\\t"
        elif character == "\r":
            piece = "\\r"
        elif character == "\n":
            piece = "\\n"
        elif character == "\\":
            piece = "\\\\"
        elif character in ("'", '"'):
            piece = "\\" + character
        elif index == 0 and _is_grapheme_extended(character):
            piece = f"\\u{{{ord(character):x}}}"
        elif _is_printable(character):
            piece = character
        else:
            piece = f"\\u{{{ord(character):x}}}"
        pieces.append(piece)
    return "".join(pieces)


def lowercase_with_boundaries(text):
    """Lower each character separately, mapping lowered chars to original spans."""
    lowered = []
    boundaries = []
    for index, character in enumerate(text):
        part = character.lower()
        lowered.append(part)
        boundaries.extend((index, index + 1) for _ in part)
    return "".join(lowered), boundaries


def field_snippet(text, query_lower):
    """Return a contextual window around the first literal lowered match."""
    if not query_lower:
        return None
    lower, boundaries = lowercase_with_boundaries(text)
    start_lower = lower.find(query_lower)
    if start_lower < 0:
        return None
    end_lower = start_lower + len(query_lower)
    start = boundaries[start_lower][0]
    end = boundaries[end_lower - 1][1]
    context_start = max(0, start - SNIPPET_CONTEXT_CHARS)
    context_end = min(len(text), end + SNIPPET_CONTEXT_CHARS)
    body = text[context_start:context_end].replace("\r\n", " ").replace("\r", " ").replace("\n", " ")
    return (ELLIPSIS if context_start > 0 else "") + body + (ELLIPSIS if context_end < len(text) else "")


def search_snippet(item, query_lower):
    """Scan all text parts first, then report the first matching stored name."""
    for part in item.get("parts", []):
        if isinstance(part, dict) and part.get("type") == "text" and isinstance(part.get("value"), str):
            snippet = field_snippet(part["value"], query_lower)
            if snippet is not None:
                return snippet
    for part in item.get("parts", []):
        if not isinstance(part, dict) or part.get("type") != "attachment":
            continue
        value = part.get("value")
        if not isinstance(value, dict):
            continue
        name = value.get("file_name")
        if not isinstance(name, str) or not name:
            continue
        window = field_snippet(name, query_lower)
        if window is not None:
            return "Filename: " + escape_debug(window)
    return None


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


def participant_spec(value) -> tuple[str, str, str] | None:
    """Return (transport, kind, exact handle value), or None for an absent filter.

    A present but malformed filter is rejected rather than treated as absent, so a
    caller can never receive unfiltered results for a scope it asked to apply.
    """
    if value is None:
        return None
    if not isinstance(value, dict):
        raise ValueError("participant")
    transport = value.get("transport")
    handle_spec = value.get("handle")
    if not isinstance(transport, str) or not isinstance(handle_spec, dict):
        raise ValueError("participant")
    kind = handle_spec.get("kind")
    handle_value = handle_spec.get("value")
    if kind not in ("tel", "email", "opaque") or not isinstance(handle_value, str):
        raise ValueError("participant")
    return transport, kind, handle_value


def participant_memberships(spec: tuple[str, str, str]) -> set[str]:
    """Scoped ids of current canonical conversations that contain the exact handle."""
    transport, kind, handle_value = spec
    return {
        scoped_id(conversation["id"])
        for conversation in CONVERSATIONS
        if conversation["id"].get("transport") == transport
        and any(participant["handle"].get("kind") == kind
                and participant["handle"].get("value") == handle_value
                for participant in conversation.get("participants", []))
    }


def search_messages(query, limit, participant=None) -> list[dict]:
    """Search the newest global rows, applying conversation membership first."""
    spec = participant_spec(participant)
    memberships = participant_memberships(spec) if spec is not None else None
    hits = []
    if not isinstance(query, str) or not query.strip() or not isinstance(limit, int) or isinstance(limit, bool) or limit <= 0:
        return hits
    query_lower = query.strip().lower()
    messages = sorted(
        (item for thread in HISTORY.values() for item in thread),
        key=history_key,
        reverse=True,
    )[:SEARCH_SCAN_LIMIT]
    for item in messages:
        conversation_id = canonical_conversation(item["conversation_id"])
        if memberships is not None and scoped_id(conversation_id) not in memberships:
            continue
        snippet = search_snippet(item, query_lower)
        if snippet is None:
            continue
        hits.append({
            "conversation_id": conversation_id,
            "message_id": item["id"],
            "created_at": item["created_at"],
            "snippet": snippet,
        })
        if len(hits) >= limit:
            break
    return hits


def _draft_grapheme_count(text):
    """Approximate extended grapheme clusters for the draft cursor check."""
    count = 0
    join_next = False
    regional_pending = False
    for character in text:
        codepoint = ord(character)
        if codepoint == 0x200D:
            join_next = True
            continue
        if join_next:
            join_next = False
            continue
        if (_is_grapheme_extended(character)
                or 0xFE00 <= codepoint <= 0xFE0F
                or 0xE0100 <= codepoint <= 0xE01EF
                or 0x1F3FB <= codepoint <= 0x1F3FF):
            # unicode-segmentation treats a leading combining run as one cluster.
            if count == 0:
                count = 1
            continue
        if 0x1F1E6 <= codepoint <= 0x1F1FF:
            if regional_pending:
                regional_pending = False
                continue
            regional_pending = True
            count += 1
            continue
        regional_pending = False
        count += 1
    return count


def _draft_key(raw):
    if not isinstance(raw, dict):
        return None
    kind = raw.get("kind")
    if kind == "conversation":
        owner = raw.get("owner")
        if (not isinstance(owner, dict)
                or not isinstance(owner.get("transport"), str)
                or not isinstance(owner.get("value"), str)):
            return None
        return ("conversation", json.dumps(owner, sort_keys=True, separators=(",", ":")))
    if kind == "new_chat":
        transport = raw.get("transport")
        address = raw.get("address")
        if (not isinstance(transport, str) or not isinstance(address, str)
                or not transport.strip() or not address.strip()):
            return None
        return ("new_chat", transport, address)
    return None


def _draft_value(raw):
    if not isinstance(raw, dict):
        return None
    body = raw.get("body")
    cursor = raw.get("cursor")
    pasted_body = raw.get("pasted_body")
    uncertain = raw.get("uncertain")
    if (not isinstance(body, str)
            or not isinstance(cursor, int) or isinstance(cursor, bool) or cursor < 0
            or not isinstance(pasted_body, bool) or not isinstance(uncertain, bool)
            or len(body.encode("utf-8")) > MAX_DRAFT_BYTES
            or cursor > _draft_grapheme_count(body)):
        return None
    return {"body": body, "cursor": cursor, "pasted_body": pasted_body,
            "uncertain": uncertain}


def _draft_revision(raw):
    if not isinstance(raw, int) or isinstance(raw, bool):
        return None
    if raw < 0 or raw > 2**63 - 1:
        return None
    return raw


def handle_draft(op, body):
    with DRAFTS_LOCK:
        return _handle_draft_unlocked(op, body)


def _handle_draft_unlocked(op, body):
    key = _draft_key(body.get("key"))
    if key is None:
        return {"result": "error", "error": "invalid_request"}
    if op == "draft_load":
        stored = DRAFTS.get(key)
        if stored is None:
            return {"result": "draft_load", "draft": None, "revision_floor": 0,
                    "corrupt_deleted": False}
        return {"result": "draft_load",
                "draft": None if stored["deleted"] else stored["draft"],
                "revision_floor": stored["revision"], "corrupt_deleted": False}
    revision = _draft_revision(body.get("revision"))
    if revision is None:
        return {"result": "error", "error": "invalid_request"}
    draft = None if op == "draft_delete" else _draft_value(body.get("draft"))
    if op == "draft_save" and draft is None:
        return {"result": "error", "error": "invalid_request"}
    stored = DRAFTS.get(key)
    if stored is not None and revision <= stored["revision"]:
        return {"result": op, "outcome": "stale"}
    DRAFTS[key] = {"revision": revision, "deleted": op == "draft_delete",
                   "draft": draft, "updated_at": int(time.time() * 1000)}
    return {"result": op, "outcome": "applied"}


def serve_client(conn: socket.socket) -> None:
    while True:
        try:
            request = read_frame(conn)
        except ValueError:
            conn.close()
            break
        if request is None:
            break
        request_id = request.get("request_id", 0)
        kind = request.get("request", {})
        op = kind.get("op")

        if op in ("draft_save", "draft_load", "draft_delete"):
            response = handle_draft(op, kind)
            conn.sendall(frame({"request_id": request_id, "response": response}))
        elif op == "sync_conversations":
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
            limit = kind.get("limit", 0)
            if not isinstance(limit, int) or isinstance(limit, bool) or limit <= 0:
                conn.sendall(frame({"request_id": request_id, "response": {"result": "error", "error": "invalid_request"}}))
                continue
            try:
                hits = search_messages(kind.get("query"), limit, kind.get("participant"))
            except ValueError:
                conn.sendall(frame({"request_id": request_id, "response": {"result": "error", "error": "unsupported"}}))
                continue
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
