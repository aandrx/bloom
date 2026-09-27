# Bloom status

This records repository-backed status, not the intended end state. Verified means the cited test or recorded run exercised that behavior; it does not imply broad production validation.

## What works today

`bloomd` serves clients over Unix-socket IPC, supervises an engine sidecar, persists messages in its encrypted SQLite store before event fanout, and serves stored history. The Dart sidecar reuses the installed OpenBubbles engine and its existing registration. The TUI provides conversation listing, thread transcript, composing/sending, session status, filtering, scrolling, and reconnect resync. Existing engine conversations can be listed, but their message history is not imported.

## How important behavior was verified

- `apps/bloomd/tests/engine_sidecar.rs` launches a fake sidecar as a child process using the real stdio protocol. It exercises readiness, inbound message persistence before fanout, outbound send confirmation and persistence, duplicate idempotence, history, conversation creation/reuse, and shutdown. It does not run the real engine.
- `apps/bloom-tui/tests/daemon_session.rs` uses a fake daemon on a real Unix socket to exercise client session behavior; `tests/interaction.rs` and `tests/ui_render.rs` cover keyboard interaction and headless rendering. The render test asserts structural details against the mockup. These tests are not a live visual review by an independent critic.
- The ignored `apps/bloomd/tests/engine_e2e.rs` is an available real-engine harness, but its existence is not evidence of a recent passing run. `docs/engine-sidecar.md` records prior authorized live runs: real state restore, inbound capture, a sent message independently confirmed by the recipient, restart/resume, conversation listing, and daemon/socket receipt and persistence. It reports a separate live sidecar check for ready/inbound/shutdown frames.

## What is not verified

The live-engine path has not been re-exercised since recent changes to startup ordering and the ready frame. The fake engine tests speak the protocol but necessarily mask differences between the fake process and the real engine. The recorded live send and receive do not establish ongoing reliability, renewal, or current behavior after those changes. Renewal is still unconfirmed. The TUI render tests are not evidence of an independent critic review against the mockups. No full build or test run was performed for this status update.

The real-engine daemon test is ignored by default and requires the installed engine library and user state. A recorded successful run in `docs/engine-sidecar.md` is evidence for that run, not a guarantee for the current checkout.

## Blocked

Independent iMessage transport remains blocked at Gate 0 because the public stub's validation path is unimplemented. See [the IDS validation gate](ids-validation-gate.md). Existing app thread history is not imported: the stored history depends on Flutter app models/services that cannot be opened headlessly by the sidecar. A safe read-only approach is not established; see item 17 in [the engine-sidecar design](engine-sidecar.md).

## Deliberately not started

The windowed GPUI surface has not started. See [the GPUI surface note](gpui-surface.md).

## Evidence and omissions

Claims above were checked against `docs/engine-sidecar.md`, `docs/ids-validation-gate.md`, `docs/gpui-surface.md`, `apps/bloomd/tests/engine_sidecar.rs`, `apps/bloomd/tests/engine_e2e.rs`, `apps/bloom-tui/tests/daemon_session.rs`, `apps/bloom-tui/tests/interaction.rs`, and `apps/bloom-tui/tests/ui_render.rs`. The earlier temporary-revert proofs had no repository evidence at the time; `scripts/check-guards` now makes five named guard proofs reproducible. The independent critic review remains external evidence, not retained here. No private addresses, handles, message content, or credentials are included.

The named regression proofs are reproducible with `scripts/check-guards`: it temporarily applies each mutation, requires its specific test to fail, and restores the source. It refuses to mutate a file with uncommitted changes. Run one with `scripts/check-guards byline`; run all with `scripts/check-guards`. This is a local verification tool, not CI enforcement. The critic reviews of the interface were run outside the repository, so the rendered captures they judged are not retained here either.
