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

## Running

```
dart run bin/bloom_engine.dart --state <state-dir> --library <engine-library>
```

`--state`/`BLOOM_ENGINE_STATE` and `--library`/`BLOOM_ENGINE_LIBRARY` are both
required; there are no defaults and no personal paths are compiled in.

The state directory must be owned exclusively. The sidecar refuses to start while
the OpenBubbles app is running, because both would write the same registration
state and the same push connection.

## Protocol

Newline-delimited UTF-8 JSON on stdin/stdout, one object per line, every frame
carrying `"version": 1`. stdout carries protocol frames only; diagnostics go to
stderr. Frames larger than 1 MiB are rejected before parsing.

Daemon to sidecar:

- `{"version":1,"type":"send","request_id":N,"recipients":["..."],"text":"..."}`
- A single-recipient send also includes the legacy `recipient` scalar for direct-send compatibility. Group sends are only issued when the sidecar advertises `capabilities.recipient_lists: true` in `ready`; an older sidecar therefore cannot silently downgrade a group to one target. A newer sidecar rejects scalar-only requests from an older daemon with a correlated negative send confirmation; update the daemon and sidecar together to enable group sends.
- `{"version":1,"type":"shutdown","request_id":N}`

Sidecar to daemon:

- `ready` - emitted once after restore, with `engine_version` and `capabilities`; `recipient_lists: true` advertises multi-recipient send support
- `inbound_message` - `sequence` plus the encoded engine payload
- `send_confirmation` - `request_id`, `message_id`, `accepted`, optional `error`
- `shutdown_ack` - `request_id`
- `conversation` - one imported conversation (`sequence`, `conversation`)
- `history_message` - one imported historical message (`sequence`, `message`)
- `history_complete` - `conversations`, `messages`, `limited`
- `session_state` - `state` plus an optional `reason`
- `error` - bounded `code`, `retryable`, optional `request_id`

Errors never carry raw exception text. Payload encoding is explicit, field by
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

## Formatting caveat

Run `dart pub get` before checking formatting. The formatter selects its style from
the package configuration, so without it `dart format` silently uses a different
style and reports the tree as clean while CI, which resolves dependencies first,
reports the same files as changed.

## Status

Implemented and covered by offline tests: encoder, protocol, configuration guards.
The startup history import frames are defined but not yet produced.
Not yet implemented: daemon integration, writing into Bloom's encrypted store, and
reading pre-existing OpenBubbles history (which lives in the app's Dart ObjectBox
database, not in the engine).

The engine re-delivers queued pushes across runs. A live 25 second run produced 37
inbound frames but only 32 distinct message ids, so consumers must deduplicate by
message id. Bloom's store already upserts by message id and reports `Unchanged`.

Tests never load the engine and never touch account state.
