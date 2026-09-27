# TUI reliability and interaction follow-up

## Baseline and evidence

User baseline: 6.5/10. This is a planning document, not a completion report.

Reported defects:
- Some outbound messages show failed in Bloom while appearing sent on iPhone.
- Multiple direct conversations appear to target the exact same recipient.
- Slash suggestions are bordered, titled, and narrower than the chat area.
- Sidebar selection marker covers only the first of two selected rows.
- Transcript content crowds the composer; End does not return to newest.
- Conversation renaming does not provide contact names in group bylines.

References supplied in this session show a borderless, full-width command list, a shaded composer, and a bottom-anchored settings-style panel. Current repository status explicitly says existing engine conversations can be listed but older app history is not imported. A refresh must not imply unsupported history recovery.

## Stage 1: Correctness and safe recovery

1. Reproduce uncertain send outcomes through the real daemon/client protocol with a controllable fake sidecar before changing behavior.
2. Trace transport acknowledgment, persistence, timeout, event correlation, and conversation identity. Recipient-prefix and timeout theories remain unconfirmed.
3. Distinguish confirmed failure from unknown outcome. Never automatically resend an uncertain message.
4. Specify refresh scope separately: conversation discovery, locally stored messages, and remote history only where actually supported.
5. Detect duplicate direct chats using exact recipient identity with only justified transport normalization. Never infer that a phone number and email belong to the same person; never merge groups by title or membership alone.
6. Offer explicit consolidation preview and confirmation. Preserve message history and identifiers, nickname/preferences, and usable routing. Define transaction, rollback, and stale-client behavior before implementing a merge.
7. Verify late acknowledgments, restart, repeated refresh, repeated consolidation, duplicate events, and failure halfway through recovery.

No live sending, destructive account modifications, or automatic merges during investigation.

## Stage 2: Layout and navigation

- Remove slash-menu border and Commands heading; fill the available chat width, including the selected row.
- Shade the composer across the available width with appropriate light/dark contrast.
- Reserve transcript/composer separation in the layout itself, not through scroll-dependent padding.
- Extend sidebar selection marker across both name and preview rows.
- Make End return the transcript to newest; explicitly define draft-cursor behavior and advertise it accurately.
- Verify short terminals, narrow widths, long wrapped messages, empty chats, scrolled history, and open command menus.

## Stage 3: Settings and contacts

- Replace full-page settings navigation with a bottom-anchored panel preserving chat context.
- Use consistent panel dismissal, selection, focus restoration, mouse interaction, and footer hints.
- Add a searchable contacts panel with local per-recipient names.
- Resolve contact names consistently in group bylines and direct chats without changing remote addresses or group titles.
- Define precedence between explicit conversation titles, local contact names, and raw handles.
- Preserve composer draft and scroll state while panels open and close.

## Earlier iteration gaps to retain

- Current changes need real-delivery verification; fake-sidecar coverage is not proof of present live-engine reliability.
- Existing app history import is unsupported according to docs/status.md; do not relabel local refresh as full iMessage resync.
- Sidebar previews are limited to messages Bloom has stored.
- Review documentation against new behavior, especially keyboard hints, settings navigation, nickname semantics, and send status claims.

## Acceptance and critique

For each implementation stage:
1. Record a user-facing reproduction before the fix where feasible.
2. Add regression coverage and run relevant tests, formatting, lint, and repository guards.
3. Inspect actual PTY renders, including narrow and short terminals.
4. Obtain an independent reviewer critique of behavior and visual artifacts.
5. Report critique outcome and screenshot paths with remaining limitations.

Scoring must use the user's 6.5 baseline, not declare improvement solely because tests pass. Judge delivery-state honesty, layout consistency, discoverability, keyboard behavior, and contact readability separately. Message loss, accidental resend, misleading send success, or unsafe merging blocks acceptance regardless of visual score.

## Investigation in progress

- Researcher: send outcomes, identities, refresh capabilities, safe consolidation, and protocol-level reproduction.
- Plan agent: UI implementation sites, contact-name model, layout/navigation reproductions, and bounded implementation sequence.

## Research findings and decisions

- Confirmed code path: engine send confirmation times out after 30 seconds, removes the waiter, and returns a protocol error despite possibly having sent. The service stores outgoing messages only on success. This explains a possible ambiguous outcome, not the user's specific incident. Protocol reproduction now passes through the real Service and fake child sidecar: the first request times out, a valid late confirmation is consumed, a second send succeeds, and only the second message is stored. Parent corrected fixture escaping/timing/transport identity errors before accepting the result. The first agent's accidental test-file deletion was restored exactly from HEAD.
- Create-time conversation IDs and engine-synced GUIDs are not guaranteed identical in the inspected code. Duplicate identity remains a hypothesis pending reproduction.
- Current sync imports conversation metadata, not old message history. CloudKit method availability alone does not establish safe history recovery.
- Group bylines already resolve handle-keyed nicknames through App::speaker_label_for. Contacts management should reuse App/Config nickname storage and preserve existing explicit-title precedence.
- Plain End will jump to newest even while composing. Ctrl+E remains draft-end editing. Preserve the draft when scrolling.
- Contacts initially cover participants known through Bloom conversations, not an implied system-wide address book.
- UI implementation order: structural layout/selection and End, then settings overlay, then contacts. Capture matching PTY baselines before edits.
- CI repair is a separate prerequisite: investigate actual failed run logs and reproduce checks before changing files. Do not weaken existing guards.

## Send recovery design constraints

- Persist an encrypted send intent before transport side effects, with stable correlation and transactional, idempotent resolution. Add deliberate schema migration rather than bypassing the version check.
- Timeout or ambiguous write/disconnect must become unknown outcome, not confirmed failure. Never automatically resend.
- Keep enough correlation to reconcile late confirmations while the daemon is alive; preserve unresolved intent across restart, without claiming authoritative recovery unsupported by the sidecar.
- The sidecar correlation map is in memory. Its acceptance boolean is not a delivery receipt. No matching by body/time heuristics.
- Client and engine currently both have 30-second timeouts, so the client can time out before receiving the daemon result. Address this race in the send-status contract.
- Explicit outcome/protocol additions require a compatibility decision, not silently changing version-1 enum contracts.

## Current implementation checkpoint

- UI batch implemented: borderless pane-width command menu, composer-only shaded
  surface with one padding row above and below when the terminal allows, reserved
  transcript spacing, repeated sidebar selection marker across both chat rows,
  and End-to-newest.
- The sidecar now correlates synchronous and asynchronous send completions in
  `SendCoordinator`. A synchronous engine return means "nothing pending", not
  "rejected", so the daemon confirms acceptance instead of surfacing a false
  failure. Push confirmations win exactly once; the coordinator suppresses a
  duplicate synchronous confirmation and drops the request on a thrown error.
  Covered by the Dart `send_coordinator_test` and the daemon
  `late_confirmation_after_send_timeout_is_not_reconciled` test.
- The TUI writes private, bounded diagnostics to `tui.log` (no message text,
  recipient identifiers, raw frames, or arbitrary error text) and shows an
  outcome-aware reason instead of a generic "Send failed".
- Dart CI formatting fix and the Rust guards are verified locally; push and a
  GitHub rerun are still pending.
- Still not implemented: durable encrypted send intent with restart-safe
  reconciliation, an explicit "unknown outcome" send status for the 30-second
  timeout, duplicate-direct consolidation, the settings overlay, and contacts.

Research and synthetic reproduction do not establish the cause of any particular
live-account incident. The send coordinator removes a confirmed false-failure
path; it is not proof that every live send succeeds.
