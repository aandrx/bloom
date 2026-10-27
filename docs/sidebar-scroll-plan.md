# Sidebar viewport scrolling and click reliability

Status: implemented with synthetic and real-PTY coverage. No live account,
engine, IPC or packaging change was made.

## Problem

Before this change, the left sidebar treated a wheel notch as a chat-selection step. The user
wants normal viewport scrolling instead: the wheel scrolls the sidebar like a page,
a scrollbar shows the position, and clicking a chat changes the selection.

Two defects are reported with the change:

- When the user reaches the bottom of the sidebar, the last visible chat cannot be
  clicked. The list forces its way back until the first chat is at the top again.
- The last selectable chat appears to be the last chat that is visible while the
  first chat is still visible, so chats below the first page cannot be reached.

## Desired behavior

- Wheel over the sidebar scrolls the sidebar viewport. It does not change the
  selected chat.
- A visible scrollbar appears when the list overflows and reflects the viewport
  position. It sits on the sidebar's right edge and does not cover chat names.
- Clicking any fully painted chat row selects it and does not jump the viewport
  away from the clicked row.
- The last chat in the list is reachable, fully painted and clickable when the
  viewport is at the bottom. No first-chat anchoring.
- Keyboard selection (`Up`/`Down`, `Ctrl+B n/p`, sidebar-focus arrows) still
  changes the selection and scrolls the selected row into view.
- Manual wheel scrolling may leave the selection offscreen. The highlight and
  scrollbar remain truthful; the next keyboard step may scroll it back into view.
- Selecting a chat while a new-chat composition is active must cancel that
  composition, park its draft under the new-chat owner, and select the chat. It
  must not leave the sidebar in a first-page-only state.
- Narrow widths and the existing Chats navigator keep their current behavior.

## Evidence and likely causes

- Wheel over the sidebar calls `select_step` (`app.rs:4999-5009`), which changes
  the selection (`app.rs:2633-2646`). It never scrolls a viewport.
- The sidebar creates a fresh `ListState` every frame and selects the current
  chat (`ui.rs:2761-2812`). Ratatui's list auto-scrolls to the selection; there is
  no persistent sidebar offset.
- Hitboxes are derived from the list's post-render offset (`ui.rs:2793-2811`).
  The render suppresses the list selection entirely while `new_conversation` is
  active (`ui.rs:2776`), leaving the offset at zero while the selected id still
  changes.
- A sidebar click calls `select` directly (`app.rs:5055-5059`) and never clears
  `new_conversation`, unlike the drawer/open path, which parks the draft and
  clears the flag (`app.rs:2848-2868`). This is the leading explanation for the
  first-chat-at-top symptom: the user pressed `n` for a new chat, then tried to
  navigate the sidebar.
- The last-row hitbox requires a full entry height (`ui.rs:2799-2808`). That is
  correct for fully visible rows, but the last row needs a regression test once
  the offset is deterministic.

The implementation must first reproduce the reported bottom-click failure with a
failing test, then confirm which cause it hits. Do not fix only the symptom.

## Design

### Scroll state

- Add a persistent sidebar entry offset to `App` (for example `sidebar_scroll`).
- The UI paints exactly the visible window from that offset. Do not let a
  ratatui `ListState` selection choose the offset on its own.
- Highlight the selected row without using list auto-scroll, either by styling
  the selected `ListItem` directly or by rendering rows in a small custom helper.
- Clamp the offset on resize, filter changes, hidden/closed chats and snapshot
  reconciliation. Reset to zero when the visible set's identity changes.
- Keep the offset numeric for this unit. Do not add a top-visible-id anchor or
  change the existing conversation ordering.

### Wheel and scrollbar

- Wheel over the sidebar changes only `sidebar_scroll`; one chat per notch in the
  first phase. The transcript wheel path is untouched.
- Add a one-column scrollbar to the sidebar's right edge using ratatui's
  `Scrollbar`/`ScrollbarState`, reserving the column from the entry width.
- The scrollbar is visible only when the list overflows. Its position follows the
  offset and its viewport length follows the number of fully visible entries.
- Phase 2 of the same unit adds track click and drag after the wheel/scrollbar
  core passes. The painted track and thumb must be part of the hit map before
  drag is enabled.

### Clicks and selection

