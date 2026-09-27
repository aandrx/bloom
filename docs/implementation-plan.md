# Bloom - Implementation Plan (subagent-optimized)

Companion to `docs/system-design.md`. This document exists so that independent
subagents can work in parallel without colliding. Read section 2 before touching
anything.

## 1. Measured constraints

These are facts about this machine, not preferences.

| Constraint | Measurement | Consequence |
|---|---|---|
| Cores | 16 | Good for one build at a time |
| Free RAM | ~8 GB of 27 GB | **Do not run many concurrent `cargo test`** |
| Cargo target dir | single shared, 17 GB | Concurrent builds **block on the lock** |
| `/tmp` | tmpfs, 8 GB free | Never put a target dir in `/tmp` |
| Disk | ~105 GB free | Room for the workspace, not for 10 target dirs |
| `sccache` / `nextest` | not installed | No shared compilation cache |

**Rule:** parallelize authoring, serialize verification. Ten agents each running
`cargo test` will thrash memory and queue on the lock. Cap concurrent
*build-running* agents at 3; authoring-only agents can run 8 or more.

A dedicated target dir per agent is explicitly rejected: a full dep tree is
several GB, so a handful would exhaust the disk and defeat the shared cache.

## 2. Anti-collision rules

1. **One writer per file.** The ownership table in section 4 is binding. If a task
   needs a file it does not own, it must request a change through the orchestrator.
2. **Interfaces freeze before fan-out.** Wave A defines every cross-crate type.
   After that, no interface changes without a coordinating task.
3. **No agent edits root `Cargo.toml`** except task A1.
4. **No agent commits or pushes.** The orchestrator runs the full workspace gate
   and commits per wave.
5. **Each task carries its own tests.** A task is not done until its own crate's
   tests and clippy pass.
6. **Feature-gate, do not delete.** `bloom-core` stays until Phase 2 completes.

## 3. Crate layout and license boundary

```
crates/bloom-domain        MIT        pure types, no I/O
crates/bloom-secrets       MIT        SecretStore + OsKeyring (extracted from bloom-core)
crates/bloom-ipc           MIT        wire protocol, framing, auth, client + server types
crates/bloom-transport     MIT        Transport trait, conformance kit, FakeTransport, RelayTransport
apps/bloomd                MIT        daemon: ipc, session, supervisor, store
apps/bloom-tui             MIT        existing UI, rebound onto bloom-ipc
apps/bloom-gpui            MIT        later
ext/bloom-transport-direct SSPL-1.0   rustpush adapter - NEVER distributed
ext/bloom-mac-bridge       MIT        macOS-side fallback bridge
spike/matrix               excluded   existing Matrix proof
spike/matrix-core          excluded   bloom-core, moved in Phase 2
~/.cache/bloom-direct-spike excluded   Phase 0 scratch harness, not shipped
```

`ext/bloom-transport-direct` lives in its **own cargo workspace** so it can never
enter a distributed artifact by accident. `bloomd` depends on it by path behind a
non-default `direct` feature. A CI check asserts the default build has no
`rustpush` in its dependency graph.

## 4. Wave A - foundation (serial critical path)

Blocks all parallel work. Interfaces defined here are frozen.

| ID | Scope | Owns | Depends | Verify |
|---|---|---|---|---|
| A1 | Workspace scaffolding: crate dirs, stub `lib.rs` with module declarations, manifest per crate, license fields, `direct` feature, CI license check, rustfmt config | root `Cargo.toml`, every new `Cargo.toml`, stub `lib.rs` files | - | `cargo check --workspace` |
| A2 | `bloom-domain`: all types, serde, invariants, unit tests | `crates/bloom-domain/**` | A1 | `cargo test -p bloom-domain` |
| A3 | `bloom-ipc` protocol: frame format, version byte, request/response/event enums, socket path, peer-cred handshake, round-trip + version-negotiation tests | `crates/bloom-ipc/**` | A2 | `cargo test -p bloom-ipc` |
| A4 | `Transport` trait, `Capabilities`, `TransportEvent`, typed error taxonomy | `crates/bloom-transport/src/lib.rs`, `trait.rs`, `error.rs` | A2 | `cargo test -p bloom-transport` |

A2 is the true bottleneck; give it the strongest agent. A3 and A4 run in parallel
after it.

## 5. Wave B - parallel authoring (target 8 concurrent)

Only 2 or 3 of these run cargo at once; the rest are authoring-dominant.

