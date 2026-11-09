# Startup readiness plan

Status: proposal only. All four units are PENDING. No product implementation, test change, workflow change, or live run exists for this plan. Work targets branch `feat/startup-readiness-ui` at baseline `ead2c43`. The main development checkout stays untouched.

The plan records the requested proposal for review, not authorization to implement it. Each unit is independently verifiable, and each unit needs its own acceptance before the next starts. An independent plan review is required before P1 starts.

## Scope

In scope, in order:

1. P1: authoritative daemon readiness and send gate, with an IPC contract.
2. P2: mandatory startup checklist in the existing terminal UI.
3. P3: truthful per-chat local activity.
4. P4: passive message transcript rail.

Out of scope: remote history, engine, ABI, or generated file changes, GPUI, multiple services, registration, live sends or live proof, read-only or degraded skip mode, and scrollbar dragging.

## Verified baseline

These source facts motivated the plan. They are not descriptions of future behavior.

- `apps/bloomd/src/main.rs:78-84` asks the engine for a conversation listing only when the store has no conversations. A populated store skips the refresh.
- `ext/bloom-engine-sidecar/dart/bin/bloom_engine.dart:280-303` emits ready before an unsolicited metadata sync request with id 0. A missing cloud client only logs a line. An explicit sync with no client fails.
- `_syncChats` at `bloom_engine.dart:446-513` caps pages at 200 and rows at 20,000. It returns `complete`, `limited`, or `failed`, and reports no remote total or history.
- `apps/bloomd/src/engine.rs:971-996` orders a `ConversationSyncComplete` acknowledgment after accepted metadata writes. It does not cover later pushes, backlog, or history.
- `apps/bloom-tui/src/main.rs:620-635` and `887` use a two second inbound quiet heuristic for the `Syncing` label. The explicit sync path at `853` calls the actual RPC. No startup barrier exists.
- `SubscriptionHub` replays only the latest session event.
- Send, new chat, and attachment execution at `apps/bloomd/src/service.rs:525`, `647`, and `699` carry no readiness token.
- `crates/bloom-ipc/src/lib.rs:25` sets protocol version 9. New variants need version 10.
- The actual transcript has no scrollbar. The scrollbar at `apps/bloom-tui/src/ui.rs:2792` belongs to the sidebar. The transcript measures wrapped rows at `ui.rs:3365-3433`, including the byline adjustment before the final top.
- `apps/bloom-tui/src/transcript.rs` keeps message anchors across readback.

## P1: authoritative readiness and send gate

One required, correlated metadata refresh runs per engine generation, even when the store already has rows. IPC accepts connections during bootstrap. The daemon exposes live engine readiness separately from metadata readiness. A required ordered persistence barrier produces one explicit outcome: complete, failed, limited, or unavailable. While the gate is closed, all three outbound paths reject immediately before preparation, upload, draft creation, or store mutation. They do not wait in a send queue.

The gate and the engine resolve to the same generation. Recovery invalidates readiness before the old generation drains. A stale, late, or timed out result cannot unlock the gate. An empty listing is valid and complete. Partial writes, errors, and limits are not ready. The daemon never queues or retries a send automatically. A send already dispatched when the connection drops stays uncertain and is not undone. Metadata retries are serial and coalesced. A timeout does not cancel the current sidecar operation, start overlapping work, or pass an old success off as a new attempt.

State is revisioned as `{generation, operation, revision, phase, outcome, observed_records, can_send, bounded_error}`. Query and replay are atomic enough that a second TUI cannot lose or reorder state. Record counts mean metadata records, not canonical chat counts. Metadata and manual sync gate new outbound work. Routine five second local reconcile and inbound updates do not globally gate sends. A second client reuses the shared readiness for that generation, while its own local snapshot and draft loading still run.

Proposed file groups: daemon `main`, `engine`, `service`, `generation`, and `subscription`; `crates/bloom-ipc/src/lib.rs`, its `src/client.rs` module, and `crates/bloom-transport` as needed; owned Dart `bin` and protocol files only. Exact schema and error variants are agreed at the P1 boundary. No generated code. Protocol version 10 updates the daemon, TUI, fake fixtures, and tests together. A child protocol review is independent of an engine ABI bump.

Acceptance:

- [ ] One correlated refresh runs per generation with a populated store, and the test fails when the refresh is skipped.
- [ ] IPC accepts a connection and reports bootstrap progress before the refresh finishes.
- [ ] Engine readiness and metadata readiness are separate observable states.
- [ ] The ordered persistence barrier yields complete, failed, limited, or unavailable exactly once.
- [ ] Send, new chat, and attachment each stop before preparation, upload, draft creation, and store mutation when the gate is closed.
- [ ] The gate and engine share one generation, and recovery invalidates readiness before the old generation drains.
- [ ] A stale generation, operation, or revision, a late result, and a timeout all leave the gate closed.
- [ ] An empty listing unlocks as complete, and a partial, failed, or limited refresh does not.
- [ ] A missing cloud client yields unavailable with the gate closed, distinct from a valid empty complete listing.
- [ ] No send is queued or retried automatically, and a dispatched send on disconnect is reported uncertain.
- [ ] Retries are serial and coalesced, and a timeout neither cancels the current sidecar operation nor starts overlapping work.
- [ ] A second TUI receives the same revisioned state without lost or reordered events.
- [ ] Metadata and manual sync gate new outbound work, while routine local reconcile and inbound updates do not.
- [ ] Protocol version 10 is updated across daemon, TUI, fake fixtures, and tests in one change.

Failure cases: empty store with a valid empty listing, missing cloud client, refresh failure, limited result, timeout, stale generation, stale operation, stale revision, lost subscription, and a second client during bootstrap. Done when the focused daemon, IPC, client, subscription, and Dart protocol tests pass and the parent inspects the diff.

## P2: mandatory startup checklist

The checklist lives in the existing ratatui client, not an external CLI. It has five checks: daemon connection, engine ready, chat metadata refresh, persistence acknowledgment, and a post-refresh selected history snapshot with selected draft restore.

Readiness means the initial list is persisted and the local view is ready. It does not mean the account is fully synced. The stage bar shows N/5 with a spinner and the observed record count. It shows no invented percent, ETA, or remote history promise. There is no Esc skip. Failed, limited, unavailable, and timeout states fail closed with Retry and Quit.

The Open Bloom confirmation appears only after all checks pass. The event is consumed with no fallthrough. Queued key presses, key repeat, Enter, paste, and mouse input from the loading state cannot execute commands or sends after the transition. Existing draft body, recipient, and cursor survive, and terminal cleanup still runs. A reusable daemon is not killed when the client quits. Reconnect and recovery reset the send gate and show progress or an error without discarding the read anchor or drafts. Existing manual sync blocks Send and shows actual progress without requiring the screen to be open every five seconds.

Proposed files: `apps/bloom-tui/src/app.rs`, `main.rs`, `daemon.rs`, and `ui.rs`, plus a pure startup state module if needed. Not a launcher rewrite.

Acceptance:

- [ ] Five checks render with N/5, a spinner, and the observed record count, with no percent or ETA.
- [ ] Readiness copy says the initial list is persisted and the local view is ready, not that the account is fully synced.
- [ ] Failed, limited, unavailable, and timeout states fail closed and offer only Retry and Quit.
- [ ] Text, new-chat, and attachment actions are refused in the TUI with a bounded status and zero IPC dispatch attempts while daemon readiness or client bootstrap is incomplete; the daemon independently rejects direct IPC attempts.
- [ ] No key skips the checklist. Quit exits instead of opening chats.
- [ ] Open Bloom appears only after all five checks pass, and the confirming event is consumed once.
- [ ] Burst Enter, paste, queued keys, key repeat, and mouse clicks from loading cannot send or run commands after the transition.
- [ ] Draft body, recipient, and cursor survive the checklist and reconnect.
- [ ] A second TUI does not launch another engine listing, but must finish its own post-refresh snapshot and selected draft load before Open Bloom is offered. A ready daemon alone cannot bypass those client checks.
- [ ] Quitting the client leaves a reusable daemon alive and cleans up the terminal.
- [ ] Reconnect and recovery reset the gate and keep the read anchor and drafts.
- [ ] Manual sync blocks Send and shows real progress without a five second screen dependency.

