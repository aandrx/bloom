# Bloom - System Design

Status: draft for review. Nothing here is committed to beyond Phase 0.

## 1. Purpose

Bloom is an iMessage client for Linux with its own UI. The user's Mac is needed once,
for activation, and then stays off. Bloom is not a Matrix client; the Matrix
implementation currently in this repository is a mistaken direction and is retained
only as a spike.

Goals:

- Send and receive real iMessage messages from Linux.
- Bloom owns every pixel of UI. No Flutter, no Dart, no third-party client shell.
- No dependency on BlueBubbles or any third-party hosted service.
- No Mac required online at runtime.

Non-goals:

- Matrix, XMPP, or any general-purpose chat federation.
- Messages for Business, RCS, or SMS as a primary transport.
- Distributing SSPL-derived code.
- A full-featured iMessage clone at parity with Apple's app.

## 2. Constraints that shaped this design

These are evidence-backed, not assumptions.

| Constraint | Evidence |
|---|---|
| Apple publishes no API for third-party iMessage clients | Apple's Messages framework covers in-Messages extensions, not replacement clients |
| Direct Apple access means reverse-engineered protocol code | `rustpush` is the only mature implementation |
| Direct code is SSPL v1 | `rustpush/LICENSE`; `LICENSE.exceptions` grants relief only to OpenBubbles |
| SSPL private use carries no publication duty | SSPL §0/§2/§5/§6/§13; obligations attach on conveyance or third-party service |
| rustpush builds from public source | `certs/legacy-fairplay/*` is tracked; `certs/fairplay/*` is gitignored and populated by copying |
| Activation certs are legacy/expired | Cert `notAfter = Oct 11 2024`; whether Apple still accepts it is unverified |
| A sleeping Mac does not run Messages | Power Nap covers Mail, Calendar, iCloud; Messages is not listed |
| Sleeping Macs still eventually receive | Apple queues iMessages for offline devices, retained up to 30 days |
| There is no supported headless OpenBubbles API | Its `rust/` crate is `cdylib`/`staticlib` with no binary; PR #211 is unmerged |

## 3. Architecture

```
+--------------------------------------------------------------+
| Surfaces (MIT)                                               |
|   bloom-tui (ratatui)          bloom-gpui (later)            |
+----------------------------+---------------------------------+
                             | local socket, framed protocol
+----------------------------v---------------------------------+
| bloomd - headless daemon (MIT)                               |
|   session/activation state machine                           |
|   conversation store + event fanout                          |
|   transport supervisor, retry/renewal, capability reporting  |
+----------------------------+---------------------------------+
                             | Transport trait
        +--------------------+---------------------+
        |                                          |
+-------v-----------------+            +-----------v--------------+
| DirectTransport         |            | RelayTransport           |
| (SSPL-1.0, never shipped)|            | (MIT)                    |
| rustpush: APS, IDS,     |            | client of bloom-mac-bridge|
| IMClient, keystore      |            |                          |
+-------------------------+            +-----------+--------------+
                                                   | LAN/VPN, own auth
                                       +-----------v--------------+
                                       | bloom-mac-bridge (MIT)   |
                                       | AppleScript send,        |
                                       | chat.db receive,         |
                                       | LaunchAgent              |
                                       +-----------+--------------+
                                                   |
                                            Messages.app
```

The important property: the transport is a deployment choice. The UI cannot tell
which one is behind the trait except through declared capabilities. The independent
transport remains blocked at Gate 0; the OpenBubbles sidecar path does not make that
path production-ready.

## 4. Components

### 4.1 `bloom-domain` (MIT)
Pure data types and no I/O: conversations, participants, messages, parts,
reactions, delivery state, capabilities. Shared by every other crate. This is what
replaces the Matrix `RoomSummary`/`MembershipState` model.

### 4.2 `bloomd` (MIT)
Headless daemon. Owns the transport, the store, and the activation state machine.
Runs as a user service. Exposes a local socket API. Never linked into the UI, so a
crash or a protocol hang cannot take the terminal down.

### 4.3 `bloom-transport` (MIT)
The `Transport` trait plus `RelayTransport` and a conformance test kit. Every
transport must pass the same suite.

### 4.4 `bloom-transport-direct` (SSPL-1.0, never distributed)
Adapter over `rustpush`. This crate, and only this crate, is a derivative work.
It must not appear in a distributed build. See section 12.

### 4.5 `bloom-mac-bridge` (MIT)
Mac-side daemon: `LaunchAgent`, reads `~/Library/Messages/chat.db` for inbound,
`osascript` into Messages.app for outbound, narrow authenticated API over
LAN/VPN. Holds no Apple credentials beyond what Messages already has.

