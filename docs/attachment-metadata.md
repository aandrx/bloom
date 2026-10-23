# Attachment metadata preservation prerequisite

Status: accepted under shipp at the synthetic mapper/persistence boundary. Parent verification and scoped independent implementation review found no remaining defects; the final rebuilt gate passed. No filename search, new filename UI, or live-engine acceptance is claimed.

## Scope

One implementation unit may change only `apps/bloomd/src/engine.rs`, including its tests. It preserves available descriptive metadata during inbound mapping. It must not change the domain schema, IPC version, sidecar codec, store schema, capabilities, outbound validation, search, TUI rendering, or generated files.

The existing `AttachmentRef` shape already accepts optional filename, MIME type, and byte length. The current UI still renders a placeholder, and attachment capabilities remain false. Preserving metadata does not imply content availability, transfer completion, or a working download reference.

## Explicit local metadata token

Each entry's `part` is the required part container. Its nested scalar `entry["part"]["part"]` and the separate `entry["idx"]` are the unverified engine numbering fields. Do not use either number, remote object, URL, signature, or key as attachment identity. Owned tests do not establish their stability or uniqueness.

For this metadata-only unit, define `AttachmentRef.id` as `local-metadata:` followed by the JSON encoding of `(transport string, raw message ID, physical position in the input parts array)`. Enumerate the physical input array, including positions of skipped entries. Do not use either engine numbering field.

This is an opaque token for metadata in the current decoded message revision, not an engine ID, file-object ID, remote-download handle, or direct index into the stored parts vector. It is deterministic for an identical payload and distinct for different positions, messages, and transports. It does not promise to track the same file across edits, reordering, or changed part segmentation. No file deduplication or guessed message identity is introduced.

Bound the complete encoded token to 256 bytes. Before serialization, reject a transport-plus-message-ID byte length above that budget; then check the encoded token length as well. This prevents a pathological shared header ID from being copied unboundedly into every attachment. Oversized context falls back to the unsupported marker, never to truncated or hashed identity. Add regressions for both the raw-context and escaped-JSON length limits, preserving surrounding text. A future download implementation must define a separate verified reference contract.

## Allowlisted scalar mapping

For an inbound part explicitly tagged `attachment`:

- `name`: preserve a nonempty JSON string as `file_name`, without inferring or rewriting it.
- `mime`: preserve a nonempty JSON string as `mime_type`. Do not substitute `uti_type` or a filename extension.
- `attachment_type.kind == inline`: accept `byte_length` only as a JSON unsigned integer representable by `u64`.
- `attachment_type.kind == mmcs`: accept `size` under the same rule.
- An unrecognized or missing `attachment_type.kind` yields no byte length, even if numeric size fields are present. Counts are reported metadata, not independent verification of downloaded content length.
- Missing, empty, or wrongly typed optional fields become `None`. Negative, fractional, string, or boolean lengths are not converted or guessed. Zero is a valid byte length.
- If all three descriptive fields are absent, retain the existing `Unsupported { kind: attachment }` marker. Failure to construct the local token/reference must defensively fall back to that marker, not drop the message or surrounding text. The token-budget refusal paths are tested. The constructor's nonempty-ID error remains defensive; do not claim artificial fault-injection coverage for it.

The codec's `complete` flag does not establish transfer completion and must not be presented as such. This unit preserves typed metadata that is actually present; it does not infer missing metadata from that flag.

Ignore all other attachment fields. In particular, MMCS object, URL, signature, and key data, `uti_type`, `iris`, and the engine numbering fields must not be copied into the domain reference, search text, logs, or plaintext preferences. Do not serialize the raw part envelope into the local token or metadata. Do not decode remote access material or perform file/network I/O.

## Verification before acceptance

Unit tests must cover named-only, MIME-only, inline and MMCS lengths, zero length, malformed/missing optional fields, metadata-free fallback, mixed text/attachment order and exact text whitespace, and multiple attachments. Tests must prove deterministic local tokens on identical input and separation across message, transport, and position. Changing or omitting `entry["idx"]` and the nested numbering value `entry["part"]["part"]` must not change the token at the same physical input position. Removing the entire `entry["part"]` container still skips that entry as before. Replace `attachment_only_messages_are_kept_as_unsupported_parts` with a positive named-metadata regression, and retain a separate metadata-free unsupported-fallback regression.

A synthetic test maps a payload, persists the resulting conversation and message through the real encrypted store, closes/reopens it, and verifies metadata round-trip. Use a private temporary directory and an explicit synthetic test key, never the real keychain or database. Assert repeated identical upserts remain unchanged. Include dummy remote-access markers of irrelevant types and assert none enters the serialized domain message or stored result. No actual credentials or account state are used.

Parent inspection and independent implementation review are required after the one-file diff. Run scoped tests first, then the relevant workspace, timezone, lint, formatting, source, and rebuilt synthetic runtime checks. This prerequisite does not permit a filename-search claim. New filename search and metadata display require their own contracts and regressions; old unsupported placeholders are not backfilled.

## Observed verification and change reach

Parent ran `cargo test -p bloomd --lib`: 107 passed, including the mapper-to-encrypted-store reopen regression. Parent also ran `cargo test -p bloomd --test engine_sidecar`: 13 fake-sidecar tests passed. The oversized-token regression failed before the resource guard and passed afterward. Exact-budget, escape expansion, independent optional fields, and both length variants are covered.

The critical safety fact is that persistence receives a constructed `AttachmentRef`, not the raw MMCS envelope. The real mapper/store/reopen test checks retained metadata, fresh-remap duplicate upsert equality, and exclusion of nested access-field canaries from the returned decrypted message. This is executable synthetic proof, not a live-account claim.

Existing outbound non-text rejection and false attachment capabilities are unchanged. The TUI still has its generic attachment-rendering branch by source inspection; no new filename UI or typed-attachment PTY acceptance is claimed. The existing placeholder unit test uses an unsupported part, not a metadata reference. No new DTO fields, schema migration, index, or remote operation is introduced. Oversized local-token context deliberately retains the unsupported marker without losing surrounding text.

The final workspace all-target, Tokyo-timezone TUI, strict Clippy, formatting, source, and rebuilt synthetic-runtime gate passed. Runtime counts: 28 fixture, 14 reading/navigation, 12 input, 4 messaging, and 8 launcher tests. Log: `/tmp/bloom-attachment-metadata-gates.log`; reading captures: `/tmp/bloom-attachment-metadata-accepted/`. These are disposable artifacts. Independent final review found no confirmed new defects and confirmed the one-file source boundary. Both reviews were source-only; parent owns the execution evidence. No real sidecar/account, historical backfill, attachment opening/sending, or content retrieval was exercised.

A non-blocking review observation remains: filename and MIME strings have no separate per-field cap, unlike the local token. They can enlarge stored/IPC payloads; this unit preserves reported metadata rather than truncating it. No performance or oversized-metadata UI acceptance is claimed.

## Next bounded unit

Add only `AttachmentRef::file_name(&self) -> Option<&str>` and its tests in `crates/bloom-domain/src/lib.rs`. Return the borrowed stored field exactly, including `None` versus present empty/whitespace/Unicode strings through serialization round-trip. Do not infer names from IDs/MIME/URLs, allocate or normalize in the getter, add other accessors, alter wire/schema/debug behavior, match search queries, or render metadata. Parent verification and a scoped independent review follow before a search unit is selected.
