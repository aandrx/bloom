# Attachment filenames: baseline and limits

This record explains the ingestion gap that preceded [local filename search](filename-search.md). Metadata, matching, display, and persistent PTY components now pass their scoped checks and reviews. Coordinated IPC v6 integration, rebuilt Rust/static checks, and the completed runtime rerun pass; final source review found no confirmed new defect. One earlier legacy unread RPC timeout remains an unresolved observation. The filename feature is accepted under shipp at its documented synthetic boundary, with that timeout and other limits retained.

## Historical baseline

`bloom-domain` already defined `MessagePart::Attachment(AttachmentRef)` with an optional filename. That type did not establish that ingestion preserved names.

The owned Dart codec emitted attachment descriptions and remote-access fields. At the research baseline, `apps/bloomd/src/engine.rs::map_inbound` kept text but converted other parts to `Unsupported`. The encrypted store could not recover a discarded filename. Search read text parts, and the TUI showed `[attachment]`.

Parent ran `cargo test -p bloomd --lib attachment_only_messages_are_kept_as_unsupported_parts` before the mapper change. It passed and asserted that a named attachment became an unsupported marker. This was synthetic proof, not a live-account inspection.

## Metadata prerequisite

The [metadata contract](attachment-metadata.md) permits reported filename, MIME type, and byte length. The mapper excludes MMCS object, URL, signature, key, UTI, and raw attachment envelopes. Tokens describe the current local revision and have a 256-byte limit. They are not download identifiers or stored-vector indexes.

Research did not establish stable or remote meanings for the outer `idx` or nested part number. Their stability is unknown, not proven absent. The local token does not depend on those fields.

The mapper prerequisite passed 107 daemon unit tests, 13 fake-sidecar tests, the rebuilt synthetic gate, and source review. The borrowed filename getter passed 10 domain tests and review without changing serialization. Supplied typed parts already survived both fake daemons. A test-only RPC/save/restart addition proved that behavior without a producer rewrite.

## Filename search components

The store now matches body text before explicit filenames within each message. It keeps the global newest-20,000-row bound, canonical membership, ordering, and hit limits. Missing names, MIME types, IDs, and unsupported markers do not supply filenames.

The fixture component passes 61 tests. Its 64 escaped-name cases include quotes, controls, bidi marks, and confirmed leading combining marks. Both fixtures matched pinned Rust for all 468 `Mc` scalars checked. Other Unicode classes and versions are not exhaustively equivalent.

The TUI displays escaped reported names in the transcript and loaded-message preview. Parent checked normal and Tokyo-timezone TUI suites, 119 render tests, static checks, and rebuilt PTY output at 100, 44, and 24 columns. The full 15-case reading suite proves an older filename-only hit, exact context, restored cursor, literal multiline input, and one text send to the original recipient. Scoped source reviews found no blockers after the documented test corrections.

The [filename-search record](filename-search.md) contains the contract and final integration evidence. These component results do not establish current live-engine reliability.

## Remaining limits

Old unsupported placeholders contain no filename to search. The implementation does not infer names from IDs, URLs, MIME types, or placeholders, and does not backfill missing metadata.

A reported name is descriptive metadata. It does not establish a local path, available content, completed transfer, or permission to open a file. Attachment sending, opening, downloading, content extraction, and remote history remain outside this work.
