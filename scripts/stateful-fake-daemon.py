#!/usr/bin/env python3
"""Synthetic persistent Bloom IPC daemon for black-box tests."""
import argparse
import copy
import json
import os
import re
import socket
import struct
import threading
import time
import unicodedata
import uuid

VERSION = 10
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
LOCK = threading.RLock()
SUBS = set()
WRITE_LOCKS = {}
MY_HANDLE = {"kind": "email", "value": "me@example.invalid"}
PEOPLE = {"alice@example.invalid": "Alice", "bob@example.invalid": "Bob"}


def frame(obj):
    """Serialize one frame, replacing an oversized response with an Internal error.

    Payload serialization completes before any bytes are written, matching the real
    dispatch, so an oversized response can be replaced safely. An oversized event
    has no valid replacement and raises before any bytes are sent.
    """
    data = json.dumps(obj, separators=(",", ":"), ensure_ascii=False).encode()
    if len(data) > MAX_FRAME:
        request_id = obj.get("request_id") if isinstance(obj, dict) else None
        is_response = (
            isinstance(obj, dict)
            and isinstance(obj.get("response"), dict)
            and isinstance(request_id, int)
            and not isinstance(request_id, bool)
        )
        if not is_response:
            raise ValueError("oversized non-response frame")
        data = json.dumps(
            {"request_id": request_id, "response": {"result": "error", "error": "internal"}},
            separators=(",", ":"),
            ensure_ascii=False,
        ).encode()
        if len(data) > MAX_FRAME:
            raise ValueError("oversized response fallback")
    return bytes([VERSION]) + struct.pack(">I", len(data)) + data


def write(c, obj):
    with WRITE_LOCKS_LOCK:
        lock = WRITE_LOCKS.setdefault(c, threading.Lock())
    with lock:
        c.sendall(frame(obj))


WRITE_LOCKS_LOCK = threading.Lock()


def read_exact(c, n):
    data = bytearray()
    while len(data) < n:
        chunk = c.recv(n - len(data))
        if not chunk:
            return None
        data.extend(chunk)
    return bytes(data)


def read_frame(c):
    header = read_exact(c, 5)
    if header is None:
        return None
    version, size = struct.unpack(">BI", header)
    if version != VERSION or size < 1 or size > MAX_FRAME:
        raise ValueError("invalid frame header")
    payload = read_exact(c, size)
    if payload is None:
        raise ValueError("truncated frame")
    return json.loads(payload)


def history_key(item):
    """Match the store ordering (created_at, id) with the id's stable JSON form."""
    return item["created_at"], json.dumps(item["id"], separators=(",", ":"), ensure_ascii=False)


def message_context(state, conversation_id, message_id, limit):
    """Return the bounded window centred on an exact stored message, or None."""
    target = next((m for m in state["messages"] if m["id"] == message_id), None)
    if target is None:
        return None
    canonical = canonical_conversation(state, conversation_id)
    conversation = next((c for c in state["conversations"] if c["id"] == canonical), None)
    aliases = conversation.get("aliases", []) if conversation is not None else []
    if target["conversation_id"] != canonical and target["conversation_id"] not in aliases:
        return None
    identities = {json.dumps(item, separators=(",", ":"), ensure_ascii=False)
                  for item in [canonical, *aliases]}
    bound = history_key(target)
    older_quota = (limit - 1) // 2
    newer_quota = limit - 1 - older_quota
    older = [m for m in state["messages"]
             if json.dumps(m["conversation_id"], separators=(",", ":"), ensure_ascii=False) in identities
             and history_key(m) < bound]
    older.sort(key=history_key)
    older = older[-older_quota:] if older_quota else []
    newer = [m for m in state["messages"]
             if json.dumps(m["conversation_id"], separators=(",", ":"), ensure_ascii=False) in identities
             and history_key(m) > bound]
    newer.sort(key=history_key)
    newer = newer[:newer_quota]

    def remap(message):
        result = copy.deepcopy(message)
        result["conversation_id"] = canonical
        return result

    return [remap(message) for message in older] + [remap(target)] + [remap(message) for message in newer]


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


def canonical_conversation(state, conversation_id):
    for conversation in state["conversations"]:
        if conversation_id in conversation.get("aliases", []):
            return conversation["id"]
    return conversation_id


def participant_spec(value):
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


