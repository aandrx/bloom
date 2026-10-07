# TUI UX upgrade: Codex-level interaction

**Status: historical reference and earlier UX plan, not an active work queue.** The listed baseline defects were addressed in later iterations. Read [current user guide](tui.md), [accepted review](iteration-review.md), and [active roadmap](implementation-plan.md). A subjective presentation score does not establish product completeness.

## Reference

Codex v0.157.1 was captured directly under a PTY (`enter`, then `/`, then `?`).
The recorded reference's patterns at that time (several are now implemented in Bloom):

- **No outer border.** The whole window is flush to the edges. Only the header
  block has a rounded frame; content starts at column 0.
- **Composer is bare.** `› Ask Codex to do anything` with no box, no side rules,
  spanning the full pane width.
- **Status and hints are their own quiet rows** under the composer:
  `model · directory` then a hint row (`? for shortcuts`).
- **Slash menu is an inline panel** that appears directly above the composer,
  full width, left-aligned, with the selected row filled. It is not a floating
  dialog and it is not capped to a narrow column.
- **Scrolling belongs to the transcript.** `pgup`/`pgdn` scroll history; the
  composer keeps focus.
- The reference's shortcuts are not Bloom's contract. Bloom uses `?` for help, Enter to send, Tab to change focus and Ctrl+J for newline; see the current user guide.

Captures: `/tmp/codex-ref-main`, `/tmp/codex-ref-slash`, `/tmp/codex-ref-help`.

## Historical reported problems

1. Direct chats render as `Conversation` instead of the recipient.
2. Transcript bubbles, composer and footer stop short of the pane's right edge.
3. Navigation keys misbehave (leader chords, scroll target).
4. Terminal editing gestures do not work (`Ctrl+W` and friends).
5. There is no discoverable way to reach settings.
6. Slash commands surface a red error instead of a menu.
7. Scrolling scrolls the sidebar selection, not the transcript.
8. No mouse support at all.
9. The outer border should go.

## Decisions

- **Naming.** `chat_name` resolves a conversation title, then the direct
  recipient's display name, then the recipient's handle. Groups without a title
  join participant names. Nicknames still win for direct chats. The header shows
  the handle (direct) or `N people` (group) as the secondary line.
- **Layout.** Drop the outer border and the `MAX_CONTENT_WIDTH` cap. Content
  fills the pane; a single subtle sidebar rule separates the rail.
- **Focus model.** `Focus::Composer` (default) and `Focus::Sidebar`. `Tab`
  toggles. In current Bloom, Up/Down first move through wrapped composer lines and scroll at edges; the wheel scrolls its target region. Sidebar focus moves chat selection. `Enter` in sidebar focus opens the chat
  and returns focus to the composer.
- **Editing.** `Ctrl+W`, `Alt+Backspace`, `Ctrl+U`, `Ctrl+K`, `Ctrl+A`, `Ctrl+E`,
  `Ctrl+Left/Right` and `Alt+Left/Right`. Paste arrives through bracketed paste.
- **Key kinds.** Only `Press` and `Repeat` are handled; `Release` is ignored, so
  terminals that report releases cannot double-fire keys.
- **Slash menu.** An inline panel above the composer, full pane width, filled
  selection, argument hint for the highlighted command. `/help` opens it instead
  of printing an error. Errors become notices.
- **Settings access.** `/settings`, the `Ctrl+B s` chord, `,`, and a clickable
  header control. `?` opens the shortcut sheet.
- **Mouse.** `EnableMouseCapture`; wheel scrolls the region under the pointer,
  left click selects sidebar rows, focuses the composer, runs menu rows, toggles
  settings rows, and opens settings from the header control. Rendering returns a
  `HitMap`; input resolves pointer events against it so hitboxes cannot drift
  from what was drawn.

## Historical phases (do not redispatch)

0. Extend `scripts/tui-shot.py` to drive mouse, wheel, modified keys and key
   releases, so every fix is verifiable end to end.
1. Chat naming for direct and untitled group chats.
2. Layout: remove the border and the width cap; footer as Codex-style rows.
3. Input: key-kind filtering, word/line editing, bracketed paste.
4. Navigation: transcript scroll target, sidebar focus, settings keys.
5. Slash popup and notice handling.
6. Mouse support with render-time hitboxes.
7. Tests, guards, PTY screenshots, and the critique loop to 8/10.

## Verification

Every phase updates the unit and integration tests and the PTY screenshot set.
New invariants get guard entries in `scripts/check-guards.manifest`. Current evidence and review limitations are in `iteration-review.md`; temporary historical reference captures may no longer exist.
