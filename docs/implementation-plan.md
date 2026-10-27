# Implementation roadmap

**Status: active next-work plan, not completed features.** The [next iteration plan](next-iteration-plan.md) proposes the sequence for the remaining live, packaging, rich-message, GPUI and multi-service tracks. Replaces the obsolete foundation/direct-transport wave list. Read [handoff](handoff.md), [current design](system-design.md), [status](status.md) and [completed iteration](iteration-review.md) first.

**Current user priority: consolidate the accepted client before adding features.** The reading/navigation loop, sidebar viewport scrolling, filename search, generation-scoped engine recovery, inbound reaction display, inbound attachments, and the opt-in release launcher are implemented with synthetic evidence. Durable drafts are now implemented across the encrypted store, strict IPC v9, a debounced TUI coordinator, exact-owner restart restore, send/cancel deletion, and uncertainty after an unsettled quit. Outbound attachment send has a synthetic slice with live delivery deferred; the outbound edit/unsend gate passes structurally. The latest consolidated universal synthetic gate passes: workspace/all-target and Tokyo tests, strict workspace Clippy, formatting, dependency and source guards, rebuilt binaries, 68 fixture tests, 13 input PTY tests, 8 messaging PTY tests, 26 reading PTY tests, 8 launcher tests, and 99 Dart tests plus format/analyze. Historical import remains blocked, packaging phase 4's live gate passed once after stdout containment; the [4c release-paperwork review](packaging-4c-release-paperwork.md) leaves distribution blocked by missing license/NOTICE artifacts, third-party notices, the SSPL/closed-engine review and an exact-tag-only range; repeated-fault evidence remains open, and no outbound reaction work is planned.

Multi-service integration and workspace/server navigation are deferred. Preserve [multi-service-research.md](multi-service-research.md) as research, not an active implementation instruction.

The [sidebar viewport follow-up](sidebar-scroll-plan.md) is implemented: wheel over the sidebar scrolls the viewport instead of changing chats, a scrollbar shows position and supports track click/drag, every visible chat including the last row is clickable, selecting while a new chat is composed parks that draft, and keyboard selection reveals the selected row. App/render tests and a real 24-chat PTY flow pass; no live access was used.

## Completed foundation

Rust daemon/domain/IPC/secret-store boundary, encrypted persistence, external Dart engine integration, local conversation/message storage, TUI text sends and safe new-chat correlation, known-participant contacts/local preferences, exact direct identity reconciliation, literal multiline paste, recipient-bound session drafts/modal suspension, metadata sync request tracking, neutral responsive UI and installed launcher lifecycle have synthetic regression coverage. The daily loop now also has keyset paging, local message search with paging jumps, and session composer undo, all with synthetic regression coverage. Cargo installs commands but not the complete runtime. Current live engine, historical import and renewal remain separate unverified gates.

## Current priority and next action

The section numbers below group work by area; they are not execution order.

1. Native filename search is implemented on IPC v6 and is recorded in the [filename-search contract](filename-search.md). Matcher units pass 118 parent-run daemon tests and static checks; the fixture component passes 61 parent-run tests with 64 golden cases and 468 checked Mc scalars matching pinned Rust; the UI passes parent TUI/Tokyo/static checks, 119 render tests, and rebuilt named-context PTY probes at 100/44/24 columns; the persistent owner/cursor/send case passes the full 15-case reading suite. Parent completed the Rust/static/build checks and the runtime rerun passes all suites. One earlier legacy unread RPC timeout remains unresolved. Final cross-component source review found no confirmed new defect. The filename feature is accepted at the documented synthetic store/IPC/real-TUI boundary, with its limits explicit. The feature is committed locally but not deployed. The accepted [metadata-preservation prerequisite](attachment-metadata.md) and [baseline gap](attachment-search-gap.md) remain historical records; do not redispatch them. Never search placeholders or infer filenames.
2. Participant-context search and deep-hit navigation are accepted with the evidence below. Attachment sending, download/open, content extraction, and remote history remain outside these metadata/search units.
3. Generation-scoped engine recovery, inbound reaction/edit/unsend handling, and durable drafts are accepted with synthetic evidence, and the consolidated universal gate passes. No repair is required before choosing another feature. Historical ingestion remains blocked; packaging phase 4 passed one live gate but the [4c review](packaging-4c-release-paperwork.md) leaves distribution blocked; inbound edits/unsend are the next optional rich-message correctness unit.

Keep the launcher on debug. Do not add durable drafts, remote read receipts, or multi-service work to this unit.

