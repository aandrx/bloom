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
- `bloom-tui` binary: parse `--demo`, `--doctor`, and help; reject unknown or
  conflicting arguments.
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
  Derived paths are fixed and not independently overrideable.
- Guards: validate an absolute, owned, private root; reject symlinks, traversal,
  and unexpected files; check the socket-path length.
- Demo ignores or removes `BLOOM_SOCKET_PATH`, legacy `BLOOM_SOCKET`,
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
  repairing it. Missing demo context must not downgrade to normal mode.
- Cleanup removes only the session root, after owned children exit.
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
- Static deterministic history by default. Optional bounded scripted traffic for
  recordings produces real persisted demo events, not renderer-only animation.
  Capture mode is quiet by default.
- `/sync` and sends operate locally; no real recipient or account is contacted.

## 5. Verification and docs

Isolation is the acceptance gate.

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
  environment.
- Engine coupling makes backend selection the main coding risk. Avoid a general
  connector architecture or duplicated IPC.
- The fixed demo key is public and gives no confidentiality for anything typed
  into demo. Document ephemeral storage.
- Demo mode prevents accidental real-state access, not hostile-host compromise.
- No P4 redesign, live engine, real account, registration, remote traffic,
  historical import, or publication/deployment action.