def search_messages(state, query, limit, participant=None):
    """Search the newest global rows, applying conversation membership first."""
    spec = participant_spec(participant)
    memberships = None
    if spec is not None:
        transport, kind, handle_value = spec
        memberships = {
            json.dumps(conversation["id"], separators=(",", ":"), ensure_ascii=False)
            for conversation in state["conversations"]
            if conversation["id"].get("transport") == transport
            and any(participant_handle.get("kind") == kind
                    and participant_handle.get("value") == handle_value
                    for participant_handle in (p["handle"] for p in conversation.get("participants", [])))
        }
    hits = []
    if (not isinstance(query, str) or not query.strip()
            or not isinstance(limit, int) or isinstance(limit, bool) or limit <= 0):
        return hits
    query_lower = query.strip().lower()
    for message in sorted(state["messages"], key=history_key, reverse=True)[:SEARCH_SCAN_LIMIT]:
        canonical = canonical_conversation(state, message["conversation_id"])
        if memberships is not None and json.dumps(canonical, separators=(",", ":"), ensure_ascii=False) not in memberships:
            continue
        snippet = search_snippet(message, query_lower)
        if snippet is None:
            continue
        hits.append({"conversation_id": canonical,
                     "message_id": message["id"],
                     "created_at": message["created_at"],
                     "snippet": snippet})
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
        return {"kind": "conversation", "owner": copy.deepcopy(owner)}
    if kind == "new_chat":
        transport = raw.get("transport")
        address = raw.get("address")
        if (not isinstance(transport, str) or not isinstance(address, str)
                or not transport.strip() or not address.strip()):
            return None
        return {"kind": "new_chat", "transport": transport, "address": address}
    return None