| ID | Scope | Owns | Depends | Verify |
|---|---|---|---|---|
| B1 | Extract `SecretStore` + `OsKeyring` from bloom-core; fail-closed tests | `crates/bloom-secrets/**` | A2 | `cargo test -p bloom-secrets` |
| B2 | Store: SQLite schema, migrations, encrypted-at-rest, CRUD for conversations/messages/parts/reactions | `apps/bloomd/src/store/**` | A2 | `cargo test -p bloomd store::` |
| B3 | IPC server: listener, socket perms, uid check, dispatch skeleton | `apps/bloomd/src/ipc/**` | A3 | `cargo test -p bloomd ipc::` |
| B4 | IPC client used by the TUI | `crates/bloom-ipc/src/client.rs` | A3 | `cargo test -p bloom-ipc client::` |
| B5 | Conformance test kit + `FakeTransport` | `crates/bloom-transport/src/testing.rs`, `conformance.rs` | A4 | `cargo test -p bloom-transport` |
| B6 | Mac bridge: chat.db tail, AppleScript send, narrow authenticated server | `ext/bloom-mac-bridge/**` | A4 spec | compiles; Mac test deferred |
| B7 | `RelayTransport` speaking the bridge protocol | `crates/bloom-transport/src/relay.rs` | A4, B5 | conformance kit passes |
| B8 | TUI event adapter: IPC events to existing `App`, against a fake client | `apps/bloom-tui/src/port.rs` (new file only) | A3, A4 | `cargo test -p bloom-tui port::` |
| B9 | **Phase 0 spike**: isolated offline decoder, then a locally sourced validation-data provider, headless IDS registration, APS, send, receive | `~/.cache/bloom-direct-spike/**` | - | live send + receive and renewal with Mac off; currently blocked by public OpenAbsinthe stub |

B9 is the highest-risk item. The direct adapter depends on it; the local daemon,
store, IPC, fake transport, and TUI can progress independently while Gate 0 is blocked.

### Local integration slices while Gate 0 is blocked

Freeze shared manifests and module exports with the orchestrator before parallel
writes. Each row owns distinct implementation files; limit simultaneous cargo
builds to three using the shared target directory.

| ID | Scope | Owns | Depends | Gate |
|---|---|---|---|---|
| L1 | Persist message, delivery, and conversation summary in one transaction | `apps/bloomd/src/store/ingest.rs`, minimal `store/mod.rs` export | B2 | duplicate and rollback tests |
| L2 | Single-consumer transport lifecycle and supervisor with FakeTransport | `apps/bloomd/src/session.rs`, `supervisor.rs` | B5 | stop/backpressure tests |
| L3 | Subscription socket with post-persistence event fanout; disconnect slow clients | `apps/bloomd/src/ipc/subscription.rs`, minimal `ipc/mod.rs` export | B3 | event/lag tests |
| L4 | Route Subscribe from the authenticated dispatcher to L3 | `apps/bloomd/src/ipc/dispatch.rs` | L3 | real socket test |
| L5 | Subscribe first, then snapshot visible state; reconnect and resnapshot on disconnect | `crates/bloom-ipc/src/client.rs` | L4 | missed-event recovery test |
| L6 | RequestHandler backed by encrypted store and supervisor; persist before fanout | `apps/bloomd/src/service.rs` | L1-L5 | integration tests |
| L7 | Runnable daemon, private runtime directory, fail-closed keychain, shutdown | `apps/bloomd/src/main.rs`, `config.rs`, daemon manifest and lib export | L6 | start/stop socket test |
| L8 | Map domain and IPC events into TUI state against a fake client | new `apps/bloom-tui/src/port.rs` | B4 | adapter tests |
| L9 | Replace Matrix TUI wiring with Bloom IPC; remove Matrix only after PTY pass | existing TUI source and manifest | L5, L7, L8 | PTY test |
| L10 | Full local test: fake transport through daemon, encrypted store, IPC and TUI | `apps/bloomd/tests/local_e2e.rs` | L7, L9 | restart/resync test |

L1, L2, L3, and L8 can begin together once their interfaces are settled. L4
follows L3; L6 follows L1 through L5. The store is authoritative: after a
subscription reconnect the client resnapshots conversations and the visible
thread before applying buffered events. No unproven durable event cursor is
assumed.

## 6. Wave C - direct transport (Gate 0 blocked) and later wiring

| ID | Scope | Owns | Depends | Verify |
|---|---|---|---|---|
| C1 | Direct: activation payload ingest, Apple ID auth, IDS register, keystore persistence | `ext/bloom-transport-direct/src/activation.rs`, `keystore.rs` | B9, A4 | registers against Apple |
| C2 | Direct: pure mapping between rustpush messages and domain types | `ext/bloom-transport-direct/src/map.rs` | A2 | offline unit tests |
| C3 | Direct: APS connect and receive loop to `TransportEvent` | `ext/bloom-transport-direct/src/aps.rs` | C1 | live receive, Mac off |
| C4 | Direct: outbound build, send, delivery-state tracking | `ext/bloom-transport-direct/src/send.rs` | C1 | live send, Mac off |
| C5 | Daemon: session state machine, renewal, transport supervisor | `apps/bloomd/src/session.rs`, `supervisor.rs` | A4, B5 | tests against FakeTransport |
| C6 | Daemon: main, config, logging, shutdown wiring | `apps/bloomd/src/main.rs`, `config.rs` | B2, B3, C5 | daemon starts, socket works |
| C7 | TUI rebind: replace bloom-core usage with the IPC client | `apps/bloom-tui/src/main.rs`, `auth.rs` | B4, B8 | PTY test against FakeTransport |

