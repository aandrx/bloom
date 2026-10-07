# GPUI surface (bookmark, not started)

**Status: not started, and deliberately so.** The shipped client is the terminal UI in
`apps/bloom-tui`. This note records the decision and the recommendations for a future
windowed surface so the reasoning is not lost, and so nobody starts that work by accident
mid-polish. The active work order is in [implementation-plan.md](implementation-plan.md); this is a future-surface bookmark, not a scheduled implementation.

## Why a windowed surface is the eventual target

Two artefacts already point at it:

- `design/mockup/index.html` is a **desktop app**, and its `gpui-*` previews in
  `design/previews/` show the intended windowed layout.
- A character grid caps fidelity in ways no amount of styling fixes: no sub-cell motion,
  monospace only, no inline images or attachments, no proportional type, no hover, no
  smooth scrolling, no drag.

The current client is nevertheless the right thing to finish first: it is usable today, it
is what Codex itself does (Codex is a ratatui TUI, so "Codex-like" does not require a
renderer change), and it keeps working over SSH and inside whatever terminal the user
already lives in.

## The two shapes, and which to pick

1. **Native window, our own rendering (GPUI).** Recommended. The window is ours; the chat
   surface is drawn with the UI toolkit. This is what the `gpui-*` previews describe, and
   it removes every character-grid limit listed above.
2. **Native window with an embedded terminal widget (herdr-style).** Not recommended.
   Herdr renders its outer UI on the client and embeds terminal views (it uses
   Ghostty/wterm helpers), which is right for a multiplexer whose payload *is* someone
   else's terminal. Bloom's payload is not a terminal: embedding one would inherit the
   character grid's limitations *and* the GUI toolkit's complexity, which is the worst of
   both.

## What is reused, unchanged

The process boundary keeps a new surface independent of the engine/store implementation. A GUI client is **another client of the same daemon**:

- Reuse the domain, IPC, daemon, sidecar and encrypted-store boundaries. GUI-specific requirements may still require reviewed protocol/domain additions rather than assuming these remain unchanged. The daemon listens on a Unix socket and speaks versioned IPC, so the UI is a
  peer, not a component.
- Only the render layer and the input layer are replaced: `apps/bloom-tui/src/{ui.rs,main.rs}`
  and their tests. Reuse concepts from `app.rs`/`daemon.rs`, but extract shared state/client logic deliberately; ratatui-specific focus, geometry and command modes are not a drop-in GUI architecture.

## Recommendations for that work

- **Keep the TUI as a supported client** rather than replacing it. The two share the
  daemon; a terminal client is genuinely useful for remote and low-resource contexts, and
  deleting it would throw away a working, tested surface.
- **Start from the previews, not from the terminal UI.** The mockups are the spec; porting
  terminal layout decisions into a GUI is how you end up with a terminal that wastes a
  window.
- **Check toolkit reality before committing.** GPUI is pre-1.0 with early Linux support;
  Flutter would split the stack in two and pull in a large runtime. Prototype the
  transcript (text layout, wrapping, selection) first, because that is where a windowed
  surface earns its keep and where the migration risk sits.
- **Move the platform-sensitive parts first**: notification integration, attachment
  previews, and anything the terminal client currently cannot express.
- **Keep the accessibility work platform-neutral.** The TUI's `NO_COLOR`, terminal
  light/dark detection and never-colour-alone rules have GUI analogues (system theme,
  high-contrast modes, screen readers); do not let the GUI path bypass them.

## What would be measured before the port is called done

Frame budget under a long transcript, text layout fidelity against the previews, inline
attachment rendering, and the same headless screenshot pipeline the TUI already has, so
the design loop keeps working after the port.

## Related

- `docs/system-design.md` describes the current domain, engine process boundary, IPC and encrypted store a GUI client would reuse.
- `docs/implementation-plan.md` holds current priorities and universal release gates; obsolete wave ownership tables are no longer active.
