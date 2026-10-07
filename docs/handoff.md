# Fresh-session handoff

This is the entry point for a new maintainer or agent. It describes repository-backed state after the reliability/UI iteration, not an aspirational complete messenger. Read `../AGENTS.md` for safety and verification rules and `README.md` for the documentation index.

## Intent and decisions

Bloom should be a trustworthy keyboard-first Linux messenger with its own restrained terminal UI. Codex inspires layout restraint and discoverability, not agent/coding features. Correct recipient routing, preserved composition, durable ingestion and honest delivery/sync labels outrank decorative similarity.

The working route is a Rust TUI communicating with a local Rust daemon. The daemon owns an encrypted store and a headless Dart child that loads the user-installed OpenBubbles engine and reuses its registration. There is no Bloom-hosted messaging relay. This route is not independent Apple registration, a supported replacement for Apple's complete messaging feature set, or a standalone Cargo-only distribution.

The TUI is the current client. Desktop/mobile previews are illustrative. GPUI, direct registration and the excluded Matrix proof are not active implementation work. External engine licensing and version/ABI compatibility remain explicit boundaries.

## Working features and limits

- Conversation listing, known-participant contacts, local nicknames/titles/hiding, filtering, sender-grouped transcript, scrolling, light/dark/automatic themes, monochrome cues, mouse hitboxes and command pickers.
- Existing/new-chat text sends with accepted-ID correlation, event-driven delivery labels and explicit failure recovery. Uncertain sends are never automatically retried. Group text is conditional on the sidecar's recipient-list capability; no group management.
- Bracketed paste is literal, multiline preserves body whitespace, editing uses grapheme positions, resize redraws promptly. Session drafts are bound to recipient; modal suspension preserves body/cursor/paste provenance. Explicit cancel may discard new-chat composition. Drafts are not durable or stored in plaintext preferences.
- Local encrypted message storage, duplicate upserts, persistence before event fanout, reconnect snapshots and five-second visible-thread reconciliation. The TUI requests only the latest 200 stored messages. No pagination or message-content search.
- `/sync` refreshes conversation metadata and rereads local state. Request lifetime has an explicit spinner. Other inbound events infer activity for two seconds after the last event. Neither means remote archives were imported or the account is fully current. Failed/blocked/disconnected session states outrank activity.
- Installed `bloom` locates executable companions, starts a daemon if needed, runs the TUI and cleans up only children it owns. It allows 30 seconds for daemon drain, restores terminal modes, and uses private daemon logs. Reused TUIs depend on the original launcher's daemon lifetime.

Missing: remote historical import, automatic engine-generation restart, Bloom sign-in/activation/account recovery, attachments send/download/open, reactions/edits/unsend workflows, unread counters, undo and standalone packaging. Domain types and historical live experiments do not establish these features.

## Implementation map

| Path | Responsibility |
| --- | --- |
| `apps/bloom-tui/src/app.rs` | Interaction state, drafts, commands, recovery, identity-aware selection |
| `apps/bloom-tui/src/composer.rs` | Grapheme wrapping and cursor geometry |
| `apps/bloom-tui/src/ui.rs` | Palettes, responsive geometry, transcript, pickers and hitboxes |
| `apps/bloom-tui/src/main.rs` | Terminal lifecycle, asynchronous input/events/results, redraw scheduling |
| `apps/bloom-tui/src/daemon.rs` | IPC reconnect, subscription/snapshot ordering and local reconciliation |
| `apps/bloom-tui/src/launcher.rs`, `src/bin/bloom.rs` | Installed launcher and process ownership |
| `apps/bloom-tui/src/config.rs`, `diagnostics.rs` | Local preferences and bounded allowlisted TUI diagnostics |
| `apps/bloomd/src/engine.rs` | Sidecar protocol, payload mapping, stream failure and child lifecycle |
| `apps/bloomd/src/service.rs`, `store/` | Request semantics, encrypted persistence and identity reconciliation |
| `apps/bloomd/src/{main,config,supervisor,session}.rs`, `ipc/` | Daemon assembly, runtime discovery and authenticated IPC |
| `crates/bloom-domain`, `bloom-ipc`, `bloom-transport`, `bloom-secrets` | Types, wire/client, transport contract and key storage |
| `crates/bloom-core` | Legacy foundation retained in workspace; not current TUI wiring |
| `ext/bloom-engine-sidecar/dart` | Owned Dart orchestration/codec/protocol plus pinned generated bridge |
| `scripts/` and app tests | Synthetic fixtures, actual PTY/process tests and visual review |

## Safe test setup

