# Engine generation recovery plan

Status: proposed under shipp, source-only planning. No recovery code exists. Unit 0, the failed-start repair, is accepted: 119 daemon library tests, strict daemon Clippy, formatting, source guard, and independent review with no confirmed defects. Unit 1, the unwired bounded retry policy, is also accepted: 124 daemon library tests, strict Clippy, formatting, source guard, and review. Review found two test gaps, both closed before commit: the 300 ms base now makes the 8-second cap actually clamp, and the reset test proves a successful start restores the full attempt budget. Units 2 to 4 remain unimplemented and unwired. Current behavior is described from source; nothing here is live-verified. A source review confirmed the invariants, corrected two citations, and found that the backoff, reader-join, and failed-start concerns above must be explicit.

## Why

The daemon spawns the Dart engine child once. If that child dies after a successful start, IPC keeps serving but sends and events are unusable until the owning launcher restarts the daemon. The TUI sees only its own IPC connection state.

## Current behavior (source facts)

- `Engine::start` spawns the child at `apps/bloomd/src/engine.rs:329`. A spawn error restores the outgoing sender and returns `TransportUnavailable` (`engine.rs:331-336`). If the child exists but its stdout or stdin cannot be taken, the code returns `Internal` without reaping that child, leaves `started` true, and drops the outgoing sender it took, so a later start cannot succeed (`engine.rs:338-340`, with the guard at `318-320`). That is an observed leak and poisoned-state path, repaired by Unit 0.
- Daemon assembly treats an initial `supervisor.start()` failure as fatal and returns exit code 3 (`apps/bloomd/src/main.rs:105-109`).
- After a successful start, `outgoing` stays `None` (`engine.rs:387-392`, handed to the reader at `435`). A second `start()` on the same `Engine` therefore returns `TransportUnavailable`; the current engine object cannot be restarted at all.
- `Service` also stores the same `Arc<Engine>` used by the supervisor (`apps/bloomd/src/service.rs:27`, built at `apps/bloomd/src/main.rs:111`). Replacing the engine in the supervisor alone would leave every service send bound to the dead engine, so a swap seam is required before any restart can work.
- `TransportEvent` has seven variants (`crates/bloom-transport/src/lib.rs:103-124`). Nothing in `apps/bloomd` carries a generation or epoch.
- On reader exit, `cleanup_reader` clears `started`, fails sync waiters with `TransportUnavailable`, drops stdin, waits then kills the child, and emits `Session{Disconnected}` (`engine.rs:520-560`). Nothing requests a new child.
- Unacknowledged sends keep their timeout outcome because stream failure cannot prove non-acceptance (`engine.rs:535-536`). In-flight sync waiters are failed explicitly (`engine.rs:523-528`). Recipient rejections map to `RecipientUnreachable` (`engine.rs:670-677`).
- Persistence drops events it cannot store and marks sync completion failed (`apps/bloomd/src/service.rs:172-185`). Subscribers receive the session event through the supervisor forwarder, which counts discarded events (`apps/bloomd/src/supervisor.rs:37-50`).
- Shutdown runs `supervisor.shutdown` under a 10-second budget, then drops supervisor and service before a bounded persistence wait (`main.rs:135-160`). `Engine::stop` writes shutdown, drops stdin, waits for the reader, then waits or kills the child (`engine.rs:376-414`).
- No restart, backoff, or generation tracking exists in `apps/bloomd`. A search for `restart|backoff|generation` returns nothing.
- The TUI knows only connection states `Connecting`, `Connected`, and `Disconnected("Disconnected, retrying")` (`apps/bloom-tui/src/daemon.rs:32-44`), rendered at `apps/bloom-tui/src/ui.rs:2881-2891`.
- Synthetic tests cover shutdown with a stuck send, ordered forwarding, rejection before start and after shutdown, stalled sinks, and persistence failures. No test kills the child unexpectedly.

## Invariants a restart must hold

1. One registration owner at a time. A new child may start only after the old child has been waited on or killed and reaped. `cleanup_reader` reaps without `lifecycle_lock` and clears `started` early (`engine.rs:525-546`), so a restarter must join the reader's completion signal instead of inferring reaping from that flag. The backoff wait must sit outside the lock and be cancellable, or held locks would block shutdown past its 10-second budget (`main.rs:135`).
2. An old generation never reaches the store, subscribers, or the TUI after a new one is active. Per-event tagging is not the chosen mechanism. The forwarder and persistence each own a FIFO channel, and the persistence queue can still hold events after `reader_done`. Two generations feeding one sink can interleave, so a restart must fully drain and join the old generation's forwarder before the new generation publishes anything. Source evidence: `supervisor.rs:37-41`, `service.rs:170-186`, `main.rs:136-138`.
3. Uncertain sends stay uncertain. Recovery must not replay, cancel, or confirm any message whose acceptance is unknown. Failed drafts remain user-driven through the existing recovery path.
4. Shutdown wins. A stop or daemon shutdown during backoff or restart cancels the restart, reaps any child it created, and never outlives the shutdown budget.