Local unread navigation is accepted under shipp with synthetic evidence. Workspace all-target tests, Tokyo-timezone TUI tests, strict workspace Clippy, formatting, and source guard pass after the review fixes. Final rebuilt runtime suites pass: 10 fixture, 12 input, 4 messaging, 10 reading/navigation, and 8 launcher tests. The scoped follow-up source review found no confirmed defects in the corrections. Regressions were observed failing before fixes for stranded alias markers, outgoing unread counts, and competing search jumps. The actual PTY flow also proves saved marker advancement after drawing and a first-unread jump distinct from latest. A settings-save defect noted here is fixed: a failed save requested no redraw and retried the same failing config on later iterations. The loop now suppresses retries of a config value that already failed, requests a frame on failure, and clears only its own stale message when nothing is pending. Unit tests cover suppression and the pending-change branch, and an actual-PTY case shows the error painting within 1.5 seconds. A red-proof showed that PTY flow also passes without the redraw request, so it bounds visible latency rather than isolating the fix. No live account or engine was accessed.

## Queued backend/product work and acceptance

### 1. Generation-scoped engine recovery (accepted, live child replacement verified)

Historical message ingestion has a [design-only plan](historical-ingestion-plan.md). It is blocked: the sidecar exposes no message archive, `history_message` and `history_complete` are stubs, and the headless ObjectBox path is unavailable. Do not start that work without the gate evidence, and do not fabricate identity, dedupe by body or time, or claim completeness from a placeholder frame.

The [engine recovery plan](engine-recovery-plan.md) is implemented and wired. A pure bounded retry policy, a swappable engine slot, a drain-and-join generation seam, an unwired-then-wired restart loop, and TUI outage labels all pass synthetic checks, including a runtime test that kills a real child process and observes a replacement. Crash loops now exhaust the attempt budget because a generation must stay up for a stability window before it resets the budget. Initial start failure stays fatal, uncertain sends are never replayed. An authorized live probe killed the real sidecar child once: the daemon survived, the old child was reaped, a different child appeared after 0.42 s, and the replacement session became active. Message transfer after recovery, repeated deaths, crash-loop exhaustion, daemon restart, renewal and ABI remain unverified.

Accepted boundary: policy, swapping, old-generation drain/join, daemon wiring, stability-window budgeting, shutdown cancellation, TUI outage presentation, a synthetic real-child replacement test, and one authorized live child-replacement run are covered. Repeated recovery, crash-loop exhaustion, delivery after recovery, daemon restart, renewal and ABI remain separate unverified gates.

### 2. Verified historical ingestion (design complete, blocked)

The [design-only plan](historical-ingestion-plan.md) records the gate. It is blocked: no sidecar command returns message bodies, the history frames are stubs, the headless ObjectBox path is unavailable, and `capabilities.history` is now correctly false. The first live step is not read-only because restoring the engine rewrites registration files.

Do not start by calling generated methods and labeling results history. First verify pinned-engine source semantics for archive availability, pagination/token lifetime, identity, edits/unsend/tombstones and resume/cancellation. ObjectBox app history is not automatically available through the headless engine. Keep unsupported cases explicit.

If a lawful safe source is established: idempotent page ingestion, live/history deduplication, encrypted checkpoints committed only after durable page acknowledgment, transactional rollback and cancellation/resume. No plaintext archive exports or guessed body/time deduplication.

Acceptance: synthetic pages cover duplicates, out-of-order live overlap, tombstones, partial persistence, invalid cursor, interruption and restart. Authorized live archive comparison must prove completeness before claiming historical sync. If semantics remain unavailable, document the block rather than fabricate import.

### 3. Daily reading/composition loop

This section covers reading and composition. Reading anchors, the measured Latest/End action, overlay wheel ownership and the narrow Chats navigator are already accepted in [the UI review](ux-polish-review.md); do not redispatch them as unimplemented features.

Shipped in this loop so far:

- Keyset local pagination end to end. The store and IPC page backwards from a known message (currently protocol version 7), and the transcript loads the previous page when the reader reaches the oldest loaded row, keeps paged history across reconciliation for that chat, and stops at a bounded retained budget instead of repeating a discarded request.
- Local message search end to end. `search_messages` matches stored text parts case-insensitively, newest first, with char-boundary-safe snippets and canonical conversation ids. Ctrl+F or `/search` opens a modal that lists hits with chat identity and snippet, and opening a loaded hit anchors directly. Unloaded hits use the exact-context view described below.
- Failure and identity handling around paging: page failures stay readable until the next interaction, alias reconciliation carries paged history and unions caches, and leaving a chat releases the in-flight page slot.
- Session-only composer undo/redo and completed-search result counts, committed in `f85ce33`. Undo is bounded and recipient-owned; durable drafts are not implemented. A search at its result cap says "may be more" rather than claiming completeness.

Local unread navigation, accepted with synthetic coverage:

- Local read position and unread navigation are implemented, not remote read state or read receipts. A successfully displayed transcript following latest advances a per-conversation marker. Reading back, overlays, and absent transcripts do not advance it. `/unread` and Ctrl+B then `u` jump to the oldest loaded message newer than that marker.
- Counts cover incoming loaded messages only; your own outgoing messages are neither unread nor unread-jump targets. A full page entirely newer than the marker shows a lower bound such as `200+`, conservatively including an exactly-full history whose end is not known. Without a stored marker or loaded history, unread state is unknown and no count is shown. This is not an authoritative count for every conversation or a complete archive.
- The current implementation stores conversation IDs, the last-seen message ID, and its timestamp in the existing private but unencrypted `tui.json`. It stores no message bodies. Unlike manually assigned titles, these markers accumulate automatically as chats are opened and have no automatic expiry or pruning. This reading-activity metadata remains a privacy and retention limit, not encrypted message storage. Alias reconciliation merges markers under the canonical ID and keeps the newest position, including after config reload.
- Actual-PTY coverage proves badge appearance, clearing after opening latest, saved marker advancement without extra input, and a first-unread jump distinct from latest. Existing ownership, paging, search, and send regressions pass.

Deep-hit navigation, accepted under shipp with synthetic coverage:

- [Deep-search design and acceptance](deep-search-navigation.md) records the exact local `message_context(conversation_id, message_id, limit)` lookup. IPC version 4 returns at most 200 rows including the target, resolves aliases, and rejects missing or wrong-conversation targets. Loaded hits anchor directly; other hits use one exact lookup, not intermediate paging.
- The TUI keeps context separate from normal latest history, with request-generation correlation. Reconciliation and arrivals cannot replace it. End or Latest returns to normal history without changing the draft. Context frames never advance read markers, and a marker advanced after drawing triggers one badge redraw.
- Final parent workspace and timezone tests, strict Clippy, formatting, and source guard pass. Rebuilt runtime suites pass: 18 fixture, 12 input, 4 messaging, 12 reading/navigation, and 8 launcher tests. The deep PTY case opens a hit 1,420 rows back, survives reconciliation and arrivals, clears its badge on Latest, and preserves literal-paste provenance and the recipient. Scoped independent source reviews and parent runtime/capture inspection informed acceptance. No live account or engine was accessed.

Participant-context search, accepted under shipp with synthetic coverage:

- [Its design and acceptance record](participant-search.md) defines current local chat membership, not sender filtering or historical group membership. Participant scope was introduced in IPC v5 and carries transport plus exact handle; the current protocol is v7. Filtering happens before the result limit within the existing global newest-20,000-row scan bound; plain queries stay literal.
- Ctrl+P inside Search opens **Chats with**. Exact identity-bearing rows, All chats, nested cancel, and request invalidation preserve the body query and recipient-owned draft. Narrow pickers own the full frame and wrap identities instead of silently hiding distinguishing text.
- Final workspace and timezone tests, strict Clippy, formatting, source guard, and rebuilt runtime suites pass: 28 fixture, 14 reading/navigation, 12 input, 4 messaging, and 8 launcher tests. Scoped independent source reviews and parent runtime/capture inspection informed acceptance. The final presentation rereview found no remaining confirmed defects in its scope. No live account or engine was accessed.

Next in this section:

- Native filename search is implemented and accepted on IPC v6; see the [filename-search contract](filename-search.md) for the matching, snippet, display and v6 rules. Do not redispatch the accepted metadata prerequisite. Old unsupported placeholders have no name to recover.

Later in this section:

- Filename search over stored text plus reported names and participant-context search are accepted. Attachment content search, file send/open/download, and old-placeholder recovery remain out of scope.
- Durable encrypted drafts have a [design](durable-drafts-plan.md): daemon-owned encrypted rows keyed by conversation or new-chat address, optimistic revisions with delete tombstones, bounded retention, and IPC v7. Undo stays session-only until phases 1 to 4 pass.
- Typed interaction modes to reduce independent flag combinations, but only behind regression coverage for modal transitions, recipient changes, aliases, filters and asynchronous results.

Acceptance: actual PTY tests for long local histories, new arrivals while reading back, search navigation, drawer/focus/mouse paths, unicode cursor behavior and cancel/recovery ownership. Preserve exact body whitespace and uncertain-send policy.

### 4. Standalone runtime packaging and onboarding (parked, phase 4 open)

Phases 1 to 3 are accepted. Phase 4 ran eight clean staged-prefix attempts after the only reproduced frame rejection, and the fault has not recurred. The redacted diagnostic is committed, so the next occurrence names the rejected line's structure without printing it. Do not restart the soak without a new lead. See the [packaging plan](packaging-plan.md).

The [packaging plan](packaging-plan.md) records the required runtime pieces, the launcher and daemon discovery rules, the licensing boundary, and four phases. The first is a read-only `bloom --doctor`; bundling the engine and claiming Cargo-only installation remain forbidden.

