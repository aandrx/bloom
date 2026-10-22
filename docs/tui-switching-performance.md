# TUI switching latency investigation

**Status: implemented and verified for the measured local rendering bottleneck.** User reported lag switching between UI surfaces after the reading/navigation pass. This is a follow-up to [that accepted UI scope](ux-polish-review.md), not live-account validation or permission to resume transport work.

## Reproduction boundary

Parent drove the real debug `bloom-tui` binary through a PTY against the isolated stateful synthetic daemon, with private XDG/socket/store paths. Two synthetic chats each had 200 messages. Input-to-readable-pyte-state latency was measured using `perf_counter`; this is not compositor presentation timing, a statistical cross-machine benchmark or a real-engine measurement. The checkout launcher also builds the debug profile, so release-only tuning would not address that default path.

Baseline, eight samples per action at 100x30:

| Synthetic history | Settings/Help median | Chat-switch body median |
| --- | --- | --- |
| Short bodies, about 35 characters | 13-15 ms | 28-29 ms |
| Repeated bodies, about 350 characters | 76-77 ms | 200-205 ms |
| Repeated bodies, about 3,500 characters | 668-673 ms | Not used for comparison; old probe could observe partially painted chat transitions |

The long-body case also produced panel stalls over one second. A 44-column long-body case had similar panel latency; opening Chats itself was usually fast, but returning to the conversation cost about 682 ms. This establishes a local rendering problem without requiring a slow daemon.

Ephemeral raw baseline measurements: `/tmp/bloom-switch-latency-before.jsonl` and `/tmp/bloom-switch-latency-medium-before.jsonl`; original reproducer: `/tmp/bloom-switch-latency.py`. These are synthetic and disposable.

## Source findings and chosen scope

Independent read-only audit identified two distinct paths:

- Every redraw formats every loaded message, measures all styled lines with `Paragraph::line_count`, and then `Paragraph::scroll` wraps the entire preceding history again while painting. The panel measurements establish that this work is a primary local bottleneck.
- Chat selection also refreshes the conversation list/history through sequential asynchronous IPC. This is not a main-loop synchronous wait, but slow store/IPC responses can separately delay uncached content. Do not claim a rendering fix removes that boundary.

Chosen correction: renderer-owned, bounded transcript-layout caching plus painting only the logical lines intersecting the viewport. Keep ratatui as the authoritative wrapper, preserve frame-owned hitboxes and semantic reading geometry, and invalidate for content/metadata/theme/width changes rather than cursor, scroll, animation or local-panel changes. The renderer remains immutable with respect to `App`; its cache is owned by the terminal loop.

## Repeatable probe

Build the normal binary first, then use the existing private Python/pyte environment:

```sh
cargo build -p bloom-tui
~/.cache/bloom-tools/bin/python scripts/benchmark-tui-switching.py
~/.cache/bloom-tools/bin/python scripts/benchmark-tui-switching.py --body-repeat 100
~/.cache/bloom-tools/bin/python scripts/benchmark-tui-switching.py --body-repeat 100 --columns 44
```

The permanent probe checks both the selected header and the correct chat body, excluding opposite-chat body remnants. It reports individual samples, medians, p95 and maxima. Timing is opt-in, not a flaky CI pass/fail threshold. Deterministic cache reuse/invalidation and output-equivalence tests should guard the implementation instead.

## Implemented correction and measured results

Parent rejected an incomplete delegated change that only added an unused cache API, then implemented the actual cache/layout/windowing logic. The terminal loop now retains a four-entry LRU; keys compare render-relevant values without cloning them on hits. Exact ratatui-measured geometry is reused for local UI state and unchanged snapshots. Painting starts at the relevant logical line with a relative row offset. Command-menu muting only touches visible clones. Edited/removed history discards obsolete cached versions for that identity; no cache is persisted or logged.

With the corrected permanent probe (eight samples per action):

| Case | Settings/Help medians | Chat switching / narrow navigation |
| --- | --- | --- |
| 200 medium bodies, 100x30 | 8.8-10.4 ms | Repeated body switches 17.3-21.9 ms; first next-chat maximum 65.3 ms |
| 200 long bodies, 100x30 | 7.8-12.7 ms | Repeated body switches 19.8-21.5 ms; first uncached next-chat maximum 338.5 ms |
| 200 long bodies, 44x30 | 8.0-11.9 ms | Chats open 6.5 ms; return to conversation 13.3 ms |

