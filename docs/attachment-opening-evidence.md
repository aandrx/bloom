# Inbound attachment opening: evidence gate

Status: accepted at the documented boundaries below. C1e records a live inbound save and C1g a live chooser save. Earlier sections keep their original gate wording and name IPC v8; outbound attachment send later moved the protocol to v9.

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

## C1c implementation evidence

C1c connects the path without adding a TUI action:

- inbound normal-message attachments are registered in the sidecar registry and
  receive an opaque token at their physical part position;
- the codec carries the token only on that attachment part, and the daemon mapper
  validates it into `AttachmentRef.fetch_token`;
- the sidecar accepts `attachment_fetch`, downloads through C1b into its private
  cache, and returns only a correlated path and byte count or a bounded error;
- the daemon engine waits on a correlated attachment waiter and rejects relative
  or parent-traversal paths; IPC v8 adds typed `fetch_attachment` and
  `attachment_fetched` operations;
- no key, signature, URL, object ID, raw content, or path is logged.

Rust workspace tests, strict Clippy, formatting, source guard, 86 Dart tests and
68 fixture tests pass. No TUI action calls the operation, and fixture daemons do
not implement a fetch result. Token identity is generation-scoped and becomes
invalid after sidecar restart.

## C1d implementation evidence

The TUI now has one explicit save action. `/attachment` selects the newest loaded
attachment in the current chat that carries a valid sidecar token, sends the v8
fetch request, and displays `Attachment saved to <path> (<bytes> bytes).` It does
not automatically download, open, reveal, or retry. A missing token, a stale
generation, or a daemon failure becomes a plain notice or error; no path is
written to logs or preferences. The command is listed in `/help` and the user
guide. Actual-PTY coverage seeds an attachment with a token, invokes
`/attachment`, and observes the saved-path result against the stateful fixture.

No TUI action opened the saved file externally at C1d. C1e, the authorized live
gate, now passes; see below. The explicit external open follows in C1f.

## C1e live gate

The guarded probe is `scripts/live-attachment-probe`. It starts one daemon against
the real registration, waits for a stored inbound attachment carrying a fetch
token, requests that token once through IPC v8, and verifies the returned cache
file is an absolute regular file under `XDG_CACHE_HOME/bloom/attachments`, has the
reported nonzero size, and is mode `0600`. It never sends, imports history,
renews, changes registration, opens the file, prints content or stores the token
or path in evidence. The temporary store and cache are deleted.

It requires this authorization:

> Authorize one live inbound attachment test: start one daemon against my real OpenBubbles registration and state, allow it to consume pushes into a temporary store, wait for one real inbound attachment, download it once into a private temporary cache, verify the cached file structurally, then delete the store and cache. No sends, history import, renewal, registration changes, external opening, or content sharing are authorized.

### Exact commands

Read the authorization statement above first. Then:

```sh
cd "$(git rev-parse --show-toplevel)"
cargo build -p bloomd

STATE="$HOME/.var/app/app.openbubbles.OpenBubbles/data/bluebubbles"
FLATPAK="$(flatpak info --show-location app.openbubbles.OpenBubbles)"
LIB="$FLATPAK/files/bluebubbles/lib/librust_lib_bluebubbles.so"
SCRIPT="$PWD/ext/bloom-engine-sidecar/dart/bin/bloom_engine.dart"
DART="$(command -v dart || printf '%s' "$HOME/.cache/dart-sdk/dart-sdk/bin/dart")"

test -d "$STATE"
test -f "$LIB"
test -f "$SCRIPT"
test -x "$DART"
```

Close OpenBubbles, then run the read-only preflight:

```sh
BLOOM_ENGINE_STATE="$STATE" \
BLOOM_ENGINE_LIBRARY="$LIB" \
BLOOM_ENGINE_SCRIPT="$SCRIPT" \
BLOOM_DART_EXECUTABLE="$DART" \
scripts/live-attachment-probe --preflight
```

After it prints `preflight: no competing owner; required paths exist`, start
exactly one authorized probe:

```sh
export BLOOM_LIVE_ATTACHMENT_ACK=one-shot-authorized
scripts/live-attachment-probe \
  --bloomd target/debug/bloomd \
  --engine-state "$STATE" \
  --engine-library "$LIB" \
  --engine-script "$SCRIPT" \
  --dart "$DART" \
  --seconds 300 \
  --evidence-dir "$HOME/bloom-live-attachment-evidence"
```

