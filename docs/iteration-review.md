# Reliability and UI iteration review

## Goal and boundaries

Preserve Bloom's working text-messaging loop while making the terminal UI coherent and its operational claims trustworthy. The parent owns design decisions and final visual critique; independent agents audit bounded subsystems and implement explicit changes.

All runtime tests use synthetic messages and isolated sockets. No real registration, credentials, engine state, or personal message history is part of this review. Mockups and domain types are not counted as working product features.

## Baseline evidence

- Rust workspace all-target tests, strict Clippy and formatting passed.
- Stateful synthetic daemon: 8 tests passed.
- Real-process PTY input suite: 7 tests passed.
- Real-process PTY messaging suite: 4 tests passed.
- Dart: 56 tests and owned-source analysis passed. The formatting gate found a drift in owned `bin/bloom_engine.dart`; reformatted it.
- Parent inspected actual PTY reconstructions of default, settings, contacts, help and narrow filter states under `/tmp/bloom-full-review-before`.
- The original visual matrix produced 19 captures but failed before completion because the script was edited while running. It was replaced by the successful 31-state matrix and focused recaptures recorded below; the partial run was not accepted as verification.

## Parent-selected plan

1. **Review safety and truthfulness.** Independently audit interaction/draft ownership, installation/runtime lifecycle, and the advertised feature inventory. Reproduce confirmed defects before changing them.
2. **UI hierarchy.** Neutral, restrained dark surfaces with a consistent accent and subtle selected-row surfaces; preserve explicit non-color selection markers. Allocate header width to conversation identity and status before branding and optional controls. Apply ellipsis and aligned gutters. Preserve popup hitboxes, composition geometry and keyboard routes. Remove unreadable orphan transcript snippets behind tall popups on small terminals.
3. **Targeted safety fixes.** Choose bounded fixes from the audits instead of rewriting the application state machine or pretending to implement unknown engine semantics.
4. **Regression gates.** Put real-terminal synthetic messaging/input suites in CI. Include the owned Dart single-flight helper in analysis/format checks. Isolate visual review preferences and diagnostics. Extend visual coverage to light theme, monochrome, new-message identity, multiline composition, failed session and long-title states.
5. **Independent verification and parent critique.** Review actual diffs, rerun tests, recapture visual states, fix follow-up defects and explicitly record remaining product gaps.

## Capability review

| Area | Working boundary | Verification / missing gate |
| --- | --- | --- |
| Existing and new-chat text sends | Implemented with explicit uncertain-delivery recovery | Synthetic daemon and PTY coverage; current real-engine path not revalidated |
| Local stored history and live-event display | Persisted local messages; selected thread periodically reread | Latest 200 messages only; no local pagination or remote archive import |
| `/sync` | Conversation metadata refresh and local-store reread | Not recovery of messages absent from Bloom's store |
| Contacts and filtering | Known conversation participants, local nicknames, names/handles/category filtering | Not an address book or message-content search |
| Group text | Existing groups, conditional recipient-list capability | No group management; generic capability declarations remain conservative |
| Attachments and rich messaging | Unsupported-part placeholders | No attachment send/open/download, reactions, edits, or unsend workflow |
| Delivery indicators | Event-driven labels | UI labels alone do not prove remote read-receipt support |
| Account setup | Reuses existing OpenBubbles registration exclusively | No Bloom-owned sign-in, activation, or account recovery |
| Settings and local labels | Theme, wrapping, density, nickname/title/hide preferences | Session drafts are not durable encrypted drafts |
| Installed command | Cargo installs `bloom`, `bloom-tui`, and `bloomd` via two packages | Dart sidecar, packages and external engine are not bundled |
| Diagnostics | TUI has private, bounded, allowlisted logs | External engine diagnostics require review before sharing |
| Accessibility | Keyboard navigation, monochrome cues, grapheme editing and responsive layout | Screen-reader and broad terminal/font validation still absent |

Sources: `apps/bloom-tui/src/{app,daemon,config,ui}.rs`, `apps/bloomd/src/{engine,config,service}.rs`, `crates/bloom-domain/src/lib.rs`, `ext/bloom-engine-sidecar/dart/bin/bloom_engine.dart`, and their synthetic tests. Earlier authorized live runs recorded in `docs/engine-sidecar.md` are historical evidence, not validation of this checkout.

## Pre-fix audit defects (repaired below)

- Overlay entry/exit clears composition state rather than suspending it. Preserve draft text, cursor, paste provenance and recipient ownership across transient panels, with real-PTY regressions.
- Metadata refresh can leave a selection outside the active filter. Reconcile eligible selection without borrowing another recipient's draft.
- The `/sync` action consumes its pending flag before the asynchronous operation runs. Track explicit request lifetime separately from event-burst activity; failure states outrank activity.
- Launcher signal handling exits before reaping children, is installed after startup, and gives the daemon less shutdown time than its two drain stages require. Observe signals in normal control flow, clean up owned children only, harden private logs, and test the actual launcher executable.
- Installation documentation omits the non-Cargo runtime. Document sidecar dependency resolution and explicit absolute script path; do not advertise a standalone package.

## Explicit non-goals

This iteration does not claim remote historical import, automatic engine-generation recovery, production account activation, attachments, message search, or a complete standalone distribution. Each needs its own transport, persistence and end-to-end acceptance work. No destructive reset, engine-library vendoring, live account operation or commits are authorized by this review.

## Implemented results

