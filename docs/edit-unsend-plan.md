# Inbound edit and unsend plan

Status: evidence gate passed for conservative inbound support; implementation is phased. No outbound operation or capability flag is planned.

## Source evidence

The owned codec emits edit frames with `tuuid`, `edit_part`, and indexed `new_parts`, and unsend frames with `tuuid` and `edit_part`. The surrounding envelope supplies sender, conversation, and `sent_timestamp_ms`.

Pinned upstream semantics are visible in OpenBubbles/rustpush `src/imessage/messages.rs`: `tuuid` becomes the raw target `message`, `edit_part` becomes `part_index`, and edit `new_parts` becomes the replacement HTML body and text. Parsed normal parts retain the HTML `message-part` value as `IndexedMessagePart.idx`; a plain single text body defaults to part zero when serialized. Bloom currently discards that index, so mutations are unsafe until the index is persisted.

Primary source: https://github.com/OpenBubbles/rustpush/blob/master/src/imessage/messages.rs

## Supported semantics

- Unknown targets never create rows.
- The event conversation must resolve to the target's canonical conversation or an alias, and the event sender must match the stored message sender.
- A target part must have a trustworthy stored engine part id. A single plain part with no explicit id may normalize to part zero; ambiguous legacy or multipart rows are skipped.
- Edit initially accepts nonempty text replacement parts only. Attachments, mentions, objects, malformed indices, and empty replacement lists are skipped explicitly rather than partially applied.
- Edit replaces every stored part belonging to exactly the targeted engine part, preserves all other parts and message fields, and sets `edited_at` only from the envelope timestamp.
- Unsend is supported only when the targeted engine part owns the whole stored body. Content is retained locally for audit and `unsent_at` is set from the envelope timestamp. Multipart partial unsend is skipped until the domain can represent per-part tombstones.
- Duplicate and older updates are no-ops. A later replayed normal message must not erase accepted reactions, edit state, unsend state, or trustworthy part metadata.
- Existing IPC `Update` events carry the resulting stored message. No IPC field or protocol-version change is needed.

## Display

An edited message keeps its body and gains a dim `Edited` annotation. A wholly unsent message keeps the locally stored body and gains `Unsent · retained locally`; Bloom does not claim remote deletion or hide audit content. Reactions remain visible. Search continues to search retained local content.

## Phases

1. Preserve engine part identity internally: done in `907ced5`. `TransportEvent::Message` carries aligned part ids, the encrypted store persists them transactionally, only a lone plain-text part normalizes to zero, and normal replays preserve accepted mutation fields and cannot replace a map that no longer aligns with stored edited parts. Scoped tests, strict Clippy, formatting, source guard, and independent review pass.
2. Add redacted typed edit/unsend transport events and mapper tests: done. The mapper requires target, part, sender, conversation, and timestamp; edit accepts only a nonempty all-text replacement list and rejects mixed or malformed content atomically. The Dart codec test pins the wire shape. Until phase 3, the service writes and publishes nothing and emits one generic warning per service instance. Rust and all 58 Dart tests, strict scoped Clippy, formatting, source guard, and independent review pass.
3. Add transactional store application and service fanout tests: unknown/mismatched/ambiguous targets change nothing; aliases work; duplicates and older events publish nothing; unrelated fields and part mappings are preserved.
4. Add fixture parity, headless rendering, and rebuilt PTY acceptance at 100, 44, and 24 columns, including replay/restart and retained-content wording.
5. Independent review and universal synthetic gate. Real-engine edit/unsend receipt remains a separately authorized live gate.

## Exclusions

No outbound edit or unsend, capability changes, partial multipart unsend, attachment replacement, edit history UI, fabricated timestamps, content deletion, remote-history recovery, or automatic retry.