While it waits, send one photo or file to the account from another device. The
probe only reads local stored metadata and the downloaded cache file; it does not
display the content. A pass prints a structural summary with `"result": "passed"`,
a nonzero byte count and `"file_mode": "0o600"`.

Then verify cleanup:

```sh
unset BLOOM_LIVE_ATTACHMENT_ACK
pgrep -x bloomd || true
ps -eo pid,args | grep -E '[b]loom_engine\.dart' || true
```

Both process checks must print nothing. The temporary store, cache and probe root
are deleted; do not copy them. The probe was exercised against a fake daemon. The
first authorized live run, made before bounded diagnostics existed, found an
attachment token and issued the fetch, but returned no `attachment_fetched`; its
temporary log was removed, so no sidecar failure code was captured for that run.
Attempts during the later diagnosis all failed before any engine request and were
captured: one printed only `download_failed`, the next `stage=thrown` with an
unclassified cause, and the next `stage=construct` with
`unclassified_DroppableDisposedException`. The first run is consistent with the
same defect but is not claimed as such, because its code was lost.

### C1e result

A live run passed on 2026-10-26 against the real registration and OpenBubbles
state. The printed summary contained only structural fields:

```json
{
  "messages_seen": 30,
  "bytes": 325019,
  "file_mode": "0o600",
  "start_seconds": 0.1,
  "fetch_seconds": 0.47,
  "result": "passed"
}
```

Post-run checks printed no `bloomd` process, no `bloom_engine.dart` process and no
remaining `/tmp/bloom-live-attachment-*` root. The retained `summary.json` is mode
`0600` and carries no path, token, filename, MIME type or content. The downloaded
bytes were verified structurally only; nothing was opened, displayed or shared.

Two earlier live attempts failed with `download_failed` before any engine request.
Bounded sidecar diagnostics identified the stage as stream construction and the
cause as a disposed opaque handle: `dupDaemonDesk` encodes every opaque field of
the restored state with `Auto_Owned`, so it takes ownership of the restored
`ApsConnection` and the Dart handle is disposed afterwards. The sidecar had kept
passing that stale `push.conn`; it now passes `deskState.conn` from the state
`dupDaemonDesk` returns, which matches how the upstream app holds its connection.
No download was attempted against Apple with the stale handle.

The diagnostic line is bounded and content-free. Failures report a fixed stage
(`size`, `reuse_empty`, `reuse_oversized`, `reuse_stat`, `cache_entry`,
`temp_create`, `temp_stat`, `size_mismatch`, `filesystem`, `progress_negative`,
`progress_over_total`, `size_threw`, `construct`, `listen`, `stream`, or the
`download` default if a call site omits one) and either an allowlisted engine
cause token (`mmcs_get_failed`, `not_connected`, `resource_timeout`, `auth`,
`http`, `io`, and similar) or `unclassified_<RuntimeType>` for anything else.
Engine error text is scanned only to pick a token and is never logged, so object
identifiers, signatures, URLs and attachment names cannot reach the daemon log.
`attachmentFailureDetail` builds that suffix and is unit-tested, so a regression
that logged raw error text would fail a test.
Current gates after the fix: 92 Dart tests pass and `dart analyze` reports no
issues for `bin`, `lib/attachment_cache.dart` and `test`. The vendored generated
bindings still report 24 pre-existing analyzer warnings, unchanged by this work.
No Rust source changed for C1e.

## C1f explicit external open

`/open` fetches the newest loaded attachment and then launches a desktop opener
with the cache path. Nothing opens automatically. `/attachment` opens the chooser
covered in C1g instead.

The path is accepted only when it is absolute, contains no `..`, is not a
symlink, is a regular file, and is non-empty. It is passed as one argv element
with null stdin/stdout/stderr; Bloom never builds a shell command, so a reported
filename cannot become shell syntax. The opener is `xdg-open` unless the
`BLOOM_OPENER` environment variable names another program (a name or path, never
a command line). The child is reaped on a detached thread, and success means the
opener process started, not that a viewer displayed the file. Failure messages
name the reason without repeating the path.

Observed gates: 166 library tests, 82 interaction tests, 12 daemon-session tests,
8 input-safety tests, 3 launcher tests, 9 binary tests, 124 render tests, and 68
fixture tests pass. A unit test launches a recorder script and proves the opener
receives exactly one argument, the cache path. Two real-binary PTY tests cover
the open surface: `/open` compares the recorder's argument with the path the
fixture daemon returned, and a missing opener shows
`No file opener is available`. The fixture daemon materializes a real cache file
instead of returning a placeholder path.

