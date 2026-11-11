# Demo mode plan

Status: planning only. No implementation, tests, or scripts have changed. This
document records the approved plan and its acceptance gate; each unit is
delegated and accepted separately.

## Scope

Demo mode is a runtime `--demo` flag plus a Cargo `demo` profile that inherits
`release`. It ships the same binaries and the same renderer as normal mode. It
runs against a generated synthetic encrypted database and requires no Python at
runtime.

Isolation is the acceptance gate. Demo mode must be provably unable to read,
write, or contact real state, not merely configured to avoid it.

## 1. Flag surface and startup

- Root `Cargo.toml`: add `[profile.demo]` with `inherits = "release"`. Build via
  `cargo build --profile demo -p bloomd -p bloom-tui`; outputs land under
  `target/demo/`.
- `scripts/bloom`: make `--release`, `--debug`, and `--demo` mutually exclusive,
  and parse the choice before sourcing `.env`. Demo never sources `.env`.
- `bloom-tui` binary (U4): parse `--demo`, `--doctor`, and help; reject unknown
  or conflicting arguments.
- Launcher: the demo launch path creates an isolated session and passes `--demo`
  to both children. It never probes or reuses the normal socket.
- Daemon: parse arguments before `DaemonConfig::from_env()`, and add a safe
  `--help`.
- TUI: resolve demo context before constructing `BloomClient`, loading
  preferences, or entering the terminal. A direct `bloom-tui --demo` requires a
  validated demo session; missing context fails and never falls back to normal
  mode.
- Share session and path validation through a small new component in
  `crates/bloom-ipc`. Normal `socket_path_from_env()` is unchanged.

### Implementation units

Work is delegated as four disjoint, ordered units. Each lands separately with
its own review before the next starts:

| Unit | Owns exclusively | Depends on |
| --- | --- | --- |
| U1: flag surface and IPC context | root `Cargo.toml`, `apps/bloomd/src/main.rs`, `crates/bloom-ipc` | none |
| U2: isolation contract | `apps/bloomd/src/store/` (all files), `apps/bloomd/src/service.rs` (backend enum seam and the demo dispatch point), `apps/bloomd/src/demo/mod.rs` context validation (excluding `seeder.rs`) | U1 |
| U3: synthetic data | `fixtures/demo/`, `apps/bloomd/src/demo/seeder.rs` | U2 |
| U4: launcher and TUI wiring | `scripts/bloom`, `apps/bloom-tui/src/bin/bloom.rs` (the shipped `bloom` launcher entry point), `apps/bloom-tui/src/main.rs`, `apps/bloom-tui/src/launcher.rs` | U1, U2, U3 |

Sections 5 and 6 are verification and boundaries, not deliverables. No unit
edits another unit's files; the shared `bloom-ipc` component is created in U1
and only extended by U2. The daemon dispatch hook lives in `service.rs`
(U2-owned): U1 scaffolds the flag surface and `--help` in `main.rs`, and U2
implements the demo dispatch there so it precedes real configuration and
discovery.

## 2. Isolation contract

One fresh private root per launch, under `/tmp`:

```text
/tmp/bloom-demo-<random>/            (0700)
  demo-session.json                  synthetic-only identity marker
  runtime/bloomd.sock
  data/store.db
  config/bloom/tui.json
  state/bloom/bloomd.log
  state/bloom/tui.log
  cache/attachments/
  home/
```

- One new coordination variable, `BLOOM_DEMO_ROOT`, is passed by the launcher.
  Derived paths are fixed and not independently overrideable. Only the launcher
  creates a demo root; a hand-set root with a hand-crafted `demo-session.json`
  may pass structural validation, which is accepted because the threat model is
  accidental access, not a hostile process.
- Guards: validate an absolute, owned, private root; reject symlinks, traversal,
  and unexpected files; check the socket-path length. "Unexpected" is measured
  against the top-level layout above only, not recursively: session data under
  `cache/attachments/`, `home/`, and the state logs is legitimate. Validation
  runs in every entry point, including direct daemon/TUI invocation with
  `BLOOM_DEMO_ROOT` set by hand, so a hostile or malformed root fails startup
  rather than being trusted.
- Demo ignores (never removes) `BLOOM_SOCKET_PATH`, legacy `BLOOM_SOCKET`,
  `BLOOM_STORE_PATH`, engine settings, opener settings, and inherited `HOME`/XDG
  destinations.
- Preferences and diagnostics resolve explicitly through demo context, covering
  direct TUI invocation and later saves.
- The daemon dispatches into demo startup before real configuration or discovery:
  no Flatpak or Dart discovery, no `default_engine_state()`, no `Supervisor`, no
  recovery construction, and no registration code.
- Open only the generated database via `EncryptedStore::open_with_key()` with a
  public fixed 32-byte synthetic demo key. Never construct `OsKeyring`. Never use
  `open_or_create()`, whose key lookup uses the real `bloom`/`bloomd.sqlite.v1`
  namespace.