The original probe was also rerun for direct panel comparison: long-body panel medians fell from about 670 ms to 9-13 ms. Do not report its partially painted chat timings. Final raw measurements: `/tmp/bloom-switch-final-medium.json`, `/tmp/bloom-switch-final-long.json`, `/tmp/bloom-switch-final-narrow.json`. Earlier after measurements and the direct-comparison rerun remain at `/tmp/bloom-switch-*-after.json` and `/tmp/bloom-switch-latency-after.jsonl`.

The Phase 0 stabilization rerun retained that result. At 100 columns, medium-history chat medians were 17.7-18.4 ms and long-history chat medians were 18.4-19.7 ms; the first uncached next-chat sample was 68.7 ms and 242.5 ms respectively. Long-history panel medians were 8.4-11.6 ms. At 44 columns, long-history Chats open/close medians were 5.9/13.3 ms. Raw results: `/tmp/bloom-r0-bench-medium.json`, `/tmp/bloom-r0-bench-stress.json`, and `/tmp/bloom-r0-bench-narrow-stress.json`.

Remaining boundary: first/cold or content-invalidated large layouts still format/measure the loaded history, individual over-height bodies still wrap, and asynchronous IPC/store refresh can separately delay newly loaded content. This is not a claim that all switches or first visits are below 20 ms, nor removal of the paragraph u16 scroll ceiling.

## Verification and acceptance

Five deterministic Rust cache tests pass: local-state/unchanged-snapshot reuse, render-dependency invalidation, bounded LRU/returning to a chat, obsolete-history removal, and exact visible-window-vs-full-Paragraph output. The last covers widths, heights, scrolling, Unicode, over-height bodies and command-menu muting, and asserts painting's logical-line count stays viewport-bounded. The initial performance acceptance had 85 render regressions. Phase 0 review added stale-click, trimmed participant/local-title search, and pending-leader regressions, bringing the current render suite to 88.

Independent focused source review found no actionable defects in cache keys, LRU, windowing, main integration, tests or probe. A later Phase 0 review found three interaction defects and three verification weaknesses: stale drawer clicks could fall back to another row, pasted search missed trimming/display names, leader state could leak across Chats, reconciliation proof did not force a missed event, one mutation guard could survive, and two PTY assertions were too weak. Parent reproduced and corrected each item. The final scoped reviewer found no remaining actionable findings but did not run builds/tests or inspect screenshots; parent owns runtime evidence.

Parent captured and directly inspected six states at `/tmp/bloom-switch-visual`: default 100x30, Settings 66x18, reading back 100x12, Chats 44x12, monochrome picker and light Settings. ImageMagick pixel comparison against the corresponding pre-performance screenshots found zero different pixels in all six. Phase 0 then reran the complete 37-state matrix at `/tmp/bloom-r0-final-visual`; parent inspected its contact sheet plus the affected narrow Chats, search, reading-back, failed-session, light-theme, and monochrome states. These remain terminal-emulator reconstructions, not universal font/terminal evidence.

The final Phase 0 source passed workspace all-target tests, Tokyo-timezone TUI tests, strict workspace Clippy, formatting, source guard, normal builds, 8 fixture tests, 11 input PTY tests, 4 messaging PTY tests, 7 reading/navigation PTY tests and 8 launcher tests. The reading suite now proves that a stored message omitted from live fanout is initially absent, arrives through normal five-second reconciliation, preserves the reading anchor, and appears in the transcript through End and mouse Latest. Final combined log: `/tmp/bloom-r0-accepted-gates.log`; PTY artifacts: `/tmp/bloom-r0-accepted-e2e`. Dart 3.13.4 formatting, analysis, and all 56 owned tests passed in the same gate after restoring the pinned user-local SDK.

Three performance mutation guards proved named failures for cache bypass, missing width invalidation and full-history painting, with copy-source restoration verified. They ran only library tests against a disposable Git snapshot, sharing the existing target directory rather than building another full dependency tree. The Phase 0 review added and separately proved three more guards for stale drawer clicks, untrimmed local-title search, and narrow-resize overlay ownership. The manifest now has 19 entries; the other 13 retain the preceding UI pass's proof boundary, not a newly claimed full-manifest rerun.

The shared target initially reused the final mutated library-test artifact when returning to the real checkout, despite correct unchanged source. Regenerated the affected crate's build artifacts, then reran the complete TUI suite, strict scoped Clippy and normal build successfully. After the Phase 0 guards, parent again cleaned only `bloom-tui` build artifacts and reran 67 library tests, 88 render tests and every other TUI target, Tokyo-timezone TUI tests, strict scoped Clippy, normal build, all seven reading PTY tests, and the final probes. No account/database/config reset occurred.

No real engine/account or personal diagnostics were accessed. The correction removes repeated whole-history work from warm local switching; cold/invalidated layout and asynchronous store/IPC boundaries remain explicit.
