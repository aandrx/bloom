# Historical product audit and completed iteration

**Status: earlier audit, preserved as evidence of decisions and defects.** Its “chosen iteration” is implemented, not a fresh task queue. Later fixes and a current feature inventory are in [iteration-review.md](iteration-review.md); fresh sessions start with [handoff.md](handoff.md) and [the active roadmap](implementation-plan.md). Counts/scores below describe that checkpoint, not the latest test totals or an absolute product guarantee.

## Product direction

Bloom should be a trustworthy, keyboard-first messenger, not a coding-agent UI with messages substituted for tool output. Codex is the reference for restrained terminal surfaces, an immediately usable editor, progressive command discovery, and predictable focus. Sending to the right person, preserving composition, and accurately describing sync matter more than decorative similarity.

The current target is the terminal client. `design/mockup/index.html` and GPUI previews are illustrative, not additional working clients. Do not spread implementation across another surface before reliable messaging is established.

## Audit method and evidence

This audit combines current-source review, independent feature/reliability reviews, parent-owned visual critique, Rust workspace tests and strict Clippy, 56 Dart tests and analysis of owned Dart source, and actual TUI processes driven through PTYs against isolated synthetic Unix-socket daemons. No real engine, account state, registration, credentials, or personal messages were accessed.

The pre-iteration PTY captures cover default/wide/narrow/tiny layouts, light theme, settings tabs, contacts/search, filter/no-results, new recipient/body, help/small help, slash commands, theme picker, wrapped composition, chat switching, paste, multiline entry, and rename. Temporary artifacts: `/tmp/bloom-iteration-audit`. Reproducible behavioral suites are `scripts/test-tui-messaging-e2e.py` and `scripts/test-tui-input-e2e.py`; they require a built TUI and Python with pyte. Visual artifacts are terminal-emulator reconstructions, not proof of every terminal's font/emoji rendering.

Passing existing tests did not imply complete functionality: the new PTY reproduction persisted `Private Alice message` to synthetic Bob after switching chats. Bracketed paste disappeared and Ctrl+J concatenated lines. These are functional failures, not aesthetic preferences.

## Feature inventory

| Feature | Current boundary | Evidence / next gate |
| --- | --- | --- |
| Conversation list and local history | Stored messages only; visible history limited to 200 | `apps/bloom-tui/src/daemon.rs`; add cursor-based local history pagination |
| Live inbound | Sidecar to daemon to encrypted store to IPC to transcript | Synthetic real-process coverage; current real-engine reception must be revalidated |
| Historical sync | Not implemented | `Engine::history` returns Unsupported; generated API signatures are insufficient evidence for pagination/token semantics |
| `/sync` | Conversation metadata, then a local-store refresh | Must not advertise recovery of absent messages |
| Reconnection | TUI reconnects to IPC; engine generation does not restart | Supervisor needs a fresh-generation restart architecture with exclusive ownership and no send replay |
| Existing/new-chat sends | Plain text; acknowledged ID correlates new-chat navigation | Synthetic send/reject/retry/relaunch coverage; uncertain delivery never auto-retried |
| Draft recovery | Failed snapshots retained in memory; explicit recovery | Must preserve recipient, newer text, pending-attempt identity, and uncertainty |
| Composer | Grapheme editing and wrapped draft; input gaps found during audit | This iteration: paste, logical multiline, resize, recipient-bound drafts |
| Search | Conversation names/handles and known-participant nicknames | Not message-content search or a system address book |
| Contacts | Known participants with local nickname overrides | Label this distinction; no remote contact discovery |
| Groups | Existing multi-recipient text send path; no group management | Capability declaration and actual supported operations need alignment and real validation |
| Attachments | Inbound unsupported-part placeholders; outbound text only | No download/open/send attachments UI; don't silently drop unknown content |
| Reactions, edits, unsend, typing, read receipts | Domain types do not establish end-to-end support | Require engine mapping, persistence, IPC, and UX together |
| Theme/layout/settings | Local light/dark/automatic theme, density, wrapping | Keyboard/mouse/headless-render coverage; inspect contrast and small sizes |
| Rename/hide | Local labels and hiding, not remote rename/delete | Keep actions non-destructive and explain scope |
| Diagnostics | Allowlisted errors and numeric timings, not message content | Continue private permissions, rotation, payload redaction |
| Encryption/identity | Encrypted store, scoped IDs, conservative alias reconciliation | Never replace safe identity checks with guessed display-name matching |
| Startup/shutdown | Existing engine registration and exclusive owner required | Fix persistence drain; package/setup and health guidance remain product work |
| Desktop/mobile | Static mockups only | Explicitly out of the current implementation scope |

