# Reaction application plan

Status: design only. Reactions are decoded by the codec but dropped by the daemon today.

## Wire evidence

`react` carries `to_uuid`, `to_part`, a wrapper with `kind: react` and `enable`, and a nested reaction that is a plain string for built-in kinds or a string payload for emoji (`payload_codec.dart:118-145`, now covered by a codec test). The reaction body has no sender. Whether the surrounding `inst` envelope carries one is unverified: review found `engine.rs` still declares `reactions: false`. The first unit must prove a real react envelope carries a sender from the same field normal messages use. If it does not, this design stops and the capability stays false.

## Rules

- A reaction never creates a message. If `to_uuid` does not resolve to a stored row for the same transport, the event is skipped and counted.
- Applying a reaction may change only the `reactions` field. A store test must prove `parts`, `sender`, `created_at`, `delivery`, `from_me`, and conversation identity are byte-identical afterwards, so this path cannot rewrite content or timestamps the way an upsert could.
- Identity is the target message plus sender plus kind plus optional part. Repeating an add is a no-op. `enable:false` removes exactly that tuple.
- Unknown reaction kinds are skipped and counted, never stored as free text.
- Ordering is deterministic: payload time when present, otherwise arrival time, then sender and kind as tiebreaks.
- No new IPC fields. Subscribers receive the existing message-update event with the updated message, so old clients are unaffected.
- No plaintext copies, no logging of reaction content, and no automatic send or retry.

## Changes

- **Domain.** Add `part: Option<u32>` to `Reaction`, serialized with a default so existing stored messages still load. Additive only.
- **Transport event.** Add `ReactionChanged { conversation_id, message_id, sender, kind, active, part, at }` to the transport event enum plus an explicit `Debug` arm that redacts kind and sender. It carries no message body.
- **Mapper.** `engine.rs` maps a decoded `react` payload into that event. The target id is transport plus `to_uuid`; a missing or malformed id skips with a reason, matching the existing skip accounting.
- **Store.** `apply_reaction` runs in one transaction: load target, reject if absent, add or remove the tuple, re-sort, write back. Returns whether anything changed so duplicate deliveries stay idempotent.
- **Service.** Persist the event through `apply_reaction`, then publish `MessageUpdated` with the stored message. `persist_message` needs a conversation, which this event does not carry, so the service resolves it from the store by the target's canonical conversation id and skips with a count if absent. Persistence failure marks the event failed exactly like other update events.
- **TUI.** Render aggregated reaction kinds and counts under the message, with the sender list available on inspection. Read-only in this phase.

## Phases

1. Domain field plus serde default test.
2. Mapper test with a synthetic react payload reaching a typed event, including skip accounting for an unknown target id.
3. `apply_reaction` store tests: add, repeat-add no-op, remove, wrong transport, missing target, part handling, ordering.
4. Service wiring and a subscriber-visible update test.
5. TUI rendering at 24, 44, and 100 columns with long emoji sets.
6. Fixture-side parity in the synthetic daemons.

## Exclusions

No reaction sending, no reaction notifications, no cross-device sync, no reaction on unloaded history beyond what the store already holds, and no inferred sender identity.