def _draft_key_id(key):
    return json.dumps(key, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


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


def handle_draft(state, args, op, body):
    key = _draft_key(body.get("key"))
    if key is None:
        return {"result": "error", "error": "invalid_request"}
    if op == "draft_save" and getattr(args, "fail_draft_saves", False):
        return {"result": "error", "error": "internal"}
    key_id = _draft_key_id(key)
    if op == "draft_load":
        with LOCK:
            stored = next((item for item in state.setdefault("drafts", [])
                           if _draft_key_id(item["key"]) == key_id), None)
            if stored is None:
                return {"result": "draft_load", "draft": None, "revision_floor": 0,
                        "corrupt_deleted": False}
            return {"result": "draft_load",
                    "draft": copy.deepcopy(None if stored["deleted"] else stored["draft"]),
                    "revision_floor": stored["revision"], "corrupt_deleted": False}
    revision = _draft_revision(body.get("revision"))
    if revision is None:
        return {"result": "error", "error": "invalid_request"}
    draft = None if op == "draft_delete" else _draft_value(body.get("draft"))
    if op == "draft_save" and draft is None:
        return {"result": "error", "error": "invalid_request"}
    with LOCK:
        drafts = state.setdefault("drafts", [])
        stored = next((item for item in drafts if _draft_key_id(item["key"]) == key_id), None)
        if stored is not None and revision <= stored["revision"]:
            return {"result": op, "outcome": "stale"}
        entry = {"key": key, "revision": revision, "deleted": op == "draft_delete",
                 "draft": draft, "updated_at": int(time.time() * 1000)}
        if stored is None:
            drafts.append(entry)
        else:
            drafts[drafts.index(stored)] = entry
        save(state, args.store)
    return {"result": op, "outcome": "applied"}


def save(state, path):
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as stream:
        json.dump(state, stream, ensure_ascii=False)
    os.replace(tmp, path)


def initial():
    chats = []
    for address, title in PEOPLE.items():
        chats.append({
            "id": {"transport": "openbubbles", "value": address},
            "remote_id": "iMessage;-;" + address, "kind": "direct", "title": title,
            "participants": [
                {"handle": MY_HANDLE, "display_name": None, "is_me": True},
                {"handle": {"kind": "email", "value": address}, "display_name": title, "is_me": False},
            ], "created_at": 1700000000000, "last_message_at": None, "aliases": [], "preview": None,
        })
    return {"conversations": chats, "messages": [], "drafts": []}


def normalize_address(raw):
    if not isinstance(raw, str):
        return None
    value = raw.strip()
    if value.lower().startswith("mailto:"):
        value = value[7:]
    elif value.lower().startswith("tel:"):
        value = value[4:]
    if "@" in value:
        value = value.lower()
        if not re.fullmatch(r"[a-z0-9.!#$%&'*+/=?^_`{|}~-]+@[a-z0-9](?:[a-z0-9.-]*[a-z0-9])?", value):
            return None
        return "email", value
    if not re.fullmatch(r"\+?[0-9][0-9 ().-]*", value):
        return None
    digits = re.sub(r"\D", "", value)
    if len(digits) < 7 or len(digits) > 15:
        return None
    return "tel", "+" + digits


def matching_conversation(state, kind, address):
    return next((conv for conv in state["conversations"]
                 if conv["id"].get("transport") == "openbubbles"
                 and any(not p.get("is_me") and p["handle"].get("kind") == kind
                         and p["handle"].get("value", "").lower() == address
                         for p in conv["participants"])), None)


def new_conversation(kind, address):
    title = PEOPLE.get(address, address)
    return {"id": {"transport": "openbubbles", "value": address},
            "remote_id": "iMessage;-;" + address, "kind": "direct", "title": title,
            "participants": [{"handle": MY_HANDLE, "display_name": None, "is_me": True},
                             {"handle": {"kind": kind, "value": address}, "display_name": title, "is_me": False}],
            "created_at": int(time.time() * 1000), "last_message_at": None, "aliases": [], "preview": None}


def push_all(event):
    dead = []
    with LOCK:
        subscribers = tuple(SUBS)
    for client in subscribers:
        try:
            write(client, {"event": event})
        except OSError:
            dead.append(client)
    if dead:
        with LOCK:
            SUBS.difference_update(dead)


def serve(client, args, state):
    subscribed = False
    try:
        while True:
            try:
                request = read_frame(client)
            except (ValueError, UnicodeError, json.JSONDecodeError):
                break
            if request is None:
                break
            request_id = request.get("request_id", 0)
            body = request.get("request", {})
            op = body.get("op")
            if op == "subscribe":
                with WRITE_LOCKS_LOCK:
                    writer = WRITE_LOCKS.setdefault(client, threading.Lock())
                subscribed = True
                with writer:
                    with LOCK:
                        SUBS.add(client)
                    # Registration precedes the acknowledgement, while this
                    # socket's write lock keeps live events behind session replay.
                    client.sendall(
                        frame({"request_id": request_id, "response": {"result": "subscribed"}})
                        + frame({"event": {"event": "session", "state": args.session_state}})
                    )
                continue
            try:
                if op in ("draft_save", "draft_load", "draft_delete"):
                    response = handle_draft(state, args, op, body)
                elif op == "fixture_append_message":
                    # Test-only persistence without fanout. This proves that the
                    # real TUI's periodic history reconciliation ran.
                    cid = body.get("conversation_id", {})
                    text = body.get("text")
                    if (cid.get("transport") != "openbubbles"
                            or not isinstance(text, str) or not text):
                        raise LookupError
                    with LOCK:
                        candidate = next((x for x in state["conversations"] if x["id"] == cid), None)
                        if candidate is None:
                            raise LookupError
                        now = int(time.time() * 1000)
                        sender = next(p["handle"] for p in candidate["participants"] if not p.get("is_me"))
                        message = {"id": {"transport": "openbubbles", "value": uuid.uuid4().hex},
                                   "conversation_id": candidate["id"], "remote_id": None, "sender": sender,
                                   "parts": [{"type": "text", "value": text}], "created_at": now,
                                   "sent_at": None, "delivery": "delivered", "from_me": False,
                                   "edited_at": None, "reactions": [], "unsent_at": None}
                        state["messages"].append(message)
                        candidate["preview"] = text
                        candidate["last_message_at"] = now
                        save(state, args.store)
                    response = {"result": "fixture_append_message", "message_id": message["id"]}
                elif op == "fetch_attachment":
                    token = body.get("token")
                    if not isinstance(token, str) or not re.fullmatch(r"[0-9a-f]{32}", token):
                        response = {"result": "error", "error": "invalid_request"}
                    else:
                        # A real cache file, so `/open` exercises its validation and
                        # the external opener path instead of a placeholder.
                        cache = os.path.join(os.path.dirname(os.path.abspath(args.store)),
                                             "attachments")
                        os.makedirs(cache, mode=0o700, exist_ok=True)
                        cached = os.path.join(cache, f"bloom-fixture-{token}.bin")
                        with open(cached, "wb") as stream:
                            stream.write(b"fixture-payload".ljust(42, b"!"))
                        os.chmod(cached, 0o600)
                        response = {"result": "attachment_fetched",
                                    "path": cached, "bytes": 42}
                elif op == "send_attachment":
                    cid = body.get("conversation_id", {})
                    path = body.get("path")
                    with LOCK:
                        candidate = next((x for x in state["conversations"] if x["id"] == cid), None)
                    if candidate is None or cid.get("transport") != "openbubbles":
                        raise LookupError
                    try:
                        valid = (isinstance(path, str) and os.path.isabs(path)
                                 and not any(part in ('.', '..')
                                             for part in path.split('/'))
                                 and not os.path.islink(path) and os.path.isfile(path)
                                 and 0 < os.path.getsize(path) <= 25 * 1024 * 1024)
                        name = os.path.basename(path) if valid else ""
                    except OSError:
                        valid = False
                        name = ""
                    if not valid or not name:
                        write(client, {"request_id": request_id,
                                       "response": {"result": "error", "error": "invalid_request"}})
                        continue
                    if args.reject_first_send and not getattr(args, "used_send"):
                        setattr(args, "used_send", True)
                        write(client, {"request_id": request_id,
                                       "response": {"result": "error", "error": "transport_unavailable"}})
                        continue
                    if args.delay:
                        time.sleep(args.delay)
                    now = int(time.time() * 1000)
                    byte_len = os.path.getsize(path)
                    with LOCK:
                        candidate = next((x for x in state["conversations"] if x["id"] == candidate["id"]), None)
                        if candidate is None:
                            raise LookupError
                        message = {"id": {"transport": "openbubbles", "value": uuid.uuid4().hex},
                                   "conversation_id": candidate["id"], "remote_id": None, "sender": MY_HANDLE,
                                   "parts": [{"type": "attachment",
                                              "value": {"id": f"local:{uuid.uuid4().hex}",
                                                        "mime_type": None, "file_name": name,
                                                        "byte_len": byte_len}}],
                                   "created_at": now, "sent_at": now, "delivery": "sent",
                                   "from_me": True, "edited_at": None, "reactions": [], "unsent_at": None}
                        state["messages"].append(message)
                        candidate["preview"] = "You: [attachment]"
                        candidate["last_message_at"] = now
                        save(state, args.store)
                    push_all({"event": "message", "conversation_id": candidate["id"], "message": message})
                    response = {"result": "send", "message_id": message["id"]}
                elif op in ("send", "start_conversation"):
                    is_start = op == "start_conversation"
                    parts = body.get("body", {}).get("parts", [])
                    if (not isinstance(parts, list) or not parts
                            or any(not isinstance(part, dict)
                                   or part.get("type") != "text"
                                   or not isinstance(part.get("value"), str)
                                   for part in parts)
                            or not any(part["value"] for part in parts)):
                        write(client, {"request_id": request_id,
                                       "response": {"result": "error", "error": "unsupported"}})
                        continue
                    candidate = None
                    if is_start:
                        normalized = normalize_address(body.get("address"))
                        if normalized is None:
                            raise LookupError
                        kind, address = normalized
                        with LOCK:
                            candidate = matching_conversation(state, kind, address)
                        if candidate is None:
                            candidate = new_conversation(kind, address)
                    else:
                        cid = body.get("conversation_id", {})
                        if cid.get("transport") != "openbubbles":
                            raise LookupError
                        with LOCK:
                            candidate = next((x for x in state["conversations"] if x["id"] == cid), None)
                        if candidate is None:
                            raise LookupError
                    flag = args.reject_first_start if is_start else args.reject_first_send
                    used = "used_start" if is_start else "used_send"
                    with LOCK:
                        reject = flag and not getattr(args, used)
                        if reject:
                            setattr(args, used, True)
                    if reject:
                        write(client, {"request_id": request_id, "response": {"result": "error", "error": "transport_unavailable"}})
                        continue
                    if args.delay:
                        time.sleep(args.delay)
                    now = int(time.time() * 1000)
                    with LOCK:
                        if is_start:
                            candidate = matching_conversation(state, kind, address)
                            created = candidate is None
                            if created:
                                candidate = new_conversation(kind, address)
                                state["conversations"].append(candidate)
                        else:
                            candidate = next((x for x in state["conversations"] if x["id"] == candidate["id"]), None)
                            if candidate is None:
                                raise LookupError
                            created = False
                        message = {"id": {"transport": "openbubbles", "value": uuid.uuid4().hex},
                                   "conversation_id": candidate["id"], "remote_id": None, "sender": MY_HANDLE,
                                   "parts": parts, "created_at": now, "sent_at": now, "delivery": "sent",
                                   "from_me": True, "edited_at": None, "reactions": [], "unsent_at": None}
                        state["messages"].append(message)
                        candidate["preview"] = " ".join(p.get("value", "") for p in parts if p.get("type") == "text")
                        candidate["last_message_at"] = now
                        save(state, args.store)
                    if created:
                        push_all({"event": "conversations_changed"})
                    push_all({"event": "message", "conversation_id": candidate["id"], "message": message})
                    response = {"result": "send", "message_id": message["id"]}
                else:
                    if op == "sync_conversations" and args.sync_delay:
                        time.sleep(args.sync_delay)
                    with LOCK:
                        if op == "list_conversations":
                            response = {"result": "conversations", "conversations": copy.deepcopy(state["conversations"])}
                        elif op == "history":
                            cid = body.get("conversation_id", {})
                            if cid.get("transport") != "openbubbles":
                                raise LookupError
                            messages = [m for m in state["messages"] if m["conversation_id"] == cid]
                            limit = body.get("limit", len(messages))
                            cursor = body.get("before")
                            if not limit:
                                selected = []
                            elif cursor is None:
                                selected = messages[-limit:]
                            else:
                                bound = history_key(cursor)
                                older = [m for m in messages if history_key(m) < bound]
                                older.sort(key=history_key)
                                selected = older[-limit:]
                            response = {"result": "history", "messages": copy.deepcopy(selected)}
                        elif op == "message_context":
                            limit = body.get("limit")
                            if (not isinstance(limit, int) or isinstance(limit, bool)
                                    or limit < 1 or limit > 200):
                                response = {"result": "error", "error": "invalid_request"}
                                write(client, {"request_id": request_id, "response": response})
                                continue
                            selected = message_context(
                                state,
                                body.get("conversation_id", {}),
                                body.get("message_id", {}),
                                limit,
                            )
                            if selected is None:
                                raise LookupError
                            response = {"result": "message_context", "messages": selected}
                        elif op == "search_messages":
                            query = body.get("query")
                            limit = body.get("limit", 0)
                            if not isinstance(limit, int) or limit <= 0:
                                # Match the daemon: an invalid limit is rejected, not
                                # silently answered with an empty result.
                                response = {"result": "error", "error": "invalid_request"}
                                write(client, {"request_id": request_id, "response": response})
                                continue
                            try:
                                hits = search_messages(state, query, limit, body.get("participant"))
                            except ValueError:
                                response = {"result": "error", "error": "unsupported"}
                                write(client, {"request_id": request_id, "response": response})
                                continue
                            response = {"result": "search_results", "hits": hits}
                        elif op == "sync_conversations":
                            response = {"result": "conversation_sync", "status": "complete", "conversations": len(state["conversations"])}
                        else:
                            response = {"result": "error", "error": "unsupported"}
                write(client, {"request_id": request_id, "response": response})
            except LookupError:
                write(client, {"request_id": request_id, "response": {"result": "error", "error": "not_found"}})
    except (OSError, ValueError):
        pass
    finally:
        if subscribed:
            with LOCK:
                SUBS.discard(client)
        with WRITE_LOCKS_LOCK:
            WRITE_LOCKS.pop(client, None)
        client.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", required=True)
    parser.add_argument("--store", required=True)
    parser.add_argument("--reject-first-send", action="store_true")
    parser.add_argument("--reject-first-start", action="store_true")
    parser.add_argument("--fail-draft-saves", action="store_true")
    parser.add_argument("--delay", type=float, default=0)
    parser.add_argument("--sync-delay", type=float, default=0)
    parser.add_argument("--session-state", choices=['active', 'failed', 'blocked', 'disconnected'], default='active')
    args = parser.parse_args()
    if args.delay < 0:
        parser.error("--delay must be nonnegative")
    args.used_send = args.used_start = False
    if os.path.exists(args.store):
        with open(args.store, encoding="utf-8") as stream:
            state = json.load(stream)
    else:
        state = initial()
        save(state, args.store)
    state.setdefault("drafts", [])
    try:
        os.unlink(args.socket)
    except FileNotFoundError:
        pass
    server = socket.socket(socket.AF_UNIX)
    server.bind(args.socket)
    server.listen(32)
    print("synthetic daemon listening", flush=True)
    try:
        while True:
            client, _ = server.accept()
            threading.Thread(target=serve, args=(client, args, state), daemon=True).start()
    finally:
        server.close()


if __name__ == "__main__":
    main()
