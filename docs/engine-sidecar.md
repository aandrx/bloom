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

10. Send one outbound message and confirm delivery.
11. Restart the sidecar and confirm inbound messages still arrive.
12. Confirm renewal behavior after the scheduled re-registration.

Phase 3, integration:

7. Wire `bloomd` to supervise the sidecar and expose it through Bloom IPC.
8. End-to-end test through the TUI with the OpenBubbles app not running.

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