### 4.6 Surfaces (MIT)
`bloom-tui` is the terminal client today; it provides a conversation list and thread
view, a composer, list filtering, transcript scrolling, light and dark themes, and
session status. See [`tui.md`](tui.md) for the user-facing guide. `bloom-gpui` is a
later surface.

## 5. Domain model

Matrix's model is rooms with membership and encryption state. iMessage's model is
conversations, handles, and message parts. The mapping is a replacement, not a
rename.

```
Conversation
  id: ConversationId          local stable id
  transport: TransportId
  remote_id: String           guid in the transport's namespace
  kind: Direct | Group
  title: Option<String>
  participants: Vec<Participant>
  last_message_at: Option<Timestamp>

Participant
  handle: Handle              "+15551234567" or "a@example.com"
  display_name: Option<String>
  is_me: bool

Message
  id: MessageId
  conversation_id: ConversationId
  remote_id: Option<String>
  sender: Handle
  parts: Vec<MessagePart>
  sent_at: Timestamp
  delivery: DeliveryState     Queued | Sent | Delivered | Read | Failed
  from_me: bool
  edited_at: Option<Timestamp>
  reactions: Vec<Reaction>

MessagePart
  Text(String) | Attachment(AttachmentRef) | Unsupported { kind: String }

Reaction   { sender: Handle, kind: String, remote_id: Option<String> }
Capabilities { reactions, edits, unsend, attachments, groups, typing, read_receipts, sms }
```

Explicitly absent: membership, join, room, homeserver, encryption state as a
user-facing concept. iMessage hides its encryption; surfacing it would be a lie by
omission and a Matrix habit.

## 6. Transport contract

### Starting a conversation to a bare address

`start_conversation` reuses an existing conversation only when it is a **Direct**
conversation that already contains that recipient. A group that happens to
include the recipient is never reused: matching on any participant sent private
messages into the group. When no direct conversation exists, a new one is created
with the transport's direct remote id, and address forms are normalised first
(`mailto:` prefixes and phone formatting).

A body carrying anything other than text parts is rejected rather than partially
sent, so a `[text, attachment]` message cannot silently arrive as text alone.

```rust
#[async_trait]
pub trait Transport: Send + Sync {
    fn id(&self) -> TransportId;
    fn capabilities(&self) -> Capabilities;

    async fn take_events(&self) -> Result<mpsc::Receiver<TransportEvent>>;
    async fn start(&self) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn conversations(&self) -> Result<Vec<Conversation>>;
    async fn history(&self, c: &ConversationId, limit: u16) -> Result<Vec<Message>>;
    async fn send(&self, c: &ConversationId, body: OutgoingBody) -> Result<MessageId>;
}

pub enum TransportEvent {
    ConversationsChanged,
    Message { conversation: ConversationId, message: Message },
    MessageUpdated { conversation: ConversationId, message: Message },
    DeliveryChanged { conversation: ConversationId, message: MessageId, delivery: DeliveryState },
    Session { state: SessionState, reason: Option<SessionReason> },
}
```

Rules:

- `capabilities()` is authoritative. The UI must not offer a feature the transport
  does not declare. This is what lets a degraded relay and a full direct transport
  share one UI honestly.
- `bloomd` takes the sole bounded event receiver before `start`. Producers await
  delivery into this backpressured queue; closing the receiver is an error, never
  permission to drop a message. UI fanout happens separately after persistence.
- `stop` is awaited and idempotent; producers make no new sends after it returns.
  Already buffered events may still be drained by the daemon.
- `send` returns once the transport has accepted the message, not when delivered.
  Delivery state arrives as an event. Adapters reject undeclared capabilities.
- Errors are a typed taxonomy, not strings: `SessionExpired`, `AccountThrottled`,
  `RecipientUnreachable`, `NotRegistered`, `TransportUnavailable`, `Unsupported`.

## 7. Daemon IPC

Frames from the engine are newline-delimited JSON, read with a 1 MiB bound enforced
as bytes arrive. The daemon distinguishes stream EOF, oversized frames, read errors,
and invalid UTF-8 in its reader diagnostics. Daemon startup logs its pid and socket,
and shutdown logs the received signal and a clean-stop marker. SIGTERM and Ctrl+C
both use the orderly drain-and-stop path.

- Unix domain socket at `$XDG_RUNTIME_DIR/bloom/bloomd.sock`, mode `0600`.
- Frames are a version byte, four-byte big-endian length, then bounded JSON.
  Any codec error closes the connection; requests and responses carry IDs. The
  engine sidecar's separate stdio protocol is newline-delimited JSON; its `ready`
  frame includes protocol version, engine version, send/history capability flags,
  and account handles. Handle lookup is best effort, so failure does not prevent
  startup but can make own-message classification incorrect.
