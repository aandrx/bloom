# Implementation roadmap

**Status: active next-work plan, not completed features.** Replaces the obsolete foundation/direct-transport wave list. Read [handoff](handoff.md), [current design](system-design.md), [status](status.md) and [completed iteration](iteration-review.md) first.

**Current user priority: UI/UX quality for the existing chat client.** The [reading/navigation pass](ux-polish-review.md) records its accepted bounded scope. The current [switching-latency follow-up](tui-switching-performance.md) records the user's subsequent lag report, fresh synthetic PTY measurements and acceptance. Backend recovery/history work below remains queued, not part of this bounded UI iteration.

Multi-service integration and workspace/server navigation are deferred. Preserve [multi-service-research.md](multi-service-research.md) as research, not an active implementation instruction.

## Completed foundation

Rust daemon/domain/IPC/secret-store boundary, encrypted persistence, external Dart engine integration, local conversation/message storage, TUI text sends and safe new-chat correlation, known-participant contacts/local preferences, exact direct identity reconciliation, literal multiline paste, recipient-bound session drafts/modal suspension, metadata sync request tracking, neutral responsive UI and installed launcher lifecycle have synthetic regression coverage. Cargo installs commands but not the complete runtime. Current live engine, historical import and renewal remain separate unverified gates.

## Queued backend/product work and acceptance

### 1. Generation-scoped engine recovery

Problem: TUI reconnects to IPC but a dead engine child is not automatically replaced.

Design before implementation: exclusive registration ownership, generation IDs, fresh event channels, stale-event rejection, bounded backoff, stop/cancellation during restart, and failure presentation. Reap the old child before a new owner starts. Uncertain sends are never automatically replayed.

Likely ownership: daemon `engine.rs`, `supervisor.rs`, `session.rs` and service/lifecycle tests. Freeze cross-file contracts before delegation.

Acceptance: fake-sidecar E2E covers crash, malformed output, delayed old-generation events, repeated failure/backoff, shutdown during backoff, queued persistence, single owner and no duplicate send. Authorized live receive/restart is a separate user-approved release gate.

### 2. Verified historical ingestion

Do not start by calling generated methods and labeling results history. First verify pinned-engine source semantics for archive availability, pagination/token lifetime, identity, edits/unsend/tombstones and resume/cancellation. ObjectBox app history is not automatically available through the headless engine. Keep unsupported cases explicit.

If a lawful safe source is established: idempotent page ingestion, live/history deduplication, encrypted checkpoints committed only after durable page acknowledgment, transactional rollback and cancellation/resume. No plaintext archive exports or guessed body/time deduplication.

Acceptance: synthetic pages cover duplicates, out-of-order live overlap, tombstones, partial persistence, invalid cursor, interruption and restart. Authorized live archive comparison must prove completeness before claiming historical sync. If semantics remain unavailable, document the block rather than fabricate import.

### 3. Daily reading/composition loop

The current bounded UI pass implements stable message-owned reading anchors, a measured Latest/End action, overlay wheel ownership and full-width narrow conversations with a searchable Chats navigator. Its acceptance evidence is in [the UI review](ux-polish-review.md); do not redispatch these as unimplemented features.

Remaining:

- Cursor-based local pagination beyond 200 stored messages, with stable viewport and bounded memory.
- Local message-content search and authoritative unread navigation. Chats searches conversation names/addresses only, not message content.
- Undo and, only after privacy/retention review, encrypted durable drafts.
- Consider typed interaction modes to reduce independent flag combinations, but only behind regression coverage for modal transitions, recipient changes, aliases, filters and asynchronous results.

Acceptance: actual PTY tests for long local histories, new arrivals while reading back, search navigation, drawer/focus/mouse paths, unicode cursor behavior and cancel/recovery ownership. Preserve exact body whitespace and uncertain-send policy.

### 4. Standalone runtime packaging and onboarding

Current install requires retained sidecar checkout, resolved Dart packages, Dart and an existing compatible engine/registration. Design portable discovery/version diagnostics and reproducible runtime staging. Do not bundle the external engine or imply Cargo alone installs it. Check licensing/ABI requirements before distribution.

Acceptance: install into an isolated prefix, hide the source checkout, run from unrelated directories with synthetic engine/registration fixtures, test missing dependencies/version mismatch and safe failure. Verify executable/file permissions, reused-daemon lifecycle and terminal restoration. Bloom-owned sign-in is not part of this packaging step.

### 5. Rich messaging and future GUI

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
