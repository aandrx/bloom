# Fresh-session handoff

This is the entry point for a new maintainer or agent. It describes repository-backed state after the reliability/UI iteration, not an aspirational complete messenger. Read `../AGENTS.md` for safety and verification rules and `README.md` for the documentation index.

## Intent and decisions

Bloom should be a trustworthy keyboard-first Linux messenger with its own restrained terminal UI. Codex inspires layout restraint and discoverability, not agent/coding features. Correct recipient routing, preserved composition, durable ingestion and honest delivery/sync labels outrank decorative similarity.

The working route is a Rust TUI communicating with a local Rust daemon. The daemon owns an encrypted store and a headless Dart child that loads the user-installed OpenBubbles engine and reuses its registration. There is no Bloom-hosted messaging relay. This route is not independent Apple registration, a supported replacement for Apple's complete messaging feature set, or a standalone Cargo-only distribution.

The TUI is the current client. Desktop/mobile previews are illustrative. GPUI, direct registration and the excluded Matrix proof are not active implementation work. External engine licensing and version/ABI compatibility remain explicit boundaries.

## Working features and limits

- Conversation listing, known-participant contacts, local nicknames/titles/hiding, filtering, sender-grouped transcript, scrolling, light/dark/automatic themes, monochrome cues, mouse hitboxes and command pickers.
- Reading is anchored to conversation/message/row across arrivals, reconciliation, edits and resize. End/clickable Latest explicitly resumes following; painted overlays consume their own wheel input.
- Below 70 columns, conversations use full width and Chats opens a dedicated searchable navigator. Query/candidate do not retarget a parked draft; Esc cancels and Enter/click commits.
- Existing/new-chat text sends with accepted-ID correlation, event-driven delivery labels and explicit failure recovery. Uncertain sends are never automatically retried. Group text is conditional on the sidecar's recipient-list capability; no group management.
- Bracketed paste is literal, multiline preserves body whitespace, editing uses grapheme positions, resize redraws promptly. Session drafts are bound to recipient; modal suspension preserves body/cursor/paste provenance. Explicit cancel may discard new-chat composition. Drafts are not durable or stored in plaintext preferences. Ctrl+Z and Ctrl+Y provide bounded session-only undo and redo; history clears on owner changes, send, and cancellation.
- Local encrypted message storage, duplicate upserts, persistence before event fanout, reconnect snapshots and five-second visible-thread reconciliation. The transcript opens the newest 200 stored messages and loads earlier pages from local storage when the reader reaches the oldest loaded row (protocol version 7; keyset paging was introduced earlier, one request at a time, bounded session budget). Paged history survives reconciliation for that chat only. Ctrl+F or `/search` searches stored message text and explicitly reported attachment file names locally. A body match wins within each message; otherwise the first matching nonempty filename supplies the hit. Filename-only snippets carry a `Filename: ` prefix and the name is shown as an escaped `[attachment: NAME]` label; missing or empty names stay generic `[attachment]`. No ID, MIME, URL or content inference is used. Loaded hits anchor directly; unloaded hits open a separate exact-context preview capped at 200 rows, without accumulating intermediate pages. The preview stays fixed through reconciliation and arrivals; End or Latest returns to normal history. Context requests are generation-correlated and never mark the chat read. Search covers stored text parts and explicitly reported attachment filenames, not attachment contents. There is no remote historical import.
- Local unread badges count incoming loaded messages newer than a saved marker. `/unread` or Ctrl+B then `u` jumps to the first such message. A successfully displayed latest transcript advances the marker; reading back, overlays, and absent transcripts do not. Without a marker or loaded history, unread state is unknown. Markers persist as IDs and timestamps in private but unencrypted TUI preferences, with no automatic expiry. They are not remote read receipts.
- `/sync` refreshes conversation metadata and rereads local state. Request lifetime has an explicit spinner. Other inbound events infer activity for two seconds after the last event. Neither means remote archives were imported or the account is fully current. Failed/blocked/disconnected session states outrank activity.
- Installed `bloom` locates executable companions, starts a daemon if needed, runs the TUI and cleans up only children it owns. It allows 30 seconds for daemon drain, restores terminal modes, and uses private daemon logs. Reused TUIs depend on the original launcher's daemon lifetime.

