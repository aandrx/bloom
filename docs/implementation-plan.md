# Implementation roadmap

**Status: active next-work plan, not completed features.** Replaces the obsolete foundation/direct-transport wave list. Read [handoff](handoff.md), [current design](system-design.md), [status](status.md) and [completed iteration](iteration-review.md) first.

**Current user priority: UI/UX quality for the existing chat client.** The [reading/navigation pass](ux-polish-review.md) records its accepted bounded scope, and the [switching-latency follow-up](tui-switching-performance.md) records the lag report, measurements and acceptance. The daily reading loop now has local paging, text search with bounded deep-hit views, participant scope, session undo, and local unread navigation. Backend recovery and historical import remain queued behind that work.

Multi-service integration and workspace/server navigation are deferred. Preserve [multi-service-research.md](multi-service-research.md) as research, not an active implementation instruction.

## Completed foundation

Rust daemon/domain/IPC/secret-store boundary, encrypted persistence, external Dart engine integration, local conversation/message storage, TUI text sends and safe new-chat correlation, known-participant contacts/local preferences, exact direct identity reconciliation, literal multiline paste, recipient-bound session drafts/modal suspension, metadata sync request tracking, neutral responsive UI and installed launcher lifecycle have synthetic regression coverage. The daily loop now also has keyset paging, local message search with paging jumps, and session composer undo, all with synthetic regression coverage. Cargo installs commands but not the complete runtime. Current live engine, historical import and renewal remain separate unverified gates.

## Current priority and next action

The section numbers below group work by area; they are not execution order.

1. The [one-file metadata-preservation prerequisite](attachment-metadata.md) is accepted under shipp: 107 daemon unit tests, 13 fake-sidecar tests, the full rebuilt gate, and scoped independent final review pass. It preserves descriptive scalars with bounded local tokens, not engine/download identity. The separate borrowed filename accessor is also accepted: 10 domain tests, strict workspace Clippy, formatting/source checks, and scoped independent source review pass, with no serialization or UI behavior changes. Direct fixture preflight and a parent probe show supplied typed parts already survive context/framing and stateful persistence. The test-only fixture RPC/save/restart proof is also accepted: 30 tests, source/diff checks, and scoped independent source review pass, without a daemon rewrite. Next is bounded read-only protocol-policy preflight, then native search/display contracts addressing compatibility and matching parity. No matching/UI writer starts before that contract. Never search placeholders or infer filenames. [The baseline gap](attachment-search-gap.md) remains relevant to old records.
2. Participant-context search and deep-hit navigation are accepted with the evidence below. Attachment sending, download/open, content extraction, and remote history remain outside these metadata/search units.
3. Start generation-scoped engine recovery only when resuming backend work. Historical ingestion, packaging, and rich messaging remain queued.

Keep the launcher on debug. Do not add durable drafts, remote read receipts, or multi-service work to this unit.

Local unread navigation is accepted under shipp with synthetic evidence. Workspace all-target tests, Tokyo-timezone TUI tests, strict workspace Clippy, formatting, and source guard pass after the review fixes. Final rebuilt runtime suites pass: 10 fixture, 12 input, 4 messaging, 10 reading/navigation, and 8 launcher tests. The scoped follow-up source review found no confirmed defects in the corrections. Regressions were observed failing before fixes for stranded alias markers, outgoing unread counts, and competing search jumps. The actual PTY flow also proves saved marker advancement after drawing and a first-unread jump distinct from latest. A non-blocking review note remains: a failed settings save occurs after drawing, so its error can wait for another redraw and retries can repeat on subsequent loop iterations. No live account or engine was accessed.

## Queued backend/product work and acceptance

### 1. Generation-scoped engine recovery (next backend phase)

Problem: TUI reconnects to IPC but a dead engine child is not automatically replaced.

Design before implementation: exclusive registration ownership, generation IDs, fresh event channels, stale-event rejection, bounded backoff, stop/cancellation during restart, and failure presentation. Reap the old child before a new owner starts. Uncertain sends are never automatically replayed.

Likely ownership: daemon `engine.rs`, `supervisor.rs`, `session.rs` and service/lifecycle tests. Freeze cross-file contracts before delegation.

Acceptance: fake-sidecar E2E covers crash, malformed output, delayed old-generation events, repeated failure/backoff, shutdown during backoff, queued persistence, single owner and no duplicate send. Authorized live receive/restart is a separate user-approved release gate.

### 2. Verified historical ingestion

