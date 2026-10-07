# Bloom status

**Current implementation, not intended end state.** Start with [handoff](handoff.md) and [documentation map](README.md). The [latest iteration review](iteration-review.md) records accepted changes and tests. “Verified” identifies a test/run boundary, not broad production reliability.

## Implemented with synthetic regression coverage

- Rust daemon serving authenticated local Unix-socket IPC, external Dart engine adapter, encrypted SQLite storage/keychain integration, idempotent message ingestion and persistence before inbound fanout.
- TUI conversation list, local stored transcript, sender grouping, text composition/sending, existing/new-chat correlation, event-driven delivery labels, uncertain-outcome warnings and explicit failed-draft recovery.
- Recipient-owned session drafts and modal suspension, literal bracketed paste, grapheme editing/wrapping, exact multiline whitespace and responsive resize.
- Known-participant contacts/search/local nicknames, local titles/hiding, conversation filtering, settings/theme/command pickers, keyboard/mouse paths, light/dark/automatic themes and monochrome cues.
- Metadata `/sync` with explicit in-flight lifetime and post-refresh local reread. Inbound message/update/delivery/list events infer activity until two quiet seconds. Failed/blocked/disconnected session states outrank activity. This is not archive completion.
- Coalesced redraw scheduling, responsive identity/status priority and neutral selection surfaces.
- Installed `bloom` command with sibling/PATH companions, owned-child cleanup, reused-daemon preservation, bounded drain and terminal restoration. Cargo installs binaries only; the sidecar/Dart/engine/registration remain external runtime prerequisites.

## Partial or missing

| Area | Boundary |
| --- | --- |
| History | Only Bloom's stored messages; TUI latest-200 limit; no local pagination or remote/app historical import |
| Live service | Synthetic sidecar/IPC/PTY coverage; current real-engine changes not live-revalidated; no automatic engine-generation recovery |
| Groups | Existing group text conditional on recipient-list capability; no group-management workflow |
| Rich messaging | Unsupported inbound parts are placeholders; no attachment send/open/download, reactions, edits or unsend UI |
| Delivery | Labels reflect mapped transport events; presence of a Read enum is not proof of remote read-receipt support |
| Accounts | Reuses an existing registration; no Bloom sign-in/activation/recovery; renewal unconfirmed |
| Composition/reading | Session-only drafts, no undo, unread count, message-content search or reviewed encrypted durable drafts |
| Packaging/platforms | Linux/OpenBubbles Flatpak is the documented setup; not a standalone Cargo-only or Windows distribution |
| Accessibility | Keyboard/grapheme/monochrome/light-theme coverage, not broad terminal/font/screen-reader validation |
| Future GUI/transport | GPUI not started; independent registration blocked at Gate 0; Matrix remains an excluded proof |

## Evidence

- `apps/bloomd/tests/engine_sidecar.rs`: real daemon/service/store code with a fake child stdio sidecar, including persistence, duplicate handling, identity/routing, metadata sync, malformed output and shutdown drain.
- TUI library/interactions/render tests: state, ownership/recovery, cursor/palette/hitboxes and responsive geometry. `daemon_session.rs` uses a real synthetic Unix socket for subscription/snapshot/reconciliation/reconnect.
- `scripts/test-stateful-fake-daemon.py`: persistent synthetic fixture contract. `test-tui-input-e2e.py` and `test-tui-messaging-e2e.py`: actual TUI processes in PTYs. `test-launcher-e2e.py`: actual installed-entry binary with synthetic children, including signal cleanup, slow drain, private logs and terminal modes.
- Dart tests exercise owned protocol/codec/coordination/configuration without loading the real engine. CI covers owned-source analysis/formatting/tests and Rust/Python regression suites; see `.github/workflows/check.yml`.
- Parent reviewed a 31-state synthetic visual matrix and focused recaptures. Artifacts under `/tmp/bloom-full-review-final` are ephemeral; the capture scripts reproduce them. These are terminal-emulator reconstructions, not universal font/terminal evidence.

Historical authorized live state-restore, receive/send/restart and conversation-list runs are recorded in [engine-sidecar evidence](engine-sidecar.md). They establish those recorded runs, not current-checkout reliability, remote archives or renewal. `apps/bloomd/tests/engine_e2e.rs` is ignored by default and may run only with fresh explicit authorization. Its existence is not a passing live test.

Independent audits informed the recent iteration; usage limits interrupted subagent implementations and prevented a fresh independent final-review pass. The parent rejected partial results, completed changes and reviewed source/tests/visual output. Do not imply an independent critic approved the final tree.

## Release blockers and next work

[Active roadmap](implementation-plan.md): generation-scoped engine recovery, verified historical ingestion semantics/implementation, local pagination/search/unread navigation, then standalone runtime packaging. Keep the existing registration exclusively owned and do not automatically replay uncertain sends. [IDS validation](ids-validation-gate.md) remains separate and blocked.

Optional mutation guards in `scripts/check-guards` require clean files and restore their mutations; they are not CI evidence or permission to reset working changes. Inspect the manifest and current tests before running them.
