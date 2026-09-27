# OpenBubbles engine sidecar

Status: researched, not implemented. Awaiting one decision (see the end).

## Purpose

Bloom's independent iMessage transport is blocked (see `ids-validation-gate.md`).
This document covers a different, narrower goal the user asked for: keep the
already-working local OpenBubbles install as the iMessage engine, and replace only
the user interface with Bloom. The Mac stays off. No hosted OpenBubbles or
BlueBubbles service is used.

This is an engine-companion design, not a fix for the independent transport. Bloom
would not hold its own Apple registration in this route.

## Verified evidence

All checks were read-only and used only installed program files. No account state,
messages, credentials or app data were read, and no Apple authentication was
attempted.

- Installed app: `app.openbubbles.OpenBubbles` v1.15.0.0, Flathub commit
  `108d51b6972853d7cd61cf1a7063383b4b229f465a1ea6227f76563bff226e7e`.
  Permissions include `--share=network` and `--filesystem=xdg-download`.
- The Flathub manifest builds that release from the upstream artefact
  `v1.15.0+205/bluebubbles-linux-x86_64.tar`, checksum
  `5584a4e79dbd22fcf19b51fb6ff6426d7f132555677e1ae9d3421f04657b563a`.
- The installed payload contains the engine as
  `files/bluebubbles/lib/librust_lib_bluebubbles.so` (33.9 MB, stripped).
- `frb_get_rust_content_hash` from the installed engine returns `-208244658`,
  identical to `rustContentHash` declared by the public tag `v1.15.0+205`.
  The generated bridge and the shipped engine are therefore ABI-compatible.
  The initial apparent mismatch was a signed/unsigned reading error, corrected.
- The public tag's generated bindings do not import `package:flutter/`; they depend
  only on `package:flutter_rust_bridge`. A headless Dart process can use them.
- The engine library's only dynamic dependencies are `libz`, `libgcc_s`, `libm`,
  `libc` and the loader. It resolves and loads outside the Flatpak sandbox.
- Engine entry points exposed by the public bindings include
  `configFromEncoded`, `configFromValidationData`, `makeAnisette`,
  `setupPush`, `registerIds`, `restoreAccount`, `restoreUser`,
  `makeImclient`, `recvWait`, `newMsg`, `send`, and `SharedPushState.restore`
  (all with an explicit state `path`).
- The app's own Dart orchestration layer uses Flutter and `path_provider`, so a
  sidecar would reimplement that thin orchestration and reuse only the generated
  bridge.

Not available: no documented or shipped local IPC, REST, CLI, plugin, or
server/host mode for an external program. The documented extension API is
Android/AIDL-specific and does not expose conversation history or general send.

## Design consequences from the source mapping

Mapping the public tag's restore path established three facts that shape the plan:

- Chats and message history live in the app's Dart-side ObjectBox database, not in
  the Rust engine. The chat list comes from `Chat.getChats` and threads load local
  messages first. A sidecar that drives only the Rust library can restore push and
  Apple service state and consume incoming messages, but cannot read existing
  history through those APIs. Reading it requires the app's ObjectBox model and the
  bundled `libobjectbox.so`, or backfilling from Apple.
- The aggregate restore is not read-only. It rewrites `hw_info.plist` and
  `id.plist`, opens the push connection, and constructs CloudKit with a hard
  expectation that can panic on invalid state. It is the same sequence the app runs
  at every launch.
- `bluebubbles.lck` is a Dart-side app-instance marker, not a Rust state lock. The
  engine does not enforce exclusive access, so one owner at a time is a rule we
  impose, not one the engine guarantees.

## Approach

1. A small Dart program (`bloom-engine`) loads the installed engine library with
   the matching generated bridge and owns all engine state.
2. It speaks Bloom's existing versioned Unix-socket IPC, so `bloomd`, the TUI and
   any future GPUI client stay unchanged in shape.
3. Hardware bootstrap uses the offline `OABS` payload from Mac Hardware Info. The
   hosted `MB` code path and Beeper relay are not used.
4. Anisette is generated locally. No hosted dependency is introduced.

## Risks and constraints

- Version locked. The content hash binds the sidecar to the installed Flatpak
  release. An app update requires regenerating the bridge from the matching public
  tag before the sidecar works again.
- Licensing. The bridge is Apache-2.0 public code; the engine embeds SSPL rustpush
  and a closed validation component. Personal local use of the installed app is
  one thing; redistributing a sidecar that depends on that binary is a separate
  licensing question that must be settled before any distribution.
- Account risk. Registration identity is the pair of Apple ID and the hardware
  identity in the `OABS` payload, so the same payload with the same Apple ID is the
  same device, not an additional one. Re-registering issues new keys and
  certificates that supersede the existing ones, which can break the working app
  until it re-registers, shows additional sign-ins to the account, and risks
  throttling if repeated. Reusing the existing state avoids a new sign-in and a
  new registration entirely.
