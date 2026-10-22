# Bloom status

**Current implementation, not intended end state.** Start with [handoff](handoff.md) and [documentation map](README.md). The [reading/navigation review](ux-polish-review.md) records that accepted UI scope; the [switching-latency follow-up](tui-switching-performance.md) records the latest rendering correction and measurements; [the preceding iteration review](iteration-review.md) remains historical evidence. “Verified” identifies a test/run boundary, not broad production reliability.

## Implemented with synthetic regression coverage

- Rust daemon serving authenticated local Unix-socket IPC, external Dart engine adapter, encrypted SQLite storage/keychain integration, idempotent message ingestion and persistence before inbound fanout.
- TUI conversation list, local stored transcript, sender grouping, text composition/sending, existing/new-chat correlation, event-driven delivery labels, uncertain-outcome warnings and explicit failed-draft recovery.
- Recipient-owned session drafts and modal suspension, literal bracketed paste, grapheme editing/wrapping, exact multiline whitespace and responsive resize.
- On-demand older-page loading from local storage: reaching the oldest loaded row fetches the previous keyset page, keeps it across reconciliation for chats you paged back, and bounds retained paged history. Chats you never paged still replace wholesale, and no remote history is fetched.
- Message-owned reading anchors across arrivals/reconciliation/edits/reflow and canonical identities; measured mouse/End Latest action and painted-overlay wheel ownership.
- Below 70 columns, full-width conversations with a searchable Chats navigator. Query/candidate stay separate from actual recipient and parked composition until explicit open.
- Known-participant contacts/search/local nicknames, local titles/hiding, conversation filtering, settings/theme/command pickers, keyboard/mouse paths, light/dark/automatic themes and monochrome cues.
- Metadata `/sync` with explicit in-flight lifetime and post-refresh local reread. Inbound message/update/delivery/list events infer activity until two quiet seconds. Failed/blocked/disconnected session states outrank activity. This is not archive completion.
- Coalesced redraw scheduling, responsive identity/status priority and neutral selection surfaces. A four-entry session-only transcript layout cache and visible-line painting avoid repeated whole-history work during warm switching; cold or changed large histories and IPC refresh remain separate costs.
- Installed `bloom` command with sibling/PATH companions, owned-child cleanup, reused-daemon preservation, bounded drain and terminal restoration. Cargo installs binaries only; the sidecar/Dart/engine/registration remain external runtime prerequisites.

## Partial or missing

| Area | Boundary |
| --- | --- |
| History | Only Bloom's stored messages; the transcript opens the newest 200 and loads earlier pages from local storage on demand (bounded paged budget, session only); no remote/app historical import |
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
- `scripts/test-stateful-fake-daemon.py`: persistent synthetic fixture contract. `test-tui-input-e2e.py`, `test-tui-messaging-e2e.py` and `test-tui-reading-e2e.py`: actual TUI processes in PTYs, including reading-anchor, overlay wheel and narrow recipient-navigation safety. `test-launcher-e2e.py`: actual installed-entry binary with synthetic children, including signal cleanup, slow drain, private logs and terminal modes.
- Dart tests exercise owned protocol/codec/coordination/configuration without loading the real engine. CI covers owned-source analysis/formatting/tests and Rust/Python regression suites; see `.github/workflows/check.yml`.
- For the preceding reading/navigation scope, parent reviewed the complete 37-state synthetic visual matrix at `/tmp/bloom-ux-final`, plus final-source focused recaptures at `/tmp/bloom-ux-accepted-visual`. Final workspace/timezone/lint/source gates and all five reading/navigation PTY tests passed; all 13 mutation guards proved named failures in an isolated disposable snapshot. See the UI review for exact gate boundaries and logs. Artifacts are ephemeral terminal-emulator reconstructions, not universal font/terminal evidence.

- Switching performance follow-up: workspace/timezone/lint/source and all synthetic fixture/input/messaging/reading/launcher suites passed; five cache regressions and three performance mutation guards cover reuse, invalidation and viewport-bounded painting. Phase 0 stabilization added three drawer regressions, narrow-resize overlay ownership coverage, a missed-event reconciliation PTY proof, a leader/mouse PTY regression, and three proven mutation guards. The current suite has 88 render tests and 7 reading/navigation PTY tests; Dart 3.13.4 formatting, analysis and all 56 owned tests pass. Six focused captures have zero pixel differences from the earlier output, and the final 37-state matrix was directly inspected. Medium-history switches measured about 200 ms before versus 18-20 ms warm afterward; long-history local panels about 670 ms before versus 8-12 ms afterward. Phase 1 added an isolated first-visit measurement and concurrent conversation-list/history snapshot reads; that measurement attributes the remaining cold render cost mainly to the debug profile `scripts/bloom` builds (31-35 ms versus 4 ms for medium bodies, 213-216 ms versus 22 ms for long ones, release measured once) rather than to redundant layout work. This is private synthetic PTY evidence, not a live-account or compositor benchmark. Cold first visits can still take more; see the performance record.

Historical authorized live state-restore, receive/send/restart and conversation-list runs are recorded in [engine-sidecar evidence](engine-sidecar.md). They establish those recorded runs, not current-checkout reliability, remote archives or renewal. `apps/bloomd/tests/engine_e2e.rs` is ignored by default and may run only with fresh explicit authorization. Its existence is not a passing live test.

Independent source review for the current UI pass found a canonical-alias reading reset; parent reproduced and fixed it with a regression. A later Phase 0 review found stale drawer-click fallback, incomplete drawer search, leaked leader state, and weak reconciliation/guard assertions. Parent reproduced and fixed them; the final scoped rereview found no remaining actionable findings. Reviewers did not run builds/tests or inspect screenshots. Parent owns runtime/visual acceptance and later follow-up regressions. The preceding iteration's interrupted delegation is recorded separately in its historical review; do not treat a source review as broad product sign-off.

## Release blockers and next work

The user-directed priority remains UI/UX; this bounded reading/navigation pass is accepted, not permission to resume integrations or access a live account. [The roadmap](implementation-plan.md) keeps backend work queued: generation-scoped engine recovery, verified historical ingestion semantics/implementation, message search and unread navigation, then standalone runtime packaging. Keep the existing registration exclusively owned and do not automatically replay uncertain sends. [IDS validation](ids-validation-gate.md) remains separate and blocked.

Optional mutation guards in `scripts/check-guards` require clean files and restore their mutations; they are not CI evidence or permission to reset working changes. Inspect the manifest and current tests before running them.
