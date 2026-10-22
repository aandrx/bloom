# Bloom system design

**Status: current implementation summary with explicitly labeled future work.** This replaces the early direct-transport architecture proposal. See [handoff](handoff.md), [status](status.md), and the [roadmap](implementation-plan.md).

## Purpose

A trustworthy keyboard-first Linux messenger with a Bloom-owned terminal UI, restrained visual hierarchy and local encrypted storage. The current transport reuses the user-installed OpenBubbles engine and registration. No Bloom-hosted relay is required. Independent registration remains blocked; no one-time-Mac bootstrap or renewal guarantee is established for that independent route.

Safety priorities: exact recipient identity, preserved drafts, persistence before inbound fanout, no automatic uncertain-send retries, explicit capability boundaries, and fail-closed key handling. Static desktop/mobile mockups are references, not implemented clients.

## Current architecture

```text
bloom launcher
  starts bloomd if needed and runs bloom-tui

bloom-tui (Rust/ratatui)
  App + composer + renderer + hitboxes + diagnostics
       |
       | authenticated local Unix socket, versioned bounded frames
       v
bloomd (Rust)
  IPC dispatch + Service + session/supervisor
  encrypted SQLite store + keychain-backed key
       |
       | child stdio: version-1 newline-delimited JSON
       v
headless Dart sidecar
  pinned generated Flutter Rust Bridge bindings
       |
       | runtime-loaded library, exclusive existing registration
       v
user-installed OpenBubbles engine
```

The TUI does not open the database, engine or credentials. The engine library stays external and is not linked into MIT Rust crates. CI requires the normal daemon dependency graph to remain free of `rustpush`; generated public bindings are pinned and attributed separately. A process boundary does not by itself settle redistribution rights for external dependencies.

## Workspace

- `bloom-domain`: transport-scoped IDs, handles, conversations, messages/parts, delivery/session states and capabilities.
- `bloom-ipc`: wire types, bounded framing, client and subscription APIs.
- `bloom-transport`: transport abstraction and synthetic transport/conformance support.
- `bloom-secrets`: secret-store abstraction and OS keyring integration.
- `bloomd`: engine adapter, encrypted store, IPC server, service, supervisor and daemon executable.
- `bloom-tui`: UI/client library plus `bloom-tui` and installed `bloom` executables.
- `bloom-core`: retained legacy foundation; current TUI uses Bloom IPC, not Matrix.
- `spike/matrix`: excluded synthetic research proof, not product architecture.

There is no implemented GPUI client, Mac bridge or distributed independent direct adapter in this layout. Historical proposals must not be mistaken for existing crates.

## Identity and sends

IDs are scoped by transport. Display titles/nicknames do not change routing identity. Direct-chat reconciliation requires exact, justified normalized participant identity; groups or ambiguous identities are not guessed into a merge. Aliases preserve stable local identity/history and preferences; tests exercise selection and draft migration.

Starting a conversation reuses a validated matching direct conversation, never a group that happens to contain the recipient. Engine handles are normalized idempotently (`mailto:`/`tel:`) and passed without double prefixes. Group text targets all recipients only when the sidecar advertises recipient-list support; generic capability declarations are conservative. Non-text outbound content is rejected rather than silently stripped.

Transport/IPC acceptance is not delivery. Delivery labels come from events. TUI attempt IDs and accepted-message-ID correlation prevent late results from overwriting new composition or navigating to a guessed new chat. Failure snapshots remain in memory and recovery is explicit; ambiguous timeouts/disconnects require checking the conversation before retrying. Durable encrypted send intents and restart-safe late confirmation reconciliation remain future work.

## Persistence and event flow

The real schema is versioned and stores serialized conversation/message domain data in encrypted SQLite tables with ordering indexes; `apps/bloomd/src/store/mod.rs` defines it. Do not implement migrations from an obsolete normalized-table proposal. Domain attachment/reaction fields do not establish attachment storage or rich-message workflows.

The database key comes from the secret store. Provision a key only for a new database. An existing database with a missing key returns `ExistingDatabaseKeyMissing`; never replace its key, fall back to plaintext, or delete the database as recovery.