`/open` is offered by the typing menu for `/`; the `?` shortcut sheet was not
changed and still does not list every slash command.

Limits: no test launches a real desktop viewer, no live-fetched file has been
opened through the TUI, and Bloom does not read, render, or verify the file's
content. Token identity remains sidecar-generation scoped.

## C1g attachment chooser

`/attachment` opens a modal chooser over every loaded attachment part that still
carries a fetch token, newest first, showing the reported name and sender label.
Up / Down, PageUp / PageDown, Home / End move; Enter fetches and saves the choice;
`o` fetches and opens it; Esc or Ctrl+C cancels. A click on a row selects and
saves it, a click outside or a right-click cancels, and a wheel over any pane
moves the chooser instead of retargeting the chat behind it. Parts without a
token are omitted, and a chat with none keeps the existing
`No fetchable attachment is loaded` error instead of opening an empty modal.

The chooser holds a snapshot: it does not follow arrivals while open, and the
sidecar-generation rule still applies, so a stale token fails cleanly at fetch.
Nothing is downloaded until Enter or `o`, and the modal never opens a file.

Observed gates: 404 TUI tests (166 library and the render/interaction suites),
strict Clippy and formatting pass; 68 fixture tests pass. Two new render tests
cover the modal rows, the selection marker and an 11-column fallback, and prove
tokens are never painted. Library tests also prove the chooser owns the wheel,
ignores modified `o`, cancels on right-click, and does not downgrade an in-flight
`/open`. Four real-binary PTY tests cover the attachment surface: save through the
chooser (and no opener invocation), choosing the older row, external open, and a
missing opener. The older-row test reseeds two attachments, moves the selection
down, and asserts the fetched path carries the older token, proving the choice
reaches IPC.

### C1g live run

The guarded probe is `scripts/live-attachment-picker-probe`. It starts one daemon
against the real registration and state with a temporary store, waits for a
replayed inbound attachment carrying a fetch token, starts the real TUI in a PTY,
opens `/attachment`, saves the selected row, and verifies the cache file. It never
presses `o`, points `BLOOM_OPENER` at a nonexistent program, and prints no name,
path, token or content.

```sh
export BLOOM_LIVE_ATTACHMENT_ACK=one-shot-authorized
~/.cache/bloom-tools/bin/python scripts/live-attachment-picker-probe \
  --engine-state "$STATE" --engine-library "$LIB" \
  --engine-script "$SCRIPT" --dart "$DART" \
  --seconds 420 --evidence-dir "$HOME/bloom-live-picker-evidence"
```

Run `--preflight` first, and only with OpenBubbles closed and no other Bloom owner
running. The same authorization envelope as C1e applies: one test, a temporary
store and cache, no sends, no history import into Bloom's real store, no renewal
or registration change, and no external opening or content sharing.

A live run on 2026-10-26 printed only structural fields:

```json
{
  "conversations_seen": 2,
  "fetchable_parts": 1,
  "picker_opened": true,
  "saved_bytes": 165257,
  "file_mode": "0o600",
  "elapsed_seconds": 16.53,
  "result": "passed"
}
```

That proves the real engine replay produced a live token, the real TUI rendered
the chooser, Enter sent the v8 fetch, and the cache write was private with the
reported byte count. Post-run checks found no daemon, engine or temporary root;
the retained summary is mode `0600` and carries no path, token, name or content.

Limits: this run had one candidate, so it does not prove live multi-choice
selection; the live `o` open path was not exercised; the store was temporary, so
no history was imported into Bloom's real store; no message was sent and no file
was opened or displayed. Tokens remain sidecar-generation scoped, and the chooser
cannot list attachments outside the loaded transcript window.

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

82 Dart tests pass, including fake-downloader atomicity, concurrency, timeout,
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
5. **C1e:** one authorized live inbound attachment download; otherwise remain display-only. **Passed** on 2026-10-26: 325,019 bytes, mode `0600`, `"result": "passed"`.
6. **C1f:** explicit external open through a desktop opener, with no automatic open and no shell.
7. **C1g:** chooser over every loaded fetchable attachment, with save and open actions.

## Exclusions

No outbound attachment send, no remote-history archive import, no automatic download, no attachment content search, no guessed file identity, no plaintext cache exports, and no claim that a reported filename implies a local file exists.