Do not run the real application merely to inspect its UI. `bloomd` discovers home registration automatically and can consume pushes. Use the scripts below; they use synthetic messages and private temporary sockets.

```sh
cargo build -p bloom-tui
cargo test --workspace --all-targets
TZ=Asia/Tokyo cargo test -p bloom-tui
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
./scripts/check-sources
python3 -m venv /path/to/private/test-venv
/path/to/private/test-venv/bin/pip install pyte==0.8.2
/path/to/private/test-venv/bin/python scripts/test-stateful-fake-daemon.py
/path/to/private/test-venv/bin/python scripts/test-tui-input-e2e.py
/path/to/private/test-venv/bin/python scripts/test-tui-messaging-e2e.py
/path/to/private/test-venv/bin/python scripts/test-launcher-e2e.py
```

The scripts use `target/debug/bloom-tui` and `target/debug/bloom`, so rebuild binaries after source changes; tests alone can leave the executable fixtures stale. Linux is the documented/tested runtime. PTY tests need Python with pyte; the launcher suite includes `/proc`-based fixture cleanup. Dependencies require network access on first setup.

Dart checks, from `ext/bloom-engine-sidecar/dart`:

```sh
dart pub get
dart test
dart analyze bin lib/payload_codec.dart lib/protocol.dart lib/single_flight.dart test
dart format --output=none --set-exit-if-changed bin lib/payload_codec.dart lib/protocol.dart lib/single_flight.dart test
```

Generated bindings are deliberately excluded. CI is `.github/workflows/check.yml`; it enforces these Rust/Dart gates, synthetic Python suites and the default transport dependency boundary.

Visual review: `scripts/tui-review.sh OUTPUT_DIR` captures a 31-state synthetic matrix, isolates preferences/logs, and creates HTML/PNG review sheets. Tooling currently assumes the local Python helper at `~/.cache/bloom-tools` and Pi browser helper location; adapt these tooling paths for another machine, not product runtime. Review runs need Node/browser tools as well as Python. `scripts/tui-shot.sh` captures individual states. Stop/reap isolated helpers when finished. Do not edit scripts/renderers during a running matrix; rerun affected states after follow-up changes.

`check-guards` is an optional mutation proof tool for clean files only. Its manifest is not proof a guard was recently exercised; never discard dirty changes to satisfy it.

## Real runtime setup is a separate authorized action

See `../README.md` and `tui.md`. Requires pinned Rust/Dart, a compatible installed OpenBubbles engine and existing registration, resolved sidecar packages, and exclusive engine-state ownership. From the checkout, install `apps/bloomd` and `apps/bloom-tui` separately. Cargo installs three Rust executables, not Dart or the sidecar. Keep the sidecar tree and export an absolute `BLOOM_ENGINE_SCRIPT`. `$CARGO_HOME/bin` must be on PATH. The installed launcher does not source `.env`; the repository script sources a trusted repository-root `.env`.

Never delete a database to work around a missing keychain entry. Never start a second account owner to diagnose a socket error. External engine logs are not covered by the TUI's diagnostic allowlist; review before sharing.

## Latest evidence and handoff state

`iteration-review.md` records the accepted changes, parent critique and synthetic verification. Workspace all-target tests, TUI timezone tests, strict Clippy/formatting, source guard, 56 Dart tests/analysis/formatting, 8 fixture tests, 11 input PTY tests, 4 messaging PTY tests and 8 actual-launcher tests passed in the iteration. Handoff preparation reran those gates successfully and proved all seven mutation guards against clean committed source, repairing a stale filter guard before rerunning normal tests. Counts are a historical checkpoint, not a substitute for rerunning tests after a new change.

Temporary visual evidence: `/tmp/bloom-full-review-final/` (31 states plus focused recaptures). These files are disposable and not committed; absence in another session is expected. Reproduce using the scripts. Earlier visual jobs were stopped/failed during iteration and replaced; do not treat those partial outputs as verification. Independent audits informed the plan, but model usage limits interrupted implementations; the parent rejected partial work and finished/reviewed it directly. There was no fresh independent post-implementation reviewer pass.

The Git history and working tree are the source of truth, not a session summary. Run `git status -sb`, `git log -5 --oneline`, and inspect uncommitted changes before starting. No prior permission to access a live account carries into a fresh session.

## Next work

Follow `implementation-plan.md`: generation-scoped engine recovery first, historical ingestion only after pinned-engine semantics are verified, then local pagination/search/unread navigation and standalone runtime packaging. Do not implement archives by assuming CloudKit/generated method names establish cursor/tombstone semantics. Do not turn unknown remote acceptance into an automatic resend. A visual score near 8/10 for reviewed states does not make overall product reliability 8/10.
