# Current-chat UI/UX iteration

**Status: accepted for this bounded UI scope with synthetic verification.** User-directed priority: improve Bloom's existing chat experience. Multi-service integration and workspace/server UI are deferred, not the active implementation scope. Preserve [the multi-service research](multi-service-research.md) as reference only.

The subsequent user-reported switching lag is tracked separately in [the rendering performance follow-up](tui-switching-performance.md). Counts and artifacts below retain this reading/navigation scope's acceptance boundary.

## Standard and scope

Compare interaction quality with Herdr, Pi, Codex and Claude Code without importing their coding-agent concepts. Priorities: stable reading, explicit input ownership, preserved composition, trustworthy status, discoverable actions, measured responsive layouts and restrained visual hierarchy. No arbitrary numerical parity score.

Reference evidence is mixed and must remain labeled:

- Parent directly inspected Herdr's published [narrow switcher](https://herdr.dev/assets/mobile-switch-menu-v2.jpeg) and [agent session](https://herdr.dev/assets/mobile-agent-session-v2.jpeg), linked from its [official usage guide](https://herdr.dev/docs/how-to-work/). The switcher replaces narrow split panes with a dedicated list, strong identities, muted supporting context and a clear close action. Bloom adopts that navigation principle, not workspaces/agents or Herdr's palette. The session image shows Codex inside Herdr; it is not an independently captured current Codex benchmark.
- The historical [Codex PTY comparison](tui-ux-upgrade.md) records v0.157.1's quiet composer, contextual hints, inline commands and transcript-first scrolling. Its temporary captures no longer exist; do not present them as newly inspected screenshots. Current official [Codex CLI documentation](https://developers.openai.com/codex/cli/features/) was also consulted. Existing Bloom styling already follows this restrained reference.
- The researcher reviewed Pi/Claude Code interaction documentation, not live app sessions. Their use here is an interaction standard, not evidence of measured visual parity.
- Parent ran only Herdr help under an empty isolated HOME/environment. No authenticated reference assistant or messaging client was opened and no model request was made for comparison.

## Baseline and reproduced issues

- Fresh `cargo test -p bloom-tui` and all 31 baseline visual states passed before source modifications. Baseline artifacts: `/tmp/bloom-ux-baseline`.
- Parent real-PTY reproduction: with 60 synthetic messages at 100x24, PageUp exposed markers 034 through 049. Three new live events moved the viewport to markers 047 through 062, forcing it to the latest messages. Artifacts: `/tmp/bloom-ux-reading-baseline`; disposable reproducer: `/tmp/bloom-ux-reading-repro.py`.
- Parent real-PTY reproduction: wheel-up over the Settings panel changed the underlying transcript. Closing Settings unexpectedly left the user reading back. Artifacts: `/tmp/bloom-ux-overlay-baseline`; disposable reproducer: `/tmp/bloom-ux-overlay-repro.py`.
- Narrow navigation is a capability/design gap: a ten-column rail below 45 columns clips identities while taking room from the transcript. It needs intentional navigation, not progressively smaller text.

## Selected work and acceptance

### Reading and latest

Preserve a deliberate semantic reading anchor through live append, updates, snapshots and reflow where the anchored message remains available. A nonzero row offset alone is insufficient when new content or wrapping changes. If anchored content disappears, fall back deterministically and do not claim unread completeness. Pin to the latest only when already following latest or explicitly requested. Keep recipient-owned drafts, cursor and literal-paste provenance untouched.

Expose a measured, clickable return-to-latest action when reading back. Any new-arrival count is local session activity, not a remotely authoritative unread count. Preserve error/uncertainty visibility and overlay input priority.

Acceptance: failing-before/passing-after real PTY flow for live arrivals; unit/render coverage for updates, snapshots, duplicate events, wrapped content, clipping and hitboxes; keyboard/mouse return without altering composition.

### Overlay wheel ownership

Wheel events must respect the rendered topmost layer. Within an overlay, scroll/select its supported content or consume the gesture; never move hidden content underneath. Uncovered transcript/sidebar regions retain deliberate normal behavior where the panel is not modal.

Acceptance: actual Settings-wheel PTY regression plus interaction tests for command/theme/context/picker regions and ordinary transcript/sidebar scrolling.

### Narrow chat navigation

Prefer a readable full-width conversation with an explicit chat-navigation control at narrow widths. Navigation must show useful identities, own input while open, make open/cancel clear, and never turn a saved draft into another recipient's message. Use one shared geometry definition for composer wrapping, drawing and mouse targets.

Acceptance: real-process narrow resize and keyboard/mouse selection, cancel and draft-safety flows; light/dark/monochrome render checks, tiny fallbacks, long names and large lists. Scope and exact breakpoint follow baseline review; do not promise a drawer before it passes these tests.

### Sidebar viewport and click ownership (follow-up, implemented)

User-reported: wheel over the left sidebar changes chats instead of scrolling; at the bottom the last visible chat cannot be clicked and the list returns to the first-chat view; while new-chat mode is active the selection can move without the list following. The unit is recorded in [sidebar viewport scrolling and click reliability](sidebar-scroll-plan.md). Implemented: wheel scrolls the viewport without changing selection, a scrollbar shows position and its track can be clicked or dragged, any visible chat including the last row is clickable, selecting while composing a new chat parks the draft and cancels the mode, and keyboard selection still scrolls into view. App/render tests and a real 24-chat PTY flow pass; no live account or engine was used.

