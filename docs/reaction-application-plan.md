# Reaction application plan

Status: inbound reactions are implemented and accepted at the synthetic sidecar/store/real-TUI boundary. Outbound reactions are intentionally unsupported and the transport capability remains false.

## Wire evidence

`react` carries `to_uuid`, `to_part`, a wrapper with `kind: react` and `enable`, and a nested built-in reaction string (`payload_codec.dart:118-145`). The surrounding envelope carries the sender in the same field as normal messages. The shared `envelope_sender` extractor has direct tests for a present, missing, and malformed sender; the mapper accepts only the six evidence-backed built-in kinds and emits `ReactionChanged`. Emoji/custom reactions remain unsupported.

## Rules

- A reaction never creates a message. If `to_uuid` does not resolve to a stored row for the same transport, the event is skipped and counted.
- Applying a reaction may change only the `reactions` field. A store test must prove `parts`, `sender`, `created_at`, `delivery`, `from_me`, and conversation identity are byte-identical afterwards, so this path cannot rewrite content or timestamps the way an upsert could.
- Identity is the target message plus sender plus kind plus optional part. Repeating an add is a no-op. `enable:false` removes exactly that tuple.
- Unknown reaction kinds are skipped and counted, never stored as free text.
- Ordering is deterministic: timestamped reactions first, missing timestamps last, then kind, serialized sender, and part as tiebreaks.
- No new IPC fields. Subscribers receive the existing message-update event with the updated message, so old clients are unaffected.
- No plaintext copies, no logging of reaction content, and no automatic send or retry.

## Changes

- **Domain.** Add `part: Option<u32>` to `Reaction`, serialized with a default so existing stored messages still load. Additive only.
- **Transport event.** Add `ReactionChanged { conversation_id, message_id, sender, kind, active, part, at }` to the transport event enum plus an explicit `Debug` arm that redacts kind and sender. It carries no message body.
- **Mapper.** `engine.rs` maps a decoded `react` payload into that event. The target id is transport plus `to_uuid`; a missing or malformed id skips with a reason, matching the existing skip accounting.
- **Store.** `apply_reaction` runs in one transaction: load target, reject if absent, add or remove the tuple, re-sort, write back. Returns whether anything changed so duplicate deliveries stay idempotent.
- **Service.** Persist the event through `apply_reaction`, then publish `MessageUpdated` with the stored message. `persist_message` needs a conversation, which this event does not carry, so the service resolves it from the store by the target's canonical conversation id and skips with a count if absent. Persistence failure marks the event failed exactly like other update events.
- **TUI.** Render aggregated reaction kinds and counts under the message. No sender inspection or interaction is implemented; the display is read-only.

## Phases

1. Domain field plus serde default test: done. `Reaction.part` round-trips and legacy rows without the field load as `None`; workspace all-target tests, strict Clippy, formatting, and source checks pass.
2. Mapper test with a synthetic react payload reaching a typed event, including skip accounting for an unknown target id: done. The mapper now emits `TransportEvent::ReactionChanged` with a redacting `Debug`, accepts only six evidence-backed kinds (`heart`, `like`, `dislike`, `laugh`, `emphasize`, `question`), and skips a bare emoji string, an object reaction, a nonempty unknown kind, and a missing target. Review caught that any string was previously accepted; that is fixed. The temporary warning-only service branch used during this phase was removed when phase 4 wired persistence.
3. `apply_reaction` store tests: done. Add, repeat-add no-op, remove, remove-missing, missing target, part identity, ordering, and a preservation test over every non-reaction field. Review found ordering was not total because `Handle`'s Debug is redacted; it now compares the sender's serialized form, which includes kind and value. The temporary dead-code allowance used before service wiring is gone.
4. Service wiring and subscriber-visible update tests: done. `ReactionChanged` resolves the canonical conversation, applies the reaction, publishes exactly one `EventKind::Update` only when something changed, and writes and publishes nothing when the conversation or target is missing or belongs to another conversation. Alias-named events apply to the canonical message and publish the canonical id. The transport capability remains false because this inbound path does not add an outbound operation.
5. TUI rendering at 24, 44, and 100 columns: done. One dim line after the body aggregates kinds alphabetically, for example `· heart 2 · like 1`, with no senders or timestamps and no change for unreacted messages. The rebuilt PTY reads transcript-only text, so a sidebar preview cannot satisfy it. Review found no defect; residual cosmetic risks are a summary under a continuation body without its own byline and an own-message surface that ends mid-row because the summary adds no trailing padding.
6. Fixture parity: done. A seeded three-reaction message, with part set and unset, survives history, context, whole-state save, and restart unchanged.

## Outbound gate: fail

The sidecar accepts only `send`, `sync_conversations`, and `shutdown` commands (`protocol.dart:128-166`). `SendCommand` carries text only, its handler always builds a normal text message, and the daemon's send path is text only. Bloom does not need outbound reactions for this project scope, so no reaction command or send API will be added. `Capabilities.reactions` stays false; inbound decoding and display remain available without advertising an outbound operation.

## Exclusions

No reaction sending, no reaction notifications, no cross-device sync, no reaction on unloaded history beyond what the store already holds, and no inferred sender identity.
