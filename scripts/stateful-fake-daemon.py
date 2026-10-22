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
import uuid

VERSION = 1
MAX_FRAME = 1024 * 1024
LOCK = threading.RLock()
SUBS = set()
WRITE_LOCKS = {}
MY_HANDLE = {"kind": "email", "value": "me@example.invalid"}
PEOPLE = {"alice@example.invalid": "Alice", "bob@example.invalid": "Bob"}


def frame(obj):
    data = json.dumps(obj, separators=(",", ":"), ensure_ascii=False).encode()
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
    return {"conversations": chats, "messages": []}


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
                if op == "fixture_append_message":
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
                            response = {"result": "history", "messages": copy.deepcopy(messages[-body.get("limit", len(messages)):])}
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
