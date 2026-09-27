# TUI rendering internals

This describes the current render loop and its constraints for engineers changing screens or animation.

## Event loop and redraws

`apps/bloom-tui/src/main.rs` draws once at the top of every loop iteration, then waits in `tokio::select!` for input, daemon/channel messages, or a timer. Any channel or key branch returns to the top and is followed by a draw. This is not an explicit redraw request mechanism: there is no central redraw queue, no coalescing of requests into one frame, and no frame-rate limiter.

The timer is 33 ms while `App::is_animating()` is true. Its branch advances `app.frame` and draws immediately. Otherwise the timer sleeps for 2 seconds and does not draw; the next draw occurs when another select branch fires. Thus 33 ms is the animation tick interval, not a general cap on redraws. A burst of ready events can cause successive loop iterations and draws.

## Animation state and spinner

`App::frame` is a wrapping `u64` counter, incremented by `advance()`. `is_animating()` is true for connection states Connecting or Disconnected, any session string other than empty or `Active`, `thread_loading`, or any outgoing message whose delivery state is Queued. The Connecting/Disconnected conditions have an actual lifecycle: initial connection and daemon reconnect status updates set those states. The other predicates are state checks, not animation lifecycle machinery; in particular there is no per-item animation state.

`ui::spinner_frame(tick)` selects one of ten Braille glyphs by `tick % 10`. The header status chip prefixes it while the connection is Connecting or Disconnected. Queued outgoing messages currently render a fixed `⠋` marker, not a frame from `spinner_frame`; the queued-message animation predicate therefore redraws without visibly animating that marker. Thread loading and non-active session status also trigger frame advances but have no corresponding spinner rendering path.

## Themes and palette

`Theme` in `app.rs` is Dark by default or Light, and can be toggled. `ui.rs` defines `Palette` with background, surface, rule, text, muted, cyan, green, yellow, and red colours. The DARK and LIGHT RGB constants are the mockup-derived values; nearby source comments tie layout and styling choices to the design mockups. At each render, `app.theme` selects one constant palette, then monochrome mode may alter selected roles. This is a fixed two-palette choice, not terminal colour-depth or contrast adaptation.

## Terminal background detection

On startup `main.rs` calls `query_background_theme()` and falls back to `Theme::default()` if it cannot determine a theme. The query is skipped unless both stdin and stdout are terminals. It opens `/dev/tty` for the OSC exchange so the query and reply use the controlling terminal rather than consuming application input from stdin. It sends OSC 11 (`ESC ] 11;? BEL`), reads up to 128 bytes with a 200 ms total `OSC_TIMEOUT`, and parses either `#RRGGBB` or `rgb:R/G/B` hexadecimal channels (1-4 hex digits each, scaled to 8-bit). `theme_for_rgb` uses the arithmetic mean of the three channels: at least 128 means Light, otherwise Dark. The parser and luminance threshold have unit tests; the actual terminal query, timeout and terminal-attachment behavior are not integration-tested here.

## Monochrome and NO_COLOR

`monochrome_from` enables monochrome for any present, non-empty `NO_COLOR` value (including values such as `0`); the result is cached by `OnceLock`. It collapses cyan, green, yellow, red, muted and rule to the text colour. It does not erase text, glyphs, layout, or bold modifiers, so selection markers, labels, status wording and spinner glyph remain available through shape/text/weight rather than colour. When changing status or selection styling, preserve such a non-colour distinction so a state cannot become invisible under NO_COLOR.

## Tests and coverage

`apps/bloom-tui/tests/ui_render.rs` contains render-to-buffer tests, including `layout_matches_the_mockup_structure`, `empty_store_renders_a_usable_screen_rather_than_nothing`, and small-terminal coverage. `apps/bloom-tui/tests/interaction.rs` exercises key handling and state transitions, for example `ctrl_c_quits_from_every_mode` and `arrows_move_only_through_conversations_matching_the_filter`. The monochrome buffer test is `monochrome_preserves_rendered_text_but_changes_styles` in `ui.rs`, which asserts that monochrome preserves all rendered text while changing styles. `app.rs` also has `animation_triggers_and_idle_state`, and `main.rs` has `parses_osc11_rgb_and_hex_replies` and `initial_theme_uses_background_luminance`.

These tests cover layout/output, key semantics, animation predicate conditions, monochrome visibility, OSC parsing, and the RGB threshold. They do not exercise the live event loop's redraw cadence, event-burst behavior, spinner progression, actual `/dev/tty` query/timeout, or terminal attachment detection. The render tests use an in-memory Ratatui backend, not a real terminal.

## Comparison: OpenAI Codex TUI

Codex's `codex-rs/tui/src/` provides three mechanisms Bloom does not currently have:

- A separate `FrameRequester` (`tui/frame_requester.rs`) accepts immediate or delayed requests, coalesces notifications and caps them at 120 FPS; `FrameRateLimiter` (`tui/frame_rate_limiter.rs`) enforces an 8.33 ms minimum interval.
- An explicit reduced-motion activity indicator path can animate, show a static bullet, or hide (`motion.rs`). Bloom has no equivalent reduced-motion mode.
- Terminal palette handling detects colour depth and default foreground/background, quantises RGB for 256-colour terminals, falls back to terminal defaults when background is unknown, and checks contrast against the painted surface (`terminal_palette.rs`, `style.rs`). Bloom only detects a coarse light/dark background and uses fixed RGB palettes.

These are reference mechanisms in Codex, not statements of Bloom plans. Consult the cited Codex files for their implementation.