- A chat click selects the painted conversation id and keeps the offset.
- If `new_conversation` is active, the click path parks the new-chat draft and
  clears `new_conversation` before selecting, reusing the existing drawer/open
  behavior rather than duplicating it.
- A click on an empty sidebar area, the header or the scrollbar must not select a
  hidden chat.
- Keyboard selection computes the target index, moves the selection and adjusts
  the offset only when the target is outside the painted window.
- The selection may be offscreen after a wheel scroll. No auto-follow on wheel.

### Geometry

`HitMap` must expose the sidebar viewport data needed by input:

- the sidebar content rectangle and entry height;
- the painted offset, visible entry count and maximum offset;
- the scrollbar track and thumb rectangles (phase 2);
- the painted rows, as today.

Input must use only these painted values. It must never recompute a row from a
stale selection or a different list state.

## Files

- `apps/bloom-tui/src/app.rs`: scroll field, wheel routing, click path, new-chat
  cancellation, keyboard scroll-into-view, tests.
- `apps/bloom-tui/src/ui.rs`: sidebar window rendering, manual highlight,
  scrollbar, hit map fields, render tests.
- `apps/bloom-tui/tests/ui_render.rs`: painted window, last-row hitbox,
  scrollbar visibility and narrow/drawer regressions.
- `scripts/test-tui-reading-e2e.py` or `scripts/test-tui-messaging-e2e.py`: real
  TUI PTY with a synthetic long conversation list.

## Acceptance

Failing-before/passing-after evidence:

1. App test: wheel over the sidebar changes the offset but not the selected chat.
2. Render test: at the bottom offset, the last chat is fully painted and its row
   hitbox maps to that chat; clicking it selects it.
3. App test: clicking a chat while `new_conversation` is active clears the flag,
   parks the draft under the new-chat owner and selects the chat.
4. Render/app test: keyboard selection at the bottom scrolls the selected chat
   into view without a top jump.
5. Render test: the scrollbar is absent without overflow and visible at top and
   bottom; it does not overlap the name/preview column.
6. App/render tests: filter changes, hidden chats, resize and snapshot
   reconciliation clamp or reset the offset safely.
7. Actual PTY with at least 30 synthetic chats: wheel to the bottom, assert the
   selection did not change, click the last visible row, assert the last chat was
   selected, and assert the first chat is not forced back into view.
8. Narrow drawer and existing reading/messaging PTY suites remain unchanged.

Broader checks: `cargo test -p bloom-tui`, workspace tests, Tokyo-timezone tests,
strict Clippy, formatting, source guard, rebuilt TUI, and the reading/messaging
PTY suites. No live account or engine is needed.

## Implemented

- `App.sidebar_scroll` persists an entry offset; `sidebar_reveal_pending` reveals a
  changed selection on the next paint. Wheel and scrollbar input clear the pending
  reveal.
- The sidebar renders from the explicit offset without selection-driven
  `ListState` scrolling. Selected rows use the marker and row background directly.
- Wheel over the sidebar scrolls one chat per notch and accumulates against the
  live offset, so a burst before one frame is not dropped. A one-column scrollbar
  appears when the list overflows; the track is clickable and draggable.
- Clicked rows use the painted ids. Selecting a chat while `new_conversation` is
  active parks the draft under its `NewChat` owner and clears the mode.
- Filter open, typing, paste, category changes and cancel reset the viewport to
  the top; closing the filter re-arms selection reveal. Keyboard selection reveals
  the selected row. The sidebar viewport metrics travel in `HitMap` and are
  recorded after each paint.
- The scrollbar/track and last-row hitboxes share the same offset as the painted
  rows, so the last visible chat is clickable at the bottom.

Evidence: 171 TUI library tests, 128 render tests, and 26 reading/navigation PTY
tests, including a real 24-chat sidebar flow that wheels without changing the
selected chat and clicks the bottom row. Workspace and Tokyo tests, strict
Clippy, formatting and the source guard pass. No live account or engine was used.

## Non-goals

- Redesigning the transcript or composer.
- Changing conversation ordering, unread semantics or search.
- Redesigning the narrow Chats navigator; it must simply not regress.
- Scrollbar interactions beyond the track click/drag implemented here.
- Live account access, sends, or engine changes.

## Roadmap placement

This was the immediate current-chat UI follow-up and is now implemented. It does
not reopen any accepted live gate; the remaining tracks resume in the
[original plan](next-iteration-plan.md).