Generation-scoped engine restart is implemented with synthetic real-child replacement evidence, bounded retries, old-generation drain/join, and no uncertain-send replay; the real Dart sidecar restart remains live-unverified. Inbound built-in reactions are mapped, persisted, fixture-preserved, and shown as read-only aggregate counts in the real TUI PTY. Inbound text edits and whole-body unsends are also applied transactionally with persisted engine part identities and truthful `Edited` / `Unsent · retained locally` notes; ambiguous legacy rows and partial multipart unsends are skipped. Missing: remote historical import, Bloom sign-in/activation/account recovery, attachment send/download/open, outbound reactions/edits/unsend, authoritative unread totals for all conversations, durable drafts, and standalone packaging.

## Implementation map

| Path | Responsibility |
| --- | --- |
| `apps/bloom-tui/src/app.rs` | Interaction state, drafts, commands, recovery, identity-aware selection |
| `apps/bloom-tui/src/composer.rs` | Grapheme wrapping and cursor geometry |
| `apps/bloom-tui/src/transcript.rs` | Message/row reading anchors and renderer-measured viewport geometry |
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
/path/to/private/test-venv/bin/python scripts/test-tui-reading-e2e.py
/path/to/private/test-venv/bin/python scripts/test-launcher-e2e.py
```

The optional `python scripts/benchmark-tui-switching.py` probe measures synthetic input-to-readable-state switching latency; `--body-repeat 100` stresses long messages, and `--columns 44` covers narrow navigation. It reports timings, not a CI wall-clock gate; deterministic cache/output tests guard regressions.

The scripts use `target/debug/bloom-tui` and `target/debug/bloom`, so rebuild binaries after source changes; tests alone can leave the executable fixtures stale. Linux is the documented/tested runtime. PTY tests need Python with pyte; the launcher suite includes `/proc`-based fixture cleanup. Dependencies require network access on first setup.

Dart checks, from `ext/bloom-engine-sidecar/dart`:

```sh
dart pub get
dart test
dart analyze bin lib/payload_codec.dart lib/protocol.dart lib/single_flight.dart test
dart format --output=none --set-exit-if-changed bin lib/payload_codec.dart lib/protocol.dart lib/single_flight.dart test
```

Generated bindings are deliberately excluded. CI is `.github/workflows/check.yml`; it enforces these Rust/Dart gates, synthetic Python suites and the default transport dependency boundary.

Visual review: `scripts/tui-review.sh OUTPUT_DIR` captures a 37-state synthetic matrix, isolates preferences/logs, and creates HTML/PNG review sheets. Tooling currently assumes the local Python helper at `~/.cache/bloom-tools` and Pi browser helper location; adapt these tooling paths for another machine, not product runtime. Review runs need Node/browser tools as well as Python. `scripts/tui-shot.sh` captures individual states. Stop/reap isolated helpers when finished. Do not edit scripts/renderers during a running matrix; rerun affected states after follow-up changes.

`check-guards` is an optional mutation proof tool for clean files only. Its manifest is not proof a guard was recently exercised; never discard dirty changes to satisfy it.

## Real runtime setup is a separate authorized action

See `../README.md` and `tui.md`. Requires pinned Rust/Dart, a compatible installed OpenBubbles engine and existing registration, resolved sidecar packages, and exclusive engine-state ownership. From the checkout, install `apps/bloomd` and `apps/bloom-tui` separately. Cargo installs three Rust executables, not Dart or the sidecar. Keep the sidecar tree and export an absolute `BLOOM_ENGINE_SCRIPT`. `$CARGO_HOME/bin` must be on PATH. The installed launcher does not source `.env`; the repository script sources a trusted repository-root `.env`.

Never delete a database to work around a missing keychain entry. Never start a second account owner to diagnose a socket error. External engine logs are not covered by the TUI's diagnostic allowlist; review before sharing.

## Latest evidence and handoff state

The current [switching-latency follow-up](tui-switching-performance.md) records fresh synthetic PTY reproduction, bounded layout caching/windowed painting and verification. Prior UI polish verification, independent source review, reference limits and parent visual critique are recorded in [the reading/navigation review](ux-polish-review.md). Use these records for the latest tree rather than treating the counts below as current.

Phase 0 stabilization reran the complete Rust/Python gate, Tokyo-timezone TUI tests, strict Clippy, formatting, source guard, normal builds, 8 fixture tests, 11 input PTY tests, 4 messaging PTY tests, 7 reading/navigation PTY tests, 8 launcher tests, Dart 3.13.4 formatting/analysis/all 56 tests, the complete 37-state visual matrix, and final switching probes. Current TUI counts are 67 library and 88 render tests. Three new mutation guards prove stale-click, trimmed local-title search, and narrow-resize overlay ownership coverage. No live account or real engine was accessed.

Phase 1 instrumentation and snapshot work: the switching probe now labels the single genuinely cold first visit separately from warm repeats, and an isolated local probe measured one full first render at 31-35 ms (medium bodies) and 213-216 ms (long bodies) in the debug profile `scripts/bloom` builds, against 4 ms and 22 ms for the same code in release (single release run). That points at the build profile rather than redundant layout work, though the end-to-end first visit also carries startup and IPC. Selection snapshots now start conversation-list and history reads concurrently while keeping the list authoritative and cancelling a speculative read that no longer matches the selection. Two cold-layout micro-optimizations were measured, found below noise, and reverted; one of them had a whitespace-only counting defect caught in review. No live account or real engine was accessed.

### Previous reliability/UI iteration

`iteration-review.md` records the accepted changes, parent critique and synthetic verification. Workspace all-target tests, TUI timezone tests, strict Clippy/formatting, source guard, 56 Dart tests/analysis/formatting, 8 fixture tests, 11 input PTY tests, 4 messaging PTY tests and 8 actual-launcher tests passed in the iteration. Handoff preparation reran those gates successfully and proved all seven mutation guards against clean committed source, repairing a stale filter guard before rerunning normal tests. Counts are a historical checkpoint, not a substitute for rerunning tests after a new change.

Temporary visual evidence: `/tmp/bloom-full-review-final/` (31 states plus focused recaptures). These files are disposable and not committed; absence in another session is expected. Reproduce using the scripts. Earlier visual jobs were stopped/failed during iteration and replaced; do not treat those partial outputs as verification. Independent audits informed the plan, but model usage limits interrupted implementations; the parent rejected partial work and finished/reviewed it directly. There was no fresh independent post-implementation reviewer pass.

The Git history and working tree are the source of truth, not a session summary. Run `git status -sb`, `git log -5 --oneline`, and inspect uncommitted changes before starting. No prior permission to access a live account carries into a fresh session.

## Next work

The user explicitly deferred multi-service integration to focus on current-chat UI/UX, with high interaction/design standards informed by Herdr, Pi, Codex and Claude Code. [The active UI iteration](ux-polish-review.md) records fresh E2E reproductions, selected changes and acceptance evidence. Preserve draft/routing safety and do not add workspace/server scaffolding or connector refactors in this iteration.

[Multi-service research](multi-service-research.md) remains a deferred reference, not implemented functionality or permission to access accounts.

Generation-scoped engine recovery and inbound reaction display are also accepted with synthetic evidence in their linked records. The transport reaction capability remains false because there is no outbound operation and none is planned for this project. Deep-hit navigation and participant-context search are accepted under shipp with synthetic evidence in their linked records. Local text search, unread navigation, and session undo are implemented with synthetic coverage. Native filename search was introduced in IPC v6 and remains available under current IPC v7; it is recorded in the [filename-search contract](filename-search.md): per-message body-first matching, then the first explicit nonempty filename, with no ID, MIME, URL or content inference. Matching keeps the same global scan bound, canonical membership before the cap, one hit per message, and hit ordering. Scoped component checks pass. Parent completed the Rust/static/build checks and the runtime rerun passes 61 fixture, 15 reading, 12 input, 4 messaging, and 8 launcher tests. One earlier legacy unread RPC timeout remains an unresolved observation; final cross-component source review found no confirmed new defect. Filename search is accepted at the documented synthetic store/IPC/real-TUI boundary and committed locally; it is not deployed, and no attachment sending, download, or content extraction is enabled. The consolidated universal synthetic gate passes after the roadmap reconciliation: workspace/all-target and Tokyo tests, strict workspace Clippy, formatting, dependency and source guards, rebuilt binaries, 65 fixture tests, 13 input PTY tests, 4 messaging PTY tests, 16 reading PTY tests, 8 launcher tests, and 58 Dart tests plus format/analyze. No repair is required before another feature. Verified historical ingestion remains blocked, and standalone runtime packaging phase 4 stays parked until the intermittent frame fault has a concrete new lead. Local keyset paging is implemented end to end: the transcript loads earlier pages from Bloom's store when the reader reaches the oldest loaded row. Do not implement archives by assuming CloudKit/generated method names establish cursor/tombstone semantics. Do not turn unknown remote acceptance into an automatic resend. Polished reviewed states do not establish overall product reliability.