Do not start by calling generated methods and labeling results history. First verify pinned-engine source semantics for archive availability, pagination/token lifetime, identity, edits/unsend/tombstones and resume/cancellation. ObjectBox app history is not automatically available through the headless engine. Keep unsupported cases explicit.

If a lawful safe source is established: idempotent page ingestion, live/history deduplication, encrypted checkpoints committed only after durable page acknowledgment, transactional rollback and cancellation/resume. No plaintext archive exports or guessed body/time deduplication.

Acceptance: synthetic pages cover duplicates, out-of-order live overlap, tombstones, partial persistence, invalid cursor, interruption and restart. Authorized live archive comparison must prove completeness before claiming historical sync. If semantics remain unavailable, document the block rather than fabricate import.

### 3. Daily reading/composition loop

This section covers reading and composition. Reading anchors, the measured Latest/End action, overlay wheel ownership and the narrow Chats navigator are already accepted in [the UI review](ux-polish-review.md); do not redispatch them as unimplemented features.

Shipped in this loop so far:

- Keyset local pagination end to end. The store and IPC page backwards from a known message (currently protocol version 5), and the transcript loads the previous page when the reader reaches the oldest loaded row, keeps paged history across reconciliation for that chat, and stops at a bounded retained budget instead of repeating a discarded request.
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

- [Its design and acceptance record](participant-search.md) defines current local chat membership, not sender filtering or historical group membership. IPC v5 carries transport plus exact handle. Filtering happens before the result limit within the existing global newest-20,000-row scan bound; plain queries stay literal.
- Ctrl+P inside Search opens **Chats with**. Exact identity-bearing rows, All chats, nested cancel, and request invalidation preserve the body query and recipient-owned draft. Narrow pickers own the full frame and wrap identities instead of silently hiding distinguishing text.
- Final workspace and timezone tests, strict Clippy, formatting, source guard, and rebuilt runtime suites pass: 28 fixture, 14 reading/navigation, 12 input, 4 messaging, and 8 launcher tests. Scoped independent source reviews and parent runtime/capture inspection informed acceptance. The final presentation rereview found no remaining confirmed defects in its scope. No live account or engine was accessed.

Next in this section:

- Resolve the [attachment ingestion prerequisite](attachment-search-gap.md) before filename search. Verify part identity and privacy boundaries, then scope metadata preservation and encrypted round-trip tests separately from search or download. Do not copy raw attachment envelopes, infer names, or claim recovery of old placeholders.

Later in this section:

- Attachment search is blocked on verified metadata preservation. Participant-context search is implemented.
- Durable encrypted drafts still need a privacy and retention review; undo stays session-only until then.
- Typed interaction modes to reduce independent flag combinations, but only behind regression coverage for modal transitions, recipient changes, aliases, filters and asynchronous results.

Acceptance: actual PTY tests for long local histories, new arrivals while reading back, search navigation, drawer/focus/mouse paths, unicode cursor behavior and cancel/recovery ownership. Preserve exact body whitespace and uncertain-send policy.

### 4. Standalone runtime packaging and onboarding

Current install requires retained sidecar checkout, resolved Dart packages, Dart and an existing compatible engine/registration. Design portable discovery/version diagnostics and reproducible runtime staging. Do not bundle the external engine or imply Cargo alone installs it. Check licensing/ABI requirements before distribution.

Acceptance: install into an isolated prefix, hide the source checkout, run from unrelated directories with synthetic engine/registration fixtures, test missing dependencies/version mismatch and safe failure. Verify executable/file permissions, reused-daemon lifecycle and terminal restoration. Bloom-owned sign-in is not part of this packaging step.

### 5. Launcher build profile (decided: stay on debug)

The isolated first-render probe measured about 31 ms (medium bodies) and 213 ms (long bodies) in the debug profile `scripts/bloom` builds, against 4 ms and 22 ms for the same code in release. The user chose to keep the debug launcher for now, so development iteration stays fast and the measured first-visit cost is accepted as a known limit rather than a bug. Release remains a future option, not a pending task.

If it is revisited: an isolated install builds and runs both profiles, terminal restoration and reused-daemon behavior are unchanged, startup cost is documented, and no test suite depends on a fixed profile path.

### 6. Rich messaging and future GUI

Attachments, replies/reactions, edits/unsend, typing/read receipts and group management require transport mapping, persistence, IPC and interaction together. Capability declarations are not enough. GPUI remains deferred; [bookmark](gpui-surface.md) records exploration without scheduling it. Independent registration stays blocked by [Gate 0](ids-validation-gate.md).

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
