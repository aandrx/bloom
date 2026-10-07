# TUI rendering and interaction internals

**Status: current engineer guide.** See [user controls](tui.md), [handoff](handoff.md), and [system design](system-design.md). Source remains authoritative when changing internals.

## Event loop and redraws

`main.rs` uses `RedrawScheduler` from `lib.rs`: event/input/result branches request a redraw; the scheduler coalesces pending requests and imposes an 8.33 ms minimum frame interval (120 FPS cap). The select loop also waits for the scheduled draw deadline. It does not redraw unconditionally on every event-loop iteration. Animation ticks are 33 ms when `App::is_animating()` is true; the cap is a maximum cadence, not a guaranteed 120 FPS.

Input comes from a blocking terminal reader into an async channel. Only key Press/Repeat are applied. Bracketed paste and resize are forwarded explicitly; paste is data, not simulated key commands. Long IPC requests run asynchronously, preserving responsiveness. Rendered `HitMap` data determines mouse targets; overlays take priority over underlying content.

## State, ownership and results

`App` maintains recipient-owned session drafts, cursor/paste provenance, a suspended composition for scratch panels, and typed send failures/recovery queues. New-chat draft ownership includes the address. Selection/alias/filter changes must not move a body to a different recipient. Restore composition only for the matching owner; preserve newer drafts and pending-attempt identity. Clear/cancel semantics are deliberate and distinct from closing a transient panel.

`main.rs` propagates App-generated send attempts without inventing another counter. New-chat navigation waits for correlation between the acknowledged message ID, actual message and conversation metadata. Neither success text nor a matching preview establishes a recipient's thread. No automatic retry follows an uncertain outcome.

## Animation and status

`App::frame` advances a wrapping tick; `spinner_frame` chooses one of ten braille glyphs. Animation predicates include connection trouble, non-idle session state, loading, queued messages, inferred sync activity and explicit sync in-flight state. The header uses a spinner for connection/retry or actual Syncing text. Queued-message text still has a fixed `⠋ queued` marker; an animation predicate alone is not per-item animated rendering.

Manual metadata sync is tracked from dispatch through receipt; duplicate dispatches are suppressed. Message/update/delivery/list events set inferred catch-up activity until two seconds of quiet. These are separate lifetimes. Failed/blocked/disconnected engine sessions and IPC connection trouble outrank healthy activity. There is no authoritative archive-progress/completion signal.

## Layout and composition

`composer.rs` wraps extended graphemes and explicit newlines, retaining blank/trailing lines and cursor positions. Helpers in `ui.rs` and `main.rs` share usable composer/prefix width so vertical editing matches rendered geometry. Long drafts have bounded visible rows and overflow cues. Horizontal mode still respects explicit multiline input.

The renderer has no outer frame, a responsive rail and shared content gutters. The compact rail retains useful identity width at 45 or more terminal columns. Header identity/status receive measured space before branding/theme/settings; narrow layouts can put status on the second row while new-message mode preserves `To:` context. Very small terminals use a bounded fallback. Tall pickers clear unreadable one-/two-row transcript fragments, including text cells, not just background styles.

Settings/contacts/filter/theme/new/emoji/slash and context menus use explicit selected-row markers and consistent surfaces. Verify selected foreground against selected background: the former inverse-cyan treatment is no longer appropriate for neutral selection surfaces. Header/popup hitboxes must correspond only to controls actually drawn.

## Themes and terminal detection

Dark uses neutral RGB surfaces; light uses readable gray/white surfaces, and both define semantic accents/selection. These remain fixed RGB palettes, not general color-depth quantization or contrast adaptation. Theme override persists; automatic theme follows startup OSC 11 background detection.

The query uses `/dev/tty` only when stdin/stdout are terminals, sends OSC 11, reads a bounded reply under a 200 ms deadline, parses hex/rgb channels and selects light/dark by mean brightness. Parser/threshold are unit-tested; broad terminal query behavior is not proven by those tests.

`NO_COLOR` is enabled by any non-empty value and cached. All palette roles become terminal-default `Color::Reset`; text, glyphs, layout and emphasis remain. State/selection must retain non-color cues. There is no reduced-motion preference or screen-reader validation.

## Terminal lifecycle and diagnostics

The TUI enables raw/alternate-screen, mouse and bracketed-paste modes and restores them on normal exit. The installed launcher additionally captures original termios before starting its child and restores it on exit/termination. An actual launcher PTY regression tests raw-mode recovery.

TUI diagnostics are private, bounded/rotated and allowlisted; never add bodies, identifiers or arbitrary raw errors. Optional numeric inbound timings do not authorize logging content. External engine/daemon stderr is a different trust boundary.

## Verification and visual tooling

Library/interactions/render tests cover ownership, cursor geometry, palette/monochrome cues, responsive modes and hitboxes. `daemon_session.rs` covers reconnect/reconciliation. Actual PTY scripts cover input, exact send/recovery, modal safety, delayed sync and resize; launcher subprocess tests cover production entry-point lifecycle. See [handoff commands](handoff.md).

`scripts/tui-shot.py` reconstructs terminal output with pyte and emits fixed-cell HTML so browser fallback-font advances do not create false ragged terminal surfaces; wide continuation cells are not rendered as extra spaces. `tui-review.sh` drives an isolated 31-state matrix. These captures are not proof of every font, terminal protocol, emoji sequence or assistive technology. Inspect actual output after layout changes and rerun affected captures; do not change capture scripts mid-run.

Codex is an interaction reference. Bloom now has coalesced redraws but still lacks reduced-motion controls and comprehensive palette-depth adaptation; do not copy old comparison documents claiming the scheduler is absent.