## Implementation and independent review

Implemented stable message/row reading anchors with renderer-owned geometry, exact ratatui wrapping measurements, explicit measured Latest/End control and painted-overlay wheel ownership. Added full-width narrow conversations and a searchable Chats navigator with an independent candidate/query; cancel never retargets the recipient or changes the parked composition. Confirm uses existing recipient-owned draft restoration, including unfinished new-chat bodies.

The first delegated implementation was incomplete and was not accepted; parent took over the code. Independent review then found that canonical alias reconciliation still reset reading. Parent reproduced that with a failing App/render regression, remapped the same reading context through alias/loading transitions, and verified the regression passes. A final independent source review found no further confirmed defects; it did not run tests or claim visual verification.

Five permanent actual-PTY regressions pass after building the normal binary: arrivals plus five-second reconciliation and keyboard/mouse Latest with literal send preservation; Settings wheel ownership; resize reading anchor; narrow query/cancel/click/open and recipient-safe draft restoration, including expansion while navigating; successive candidate identity changes across two daemon reconnects. Render/state regressions also cover earlier-message edits, duplicate events, snapshots, canonical history loading gaps, tiny fallbacks, hidden hitboxes, error visibility, deep lists and suspended/new-chat composition.

After the full visual run, parent reproduced a drawer candidate moving to a different logical chat when its ID canonicalized. Both App/render and actual-PTY regressions first failed. A one-hop read-time lookup passed one transition but failed A to B to C when each snapshot exposed only its immediate predecessor. The final correction migrates the stored candidate in `set_conversations`, without committing navigation or changing the actual recipient. An independent focused reviewer confirmed the minimal approach, inspected the final correction/tests and found no actionable defects; no runtime testing was attributed to that review.

Parent visual critique also caught a delivered label stranded on a different row from its checkmark at 44 columns. A new render regression reproduced this at 24 columns. Generated status icons/words now use nonbreaking spaces, verified at 24/44/66 columns; message data and literal composition are untouched.

## Final verification and parent visual critique

Full initial gates passed: workspace all-target tests, Tokyo-timezone TUI tests, strict workspace Clippy, formatting, source guard, rebuilt TUI/daemon, 8 stateful-fixture tests, 11 input PTY tests, 4 messaging PTY tests, 4 reading/navigation PTY tests, 8 launcher tests, and 56 owned Dart tests plus formatting/analysis. Log: `/tmp/bloom-ux-final-gates.log`. After the two follow-up fixes, reran workspace all-target tests, Tokyo TUI tests, strict workspace Clippy, formatting, source guard, normal builds and all five reading/navigation PTY tests. Both normal/timezone runs include 85 render tests. Final log: `/tmp/bloom-ux-accepted-gates.log`; actual-PTY artifacts: `/tmp/bloom-ux-accepted-e2e`.

All 13 final mutation guards passed against a disposable isolated Git snapshot of the working source, with each named regression failing under its mutation and source restoration verified. The actual working tree was never reset or committed. Added six guards for reading anchors, overlay wheels, drawer input ownership, narrow rail removal, successive candidate identities and atomic delivery labels. An earlier run revealed that the old header assertion no longer distinguished its mutant at the new full-width geometry; strengthened its actual 44-column second-row status assertion, reran normally, then proved the complete final manifest. Log: `/tmp/bloom-ux-accepted-guards.log`.

The complete 37-state matrix passed at `/tmp/bloom-ux-final`; production source and capture scripts stayed fixed throughout that run. Parent inspected every state through six-image review sheets plus direct critical-state PNGs. After the final candidate/status fixes, captured and directly inspected fresh 44-column Chats/long-title, 100x12 reading-back and light 66-column Chats states at `/tmp/bloom-ux-accepted-visual`.

Parent critique and acceptance:

- At 44 columns, the previous clipped ten-column rail is gone. Conversation identity and connection/failure status take precedence over optional chrome; the latest body gets full width.
- Chats has a separate query, strong highlighted identity, muted clipped previews, useful empty state and visible Escape close path. Light and monochrome remain understandable without relying only on accent color. Underlying composer/cursor/hitboxes do not leak through.
- Latest is discoverable in the existing separator; existing error/uncertainty feedback retains its own space. Real keyboard/mouse tests verify the affordance and exact literal-body preservation.
- Existing pickers, compact/tiny Help, long titles, multiline composition and failed-session states remain coherent. The final delivery recapture keeps checkmark and label together instead of exposing an unexplained bare word.

These artifacts are ephemeral terminal-emulator reconstructions, not universal font/terminal or screen-reader evidence. Capture scripts and permanent regressions reproduce the relevant cases.

No live-account validation is authorized by this iteration. Existing transport, session-only drafts, no unread authority, no attachment search, fixed-palette and accessibility limits remain. Local paging and local message-text search were added after this review's acceptance and are recorded in the roadmap and TUI guide. Ratatui's u16 paragraph scroll ceiling for exceptionally large wrapped transcripts is not removed or verified by this pass. Do not claim unbounded-history support or parity with a complete messenger.