- Refuse an existing non-demo database or session rather than importing or
  repairing it: refusal is enforced by the fresh-root guard, which rejects any
  pre-existing unexpected file under the new root, not by inspecting the
  database. Missing demo context must not downgrade to normal mode. A malformed
  root fails startup; a well-formed hand-built root is accepted because the
  threat model is accidental access, not a hostile process.
- Cleanup removes only the session root, after owned children exit. A crashed
  or killed session leaves its root behind; the next launch uses a fresh random
  root and never reuses, adopts, or repairs a stale one. Stale roots are swept
  manually, never automatically.
- Implementation seam: `Service` depends on a concrete `EngineSlot`. Add a
  narrowly scoped real/demo backend enum for engine-facing operations in
  `service.rs`. Keep store-backed history, search, and drafts plus
  persist-before-fanout shared. The demo backend has no engine configuration or
  spawn capability.
- Disable demo local-file upload and external opening initially.

## 3. Synthetic data

- `fixtures/demo/venue.json` plus a Rust loader/seeder under
  `apps/bloomd/src/demo/`. Generate the SQLCipher database at startup; never
  commit a binary.
- Reuse the stateful fixture structure: conversations, messages, drafts, scoped
  IDs, participants, parts, delivery states, reactions, and demo read positions.
- Seed four to six curated chats with natural multi-speaker exchanges, multiline
  text, varied lengths, grouped follow-ups, named synthetic attachments,
  read-only reactions, a parked draft, unread markers, and one longer history for
  paging and search.
- Use reserved `example.invalid` handles, stable IDs and order, and versioned
  fixture content. Fix the clock in capture tests.
- Correct participant membership rather than copying fixtures blindly.

## 4. Launcher and presentation

- `scripts/bloom --demo` builds `target/demo`, then invokes the installed-style
  `bloom --demo` launcher, avoiding a second lifecycle implementation.
- Fresh database and preferences every launch. Settings and simulated sends
  persist only within that session.
- Preserve themes, the startup checklist, activity tokens, and the passive rail.
- A restrained Demo indicator sits outside the transcript; captions disclose
  synthetic content.
- Static deterministic history only; there is no scripted traffic. Recordings
  repeat captures over the static history with a fixed clock; a capture over a
  fresh root persists no new events beyond its own session data. Capture mode is
  quiet by default.
- `/sync` and sends operate locally; no real recipient or account is contacted.

## 5. Verification and docs

Per-unit verification, each independently runnable before Section 5:

- U1: `cargo build --profile demo -p bloomd -p bloom-tui`, plus unit tests for
  daemon flag parsing, `--help`, and unknown/conflicting-argument rejection. The
  TUI binary's flag tests run at U4.
- U2: `cargo test -p bloomd` for root validation, the environment
  ignore-list, `open_with_key` use, non-demo refusal, and stale-root behavior.
- U3: `cargo test -p bloomd demo` for fixture referential integrity,
  deterministic seeding, and seeder output.
- U4: `cargo test -p bloom-tui` plus `scripts/test-launcher-e2e.py` for
  launcher flow, the indicator, and the session lifecycle.

Section 5 (integration and PTY) runs only after U1-U4 land. Isolation is
the acceptance gate.

- Unit-test option parsing, path derivation, missing context, and
  symlink/traversal refusal of unsafe roots.
- Test fixture referential integrity, deterministic seeding, drafts, reactions,
  and attachment metadata.
- Integration tests run the actual demo daemon and TUI with poisoned real
  socket, store, engine, `HOME`, and XDG sentinels; assert no connections,
  reads/writes, or sentinel changes.
- Add spies and traps proving demo startup never invokes the keychain or external
  discovery/spawn.
- Failed demo startup never falls through to normal startup.
- Test simultaneous isolated sessions, shutdown/signals, cleanup, fresh-launch
  reset, and unchanged normal launcher behavior.
- Extend `scripts/test-launcher-e2e.py`; add `apps/bloomd/tests/demo.rs` plus an
  actual-PTY demo test covering checklist completion, seeded chats/unread/draft,
  local send, `/exit`, `/quit`, narrow layout, and theme variants.
- Adapt `scripts/tui-shot.py`/`tui-shot.sh` to capture demo binaries and the demo
  session; review normal/narrow and light/dark/monochrome output.
- Add `docs/demo.md` linked from the root `README.md` and `docs/README.md`;
  update `docs/tui.md`, `handoff.md`, `status.md`, and the system design with the
  isolation contract and synthetic evidence boundary.
- Run the existing Rust, source-check, and synthetic Python gates.

## 6. Risks and boundaries

- Isolation is enforced in every entry point, not inferred from `target/demo` or
  environment. Normal mode never consults `BLOOM_DEMO_ROOT`.
- Engine coupling makes backend selection the main coding risk. Avoid a general
  connector architecture or duplicated IPC.
- The fixed demo key is public and gives no confidentiality for anything typed
  into demo. Document ephemeral storage.
- Demo mode is designed to prevent accidental real-state access; it is not a defense against a hostile-host compromise.
- No P4 redesign, live engine, real account, registration, remote traffic,
  historical import, or publication/deployment action.