## Critique before the iteration

- **Composition safety is below the visual quality.** One global draft can move to another recipient. Paste and modified Enter behavior do not meet normal terminal-editor expectations.
- **New-message context is ambiguous.** The old chat remains in the header/transcript while the new address is abbreviated inside the input prefix. Identity should have a dedicated, readable location, not compete with the text being written.
- **Empty-state copy overclaims.** An empty local history is not evidence that a conversation has never had messages. Disconnected and loading states need different copy and actions.
- **Hierarchy is busy at small sizes.** Participant counts look like unread badges; direct-chat counts waste width. The header competes with identity for theme/settings/help/branding space. Changes should preserve discoverability rather than simply remove controls.
- **Documentation and help lag capabilities.** Shortcut help should include recovery, multiline fallback and local-only sync semantics. Not-yet-implemented engine operations must not be presented as complete because the domain model contains them.
- **The TUI has too many independent mode flags.** A future explicit composer/mode state model is justified, but it should follow regression coverage rather than precede it as a large speculative rewrite.

## Chosen iteration

1. Safe literal paste, predictable newline shortcuts, logical multiline rendering, immediate idle resize, exact body preservation.
2. Session-local drafts bound to conversation identity, with cursor and literal-text provenance. Separate new-message/modal input from chat drafts. Never store plaintext message drafts in preferences.
3. Parent-owned UI changes: new-message header and full recipient row, no unrelated transcript in new-message mode, truthful local-history/connection/loading empty states, accessible light-theme muted text, updated shortcut hierarchy.
4. Drain daemon persistence before claiming clean exit, not merely forward events into another queue and abort its consumer.
5. Independent review, full tests, actual terminal regression flows, and a final parent-owned visual critique.

## Next milestones and release gates

### P0: Trustworthy messaging

- Implement generation-scoped engine recovery: old child reaped before new owner starts, bounded backoff, fresh event channels, generation guards, shutdown during backoff, no automatic replay of uncertain sends.
- Implement genuine historical ingestion only after pinned-version source semantics are verified: page-level idempotent ingestion, live/history deduplication, explicit tombstone policy, encrypted checkpoint committed only after durable page acknowledgment, cancellation/resume tests.
- Revalidate authorized live receive while open, receive after disconnect/restart, text send confirmed independently, and historical import. A synthetic fixture cannot pass this gate.
- Represent remote acceptance plus local persistence failure distinctly; a storage error after remote acceptance is not proof of non-delivery.

### P1: Complete the daily conversation loop

- Cursor-based local pagination and in-message search, not a silent 200-message archive ceiling.
- Explicit unread state, jump-to-latest with unseen count, and no forced jump while intentionally reading older messages.
- Durable drafts only via an encrypted store and a reviewed retention/privacy model; session-only preservation must remain clearly documented.
- Keyboard paths for every mouse-only action, consistent overlay ownership, undo, and optionally a private external-editor handoff.
- Responsive chat rail with a deliberate narrow-screen drawer rather than tiny clipped identities.

### P2: Broader messenger features

- Capability-driven attachments, replies/reactions, edits/unsend, delivery receipts and group management. Each requires persistence and end-to-end transport tests, not only widgets.
- Standalone packaging, dependency/version diagnostics, account/session recovery guidance, and accessible user documentation.
- Reconsider GPUI only after the shared messaging core satisfies P0 and P1 gates.

## Reference patterns

- Codex CLI command discovery and session orientation: https://learn.chatgpt.com/docs/cli/slash-commands and https://learn.chatgpt.com/docs/codex/cli
- Claude Code multiline fallback, editing, recovery and contextual help: https://docs.anthropic.com/en/docs/claude-code/interactive-mode
- Pi's local `docs/usage.md`, `docs/keybindings.md`, `docs/tui.md`, and `docs/terminal-setup.md`: keyboard alternatives, fixed editor with scrollable transcript, terminal protocol differences, grapheme widths and IME/focus handling.

