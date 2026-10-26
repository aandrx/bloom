# Bloom engine sidecar

Headless Dart companion that owns the user-installed OpenBubbles engine and exposes
it to the Bloom daemon over stdio.

## Licensing boundary

The engine library is loaded at runtime only. It is never included, copied,
downloaded, or bundled here. The engine contains SSPL and closed-source components,
so it stays on the other side of a process boundary: no Bloom MIT crate depends on
rustpush, and the default `cargo tree -p bloomd` must stay free of it.

The generated bindings under `dart/lib/upstream` are Apache-2.0 code from the
OpenBubbles public release tag recorded in `ATTRIBUTION`. They are pinned by the
engine's Flutter Rust Bridge content hash: if the installed engine changes, the
bindings must be regenerated from the matching tag with
`dart/tool/refresh_bindings.sh <path-to-tag-checkout>`.

## Running (requires explicit authorization for real account state)

From `ext/bloom-engine-sidecar/dart`, resolve packages with `dart pub get`, then:

```sh
dart run bin/bloom_engine.dart --state <state-dir> --library <engine-library>
```

The normal application launches this child through `bloomd`; see the [setup guide](../../README.md#running-it) and [fresh-agent handoff](../../docs/handoff.md). Cargo does not install Dart, this sidecar tree/package dependencies, the engine library, or a registration.

`--state`/`BLOOM_ENGINE_STATE` and `--library`/`BLOOM_ENGINE_LIBRARY` are both
required; there are no defaults and no personal paths are compiled in.

The state directory must be owned exclusively. The sidecar refuses to start while
the OpenBubbles app is running, because both would write the same registration
state and the same push connection.

## Protocol

Newline-delimited UTF-8 JSON on stdin/stdout, one object per line, every frame
carrying `"version": 1`. Every protocol line on stdout begins with the ASCII
record-separator marker `\u001e`; diagnostics go to stderr. The daemon also
accepts legacy unmarked JSON, and skips a bounded number of unmarked non-JSON
lines with a structural log because the linked engine can write its own logger
output to stdout. Repeated foreign output still closes the connection. Frames larger than 1 MiB are rejected before parsing.

Daemon to sidecar:

- `{"version":1,"type":"send","request_id":N,"recipients":["..."],"text":"..."}`
- A single-recipient send also includes the legacy `recipient` scalar for direct-send compatibility. Group sends are only issued when the sidecar advertises `capabilities.recipient_lists: true` in `ready`; an older sidecar therefore cannot silently downgrade a group to one target. A newer sidecar rejects scalar-only requests from an older daemon with a correlated negative send confirmation; update the daemon and sidecar together to enable group sends.
- `{"version":1,"type":"sync_conversations","request_id":N}` for conversation metadata, not historical messages
- `{"version":1,"type":"attachment_fetch","request_id":N,"token":"<32 lowercase hex>"}` for one explicit cached attachment; the token is generation-scoped
- `{"version":1,"type":"shutdown","request_id":N}`

Sidecar to daemon:

- `ready` - emitted once after restore, with `engine_version` and `capabilities`; `recipient_lists: true` advertises multi-recipient send support
- `inbound_message` - `sequence` plus the encoded engine payload
- `send_confirmation` - `request_id`, `message_id`, `accepted`, optional `error`
- `shutdown_ack` - `request_id`
- `conversation` - one listed conversation's metadata (`sequence`, `conversation`)
- `sync_result` - correlated metadata result (`request_id`, `status`: complete/limited/failed, `conversations`)
- `attachment_fetched` - correlated local cache `path` and `bytes`; the path is never logged
- `history_message` and `history_complete` - legacy/future protocol definitions; not emitted as a working remote-message import
- `session_state` - `state` plus an optional `reason`
- `error` - bounded `code`, `retryable`, optional `request_id`

Protocol error frames never carry raw exception text. Sidecar/engine stderr is a separate diagnostic boundary and may contain external exception details; review it before sharing. Payload encoding is explicit, field by
field; variants that are not modelled yet are emitted with
`"complete": false` and an `unsupported` marker naming the variant and the reason,
so content is never silently replaced by a `toString()` placeholder.

`send_confirmation` has two sources and exactly one is emitted per request. When
the engine finishes the send synchronously it returns `false` from its send call,
which means no asynchronous completion is pending; the sidecar then confirms
`accepted: true` directly. When the engine returns `true` it will push a
`SendConfirm` later, and the sidecar waits for that push before confirming (with
`accepted: false` plus `error` when the push reports one). Interpreting the
synchronous `false` as a rejection is the bug this coordinator prevents.

## Attachment registry and cache (C1b)

`dart/lib/attachment_cache.dart` adds a sidecar-only attachment registry and
private cache writer. Tokens are opaque 16-byte hex values retained in memory for
one sidecar generation; the cache chooses its own root under
`XDG_CACHE_HOME/bloom/attachments` (or `~/.cache/bloom/attachments`), rejects a
symlink root, writes `<token>.bin` atomically from a per-attempt `.<token>.<attempt>.part`
temporary file, and enforces 64 MiB per file, 512 MiB total, 30-day age, 128
entries and a 120-second timeout. Reported engine size is an upper bound because
MMCS attachments can contain multiple variants. No IPC, daemon, store, TUI or protocol change is included, and no live
engine download has been proven.

## Formatting caveat

Run `dart pub get` before checking formatting. The formatter selects its style from
the package configuration, so without it `dart format` silently uses a different
style and reports the tree as clean while CI, which resolves dependencies first,
reports the same files as changed.

## Status

Implemented: explicit payload codec, versioned commands/results, configuration guards, send coordination, single-flight metadata listing, daemon integration, and daemon-side encrypted persistence before fanout. Latest iteration: 56 offline Dart tests plus fake-sidecar daemon coverage. The child handles EOF/shutdown; malformed output causes daemon-side stream failure and cleanup.

Not implemented: remote/pre-existing message history import and automatic engine-generation restart. Existing app history depends on app-side storage/services; a safe headless read-only import source is not established. `history` capability is false. Listing existing conversations is not importing their messages. Group text is conditional on recipient-list support; no group-management or rich-message workflows.

For current evidence and limits, see [status](../../docs/status.md). Historical live runs below do not validate current-checkout send/receive or renewal.

The engine re-delivers queued pushes across runs. A live 25 second run produced 37
inbound frames but only 32 distinct message ids, so consumers must deduplicate by
message id. Bloom's store already upserts by message id and reports `Unchanged`.

Tests never load the engine and never touch account state.
