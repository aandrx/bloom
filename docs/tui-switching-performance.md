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

## Phase 1: first-visit cost and serialized snapshot reads

The probe now also records the first post-startup switch under `chat_first_visit`, because only that sample is a cold layout miss; the rest of `chat_next_body` is warm. Returning to ALICE is not a cold case and is deliberately not labelled as one: it was laid out at startup and stays inside the four-entry cache.

An isolated measurement separates layout cost from process startup and IPC noise: a local, uncommitted probe built a 200-message conversation and timed one full `ui::render` into a 100x30 test backend. The debug rows are four runs each under identical code; the release rows are a single run each, so they bound rather than characterise the profile gap:

| Profile | 200 medium bodies (~350 chars) | 200 long bodies (~3,500 chars) |
| --- | --- | --- |
| debug (launcher default) | 31.3-35.1 ms | 212.7-216.4 ms |
| release, single run | 4.2 ms | 21.6 ms |

The same renderer is therefore several times slower in the profile `scripts/bloom` builds: about eight times for medium bodies and about ten for long ones. The first-visit PTY sample is larger than this render alone (68.7 ms medium, 242.5 ms long), so the remainder also carries process startup, IPC and the probe's own polling. What the isolated number does establish is that the layout closure's work is intrinsic exact wrapping, not redundant formatting: two source-level candidates that removed provable work moved no measurement outside noise. Choosing a release launcher is a build-time and disk trade-off for the user, not a renderer bug, so the launcher still builds debug and no profile change was made here.

Two source-level cold-layout candidates were implemented, measured with that isolated probe, and reverted. Avoiding a cloned line when a line already fits and building bylines only for group starts moved no measurement outside noise, and independent review found the fit fast path could undercount whitespace-only lines, which `Paragraph` counts as two rows under `trim: false`. They are not carried, so the renderer's geometry stays exactly ratatui's.

Selection snapshots previously issued `list_conversations`, then `history` for the resolved conversation. `daemon::snapshot` now starts both reads for an explicit selection and keeps the list authoritative: a list error returns immediately, a completed history result is retained while the list finishes, history is used only when the requested conversation is still visible, and a fallback selection drops the speculative read before fetching its own. `requested=None` is unchanged, one revision is still assigned before any IPC, and the trade-off is documented in the function: history can now predate the list by one list round trip, so a message stored in that window arrives on the next reconciliation instead, at worst one further poll interval. On the local synthetic daemon this is not visible, because IPC costs about a millisecond; it removes an additive wait that a busy store would otherwise impose.

## Verification and acceptance

Five deterministic Rust cache tests pass: local-state/unchanged-snapshot reuse, render-dependency invalidation, bounded LRU/returning to a chat, obsolete-history removal, and exact visible-window-vs-full-Paragraph output. The last covers widths, heights, scrolling, Unicode, over-height bodies and command-menu muting, and asserts painting's logical-line count stays viewport-bounded. The initial performance acceptance had 85 render regressions. Phase 0 review added stale-click, trimmed participant/local-title search, and pending-leader regressions, bringing the current render suite to 88.

Independent focused source review found no actionable defects in cache keys, LRU, windowing, main integration, tests or probe. A later Phase 0 review found three interaction defects and three verification weaknesses: stale drawer clicks could fall back to another row, pasted search missed trimming/display names, leader state could leak across Chats, reconciliation proof did not force a missed event, one mutation guard could survive, and two PTY assertions were too weak. Parent reproduced and corrected each item. The final scoped reviewer found no remaining actionable findings but did not run builds/tests or inspect screenshots; parent owns runtime evidence.

Parent captured and directly inspected six states at `/tmp/bloom-switch-visual`: default 100x30, Settings 66x18, reading back 100x12, Chats 44x12, monochrome picker and light Settings. ImageMagick pixel comparison against the corresponding pre-performance screenshots found zero different pixels in all six. Phase 0 then reran the complete 37-state matrix at `/tmp/bloom-r0-final-visual`; parent inspected its contact sheet plus the affected narrow Chats, search, reading-back, failed-session, light-theme, and monochrome states. These remain terminal-emulator reconstructions, not universal font/terminal evidence.

The final Phase 0 source passed workspace all-target tests, Tokyo-timezone TUI tests, strict workspace Clippy, formatting, source guard, normal builds, 8 fixture tests, 11 input PTY tests, 4 messaging PTY tests, 7 reading/navigation PTY tests and 8 launcher tests. The reading suite now proves that a stored message omitted from live fanout is initially absent, arrives through normal five-second reconciliation, preserves the reading anchor, and appears in the transcript through End and mouse Latest. Final combined log: `/tmp/bloom-r0-accepted-gates.log`; PTY artifacts: `/tmp/bloom-r0-accepted-e2e`. Dart 3.13.4 formatting, analysis, and all 56 owned tests passed in the same gate after restoring the pinned user-local SDK.

Three performance mutation guards proved named failures for cache bypass, missing width invalidation and full-history painting, with copy-source restoration verified. They ran only library tests against a disposable Git snapshot, sharing the existing target directory rather than building another full dependency tree. The Phase 0 review added and separately proved three more guards for stale drawer clicks, untrimmed local-title search, and narrow-resize overlay ownership. The keyset-paging work added two more, proved the same way: reversing the SQL tie-break comparison and reversing the merged-page id comparison each fail their named test. The manifest now has 21 entries; the other 13 retain the preceding UI pass's proof boundary, not a newly claimed full-manifest rerun.

The shared target initially reused the final mutated library-test artifact when returning to the real checkout, despite correct unchanged source. Regenerated the affected crate's build artifacts, then reran the complete TUI suite, strict scoped Clippy and normal build successfully. After the Phase 0 guards, parent again cleaned only `bloom-tui` build artifacts and reran 67 library tests, 88 render tests and every other TUI target, Tokyo-timezone TUI tests, strict scoped Clippy, normal build, all seven reading PTY tests, and the final probes. No account/database/config reset occurred.

Phase 1 adds one deterministic session regression: explicit history must reach the daemon before a delayed list response completes. Two further tests are forward-looking guards for the speculative design rather than defects in `HEAD`. A vanished selection must still fall back to the first conversation's history without publishing the missing thread's rows, which the previous list-first order also achieved by construction. A list error must not be held up by a concurrent history read, which cannot regress on `HEAD` because `HEAD` never issued history first. Independent review found that the first `tokio::join!` version could delay a fast list error behind a 30-second request timeout, that two assertions encoded scheduling rather than contract, and that the guard was originally mislabelled as a regression; parent corrected all three before acceptance.

No real engine/account or personal diagnostics were accessed. The correction removes repeated whole-history work from warm local switching; cold/invalidated layout and asynchronous store/IPC boundaries remain explicit, and the isolated probe now separates first-visit render cost from startup and IPC while pointing at the debug build profile rather than redundant renderer work.