Current install requires retained sidecar checkout, resolved Dart packages, Dart and an existing compatible engine/registration. Design portable discovery/version diagnostics and reproducible runtime staging. Do not bundle the external engine or imply Cargo alone installs it. See the [4c release-paperwork review](packaging-4c-release-paperwork.md); distribution remains blocked.

Acceptance: install into an isolated prefix, hide the source checkout, run from unrelated directories with synthetic engine/registration fixtures, test missing dependencies/version mismatch and safe failure. Verify executable/file permissions, reused-daemon lifecycle and terminal restoration. Bloom-owned sign-in is not part of this packaging step.

### 5. Launcher build profile (debug default; opt-in release implemented)

The isolated switching probe was rerun on 2026-10-27 with `--binary` and the same synthetic fixture: 200 messages per chat, 100 columns, fake daemon. Warm chat switching was 22.2 ms in debug versus 8.7 ms in release for medium bodies, and 20.1 versus 9.5 ms for long bodies. The cold first visit was 53.7 versus 27.8 ms, and 351.8 versus 61.0 ms for long bodies. Overlay timings were ~9-12 ms in both and sit near the PTY floor. This measures input to readable reconstructed terminal state, not compositor paint. These numbers are not directly comparable to the earlier 31/213 ms figures because the fixture differs.

The earlier choice to keep debug remains the launcher default, so development iteration stays fast. The opt-in release profile is implemented as `./scripts/bloom --release`, which builds and runs the release daemon and TUI. The installed `bloom` command still uses the cargo profile it was built with.

Live check: a `./scripts/bloom --release` launch produced the release daemon, one engine, a release TUI and an active session; a second release TUI reused the daemon and exited 0 without disturbing it. After the owning TUI was quit, the launcher, release daemon, engine and TUI exited, the socket was removed, and no debug or replacement process remained.

If it is revisited: an isolated install builds and runs both profiles, terminal restoration and reused-daemon behavior are unchanged, startup cost is documented, and no test suite depends on a fixed profile path.

### 6. Rich messaging and future GUI

Inbound built-in reactions are accepted through mapping, persistence, fixture parity and read-only TUI display. Inbound text edits and whole-body unsends are also accepted: part identities are preserved, mutations are transactional and evidence-gated, and display is PTY-proven. `Capabilities.reactions`, `edits`, and `unsend` remain false because Bloom has no outbound operation for them and does not plan to add one for this project scope. Partial multipart unsend, attachment/mention replacement, and mutation of ambiguous legacy rows remain unsupported. Inbound attachment saving and opening are accepted at the documented live boundary: one authorized probe fetch downloaded 325,019 bytes into the private cache with mode `0600`, and a later live TUI run saved 165,257 bytes through the chooser with mode `0600`; `/attachment` opens a chooser over loaded fetchable attachments, and Enter saves while `o` opens the choice externally. Nothing opens automatically. Outbound attachment send is implemented at the synthetic boundary: `/attach <absolute path>` validates one local file, sends it through the sidecar with IPC v9, records one outbound attachment part, and reports uncertainty on timeout. Remote delivery, echo/dedupe and the live `o`/attachment-send boundary remain unverified. Replies, typing/read receipts and group management still require transport mapping, persistence, IPC and interaction together; capability declarations alone are not implementation. GPUI remains deferred, and independent registration stays blocked by [Gate 0](ids-validation-gate.md).

## Universal release gates

- Workspace all-target tests, timezone TUI tests, strict Clippy, formatting and source guard pass.
- Owned Dart format/analyze/test gates pass after `dart pub get`; generated bridge is not manually edited.
- Synthetic fixture, actual TUI PTY input/messaging/reading-navigation and actual launcher suites pass against rebuilt binaries.
- Normal daemon dependency graph contains no `rustpush`; engine library is not vendored; staged files contain no credentials or personal content.
- User-facing bugs reproduced before fixes; regressions fail for the original defect and pass afterward. Inspect actual UI output and mouse hitboxes, not just snapshots or test counts.
- Current-account operations require explicit authorization. Synthetic results cannot pass live delivery/history/renewal gates.
- Update docs/handoff with behavior, limits and evidence. Do not declare complete on partial agent output or stop with known test failures.

## Delegation and Git policy

One writer per file, named bounded tasks, parent-owned decisions and visual critique. Serialize build-heavy verification against the shared target directory; do not create multiple full target trees or rely on historical machine RAM/disk numbers. Read-only researchers return source evidence; implementers get exact scope and acceptance; reviewers inspect actual changes. Agent success messages are not verification.

Before committing, inspect status and diff, exclude secrets/account data/artifacts, run gates, and stage only intended source/docs/test changes. Commit coherent verified milestones without an agent co-author. Push normally to the configured remote when authorized; do not generate SSH keys or force-push. A fresh session checks actual branch status rather than trusting a recorded “pending push” note.