Failure cases: each failed check, timeout, limited metadata, missing client, late daemon event, second client with a delayed snapshot or draft load, queued input burst, click-through, reconnect during the checklist, and quit during loading. Done when the TUI input safety, daemon session, and UI render tests pass and the parent inspects the diff.

## P3: truthful per-chat local activity

Each conversation owns a `ConversationId` plus a request or epoch owned loading token for the local history snapshot, older pages, and context requests. Success, failure, cancellation, and disconnect clear only the matching token. A late reply from an old request cannot clear a newer busy state. Alias and reselection are handled.

The spinner row appears in the Chats drawer and the selected header says `Loading local messages`. A global metadata refresh does not fabricate per-chat remote sync. An incoming persisted event can read `Updated` without claiming an unfinished remote operation. Unread badges and selection survive. A request for one chat does not mark every row busy. The activity map is bounded.

Proposed files: TUI `app.rs`, `main.rs`, `daemon.rs`, and `ui.rs`, with activity and rendering tests. No new per-chat remote sync API.

Acceptance:

- [ ] Snapshot, older page, and context requests each carry a conversation and epoch token.
- [ ] Only the matching token clears busy state on success, failure, cancel, or disconnect.
- [ ] A late reply from an old request leaves a newer request busy.
- [ ] Alias and reselection keep activity attached to the right conversation.
- [ ] The drawer spinner and selected header text describe local loading only.
- [ ] Global metadata refresh and persisted inbound events do not claim per-chat remote sync.
- [ ] Unread badges and selection survive activity changes, and the activity map is bounded.

Failure cases: late reply, cancel, disconnect, reselection, alias, overlapping requests for two chats, and a burst of inbound events. Done when the app activity unit tests and TUI render tests pass.

## P4: passive transcript rail

Reserve one column to the right of the message transcript before wrap and cache width measurement. Derive the thumb from the final painted wrapped rows, top, and height after the byline adjustment, not from message count. The rail reflects logical loaded and context extent, not the remote account. Anchors survive arrival, prepend, reconciliation, and resize. The existing Latest action is unchanged. Overlays own their wheel and click input. Hide the rail when the transcript fits, when the range is zero, and in tiny layouts. The first slice is passive: no drag, no track click, and no remote autoload beyond existing local paging.

Proposed files: TUI `ui.rs`, `transcript.rs`, and anchor/render/reading tests.

Acceptance:

- [ ] The rail reserves one column before wrap and width measurement.
- [ ] The thumb uses final painted rows, top, and height after the byline adjustment.
- [ ] The rail reflects loaded and context extent, not message count or remote history.
- [ ] Anchors survive arrival, prepend, reconciliation, and resize.
- [ ] Latest keeps its current behavior.
- [ ] Overlays own wheel and click input, and the rail does not steal them.
- [ ] The rail hides when content fits, the range is zero, or the layout is tiny.
- [ ] Drag, track click, and remote autoload are absent.

Failure cases: 100, 66, 44, and 24 column widths, tiny layout, light and dark themes, `NO_COLOR`, page load, search context, arrival, and resize. Done when the UI render and interaction tests pass and the parent inspects the diff.

## Progress and evidence

Only parent-verified evidence enters this table. A unit stays PENDING until its acceptance is checked and the parent inspects the diff and test output.

| Unit | Status | Commit | Test command and result | Review | Remaining limits |
| --- | --- | --- | --- | --- | --- |
| P1 | PENDING | none | not run | none | schema and error variants not agreed |
| P2 | PENDING | none | not run | none | depends on P1 |
| P3 | PENDING | none | not run | none | depends on P1 |
| P4 | PENDING | none | not run | none | scheduled after P3; independently testable |

Iteration rule: check off a box only with parent-verified evidence. Do not mark a unit accepted from an agent summary, a green local run alone, or a spinner that disappeared.

## Reviewer feedback resolutions

An independent read-only plan review found the three gaps below. The amendments resolve planning gaps, not runtime behavior. Targeted independent re-review confirmed all three amendments and found no remaining plan blockers. This is plan acceptance only. Product implementation and all acceptance boxes remain PENDING.