These are interaction references, not evidence that Bloom implements their features or a reason to copy coding-specific agent controls.

## Acceptance and rating

A functional score of 8/10 requires the P0 live-message/history gates; visual polish alone cannot earn it. The final critique should score composition safety, recipient orientation, reading/navigation, responsiveness/accessibility, and operational trust separately, list remaining defects, and distinguish synthetic verification from live proof.

Final iteration verification and critique are recorded below after implementation.

## Iteration result: verified changes

All of the following were reproduced against the real TUI process first, then fixed, then re-verified:

- **Draft ownership.** A draft typed for Alice was previously persisted to Bob by switching chats and pressing Enter. Drafts are now held per recipient for the session, with cursor and literal-text provenance. New-chat text is owned by the new-chat flow, not the selected chat. An identity alias migrates the draft instead of stranding it, and the initial snapshot-driven selection now establishes ownership. Reproduced end to end in `scripts/test-tui-input-e2e.py`; regression-tested in `interaction.rs` (`drafts_follow_their_chat_even_when_a_snapshot_establishes_the_selection`).
- **Literal paste.** Bracketed paste is enabled, forwarded, and inserted as text with control-character sanitation and exact body whitespace. A pasted `/new` stays a message body and never runs a command or sends without an explicit Enter. Confirmed in the same PTY suite.
- **Multiline editing.** Ctrl+J (plus Alt+Enter and Shift+Enter where reported) inserts a logical newline; blank and trailing lines survive, and the composer wraps explicit lines even when wrapping is off.
- **Idle resize.** Resize events are no longer discarded; an idle terminal redraws after SIGWINCH without waiting for the reconciliation tick.
- **New-message identity.** The new-chat composer gets a dedicated `New message` header and a full-width `To:` row, hides any other transcript, and shows a New conversation empty state. Previously the prior chat stayed on screen and the recipient was clipped inside the input prefix.
- **Honest empty and connection states.** An empty local transcript reads "No stored messages / Older history is not imported" instead of implying the chat never had messages; disconnected and loading states get their own copy.
- **Rail tidy-up and footer.** Participant counts no longer mimic unread badges; names use the full rail width and clip with an ellipsis. The footer leads with `Enter send` and surfaces `Ctrl+J newline`; the shortcut sheet documents multiline and recovery.
- **Daemon shutdown drain.** Shutdown no longer aborts the persistence consumer; the supervisor sender is dropped so persistence observes end-of-stream, then it is awaited within a bound, and "clean shutdown" is only logged when forwarding and persistence both complete.

## Verified locally

`cargo test --workspace` (all crates, including 54 interaction, 74 UI-render, 8 input-safety tests and 13 daemon sidecar tests), strict `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, the eight stateful-fixture black-box tests, the seven PTY input/draft-safety tests, and the four messaging PTY tests. 56 Dart tests and owned-source analysis pass; generated bindings are neither analyzed nor edited. No real engine, account, registration, credential, or personal message was accessed; every live-transport claim and historical-sync claim above remains a separate unverified gate.

## Parent critique and score

| Dimension | Before | After | Why |
| --- | --- | --- | --- |
| Composition safety | 4 | 8 | Drafts can no longer cross recipients; paste cannot execute; recovery invariants retained. Remaining: session-only drafts, no undo. |
| Recipient orientation | 4 | 8 | Dedicated header and full `To:` row; no transcript bleed. Remaining: no inline recipient validation feedback. |
| Reading / navigation | 7 | 7.5 | Stable and truthful, but still capped at the stored history and lacks unread/jump-to-newest affordances. |
| Responsiveness / accessibility | 6 | 7.5 | Resize, multiline, paste, `NO_COLOR`, light-theme contrast. Remaining: message-content search and a narrow-screen drawer are absent. |
| Operational trust | 6 | 7 | Retry, uncertain-delivery warnings, honest empty states, persisted shutdown drain. Remaining: no automatic engine restart and no real historical sync. |
| **Overall** | **~6** | **~7.4** | Strong composition/UX gains, but the 8/10 bar is gated by live message/history reliability, not polish. |

The honest ceiling is set by the unreached P0 gate: remote history ingestion and engine recovery are still unimplemented, so a user with a full message history would still see an empty local transcript. The next iteration should implement generation-scoped engine restart and verified historical ingestion before further surface work.
