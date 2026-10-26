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

## 2026-10-26 bounded spike result

A disposable standalone spike lives in `spike/gpui/`; it is outside the root workspace and uses no daemon, IPC, account or engine. GPUI 0.2.2 builds under Bloom's pinned Rust 1.97.1 after pinning `libc = 0.2.186`: GPUI's transitive `xattr 0.2.3` still references `libc::ENOATTR`, which newer libc releases removed.

The spike opens a 1200x780 synthetic three-pane messenger with a conversation list, a `uniform_list` transcript of 10,000 fixed-height 176 px rows, an optional 260 px details panel and a composer. The row height is sized for the worst synthetic combination of three body lines, metadata, attachment and reactions, so virtualization does not hide clipped content. Rows cover incoming/outgoing surfaces, multiline Unicode, edited/unsent labels, reactions and attachment placeholders. Pane rules are responsive: details hide below 1100 px, the sidebar narrows below 800 px, and the 480 px minimum width keeps the transcript usable.

A second bounded pass adds a real `EntityInputHandler` composer with Unicode/grapheme editing, selection, mouse placement and marked-text methods, plus a dark/light toggle. Editorial behaviour is compiled and unit-tested, but no manual IME or selection session has been run, so platform input integration is wired rather than validated.

Observed gates: `cargo check`, seventeen unit tests, strict Clippy, formatting and release build pass. Ordinary windows stayed alive with no stderr under 5-second X11/Xwayland and native Wayland (`DISPLAY=`) smokes. The release binary is 25,669,552 bytes and links XCB/xkbcommon libraries.

Headless `--measure --rows 10000` reported `build_ms=2.923`, `slices_ms=0.067`, checksum `481e356c24740c83`; this measures model construction and 100 six-row synthetic viewport slices, not GPUI frame rendering, and private timings varied by a few milliseconds between runs.

`--frame-measure --rows 10000` drives 120 `on_next_frame` callbacks while scrolling the list. It reports the position the list actually applied as `last_top`, derived from the playback handle's pixel offset. That is a movement check rather than an exact first-visible-row index, since list padding and clamping can shift it; `last_top=9986` shows the scrolls reached layout instead of only being requested. Total runtime still varied from 2.0 s to 28.1 s across focused and unfocused runs with p95 callback intervals from 17 ms to 1,000 ms, and the checksum stayed constant. Focus, visibility and compositor throttling dominate the result, so this mode proves callback and scroll plumbing, not a frame budget.

Decision: **GPUI remains viable but not selected for a production port.** The spike proves toolchain, virtualization, responsive layout and input-handler compatibility. It does not prove presented-frame budget, manual IME behaviour, accessibility, inline image rendering, deterministic screenshot fidelity or packaging. Before proceeding, the next bounded unit must capture and review the real window against the previews and use presentation-aware performance instrumentation rather than raw callback intervals. The TUI remains supported regardless.

## Related

- `docs/system-design.md` describes the current domain, engine process boundary, IPC and encrypted store a GUI client would reuse.
- `docs/implementation-plan.md` holds current priorities and universal release gates; obsolete wave ownership tables are no longer active.