- Peer credentials checked via `SO_PEERCRED`; reject any uid but the owner's.
- Request/response for reads and sends; a subscription stream for events.
- One writer to the message store and to the transport session. The daemon claims
  its IPC socket before starting the engine sidecar, so a competing daemon fails
  before it can also open the user's live registration state. The daemon is a
  singleton per account.

The UI never touches the transport, the store, or credentials directly. That
boundary is the security boundary.

## 8. Persistence

SQLite, encrypted at rest, key held in the OS keychain. This mirrors the pattern
already proven in `bloom-core`, which stores a 32-byte store key via a
`SecretStore` trait and opens an encrypted SQLite store.

```
accounts        (id, handle, transport, created_at)
conversations   (id, transport, remote_id, kind, title, last_message_at)
participants    (conversation_id, handle, display_name, is_me)
messages        (id, conversation_id, remote_id, sender, sent_at,
                 delivery, from_me, edited_at, deleted)
message_parts   (message_id, idx, kind, text, attachment_id)
attachments     (id, path, mime, byte_len, sha256, remote_id)
reactions       (message_id, sender, kind, remote_id)
sync_state      (transport, cursor, updated_at)
```

Attachments live outside the database as content-addressed files with restrictive
permissions. Nothing plaintext, no session material, no key material in SQLite.

### Store key provisioning

A key is generated only when there is no existing database. If the database file
exists and the keychain has no key for it, `open_or_create` fails with the distinct
`StoreError::ExistingDatabaseKeyMissing` and does not provision a replacement key.

This is deliberate and was a real defect before: the store used to generate and
store a fresh key whenever the keychain read returned nothing, so a transient
keychain failure (daemon not running, locked, entry removed) replaced the real
key with a new one. The existing database then could not be decrypted with it,
and the key that could have opened it was gone, which made the store
unrecoverable rather than merely unopenable. Failing closed keeps the data
recoverable once the keychain is available again.

## 9. Security model

### Assets
Apple ID credentials; IDS identity private keys; device certificates; Mac hardware
identifiers; the message database; attachment contents.

### Rules

1. Identity and credential material never leave the machine that registered it.
   The Mac bridge never receives Apple credentials; the daemon never sends them.
2. No plaintext fallback. If the keychain is unavailable, fail closed, exactly as
   the existing `bloom-core` and TUI auth do today. For the message store this is
   enforced in `open_or_create`: a missing key never causes a new one to be
   provisioned over an existing database (see section 8).
3. Secrets never enter logs, error strings, panics, or support dumps. Hardware
   identifiers and activation material are treated as secrets.
4. The socket is owner-only, uid-checked, and never network-exposed.
5. The Mac bridge exposes a narrow message API. It must not proxy raw `chat.db`
   access, because that database is the entire message history plus metadata.

### Threats and mitigations

| Threat | Mitigation |
|---|---|
| Local user reads the store | Unix socket perms, uid check, disk encryption, restrictive file modes |
| Malware in the user session | Out of scope as a boundary; store is encrypted at rest only |
| Credential exfiltration via logs | No-secret logging discipline plus a test asserting redaction |
| Apple throttles or blocks the account | Isolated Apple ID; degrade to relay transport |
| Mac compromise via the bridge | Bridge holds no new credentials; narrow API; auth over VPN/TLS |
| Protocol breakage leaks data | Failing closed on unknown message kinds, never rendering raw payloads |

## 10. Activation and lifecycle

`bloomd` logs its start, engine child pid, received termination signal, and clean
shutdown. It handles both SIGTERM (the normal service-manager stop path) and Ctrl+C
(SIGINT), drains and shuts down the supervisor on either path, then removes the
socket. The sidecar exits on stdin EOF as well as the explicit shutdown command.
On Linux the daemon also requests `PR_SET_PDEATHSIG` for the direct child it spawns;
that is only a backstop for that child, not a process-tree guarantee. Stdin EOF is
the sidecar's actual orphan-shutdown path.

States: `Unregistered -> Activating -> Registered -> Active -> RenewalDue ->
(Renewing | Throttled | Blocked | Failed)`.

One-time flow:

1. Mac Hardware Info copies an `OABS`-prefixed base64 protobuf containing Mac hardware fields locally. This is **not** the short-lived IDS validation data.
2. The protobuf is decoded into `MacOSConfig`. Our isolated decoder compiles against OpenBubbles' pinned rustpush fork, but registration is not proven.
3. A local implementation must generate short-lived IDS validation data. The public fork points to `OpenAbsinthe-Stub`; its `ValidationCtx` methods are `todo!()`. This is a **Gate 0 blocker**, not a solved setup step.
4. Only after that blocker is resolved: authenticate the Apple ID, register IDS with the identity keys, then connect APS and construct `IMClient`.
5. Exercise send, receive, and automatic renewal with the Mac powered off before claiming Mac-free runtime.