- Do not run the sidecar and the OpenBubbles app against the same state directory
  concurrently.
- The engine's event loop and callbacks must be driven correctly; an incorrect
  initialization order risks duplicate or lost messages.

## Test plan

Phase 1, no account state, no credentials: **PASSED.**

1. A user-local Dart SDK is installed at `~/.cache/dart-sdk` (no system packages,
   no sudo).
2. The spike harness at `~/.cache/bloom-engine-spike` loads the installed engine
   library through the public, ABI-matched generated bindings via
   `RustLib.init(externalLibrary: ExternalLibrary.open(...))` and runs without a
   Flutter engine. `newNgmIdentity()` and `generateUdid()` both executed.
3. Verified engine artifact: `librust_lib_bluebubbles.so`, 33,912,920 bytes,
   SHA-256 `67d91b7024ed94bcd0b2cb3b1bb295cc518065c43afd329b4fe71b8dfbcac7a3`.

Phase 2a, restored state, read-only, user authorized: **PASSED.**

3. The harness at `~/.cache/bloom-engine-spike/bin/state_probe.dart` read the
   user's existing state through `readHardware` and `restoreUsers` only, and
   reported presence, variant types and a user count. No identifiers, handles or
   key material are printed.
4. Verified inert: a size and mtime snapshot of every file in the state directory
   was identical before and after the probe, with 25 files unchanged.

Phase 2b, live engine restore and bounded receive window, user authorized:
**PASSED.**

5. `bin/engine_restore.dart` restored the user's real state headlessly: push state
   restored, `ApsWatcher` created, iCloud services present, and the poll loop ran a
   bounded window with the Mac off, then shut down cleanly.
6. Inbound messages are delivered to whoever holds the push connection. The probe
   therefore consumes them, so it must only be run when Bloom can persist them.
7. The first capture attempt was inadequate: the log recorded only the runtime type
   name, so those pushes were consumed without usable content. `bin/payload_codec.dart`
   now encodes `PushMessage` by discriminant with real nested fields, and
   `bin/codec_test.dart` verifies it synthetically before any live run.
8. Verified capture over a 20 second window: 7 iMessage pushes, 7 distinct payloads,
   134 distinct key paths, 6 with message text captured (3 to 70 characters each),
   no reflection errors and no unreadable fields. The remaining record had no text
   parts. `hw_info.plist` was the only state file written.
9. Payloads append to `~/.cache/bloom-engine-spike/received/payloads.jsonl`, with
   directory mode 0700 and file mode 0600. No content is printed to stdout.

The codec's mirror-based fallback is spike-grade. The production sidecar should use
an explicit encoder per type and write straight into Bloom's encrypted store
instead of a JSONL file.

Phase 3, first write, user authorized:

10. **PASSED.** `bin/send_test.dart` sent one iMessage from the restored state with
    no registration and no login. `newMsg` built the message, `send` returned
    accepted, and exactly one send confirmation arrived for that message id with no
    error. The recipient independently confirmed receiving the message. Four inbound
    iMessages also arrived during the send window, so both directions were exercised
    in one run. State files written: `facetime.plist`, `hw_info.plist`,
    `id_cache.plist`, `statuskit.plist`.
11. Verified send path: `getHandles` supplies the sending addresses; the sender is
    chosen to match the recipient's scheme; `ConversationData` carries the
    participant handles, the local chat guid and the previous message guid;
    `newMsg` returns a `MessageInst` whose `id` keys the send confirmation.

Remaining:

12. **Restart and resume: PASSED.** Two consecutive runs after the send each
    restored state cleanly, reconnected, and received fresh inbound messages (2 then
    3, all with text captured). Restore is repeatable and only rewrites
    `hw_info.plist`.
13. Confirm the user's normal OpenBubbles setup still works after the sidecar has
    written state. Verified: the app started and ran normally afterwards, and the
    user confirmed it behaved as usual.
14. Confirm renewal after the scheduled re-registration.

A consequence of the user not running the app any more: a Flatpak update would
replace the engine library and break the content-hash match until the bindings are
regenerated from the matching public tag. Pin or mask updates for that app, or plan
the refresh step.

## Confidence

Verified so far, all with the Mac off and with the user's real registration:

- A headless process can load the shipped engine and drive it without Flutter.
- The existing state restores, and the engine connects to Apple and polls.
- Inbound iMessages arrive and are captured with real content.
- Outbound iMessage sends are accepted, confirmed, and independently observed as
  delivered by the recipient.

So the route works for send and receive. What remains before calling it intended
behavior is restart/resume, coexistence with the app, and one observed renewal.

## Integration work still to do