C5, C6, and C7 are covered by L2, L7, and L9 above; do not dispatch them twice.
C1-C4 remain blocked by Gate 0. C2 is offline-testable but remains in the
SSPL-separated adapter and should not be integrated until that boundary is set.

## 7. Wave D - integration and retirement

| ID | Scope | Depends | Verify |
|---|---|---|---|
| D1 | End-to-end PTY test through bloomd with FakeTransport | C6, C7 | PTY transcript asserts |
| D2 | Live iMessage acceptance: send + receive, Mac powered off | C3, C4, C6 | two-way, exact-once |
| D3 | Retire Matrix: move bloom-core to `spike/matrix-core`, update workspace | C7, D1 | workspace builds |
| D4 | Reviewer pass on security paths: socket auth, secret handling, no-secret logging | C6, D1 | findings resolved |
| D5 | Docs: reconcile system-design with shipped reality | D2 | review |

## 8. Gates

- **Gate 0:** See [validation evidence](ids-validation-gate.md). Find a lawful, locally runnable IDS validation-data implementation;
  then Phase 0 spike must register, send, receive, and renew with the Mac off.
  Public `OpenAbsinthe-Stub` has `todo!()` in this path. The author's legacy
  [pypush mechanism](https://jjtech.dev/reverse-engineering/imessage-explained/)
  emulates an obfuscated Mach-O binary, but a runnable, properly licensed local
  implementation and renewal behavior are not confirmed. Do not start C1-C4
  or claim a Mac-free runtime until this is resolved and tested. Local slices
  L1-L10 do not require Gate 0; C1-C4 do.
- **Gate 1:** `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
  -- -D warnings`, `cargo test --workspace` all pass on the default feature set.
- **Gate 2:** the default build's dependency graph contains no `rustpush`.
- **Gate 3:** no secrets in logs, verified by test.
- **Gate 4:** live two-way iMessage with the Mac off.

## 9. Review policy

Reviewers are expensive and slow. Use them per wave, not per task:

- After Wave A: one reviewer on the frozen interfaces (highest leverage; a bad
  interface poisons every parallel task).
- After Wave B: one reviewer for license boundary and socket auth, one for store.
- After Wave C: one reviewer for the direct transport, one for daemon wiring.
- Wave D4 always runs.

Tasks B9, C1, and C3 are correctness- and security-critical: review those
individually regardless.

## 10. Commit policy

One commit per independently verified foundation or integration milestone after
the full workspace gate passes. Task-level commits are rejected: they create
churn and break bisectability. Matrix retirement (D3) is its own commit. Push
after each verified milestone; optional bridge tasks do not block local core.

## 11. Risks that can invalidate this plan

| Risk | Affects | Mitigation |
|---|---|---|
| Public OpenAbsinthe stub cannot generate IDS validation data | B9, C1-C4, D2 | Find and test a lawful local implementation before Gate 0; never substitute a hosted service silently |
| Apple rejects the tracked legacy FairPlay cert at runtime | C1-C4, D2 | Exercise actual activation in Gate 0; a successful library build is insufficient |
| Apple throttles or blocks the account | D2 | throwaway Apple ID; relay fallback |
| rustpush upstream drifts (last commit Apr 2026) | C1-C4 | pin to a commit; vendor the patch |
| Concurrent agents exhaust RAM or the cargo lock | all waves | cap build-running agents at 3 |
| `chat.db` unreadable on current macOS | B6, B7 | relay is optional; verify before B6 deepens |
| Interface change after fan-out | all of Wave B | section 2 rule 2; re-freeze explicitly |

## 12. Later work, intentionally not scheduled

### Windowed surface (GPUI)

The shipped client is the terminal UI, and it stays the client until the terminal
surface reaches parity with Codex, which is itself a ratatui TUI. A windowed
surface is the eventual target because `design/mockup/index.html` is a desktop app
and a character grid cannot express sub-cell motion, proportional type or inline
attachments.

Recommendation: a native window with our own rendering (GPUI), not a native window
wrapping an embedded terminal. Only the render and input layers change; the
domain, IPC, daemon, engine sidecar and store are reused unchanged, because the
UI is already just another client of the daemon's Unix socket.

The full reasoning, the two shapes considered, the migration risks and what would
be measured before the port is called done are in
[`gpui-surface.md`](gpui-surface.md). No work on it starts until the terminal
surface is finished.
