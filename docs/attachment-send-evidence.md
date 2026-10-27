# Outbound attachment send: engine gate

Status: implemented against synthetic boundaries, with a live self-pair upload test.
The structural engine gate passes, the daemon and sidecar paths are covered by Rust
and Dart tests, and the real TUI is covered by a PTY test. The live self-pair test
proved the engine upload and showed that the engine rejects a self-target send
with `no_valid_targets`; no third party was contacted and live delivery remains
unproven.

## Question

Can the pinned OpenBubbles engine take a local file and send it as an iMessage
attachment through the bridge the sidecar already loads?

## Pinned upstream evidence

The sidecar's bindings come from OpenBubbles v1.15.0+205, commit
`852c9910f456492f7f840ac4ff2a8bad7a1ee8c8` (`ext/bloom-engine-sidecar/ATTRIBUTION`).
At that commit, `lib/services/rustpush/rustpush_service.dart`,
`RustPushBackend.sendAttachment`, does this:

1. Streams `api.uploadAttachment(aps: state.conn, path: <local path>, mime: ...,
   uti: ..., name: ...)` and waits for the event whose `attachment` field is set.
2. Calls `api.saveAttachment` to serialize that `api.Attachment` for its local
   database.
3. Builds `api.newMsg(... api.Message.message(api.NormalMessage(parts:
   api.MessageParts(field0: [api.IndexedMessagePart(part_:
   api.MessagePart.attachment(attachment))]), ...)))`.
4. Sends it with `api.send(state.client, state.localBroadcast, msg)`, the same
   call Bloom's text send already uses.

The upstream function also supports rich-link objects, replies, effects, subjects
and SMS routing. None of those are part of the minimal slice below.

## Local binding evidence

The vendored bindings at the same tag expose the same surface:

- `uploadAttachment` returns `Stream<TransferProgress>`; `TransferProgress` has
  `int prog`, `int total`, and `Attachment? attachment`.
- `saveAttachment` takes an `Attachment` and returns a serialized `String`.
- `MessagePart.attachment(Attachment)` and `Message.newMsg` are present.
- `api.send(ArcImClient state, ArcSenderPushMessage local, MessageInst msg)` is
  already used by `bloom_engine.dart` for text.

`Attachment` carries `aType`, `utiType`, `mime`, `name`, `part_` and `iris`.
The uploaded value is `AttachmentType.mmcs(MMCSFile)`, so Bloom does not construct
MMCS material itself.

## What the gate proves

- The engine bridge accepts a caller-supplied local path for upload.
- The upload stream yields a sendable attachment object.
- The same `newMsg` and `send` path the sidecar already relies on accepts a normal
  message whose only part is an attachment.

## What it does not prove

- That the sidecar upload-and-send composition itself runs correctly: it calls the
  loaded engine, so only its command decoding, path validation, MIME inference and
  error framing are unit-tested. The upload stream and `api.send` composition are
  proven by the pinned upstream source, not by a local execution.
- That this account and server accept the send, or how the sent message is echoed
  back.
- Upload limits, time limits, MIME/UTI requirements, or failure semantics.
- That the installed engine library matches the pinned tag. The sidecar resolves
  it at runtime and validates the bridge content hash at startup; a mismatch
  fails before any command runs.
- That a synthetic payload is remotely delivered. Only an authorized live send
  can establish that, and it must not be turned into an automatic retry.

## Implemented slice

One file, no caption, no reply, no scheduling, no multiple attachments.

- TUI: `/attach <absolute path>` sends to the open chat. The notice and every
  later message show only the basename; the source path is cleared with the
  command and never painted. A relative path, a missing selection, a concurrent
  send or a pending text send is refused before any request.
- Daemon: `send_attachment` validates absolute, no `.` or `..` components, not a
  symlink, regular, non-empty and at most 25 MiB, resolves the conversation and
  recipients from the store, and calls the engine once. The source path is not
  persisted and not logged.
