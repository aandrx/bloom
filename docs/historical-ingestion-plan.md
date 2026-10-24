# Historical message ingestion plan

Status: design only, and blocked at the engine boundary. No ingestion code exists, no live engine has been started, and none of the ingestion gate evidence exists. One small sidecar correctness fix was made independently: `capabilities.history` is now hard-wired false, with a Dart protocol test. The Dart toolchain is not installed locally, so that test was not run here and relies on CI.

## What exists today

- The sidecar protocol exposes the `send`, `sync_conversations`, and `shutdown` commands, plus `ready`, `inbound_message`, `send_confirmation`, `sync_result`, `shutdown_ack`, and `conversation` frames (`ext/bloom-engine-sidecar/dart/lib/protocol.dart:30,57,168,209,242,253,268,281,286`).
- The `ready` frame advertises `capabilities.history` from the `historyCapability` constant (`protocol.dart:199-206,230-241`), which is false because no command produces history (`bin/bloom_engine.dart:252-259`). Chat sync returns conversation metadata only and no longer enables this flag.
- `history_message` and `history_complete` are declared protocol frames with no producer or consumer semantics (`protocol.dart:297,310`; `bin/bloom_engine.dart:455` emits only conversation frames). They are placeholders, not a contract.
- `sync_chats` returns paginated conversation metadata, not message bodies (`upstream/api/api.dart:962`; `bin/bloom_engine.dart:416-458`). `sync_conversations` maps that to a limited count (`apps/bloomd/src/engine.rs:257`).
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

## Capability flag fixed

`capabilities.history` was advertised from `config.syncChats` although no history producer exists. The capability now comes from `historyCapability = false` through `sidecarReadyFrame`, the only ready-frame constructor the bin calls, so a revert to chat sync would require editing that function. The protocol test asserts the emitted frame, not a locally rebuilt one. Enabling it requires a working history command and tests, not chat sync alone. The Rust fake sidecar used by daemon integration tests still advertises `history: true`; the daemon ignores that field, and changing the fixture is left to a separate test-hygiene unit.