Engine events enter the daemon's bounded event path, are persisted, and then fan out to IPC subscribers. Duplicate messages upsert by message ID. After IPC reconnect the client subscribes/resnapshots and preserves revision ordering; visible history reconciles every five seconds. The TUI opens the latest 200 stored messages and asks for the previous keyset page when the reader reaches the oldest loaded row. Paged history is kept in the session up to a bounded budget and survives reconciliation for that chat; a chat the user never paged still replaces wholesale. Remote historical import is absent.

Outbound sends first await engine acceptance, then persist the outgoing message as Sent and publish it before returning the accepted ID. There is no durable pre-send intent. A local persistence failure after remote acceptance cannot undo that send and must not justify automatic retry.

Conversation metadata refresh is a separate operation with a correlated result. `/sync` cannot recover a message absent from the local store. The UI's two-second event-quiet activity indicator is not a daemon-defined archive completion signal.

## Lifecycle and concurrency

The daemon claims its private IPC socket before starting the engine child. Peer identity, private socket parent and bounded protocol framing are enforced. One owner per registration is a deployment requirement; using different sockets is not permission for multiple owners.

IPC carries an explicit protocol version byte. It is currently 2: version 2 adds an optional keyset `before` cursor to history reads, so a client can page backwards from a known message without offsets. A peer that disagrees on the version is rejected when the frame is read rather than having fields silently ignored; the launcher and the documented `cargo install` steps build both binaries from the same tree. Because the launcher reuses a daemon that is already listening, an upgraded TUI talking to a daemon left over from an earlier build fails with "unsupported IPC protocol version" until that daemon is restarted. The TUI does not send a cursor yet, so user-visible history is still the latest stored page.

The engine adapter rejects malformed/oversized output, fails pending waiters on stream failure and reaps the old child. Clean shutdown drains forwarding and persistence within bounds; it does not simply abort the store consumer. The launcher grants a 30-second daemon stop budget to cover drain stages, then forces termination/reaps if necessary. Signals during startup and active TUI operation clean up owned children. Pre-existing daemons are not terminated by a reusing launcher. Original terminal modes are restored on launcher exit.

There is no automatic engine-generation recovery after child failure. IPC reconnect is not engine restart. Future recovery must reap the old owner before creating a fresh generation, reject stale-generation events and never replay uncertain sends automatically.

## UI architecture

`App` owns interaction state, draft ownership and recovery. Session-only drafts preserve text, cursor and literal-paste provenance; new-chat keys include recipient context. Transient panels suspend composition and restore only the correct owner. Preferences store display settings/labels, not plaintext drafts.

`composer.rs` wraps extended graphemes and maps cursor geometry. `ui.rs` returns hitboxes from the same frame it draws. The terminal loop owns a bounded session-only transcript layout cache; unchanged layouts reuse exact ratatui geometry and paint only visible logical lines, without mutating App. `main.rs` routes input and asynchronous events/results through a coalescing `RedrawScheduler` capped at 120 FPS; animation ticks are 33 ms. See [rendering internals](tui-internals.md).

## Security and capability boundaries

- Never log message bodies, recipient IDs, raw protocol payloads or secrets in TUI diagnostics. Those logs are private, bounded and allowlisted.
- Launcher daemon logs are private, but external engine stderr is not governed by the TUI allowlist; review it before sharing.
- Engine startup reads/writes registration and consumes pushes. Only synthetic fixtures run by default. Live tests require fresh explicit authorization and safe durable capture.
- No engine binary vendoring or generated-binding hand edits. Matching engine version/content hash is required.
- No Bloom-owned authentication, registration or account recovery. Current state labels do not prove renewal support.
- No rich-message UI, message-content search, unread counter or complete archive support. Keep product claims aligned with end-to-end implementation.

## Future surfaces and transports

GPUI is deferred until messaging reliability gates pass. It should be another daemon client, with shared domain/client concepts extracted deliberately rather than copying ratatui-specific App state unchanged. Mac relay and independent transport remain separate proposals. [IDS validation gate](ids-validation-gate.md) defines what would unlock independent registration; installed-engine success does not unlock it.

## Verification

[Handoff](handoff.md) supplies repeatable commands; [iteration review](iteration-review.md) records completed fixes and the parent critique. Unit/render tests, fake-sidecar daemon tests, actual PTY flows and actual launcher subprocess tests cover different boundaries. None substitute for current authorized live send/receive/restart/renewal evidence. Historical live runs remain in [engine sidecar evidence](engine-sidecar.md).
