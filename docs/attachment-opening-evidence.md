# Inbound attachment opening: evidence gate

Status: evidence gathered, no attachment download or opening implemented. This record answers whether Bloom has a lawful, reachable local path for inbound attachment content before any UI or storage change.

## Question

Can the owned sidecar turn an inbound attachment into a local file without reading the engine database directly, inventing a path, or bypassing Bloom's ownership and privacy rules?

## Observed evidence

### The bridge exposes real download calls

The pinned generated Dart bindings already include:

- `api.downloadAttachment({aps, attachment, path})`
- `api.downloadMmcs({aps, attachment, path})`

at `ext/bloom-engine-sidecar/dart/lib/upstream/api/api.dart:698-716`.

Both take an `ApsConnection`, the engine attachment object, and a destination path, and return a progress stream. The sidecar already restores `SharedPushState`, whose `conn` field is an `ApsConnection` (`bin/bloom_engine.dart:229-245`). The generated bindings are part of the pinned sidecar tree, so bridge reachability is observed in this repository.

### The upstream implementation writes a file and streams progress

In the OpenBubbles app source branch `rustpush` (observed commit `eed1b63`), `rust/src/api/api.rs:1889-1932` implements both calls. They:

- create the destination parent directory;
- create or truncate the destination file;
- download into it;
- stream `TransferProgress { prog, total, attachment: None }`.

The Rustpush implementation behind `MMCSFile::get_attachment` uses the MMCS signature, object, URL, key and size and the APS connection (`rustpush/src/imessage/messages.rs:1315-1396`, observed commit `b3c96c2`). `Attachment::get_attachment` also writes inline attachments directly from `AttachmentType::Inline` (`:1454-1470`). No local engine database read is required.

Source evidence comes from the current upstream branches, not a proven checkout identity with the installed `v1.15.0+205` engine. The pinned generated bindings establish that the bridge surface exists; live behavior against the installed engine is still unverified.

### The download implementation writes to stdout

`download_attachment` in the upstream API contains `println!("donwloading file {}", path)` and progress `println!` calls. This independently confirms that the engine library can write non-protocol text to the sidecar's stdout. That matches the packaging containment unit and makes the marker/skip behavior necessary; it also means attachment download must not rely on stdout being clean.

### Bloom currently discards the download material

- The Dart codec encodes MMCS `object`, `url`, `size`, `signature_base64` and `key_base64` for inbound parts (`payload_codec.dart:252-300`).
- The daemon mapper deliberately keeps only name, MIME and a reported byte length. `attachment_part` rejects or ignores MMCS object, URL, signature, key, UTI and engine numbering (`apps/bloomd/src/engine.rs:966-1035`).
- `AttachmentRef` has no download token, local path, or part identity beyond a local metadata token (`crates/bloom-domain/src/lib.rs:386-425`).
- The TUI renders a name or placeholder only. There is no sidecar command, attachment registry, path policy, or opening action.

## Verdict

The API-availability gate **passes**: a real download path exists through the owned bridge and does not require engine-database access.

The end-to-end opening gate **does not pass**: Bloom has no verified way to retain the engine attachment object, request a download safely, choose a destination, bound the result, or open it. A live download against the installed engine has not been run and would require an explicit authorized test with a real inbound attachment.

## C1b implementation evidence

The sidecar-only registry and cache are implemented in
`ext/bloom-engine-sidecar/dart/lib/attachment_cache.dart`:

- opaque 16-byte hex tokens with a 128-entry bounded registry, retaining only the
  message ID, physical part index and engine attachment;
- a private root under `XDG_CACHE_HOME/bloom/attachments`, symlink-root refusal,
  per-attempt temporary names and atomic `<token>.bin` rename;
- 64 MiB per file, 512 MiB total, 30-day age, 128-entry and 120-second timeout
  bounds, with per-token request deduplication and stale-part cleanup;
- reported engine size treated as an upper bound rather than an exact content
  assertion, because MMCS attachments may sum multiple variants;
- content-free failure types and redacted metadata/debug strings.

81 Dart tests pass, including fake-downloader atomicity, concurrency, timeout,
size and quota cases, plus format and analysis. The registry/cache is not wired
into the sidecar command loop, no IPC/daemon/store/TUI change exists, and no real
download has run.

## Required design before implementation

1. **Retain only in the sidecar.** Keep a bounded map from a random opaque attachment token to the original `Attachment`/`MMCSFile` for the current sidecar generation. Do not send keys, signatures, URLs, or object IDs to the daemon, store, logs, IPC, or preferences. Clear the map on restart; a missing token is a clean "fetch again later" failure.
2. **Token identity.** Bind each token to the inbound message and part position, not to a reported filename. Do not infer identity from names, MIME types, or engine numbering. Re-delivery of the same message may replace or reuse the token only under a documented rule.
3. **Daemon request, sidecar path.** Add a sidecar command such as `attachment_fetch { request_id, token }`. The sidecar chooses a path under a private per-user cache, never accepts a caller path, rejects traversal and symlinks, creates with mode `0600`, writes to a temporary name, fsyncs, and renames only on success. Return a capability/status frame, not the path in logs.
4. **Bounds and failure.** Enforce a per-file byte cap, total cache quota, timeout and cancellation. Reject unknown MIME types for automatic handling but allow explicit save. Never claim success before the stream completes. Do not retry automatically.
5. **IPC and TUI.** A typed request/response (likely the next protocol version) exposes an opaque fetch result and a user-initiated open/save action. Opening must be an explicit local action; never auto-open downloaded content. Terminal display should show a short status and the saved location, not file content.
6. **Privacy.** No attachment content in diagnostics; no plaintext cache paths in preferences; cache cleanup on logout/revocation and a bounded age policy. Engine source prints paths to stdout, so the protocol marker remains required.
7. **Live evidence.** A synthetic fake bridge test can prove command validation and path safety, but only an authorized live inbound attachment can prove the installed engine's download. That test must not send, import history, renew, or change registration.

## Suggested units

1. **C1a (this record):** source and bridge evidence. Accepted if this record's limits are explicit.
2. **C1b:** sidecar attachment registry, opaque tokens, private-path writer, bounds, and fake-bridge tests. No IPC/UI.
3. **C1c:** typed IPC fetch/open protocol and daemon dispatch, with no-store secret handling.
4. **C1d:** TUI explicit save/open action plus PTY failure/success states against a synthetic fixture.
5. **C1e:** one authorized live inbound attachment download; otherwise remain display-only.

## Exclusions

No outbound attachment send, no remote-history archive import, no automatic download, no attachment content search, no guessed file identity, no plaintext cache exports, and no claim that a reported filename implies a local file exists.
