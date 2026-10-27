# Outbound edit and unsend: engine gate

Status: the structural API gate passes. No outbound edit or unsend is implemented,
and no live mutation was sent. This record answers the C2 gate; it is not a
capability claim.

## Question

Does the pinned OpenBubbles engine accept edit and unsend message types through
the same `api.newMsg` and `api.send` path the sidecar already uses for text?

## Pinned upstream evidence

Pinned tag: OpenBubbles v1.15.0+205, commit
`852c9910f456492f7f840ac4ff2a8bad7a1ee8c8`.

`lib/services/rustpush/rustpush_service.dart` at that commit:

- `unsend` (line 1183) builds
  `api.Message.unsend(api.UnsendMessage(tuuid: msgObj.guid!, editPart: part.part))`
  through `api.newMsg` and calls `sendMsg`.
- `edit` (line 1201) builds
  `api.Message.edit(api.EditMessage(tuuid: msgObj.guid!, editPart: part,
  newParts: await partsFromBody(text)))` through `api.newMsg` and calls `sendMsg`.
- `sendMsg` (line 409) calls
  `api.send(state: ..., local: ..., msg: ...)` and returns the engine's
  `stillRunning` flag.
- `canEditUnsend()` (line 1233) returns true.

The UI gates these actions on `message.isFromMe`, a non-temporary guid, and no
scheduled date (`message_popup.dart` lines 1281 to 1290). Edit additionally
requires a nonempty text part and an age check (`message_holder.dart` line 161;
`message_popup.dart` line 1118). Unsend is invoked with one selected `part`
(`message_popup.dart` line 1106), not with a whole-message flag.

## Local binding evidence

`ext/bloom-engine-sidecar/dart/lib/upstream/api/api.dart` at the same tag exposes
exactly those shapes:

- `EditMessage(tuuid, editPart, newParts)` at line 2259.
- `UnsendMessage(tuuid, editPart)` at line 5350.
- `Message.edit(EditMessage)` and `Message.unsend(UnsendMessage)` at lines 3265
  and 3266.
- `api.send` at line 662 takes any `MessageInst`.

The sidecar's existing send case builds a `MessageInst` and calls `api.send`; that
boundary is type-agnostic. A new command would only need to build the mutation
`Message` and reuse the send-confirmation correlation.

A local synthetic test constructs both payloads and checks the target guid, part
and replacement text:

```sh
cd ext/bloom-engine-sidecar/dart
dart test test/outbound_mutation_test.dart
```

## Gate result

- **Edit: pass, structurally.** The pinned upstream app sends an `EditMessage`
  through the same engine path Bloom uses.
- **Unsend: pass, structurally, but part-scoped.** The API unsends one
  `editPart`. It has no whole-message flag. A whole-message unsend is a single
  call only when the stored message has one part; a multi-part message would need
  one unsend per part.

## Prerequisites before implementation

1. **Part identity for Bloom-sent messages.** Inbound messages, including
   `from_me` messages sent from another device, already carry an engine part map
   (`engine.rs` mapping, `message_part_maps`). Messages sent by Bloom's own
   `send` and `send_attachment` handlers do not. `load_mutation_target` returns
   `Unsupported` when the map is absent, so both outbound paths must persist a map
   (for example `[Some(0)]` for the single sent part) before an edit or unsend can
   be applied locally.
2. **Single-part unsend only.** `apply_unsend` accepts only a target whose part
   ids all equal the target part. A multi-part message must be refused, not
   partially unsent; partial multipart unsend remains out of scope. This matches
   the engine's part-scoped API.
3. **Text-only edit.** Build `newParts` from nonempty text only, target a stored
   text part, and leave attachments, mentions and unsupported parts unchanged.
   The engine's grouping of `newParts` against `editPart` is inferred from the
   inbound edit application, not observed live.
4. **Mutation confirmation.** The engine's `sendConfirm` push path is proven for
   normal sends, not for edits or unsends. The sidecar must not report success
   from the command return alone; a mutation needs its own correlated
   confirmation or an explicit uncertain outcome.
5. **Guards and capability.** Offer edit/unsend only for own, accepted messages
   with a known engine guid and part map; never for temporary or uncertain sends.
   Keep `Capabilities.edits` and `unsend` false until the outbound path exists.
   No automatic retry.

## What remains unverified

- Remote acceptance of an edit or unsend, and the engine event that echoes it.
- The installed library's tag and ABI, and whether a mutation confirmation is
  emitted at all.
- Exact `EditMessage.newParts` grouping and multi-part behavior.
- Any live delivery; this gate is source and binding evidence only.

## Decision

The gate passes structurally. Implementation is a separate unit and is not
started. Because live outbound sends are currently deferred in this setup, keep
outbound edits and unsends unimplemented until outbound delivery has a working
live boundary. If implementation resumes, restrict unsend to single-part messages
and edits to text-only parts, and add the outbound part map first.
