# Historical message ingestion plan

Status: design only. Blocked at the engine boundary. No code, protocol change, or live engine start is authorized or implemented. Design review found three issues, all corrected here: the probe is not read-only, the advertised `capabilities.history` flag is wrong, and two citations were off.

## What exists today

- The sidecar protocol exposes the `send`, `sync_conversations`, and `shutdown` commands, plus `ready`, `inbound_message`, `send_confirmation`, `sync_result`, `shutdown_ack`, and `conversation` frames (`ext/bloom-engine-sidecar/dart/lib/protocol.dart:30,57,168,201,219,238,252,258,263`).
- The `ready` frame advertises `capabilities.history` from `config.syncChats` (`protocol.dart:203-215`, `bin/bloom_engine.dart:256-261`). That flag is currently wrong: it is true whenever chat sync is enabled, while no history producer exists. Do not trust it for import decisions until a real producer backs it; a scoped fix is recorded below.
- `history_message` and `history_complete` are declared protocol frames with no producer or consumer semantics (`protocol.dart:274,287`; `bin/bloom_engine.dart:452` emits only conversation frames). They are placeholders, not a contract.
- `sync_chats` returns paginated conversation metadata, not message bodies (`upstream/api/api.dart:962`; `bin/bloom_engine.dart:403-463`). `sync_conversations` maps that to a limited count (`apps/bloomd/src/engine.rs:257`).
- The engine transport's `history` method returns `Unsupported` (`engine.rs:375-382`).
- The engine's local ObjectBox history is not reachable headlessly because its models import Flutter (`docs/engine-sidecar.md`, item 17).
- Inbound identity is transport plus the engine payload `id` (`engine.rs:1006-1011,1050`); the store keys on that `MessageId` and upserts by conversation plus serialized message (`store/ingest.rs:103`, `store/mod.rs:291`, `store/messages.rs:30-51`).
- Documented prohibitions: do not assume archive availability, pagination-token lifetime, identity, edit/unsend/tombstone behaviour, or resume support; do not dedupe by body or time; do not export plaintext archives; do not claim completeness without a live archive comparison; only one registration owner (`docs/implementation-plan.md:39-45`, `docs/engine-sidecar.md`).

## Gate

Implementation starts only when all of these are established, with evidence:

1. A lawful source of message bodies that does not require a Flutter UI or a second registration owner. `capabilities.history` is not evidence: it is currently derived from chat sync and has no producer.
2. Stable identity for imported rows matching live ingestion, so upsert deduplicates rather than duplicating.
3. Ordering with a defined tiebreak, and pagination with documented cursor lifetime, resume, and cancellation behaviour.
4. Defined edit, unsend, reaction, and tombstone semantics, including what an imported deleted message should become.
5. An explicit per-conversation completeness signal with a stated meaning.
6. Resource bounds: page size, total rows, memory, and a stop condition.

None of these is satisfied today. The first step is an authorized live observation against a sample conversation, comparing candidate historical ids and timestamps with live ingest for the same messages. It is not read-only: restoring the engine rewrites `hw_info.plist` and `id.plist`, opens the push connection, and constructs CloudKit (`docs/engine-sidecar.md:59-62`). It therefore needs explicit approval, one registration owner, and the same care as any live start. Without an id match, the design stops there.

## Design shape, when the gate passes

- **Sidecar.** Add one command with explicit semantics instead of reusing the stubs: request one conversation page with a cursor and limit, and answer with mapped messages plus a page terminator carrying `reached_start` and any resume token. An explicit `unsupported` answer is required before the gate passes.
- **Daemon.** One bounded import worker, one conversation at a time, cancellable on shutdown. Persist the cursor in the encrypted store, never plaintext preferences. A page is committed only after its messages are persisted, so a crash resumes from the last confirmed cursor.
- **Mapping.** Reuse the existing payload mapping so parts, ids, and omitted fields match live events. Missing or unstable ids are skipped and counted, never fabricated. Unknown parts stay `Unsupported`.
- **Dedupe and order.** Upsert on transport plus raw id. Keep existing ordering rules. Imported rows never advance local read markers and never trigger sends.
- **Presentation.** The TUI distinguishes "not imported", "partially imported", and "complete per source signal". It must not show an archive-complete claim from a placeholder frame or a page count alone.
- **Privacy.** Imported bodies and metadata stay in the encrypted store. Logs carry counts and outcomes only.

## Phases

1. Authorized sample observation that proves the source, identity match, ordering, and completeness signal. This starts the engine and mutates registration files; it is not read-only and needs explicit approval and one registration owner.
2. Sidecar command plus Dart tests for pagination, cursor lifetime, cancellation, and explicit unsupported.
3. Daemon worker, encrypted cursor, synthetic sidecar fixture with duplicate pages, interruptions, and tombstones.
4. TUI progress and limit states with actual-PTY coverage.

## Explicit exclusions

No direct ObjectBox reads, no plaintext archive export, no dedupe by body text or timestamp, no fabricated ids, no completeness claim without a source signal, no automatic resend, no second registration owner. This plan does not change live accounts or start an engine.

## Open defect outside this design

`capabilities.history` is advertised from `config.syncChats` with no history producer (`bin/bloom_engine.dart:256-261`). Any client that trusts it is misled. A scoped unit should set that capability to false until a real producer exists and add a Dart protocol test. That unit is separate from the ingestion work and has not been started.