15. Wire `bloomd` to supervise the sidecar and expose it through Bloom IPC. **DONE**:
    `apps/bloomd/src/engine.rs` spawns the sidecar child process, maps payloads into
    domain types, and is owned by the existing `Supervisor`; `service.rs` persists
    before fanout; `main.rs` assembles store, listener, subscriptions and shutdown.

    **Ownership rule, learned the hard way.** The sidecar loads the user's real
    engine state, so at most one process may own that state at a time. An observed
    failure: `bloomd` died without its graceful path, the child was reparented to
    `systemd --user`, and it kept polling for over an hour while still holding the
    engine. The child now exits on stdin EOF (shared with the explicit `stop`
    command) and the daemon requests `PR_SET_PDEATHSIG` on Linux as a backstop.
    Because `PR_SET_PDEATHSIG` covers only the direct `dart run` child, stdin EOF
    is the real shutdown path. Before starting the engine, check for a leftover
    sidecar (`pgrep -f bloom_engine`) and for a live `bloomd`, not just for the
    OpenBubbles app, since either can still hold the state.
16. End-to-end test through the TUI with the OpenBubbles app not running. **DONE**:
    `apps/bloom-tui` no longer depends on Matrix; it talks to `bloomd` over the
    Bloom IPC socket, with conversation list, thread view, composer, session status,
    transcript scrolling, filtering, resync on reconnect, and headless tests. The
    suite is currently 20 unit tests, 2 daemon-session tests, 13 key-path
    interaction tests and 13 render tests, plus a fake-daemon integration test
    covering snapshot, live events, resync on selection change, and subscription
    loss. A PTY smoke run against the real daemon rendered the UI and reported the
    engine session as active. The tests run under two timezones in CI because a
    clock-dependent assertion passed locally and failed there.
17. Read existing history from the app's ObjectBox store, or accept starting from
    takeover and backfilling from Apple. **BLOCKED**: the app's model classes import
    Flutter and app services, so they cannot be reused headlessly. A safe read-only
    path has not been established, and the requirement is to open the database
    read-only and stop rather than risk a schema migration against real history.
18. Replace the spike's mirror-based encoder with explicit encoders and write into
    Bloom's encrypted store instead of a JSONL file. **DONE** for the encoder and for
    store writes during daemon operation.

The sidecar half of item 18 is done: `ext/bloom-engine-sidecar` holds the packaged
sidecar with an explicit field-by-field encoder, a versioned stdio protocol, config
guards, and 34 offline tests, and it was verified live against the real engine
(ready frame, inbound frames, clean shutdown).

## Conversation listing from the engine

A fresh Bloom store is empty, so the sidecar also lists the account's existing
conversations from the engine's iCloud/CloudKit chat sync (`syncChats`), on by
default and disabled with `--no-sync-chats`. It runs after the `ready` frame and off
the receive path, paginates with a bounded page and row limit, deduplicates by guid,
and ends with `history_complete`, which the daemon turns into a single
`ConversationsChanged` event so clients refresh once rather than once per row.

Verified against the real account: 4 pages, 568 conversations synced,
`limited=false`, no frames skipped, and all 568 served back over the socket. Each
listing takes tens of seconds, so the UI is usable immediately and fills in when the
listing finishes. Thread history itself is not imported yet.

Two implementation traps worth remembering, both found by running it:

- `dupDaemonDesk` consumes the restored state, so handles read from the original are
  disposed with it. The CloudKit client must be taken from the state it returns,
  which is what the app does; otherwise every sync call throws
  `DroppableDisposedException`.
- A client that subscribes after startup would otherwise never learn the engine's
  session state, because the event was published before it existed. The hub now
  retains the last session event and replays it on subscription.

## Running it locally

```sh
export BLOOM_ENGINE_STATE=<openbubbles state dir>
export BLOOM_ENGINE_LIBRARY=<installed librust_lib_bluebubbles.so>
export BLOOM_ENGINE_SCRIPT=<repo>/ext/bloom-engine-sidecar/dart/bin/bloom_engine.dart
export BLOOM_DART_EXECUTABLE=<dart>

bloomd          # opens the encrypted store, spawns the engine, serves the socket
bloom-tui       # reads BLOOM_SOCKET_PATH (or BLOOM_SOCKET)
```

The store starts empty, so the UI shows conversations only for messages received
while Bloom is running until history import exists.

## Verified end to end through the daemon

`apps/bloomd/tests/engine_e2e.rs` (ignored by default) starts the real `bloomd`
binary against the real engine and drives it over the real socket. A run on this
machine reported: conversations listed over IPC, session observed active, and an
inbound message received, persisted, and served back through `history` before it was
fanned out. The store key is provisioned into the OS keychain on first run and there
is no plaintext fallback.

## Open decision

Which state directory the engine uses:

- **Reuse the existing app state (recommended).** No new Apple sign-in, no new
  registration, no new keys; the engine restores the session and registration the
  app already holds. The sidecar must not run concurrently with the app, and the
  engine will read and update that private state directory while it owns it.
- Fresh dedicated state. No contact with existing app data, but the engine must
  sign in to Apple again and re-register the same hardware identity, which
  supersedes the current keys for that Apple ID. Only appropriate with a throwaway
  Apple ID, or a hardware code that differs from the one in use.
