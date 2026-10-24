# Durable drafts plan

Status: design only. Drafts are session-only today; nothing here is implemented.

## Rules

- A draft stays bound to its owner: transport plus exact handle, and to its conversation when one exists. Restoring must never move text to another recipient.
- Nothing is sent, retried, or confirmed by a restore. A draft saved while a send outcome was uncertain returns marked uncertain.
- Drafts live only in the daemon's encrypted store. Never in `tui.json`, logs, diagnostics, or plaintext temp files.
- Retention is bounded and explicit: one row per owner and conversation, an age limit, and a byte cap. Clearing deletes the row.
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

1. Schema, store functions, and tests: save, load, revision-rejected stale save, delete tombstone, expiry purge, per-owner cap, corrupt-row delete. No IPC.
2. IPC v7 operations, dispatch validation, and round-trip tests.
3. TUI debounce, restore, clear, and actual-PTY coverage across recipient switches and restart.
4. Retention and privacy review, then docs and acceptance.

## Exclusions

No cross-device sync, no plaintext export, no automatic send or retry, no draft merging between owners, no rich content beyond what the composer already supports.
