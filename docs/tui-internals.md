# TUI rendering and interaction internals

**Status: current engineer guide.** See [user controls](tui.md), [handoff](handoff.md), and [system design](system-design.md). Source remains authoritative when changing internals.

## Event loop and redraws

`main.rs` uses `RedrawScheduler` from `lib.rs`: event/input/result branches request a redraw; the scheduler coalesces pending requests and imposes an 8.33 ms minimum frame interval (120 FPS cap). The select loop also waits for the scheduled draw deadline. It does not redraw unconditionally on every event-loop iteration. Animation ticks are 33 ms when `App::is_animating()` is true; the cap is a maximum cadence, not a guaranteed 120 FPS.

Input comes from a blocking terminal reader into an async channel. Only key Press/Repeat are applied. Bracketed paste and resize are forwarded explicitly; paste is data, not simulated key commands. Long IPC requests run asynchronously, preserving responsiveness. Rendered `HitMap` data determines mouse targets; overlays take priority over underlying content for both clicks and wheel gestures. The full-screen Chats navigator owns keyboard, paste and mouse before the underlying modal or composer.

## State, ownership and results

`App` maintains recipient-owned session drafts, cursor/paste provenance, a suspended composition for scratch panels, and typed send failures/recovery queues. New-chat draft ownership includes the address. Selection/alias/filter changes must not move a body to a different recipient. Restore composition only for the matching owner; preserve newer drafts and pending-attempt identity. Clear/cancel semantics are deliberate and distinct from closing a transient panel.

The Chats navigator keeps its query/candidate separate from actual selection. Incoming aliases migrate the stored candidate in `set_conversations`, without committing navigation, so successive snapshots that expose only the previous identity still retain the same logical chat. Read-time alias lookup alone would strand the stored candidate after two transitions.

`main.rs` propagates App-generated send attempts without inventing another counter. New-chat navigation waits for correlation between the acknowledged message ID, actual message and conversation metadata. Neither success text nor a matching preview establishes a recipient's thread. No automatic retry follows an uncertain outcome.

## Animation and status

`App::frame` advances a wrapping tick; `spinner_frame` chooses one of ten braille glyphs. Animation predicates include connection trouble, non-idle session state, loading, queued messages, inferred sync activity and explicit sync in-flight state. The header uses a spinner for connection/retry or actual Syncing text. Queued-message text still has a fixed `⠋ queued` marker; an animation predicate alone is not per-item animated rendering.

Manual metadata sync is tracked from dispatch through receipt; duplicate dispatches are suppressed. Message/update/delivery/list events set inferred catch-up activity until two seconds of quiet. These are separate lifetimes. Failed/blocked/disconnected engine sessions and IPC connection trouble outrank healthy activity. There is no authoritative archive-progress/completion signal.

## Layout and composition

`composer.rs` wraps extended graphemes and explicit newlines, retaining blank/trailing lines and cursor positions. Helpers in `ui.rs` and `main.rs` share usable composer/prefix width so vertical editing matches rendered geometry. Long drafts have bounded visible rows and overflow cues. Horizontal mode still respects explicit multiline input.

The renderer has no outer frame, a responsive rail and shared content gutters. Below 70 columns, hide the rail/divider and offer `‹ Chats` plus Tab to open a full-screen navigator. Its query/candidate are separate from App's actual selection and recipient-owned composition; only explicit open commits navigation. Cancel preserves the underlying panel, cursor, paste provenance and reading position. The list uses rendered ListState offsets for mouse rows and remains modal across resize. Header identity/status receive measured space before branding/theme/settings; narrow layouts can put status on the second row while new-message mode preserves `To:` context. Very small terminals use a bounded fallback. Tall pickers clear unreadable one-/two-row transcript fragments, including text cells, not just background styles.

Settings/contacts/filter/theme/new/emoji/slash and context menus use explicit selected-row markers and consistent surfaces. Verify selected foreground against selected background: the former inverse-cyan treatment is no longer appropriate for neutral selection surfaces. Header/popup hitboxes must correspond only to controls actually drawn.

## Transcript reading positions

`transcript.rs` stores per-message display-row spans, a message/row anchor and the last painted viewport. `ui.rs` measures each styled line with ratatui 0.29's `Paragraph::line_count` and the same `Wrap { trim: false }` used to paint it (the dependency enables `unstable-rendered-line-info`). There is no second word/grapheme wrapping implementation to drift from the renderer.

The terminal loop owns a `RenderCache` and calls `render_with_cache`; `render` remains a fresh-cache compatibility entry point. A four-entry LRU retains formatted lines and ratatui-measured geometry, keyed by selected identity, message values, selected conversation metadata, nicknames, resolved palette and width. Height, scrolling, cursor/draft edits, animation and panel visibility do not invalidate layout; command-menu muting applies to visible clones. Equal reconciliation snapshots reuse layouts. Edited/removed history drops obsolete cached versions for that identity.