## Phased units

### Unit 0: repair the failed-start path

Only `apps/bloomd/src/engine.rs`. When the child spawns but its stdout or stdin cannot be taken, kill and wait that child, restore the outgoing sender, clear `started`, and return `Internal`. Extract a private `start_with(command)` seam used by `start()` so a test can pass a command whose stdout is null. The test asserts the error, `started == false`, the sender restored, no child retained, and that the failed attempt leaves the engine startable again. Unit 3 depends on this path being sound.

Verify with `cargo test -p bloomd --lib` and strict daemon Clippy.

### Unit 1: recovery policy, no wiring

Add a pure policy type, in a new `apps/bloomd/src/recovery.rs` plus `mod recovery;` in `apps/bloomd/src/lib.rs`. Inputs are the failure kind (`SpawnFailed`, `ChildExited(status)`, `StreamFailed`) and the attempt number. Output is either a bounded delay or a terminal `GiveUp` with a typed reason. Delays grow and cap; attempts cap.

Tests in the same file cover: attempt sequence and cap, cap on the delay, spawn failure treated as retryable only after a successful first start, and reset on a successful start. No engine, IPC, or UI behavior changes. Verify with `cargo test -p bloomd --lib` and strict daemon Clippy.

### Unit 2: swappable engine slot, no restart yet

Only `apps/bloomd/src/engine.rs`, `apps/bloomd/src/service.rs`, and `apps/bloomd/src/main.rs`, plus tests. Introduce a small shared slot, for example `Arc<tokio::sync::RwLock<Arc<Engine>>>`, owned by `main`, stored by `Service` instead of `Arc<Engine>`, and used to resolve the current engine for each send and handle lookup. `main` builds the first engine into the slot, and the supervisor still receives the current `Arc<Engine>`. Behavior with one generation is unchanged.

Tests: a service send resolves the current engine; after the slot is replaced, the next send uses the replacement; the slot is never held across an await boundary in a way that blocks a swap. Verify with `cargo test -p bloomd --lib` and strict daemon Clippy. No restart, no death detection, no channel change.

### Unit 3: generation construction and drain ordering

Source review showed the engine object is single-use and no epoch exists, so tagging every event would add a field to seven variants without fixing cross-generation interleaving. Instead, make a generation the unit of construction: a small factory that builds a fresh `Engine`, its event channel, and its forwarder. This unit adds the factory and proves ordering; it does not restart on death yet.

The factory takes the existing sink channel as a parameter; it must not create a second sink, because persistence consumes exactly one receiver (`apps/bloomd/src/main.rs:104-111`). Tests build two generations in sequence over that shared sink. Every first-generation event, including its `Session{Disconnected}`, must reach the sink before any second-generation event, with no interleaving. The test also asserts the first forwarder is joined before the second publishes. If the old forwarder is aborted instead of drained, the test must show the resulting loss of its final event rather than pretending ordering held. No restart policy, no death detection, and no IPC change in this unit.

### Unit 4: supervised restart after an unexpected death

On `ChildExited` or `StreamFailed` after a successful start, the supervisor joins the reader completion signal and the generation's forwarder, then builds a new generation through the Unit 2 factory after a cancellable delay held outside the lifecycle lock. Initial start failure stays fatal as today; only a death after success is recovered. `GiveUp` leaves the daemon serving IPC with a typed unavailable state. The old generation is dropped, not reused, because its engine object is single-use.

Tests use a synthetic sidecar script that can exit on request and record its own pid: at most one live child at any time (not merely one spawn call), restart after a death, a second death during backoff does not double-spawn, `GiveUp` stops retries, shutdown during backoff reaps any child and finishes inside the budget, an initial start failure remains fatal, and uncertain sends are not replayed.

### Unit 5: verify and, only if needed, adjust TUI presentation

The engine already emits `Session{Disconnected}` on reader exit (`engine.rs:552-560`), and the TUI stores that session string (`apps/bloom-tui/src/app.rs:4988`) for the header. Connection state is TUI to daemon, so it stays `Connected` while the daemon lives. Unit 4 first verifies with a test that engine death reaches the TUI as a visible session state; it changes wording only if that state is indistinguishable from a normal disconnect. No new IPC fields are planned.

## Risks and unknowns

- Unverified by source: whether the real Dart sidecar exits on stdin EOF, and whether a fresh child can re-open the same registration after the previous one exited. A live probe is required before claiming real recovery, and is not authorized here.
- A restart during shutdown, or a second death during backoff, can create a second owner if the reaping and spawning windows are not held under one lock. Unit 3 tests must prove single ownership.
- Cross-generation ordering depends on draining the old forwarder before the new generation publishes. A join that is skipped lets old and new events interleave; an abort instead loses the old generation's final events, including the `Session{Disconnected}` that Unit 5 relies on. Unit 3 must prove which of the two happens rather than assume ordering held.

## Exclusions

No live account or engine verification, no historical import, no packaging, no rich messaging, no automatic resend, no multi-service work. No registration changes. Old messages and drafts are untouched.