| ID | Finding | Plan amendment | Required implementation proof |
| --- | --- | --- | --- |
| R1 | TUI-side gate enforcement not checkable | P2 now rejects all three outbound intents before IPC dispatch; P1 remains independent daemon enforcement | Fake-daemon outbound request log stays empty during gated PTY input; direct IPC makes zero outbound engine calls |
| R2 | Missing client versus valid empty listing only in prose | P1 now has a separate unavailable-versus-empty-complete checkbox | Dart and daemon fixtures produce distinct outcomes; only empty complete can become ready |
| R3 | Second client local bootstrap not tested | P2 requires its own delayed snapshot and draft-load checks without another listing | Two-client fixture: daemon is ready, second TUI cannot offer Open Bloom until its reads finish |

Parent also clarified immediate rejection rather than waiting/queueing, corrected the IPC client module path, and specified the final verification commands. Do not close an implementation finding by changing the test to match the code.

## Final acceptance matrix

| Scenario | Required evidence |
| --- | --- |
| UI dispatch and direct IPC while blocked | TUI emits zero send/new-chat/attachment requests; direct IPC makes zero outbound engine calls or send-preparation side effects |
| Delayed ready, list complete, persistence acknowledgment, post-sync snapshot, and draft load | Deterministic barrier reaches each stage in order |
| Empty, failed, limited, and missing client | Gate stays closed or opens only for a valid empty complete result |
| Stale generation, stale operation, stale revision, and lost subscription | Gate stays closed and the TUI shows a bounded error |
| Second TUI | Shared revisioned state without another listing; that client independently waits for its post-refresh snapshot and draft restore |
| Burst Enter, paste, commands, and click-through | No buffered action confirms Open Bloom or dispatches a send when readiness changes; a fresh confirmation is consumed by the startup screen |
| Owned cleanup | No leaked child or stale owned socket; existing drafts stay intact, and a reusable daemon survives client quit |
| Chat busy ownership and alias | Correct conversation stays busy and a late reply cannot clear it |
| Transcript rail at 100, 66, 44, and 24 columns, tiny layout, light and dark, `NO_COLOR` | Rail geometry and visibility match the acceptance boxes |
| Page, search context, arrival, and resize | Anchors and Latest behavior hold |

Evidence comes from a synthetic real daemon with a fake child and a real PTY. Use deterministic barriers, not sleeps or a disappearing spinner.

## Verification commands

The commands below are future implementation acceptance, not work already performed by this documentation PR. Rebuild both runtime packages before PTY runs. `AGENTS.md` and `.github/workflows/check.yml` remain the command authorities.

```sh
cargo fmt --all -- --check
cargo test --workspace --all-targets
TZ=Asia/Tokyo cargo test -p bloom-tui
cargo clippy --workspace --all-targets -- -D warnings
cargo build -p bloomd -p bloom-tui
./scripts/check-sources
```

In an activated Python virtual environment with `pyte==0.8.2`:

```sh
python scripts/test-launcher-e2e.py
python scripts/test-stateful-fake-daemon.py
python scripts/test-tui-input-e2e.py
python scripts/test-tui-messaging-e2e.py
python scripts/test-tui-reading-e2e.py
```

From `ext/bloom-engine-sidecar/dart`, run `dart pub get`, the owned-source format and analysis commands in CI, and `dart test`. Extend CI's explicit owned-source allowlist if a new owned Dart file is introduced; never broaden it to format or edit generated upstream bindings.

At P1, add a test that v9/v10 client-daemon mismatches fail closed without sending. For the plan PR, an existing green CI run is baseline health only, not evidence that any proposed feature or acceptance criterion is implemented.

Existing suites that must stay green include daemon `service`, `engine_sidecar`, and `engine_recovery`; client and subscription tests; Dart protocol and single-flight tests; TUI `input_safety`, `daemon_session`, `ui_render`, and `interaction`; and Python `test-tui-input`, `test-tui-messaging`, `test-tui-reading`, `test-launcher`, and the fake daemon fixtures. The 105 Dart test count is historical and is not a fixed target.

This documentation change is checked now by link, source reference, and diff only. Full product tests are FUTURE work and are not claimed as run. A final independent reviewer and a parent inspection of actual PTY captures are required before the draft pull request is ready or merged. No auto-merge. Live validation needs separate authorization and is not a prerequisite for honest synthetic acceptance. No document may claim the account is fully synced.