Painting slices the logical lines intersecting the viewport and scrolls relative to their first measured row. It no longer rewraps all preceding messages on every redraw. A single over-height body still needs wrapping, and cold/invalidated layouts still measure the loaded history. Cache storage is session-only, bounded by entry count, and never logged or persisted. `ui_cache_tests.rs` checks reuse/invalidation/LRU and compares sliced rendering against full-history Paragraph pixels, including Unicode, over-height messages, muting and clipped rows. See [measured switching evidence](tui-switching-performance.md).

`HitMap` returns that frame's geometry; `main.rs` records it only after successful drawing. Gestures anchor against the last displayed spans, including coalesced event/gesture races; fresh rendering resolves the identity against the updated layout. Following latest is zero, not an implicit reaction to every event. Explicit conversation navigation resets reading; canonical identity aliases remap the same context. Missing canonical history and tiny fallback frames retain the anchor until a transcript can be drawn. An unavailable/deleted anchor falls back to a deterministic clamped distance within the loaded history, not a promise to recover older messages.

Generated delivery icons and their words use a nonbreaking space, so wrapping cannot separate the checkmark from its label. This changes display text only, never message content. Ratatui's paragraph scroll offset remains u16; exceptionally large wrapped transcripts are not unbounded by this pass.

The measured Latest/End control occupies the existing separator, not an error/uncertainty feedback row. It has no hidden hitbox under an open panel or in a layout that cannot fit it. A short history clamps away stale reading-back state. The existing no-orphan-byline adjustment runs before recording the actual viewport.

## Exact search context

`SearchContext` holds one generation-correlated preview of at most 200 rows, separate from `App.messages` and normal older-page state. Unloaded search hits use the v4 exact `message_context` request; no intermediate pages are accumulated. `displayed_messages` supplies the formatter and cache key, while `displayed_transcript_conversation` supplies matching viewport identity.

Normal snapshots, arrivals, and older-page replies update normal history. They do not extend the preview. Canonical alias changes restart the same target under a new generation. Replies for a superseded generation, target, or selection are ignored. Context replies must contain the target, stay within the row cap, and use the expected conversation ID.

Context frames never advance local read markers. Normal older paging is disabled in the preview. End or the measured Latest control drops context, clears its viewport geometry, and returns to normal history without modifying composition. Short contexts retain that exit even when their local scroll distance is zero. Overlays and tiny layouts still suppress hidden mouse targets.

## Themes and terminal detection

Dark uses neutral RGB surfaces; light uses readable gray/white surfaces, and both define semantic accents/selection. These remain fixed RGB palettes, not general color-depth quantization or contrast adaptation. Theme override persists; automatic theme follows startup OSC 11 background detection.

The query uses `/dev/tty` only when stdin/stdout are terminals, sends OSC 11, reads a bounded reply under a 200 ms deadline, parses hex/rgb channels and selects light/dark by mean brightness. Parser/threshold are unit-tested; broad terminal query behavior is not proven by those tests.

`NO_COLOR` is enabled by any non-empty value and cached. All palette roles become terminal-default `Color::Reset`; text, glyphs, layout and emphasis remain. State/selection must retain non-color cues. There is no reduced-motion preference or screen-reader validation.

## Terminal lifecycle and diagnostics

The TUI enables raw/alternate-screen, mouse and bracketed-paste modes and restores them on normal exit. The installed launcher additionally captures original termios before starting its child and restores it on exit/termination. An actual launcher PTY regression tests raw-mode recovery.

TUI diagnostics are private, bounded/rotated and allowlisted; never add bodies, identifiers or arbitrary raw errors. Optional numeric inbound timings do not authorize logging content. External engine/daemon stderr is a different trust boundary.

## Verification and visual tooling

Library/interactions/render tests cover ownership, cursor geometry, palette/monochrome cues, responsive modes and hitboxes. `daemon_session.rs` covers reconnect/reconciliation. Actual PTY scripts cover input, exact send/recovery, modal safety, delayed sync and resize; launcher subprocess tests cover production entry-point lifecycle. See [handoff commands](handoff.md).

`scripts/tui-shot.py` reconstructs terminal output with pyte and emits fixed-cell HTML so browser fallback-font advances do not create false ragged terminal surfaces; wide continuation cells are not rendered as extra spaces. `tui-review.sh` drives an isolated 37-state matrix, including narrow navigation/search/empty/monochrome/light states and return-to-latest. These captures are not proof of every font, terminal protocol, emoji sequence or assistive technology. Inspect actual output after layout changes and rerun affected captures; do not change capture scripts mid-run.

Codex is an interaction reference. Bloom now has coalesced redraws but still lacks reduced-motion controls and comprehensive palette-depth adaptation; do not copy old comparison documents claiming the scheduler is absent.