- Sidecar: `attachment_send` validates the path again, infers MIME and UTI from
  the extension with generic fallbacks, uploads with `deskState.conn`, sends one
  `MessagePart.attachment`, and reports through the existing send-confirmation
  path. Failures log a bounded code, never the path or an exception string.
- Store: after acceptance, one outbound message is recorded with an attachment
  part carrying name, byte length, no MIME (Bloom has not independently classified
  it) and a local id namespaced to the message. Content is not copied.
- IPC v9: `SendAttachment { conversation_id, path }` reuses the `Send` response.
  The client waits 250 s; the daemon engine waits 240 s so it can report a timeout
  first. A protocol timeout is shown as an uncertain outcome.
- Tests: Rust IPC round-trip and client tests, path-validation unit tests, a
  daemon end-to-end test against a recording fake sidecar, Dart protocol and path
  tests, and four real-TUI PTY tests (success, relative refusal, missing file
  and an interior-dot path).
  The success PTY test also checks the stored part name and byte length and that
  the source path is not painted.

### Live self-pair upload run

The guarded probes are `scripts/live-attachment-send-probe` and
`ext/bloom-engine-sidecar/dart/tool/live_send_probe.dart`. They restore the real
engine state, use one of the account's own mailto handles as recipient and its tel
handle as sender, upload a 33-byte generated text file, and call the engine send.
They print no handles, ids, paths or content. The run was authorized only for this
self-pair; no third party was contacted.

```sh
BLOOM_LIVE_ATTACHMENT_SEND_ACK=one-shot-authorized \
~/.cache/bloom-tools/bin/python scripts/live-attachment-send-probe
```

Observed summary:

```json
{
  "result": "send_failed",
  "handles": 2,
  "mailto": 1,
  "tel": 1,
  "upload_events": 2,
  "upload_attached": true,
  "bytes": 33,
  "error_type": "AnyhowException",
  "error_code": "no_valid_targets"
}
```

The account has one mailto and one tel handle, so no second handle of either
scheme exists; the only non-third-party pair is the user's own handles. The upload
completed with two progress events and a sendable attachment. `api.send` then
threw and the bounded classifier reported `no_valid_targets`: the engine filtered
the self target and had nothing left to send to. No delivery is claimed. The probe
temporarily used the tel as sender and the mailto as recipient, deliberately
bypassing `chooseSender`; the product path would choose the mailto sender for a
mailto recipient and reject the pair earlier.

This proves the real engine upload path works on this account for a small text
file. It does not prove remote delivery, the sidecar `attachment_send` command, or
the product sender selection. A complete live send needs an explicitly named
third-party chat and a fresh authorization; the self-pair is not a valid target.

## Risks

- Local path races: the engine bridge accepts a path, not a descriptor. The daemon
  and sidecar both reject a final symlink and raw `.`/`..` segments, and the
  sidecar validates after its last await before upload, but a same-user process
  that can write the containing directory could still swap the file between that
  check and the engine's read. Parent-directory symlinks are followed. This is a
  local-threat limit, not a remote one; closing it fully would need the bridge to
  upload from an already-open descriptor or a private copy.
- Echo and dedupe: the engine may push the sent message back under the same id.
  The store must not create a second row or a second attachment id.
- UTI inference: upstream always has a stored UTI; Bloom must infer one without
  misrepresenting unknown types.
- Upload duration: a fixed timeout can expire while the remote side still accepts
  the file. That is the uncertain-send case the design must surface.
- A successful synthetic path is not remote delivery evidence.

## Decision

Implemented at the synthetic boundary: IPC v9, daemon validation and persistence,
the sidecar command and the TUI action all pass their tests, including a real-TUI
PTY run. The live boundary remains blocked pending explicit authorization for one
send to a chosen chat, observed locally and remotely, with no retry. Until then,
Bloom must not claim remote attachment delivery.