- Parent rejected incomplete agent passes and finished the changes directly. Their initial summaries were not treated as verified completion. The model provider's usage limit prevented a fresh independent post-implementation review; parent source review, regression tests and visual inspection are the final verification here.
- Composition is suspended with exact owner, text, cursor and literal-paste provenance. Modal-to-modal transitions retain the original body. A selection changed during a modal gets its own draft; the prior draft returns only to its original chat. New-chat draft keys include the recipient. Settings no longer erase the new-chat address. Theme apply/Esc, contacts close and preference outside-click paths restore composition.
- Metadata refresh reconciles selection with active filter eligibility without falling back to an excluded row. Alias/hidden-selection behavior remains covered by the existing regressions.
- Metadata `/sync` has an explicit in-flight state set when dispatched and cleared on receipt; duplicate requests are suppressed. The spinner animates throughout the request, independently of the two-second event-activity debounce. Failure/blocked/disconnected session states outrank activity.
- Dark surfaces are neutral instead of purple-heavy; selection uses a subtle contrasting surface and a visible accent marker. Parent caught and fixed selected text that had retained the old inverse foreground, including theme/context menus. Monochrome markers and bold cues remain.
- Header allocation gives identity/status a measured budget before optional controls; short labels, ellipsis, aligned gutters and a second status row keep narrow identities readable. The compact rail retains at least 16 columns when the terminal has 45 or more columns. Theme footer variants retain a close action instead of silently dropping it. Short orphan transcript fragments above tall popups are suppressed.
- Launcher observes termination signals during startup and TUI wait rather than calling `_exit`. It reaps owned children, preserves reused daemons, allows 30 seconds for daemon draining and two seconds for TUI termination, probes sockets without blocking on a full accept queue, restores original terminal modes and disables terminal UI modes on exit. Logs require a private owned directory and regular owned single-link file, reject final-component symlinks and use mode 0600. Companion candidates must be executable.
- Setup documentation now says Cargo installs Rust binaries only. The sidecar checkout, `dart pub get`, Dart, external engine and existing registration remain required. CI now exercises the synthetic fixture, real-terminal input/messaging and actual launcher executable, and checks all owned Dart helper sources.

## Verification

- Rust workspace all-target tests passed, including daemon persistence-drain regressions. TUI tests passed under the default timezone and Asia/Tokyo. Strict workspace Clippy, Rust formatting and the source guard passed. Handoff preparation reran these gates and all four Python suites successfully.
- After committing clean source, all seven mutation guards proved their named regressions and restored source. The first run found a stale filter-test name and ambiguous mutation site; these were repaired, exact-once mutation matching was enforced, and the complete guard run passed. Normal render/interaction suites were rerun after restoration.
- TUI suite: 62 library tests, 2 binary unit tests, 4 daemon-session tests, 8 input-safety tests, 58 interaction tests, 3 launcher integration tests and 76 render tests.
- Synthetic fixture: 8 tests. Real-terminal input: 11 tests, including preserved settings/new-chat drafts, changed-modal recipient safety, local nickname command persistence, delayed `/sync` status and failure priority. Real-terminal messaging: 4 tests for existing/new-chat send, rejection/recovery and restart.
- Actual `bloom` executable: 8 subprocess tests, including SIGTERM during startup/active TUI, preservation of reused listener, an 11-second graceful daemon drain, TUI execution failure cleanup, private logs, symlink refusal, and restoration of original raw terminal modes through a PTY.
- Dart: 56 tests, owned-source analysis and formatting passed. No generated bindings were edited.
- Parent-owned 31-state PTY visual matrix, followed by focused recaptures after critique, covers dark/light, normal/narrow/tiny, all pickers, no-match/search, help, new-message recipient/body, multiline, failed session, long title and monochrome. Captures are terminal-emulator reconstructions, not proof of every terminal/font/assistive technology.

## Parent critique and next decisions

**Presentation is materially improved, but “UI completely fixed” is not an honest product guarantee.** Neutral surfaces, coherent selection and readable narrow identity are improvements, not substitutes for messenger capabilities.

- Composition/recipient safety: substantially stronger, with actual PTY proof for the repaired paths. Drafts remain session-only; no undo or reviewed durable encrypted draft retention.
- Reading: clear sender groups, restrained delivery indicators and stable composition, but the local 200-message ceiling and missing pagination/search/unread count limit daily use. No remote historical completion can be inferred from the spinner.
- Responsive UI: better identity/status priority and meaningful compact rail width. At 44 columns the rail still clips secondary chat names; a deliberate drawer remains future work. Small terminal help and pickers necessarily show fewer rows.
- Accessibility: text/symbol cues survive monochrome and light surfaces remain readable. Screen-reader operation and broad terminal/emoji-font compatibility are not verified.
- Operations: startup/termination safety is better tested and setup guidance is honest. The sidecar is still not bundled and automatic engine-generation recovery is absent. Reused clients depend on the owning launcher's daemon lifetime.

Subjective presentation score: approximately 8/10 for the reviewed states. Overall product remains below that level because historical import, engine recovery and current authorized live-engine validation are not complete.

Next iteration order: (1) generation-scoped engine restart with exclusive ownership, bounded backoff and no uncertain-send replay; (2) verify historical ingestion semantics against the pinned engine, then implement durable idempotent pages/checkpoints; (3) local pagination/search/unread navigation; (4) staged standalone runtime packaging and onboarding. Each requires a separately scoped design and real acceptance evidence. No live-account test should run without explicit authorization.
