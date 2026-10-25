# Durable drafts plan

Status: phase 1 store support is implemented and accepted. Drafts remain session-only to users until IPC and TUI phases are complete.

## Rules

- A draft stays bound to its owner: transport plus exact handle, and to its conversation when one exists. Restoring must never move text to another recipient.
- Nothing is sent, retried, or confirmed by a restore. A draft saved while a send outcome was uncertain returns marked uncertain.
- Drafts live only in the daemon's encrypted store. Never in `tui.json`, logs, diagnostics, or plaintext temp files.
- Retention is bounded and explicit: one row per exact owner, 65,536 UTF-8 body bytes, 30 days, and at most 100 active rows or tombstones per exact transport/account in this database. Clearing writes a tombstone; expiry or cap eviction eventually removes it, so stale-write protection is bounded to the retained tombstone lifetime.
- Failures fail closed: an unreadable or corrupt row is skipped with a notice and deleted, so later saves to that key are not blocked forever.
- A save must be newer than what the store holds. Each draft carries a client revision; the store rejects a revision that is not greater, and a delete leaves a tombstone with a revision so a late save cannot resurrect it.

## Why the daemon owns it

The store, its key, and the schema belong to `bloomd`. The TUI has no database handle, and adding a second encrypted file plus a second key would create two privacy surfaces and two migration paths. Drafts therefore go through the existing keyring-backed store, which requires a protocol addition.

## Data

A `drafts` table, additive under the next schema version:

- Key columns: `conversation_id` (nullable) and `new_chat_address` (nullable), with the constraint that exactly one is set. `ConversationId` already carries its transport, and the new-chat address is the raw input the TUI stores. A new-chat draft with no address is not persisted; it stays session-only, matching `DraftOwner::NewChat(None)` today.
- `body`, `cursor`, `paste_provenance` flag, `uncertain` flag, `revision`, `deleted` tombstone flag, `updated_at`.
- One logical row per key, including tombstones. Saves replace it when the revision increases. Loads purge rows older than the age limit and enforce a per-owner count cap, not a global one.

IPC v7 adds `draft_save`, `draft_load`, and `draft_delete` with typed responses. Version changes follow the existing strict-rejection rule, so an old daemon cannot silently ignore drafts. No draft content appears in error text.

## Behaviour

- The TUI keeps a revision per draft key, increments it on each debounced save, and sends it with the save. A rejected stale revision is ignored silently; the newer content already won.
- The TUI saves on a debounce after edits and on focus or modal changes, and restores when a conversation is selected with an empty composer.
- A newer in-memory draft always wins over a stored one.
- Save failures keep the session draft and surface the existing settings-save style error; they do not block typing or sending.
- Sending or cancelling a draft deletes its row. Quitting leaves the row.
- Restart restores drafts; an uncertain one shows the existing uncertainty wording.

## Phases

1. Schema, store functions, and tests: done. An additive table under schema v1 stores only in the encrypted daemon database. Exact conversation/new-chat keys, grapheme cursor validation, revision-rejected stale saves, delete tombstones, corruption tombstoning, 30-day expiry, a deterministic 100-row per-transport cap, encrypted reopen, and non-destructive existing-v1 upgrade are covered. Save/delete prune transactionally and protect their own row from cap eviction; load captures the requested revision floor before pruning. 15 focused and 200 daemon library tests, strict daemon Clippy, formatting, source guard, and independent review pass. No IPC or TUI behavior changed.
2. IPC v7 operations, dispatch validation, and round-trip tests: done. Strict v7 framing adds typed draft save/load/delete DTOs and client methods. The daemon converts exact owner identities to store keys, supplies wall-clock timestamps, maps invalid drafts to `InvalidRequest`, returns stale/applied outcomes and corruption notices, and never logs draft bodies or keys. Both synthetic fixtures implement v7 draft parity; stateful drafts survive restart. Current evidence: 36 IPC tests, 206 daemon library tests, 68 fixture tests, strict scoped Clippy, formatting, source guard, and diff check. The TUI does not call these operations yet, so drafts remain session-only to users.
3. TUI debounce, restore, clear, and actual-PTY coverage across recipient switches and restart: done. `apps/bloom-tui/src/drafts.rs` provides a pure 300 ms coordinator with one global in-flight operation, immediate activation loads, content baselines, save-before-delete ordering, bounded delete retry, a three-attempt stale cap, failure suppression, revision-exhaustion handling, and request/generation correlation. Invalid local content holds the previous stored draft instead of implying deletion, and a stale delete reload cannot restore cleared content. The main loop observes owner/modal changes every 100 ms, restores only into an empty exact owner, surfaces a generic non-body error, and runs a bounded drain on quit. A submitted body is parked durably until acknowledgment; success deletes it, a deferred failure keeps it and its uncertainty wording, and parking a later send preserves an earlier deferred copy. A quit that cannot settle a send marks the restored draft uncertain instead of implying delivery.

   Actual-PTY coverage proves debounced restart restoration only to the same chat, cross-chat isolation, no resurrection after a successful send, cancel/deletion, failed-save retention with a visible generic error, and rejection/retry without duplicated bodies. Full TUI tests, strict TUI Clippy, formatting, source and diff checks pass, with 13 input and 8 messaging PTY cases and the 68-case fixture suite. Synthetic fixture state is plaintext test data, not encryption evidence; no live account was used.
4. Retention and privacy review, then docs and acceptance.

## Exclusions

No cross-device sync, no plaintext export, no automatic send or retry, no draft merging between owners, no rich content beyond what the composer already supports.
