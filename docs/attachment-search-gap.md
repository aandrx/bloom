# Attachment search prerequisite

Status: the baseline ingestion gap is documented, and the one-file metadata-preservation prerequisite is accepted under shipp with synthetic evidence. The filename-access bridge is accepted separately; search matching and display remain unimplemented units. No filename-search feature is accepted.

## Observed data flow

- `crates/bloom-domain/src/lib.rs` defines `MessagePart::Attachment(AttachmentRef)`. The reference can hold an ID, MIME type, optional filename, and optional byte length. Its existence does not prove real ingestion.
- `ext/bloom-engine-sidecar/dart/lib/payload_codec.dart` emits attachment `name`, `mime`, `uti_type`, `part`, and attachment-type metadata. Inline metadata includes `byte_length`; MMCS metadata includes `size` and remote-access fields.
- At the research baseline, `apps/bloomd/src/engine.rs::map_inbound` preserved text parts but mapped every other part type to `MessagePart::Unsupported { kind }`. It did not construct the attachment reference or preserve the filename.
- `apps/bloomd/src/store/messages.rs::upsert_message` serializes the mapped Message into encrypted storage. It cannot recover fields discarded before storage. Search currently matches text parts only.
- `apps/bloom-tui/src/app.rs::message_text` displays attachments and unsupported parts as placeholders. A generic `[attachment]` is not filename evidence.

Before the prerequisite implementation, parent ran `cargo test -p bloomd --lib attachment_only_messages_are_kept_as_unsupported_parts`. The test passed and explicitly asserts that a named attachment becomes an unsupported attachment marker. This is synthetic runtime proof of the baseline mapper behavior, not inspection of a live account or every historical database.

## Boundary before any writer

Filename search cannot be advertised until separate filename-access, search, and display units pass verification. Messages entering through the baseline mapper have no preserved filename evidence. Do not infer filenames from URLs, IDs, MIME types, or placeholders. Old stored placeholders cannot be backfilled without a separately verified source; no remote history or account access is authorized.

The owned codec also emits MMCS object, URL, signature, and key fields. Raw attachment JSON must not be copied into domain metadata, search text, logs, or plaintext preferences. Any future metadata mapping needs a small scalar allowlist, not an envelope dump.

Baseline research questions (resolved or explicitly left unverified in the accepted contract):

1. What `part` and outer `idx` mean in owned tests, including any limits on identity stability across edits.
2. Whether an explicitly local metadata reference is appropriate. It must not be presented as a remote-download identifier or a guessed engine identity.
3. A safe filename, MIME, and byte-length mapping with explicit missing/malformed-field behavior. Preserve surrounding text and honest unsupported placeholders when metadata is insufficient.
4. Synthetic mapping and encrypted round-trip regressions proving allowlisted metadata survives while remote-access fields do not.

This prerequisite is metadata preservation only. Search matching, attachment rendering, sending, opening, downloading, and content extraction remain separate units. The four-file follow-up found no evidence that either engine numbering field is stable or a download identity. Those properties are unknown, not proven unstable. The [accepted metadata-only contract](attachment-metadata.md) instead defines an explicitly local current-revision metadata token, a three-field metadata allowlist, and a one-file mapper/test boundary. Independent design review found no blocker after clarifying the nested numbering field, unknown-kind size handling, and replacement regression. The `engine.rs` implementation and parent hardening pass 107 daemon unit and 13 fake-sidecar tests, the full rebuilt gate, and scoped final source review. No confirmed new defects remain in that scope. The borrowed filename accessor is accepted with focused API verification and source review. Synthetic-fixture preflight now precedes matching/display contracts; old unsupported placeholders remain without filename evidence.