Runtime:

- `APSConnection` delivers `APSMessage`; `IMClient::handle(msg)` yields an
  optional `MessageInst`; that becomes a domain `Message` and an event.
- Outbound: build `MessageInst`, then `IMClient::send(&mut inst) -> SendJob`.
- Identity keys are persisted through the `keystore` crate
  (`SoftwareKeystore<T>` with a 32-byte encryptor), never in plaintext.

Renewal is explicitly treated as a state machine with a user-visible degraded
state, because the documented "renews without the Mac" behavior is unverified.

## 11. Failure modes and degradation

| Failure | Behavior |
|---|---|
| Protocol/version change breaks rustpush | Direct transport reports `TransportUnavailable`; user can switch to relay |
| Activation cert rejected | Direct transport cannot register; relay is the fallback |
| Account throttled or blocked | Surface clearly; do not retry aggressively; relay fallback |
| APS connection drops | Reconnect with backoff; queue outbound; show session state |
| Renewal fails | Degraded banner, messages may stop; keep local history readable |
| Mac offline (relay) | Outbound fails fast; inbound batches on wake, since iMessage queues 30 days |
| Daemon crashes | UI shows disconnected; restart is safe, store is consistent |
| Unknown message kind | Render a placeholder part; never drop the message |

## 12. Repository and licensing layout

```
crates/bloom-domain              MIT
crates/bloom-transport           MIT      trait + RelayTransport + conformance kit
apps/bloomd                      MIT      daemon
apps/bloom-tui                   MIT
apps/bloom-gpui                  MIT      later
ext/bloom-transport-direct       SSPL-1.0 rustpush adapter, never distributed
spike/matrix                     excluded from workspace
```

Two honest options for the direct adapter: keep it out of this repository
entirely, or keep it here with `license = "SSPL-1.0"` in its manifest. What must
not happen is SSPL-derived code sitting in an MIT-declared workspace. A CI check
should assert that no distributed artifact depends on `rustpush`.

## 13. Concurrency model

Tokio, one runtime. Single-writer discipline on the transport session and the
store. Event fanout by broadcast channel, matching the proven pattern in the
current TUI.

Carried over from the existing implementation because it works:

- Operation results are matched by identity (conversation id, body, generation)
  so a late result cannot clobber newer state.
- Long operations are spawned and selected against, so key input stays responsive.
- Shutdown is awaited on every exit path, including error paths.

## 14. Testing strategy

1. Unit tests for domain invariants and each transport adapter.
2. A transport conformance suite both transports must pass, skipping cases gated
   by `capabilities()`.
3. Daemon IPC tests: framing, auth rejection, version negotiation.
4. Security tests: no secrets in logs, socket permission enforcement, fail-closed
   on keychain absence.
5. PTY end-to-end tests driving the real TUI, as already done for Matrix.
6. A live iMessage acceptance test (Phase 0) with explicit pass criteria and no
   credentials in logs.

## 15. Phasing

Phase 0, transport spike. Thin daemon proving direct send and receive with the
Mac powered off, then a re-registration. Gate: if Apple rejects the legacy
activation cert, the direct path is abandoned or reduced to send-only.

Phase 1, core. `bloom-domain`, `Transport` trait, one working transport, `bloomd`
with IPC, store, and event fanout.

Phase 2, UI rebind. Point the existing TUI at `bloomd`; retire Matrix to `spike/`.
The current terminal client has a conversation list, thread view, composer,
filtering, transcript scrolling, light and dark themes, and session status; see
[`tui.md`](tui.md).

Phase 3, second transport. Capability-driven UI so relay and direct coexist
honestly.

Phase 4, GPUI surface on the same daemon.

## 16. Open questions

1. Does Apple accept the expired `legacy-fairplay` certificate for activation today?
2. What is the real renewal cadence, and does it need a Mac or validation data again?
3. Can a fresh Apple ID that was never activated on a device register, or must
   iMessage be enabled on the Mac first?
4. Does email-only iMessage work, or is an always-on iPhone required for
   phone-number threads?
5. Which features does the rustpush send path actually implement end to end:
   tapbacks, edits, unsend, attachments, group conversations?
6. Legal exposure of a reverse-engineered protocol client, beyond the SSPL question.
7. Does `chat.db` remain readable and complete on current macOS for the relay?
