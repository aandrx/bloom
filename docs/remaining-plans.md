# Remaining plans: packaging tail, launcher profile, rich messaging

Status: planning only. Packaging phases 1 to 3 are accepted; everything below is unscoped work turned into bounded units.

## 4. Packaging leftovers

Phase 4 is parked with one open fault: the daemon intermittently rejected a sidecar frame (768 printable bytes, first byte `8`). Diagnostics now log only structure.

Entry criteria to resume: a reproduced fault with its redacted line, or a concrete new hypothesis that names what the next run would measure. "Run more attempts" alone is not a lead. Exit criteria for each unit:

- **4a, frame-fault root cause.** Keep the redacted summary. Reproduce with a bounded loop, then classify the line (digit, letter, space split) and map it to a writer by source inspection, not by printing payload. Done when the writer is named and a regression exists, or the fault is proven environmental and documented.
- **4b, gate rerun in two verifiable halves.** First, staged-prefix checks: doctor passes, the resolved sidecar lives under the prefix, and the dependency-break cases fail cleanly. Second, lifecycle checks in the same run: socket appears, TUI reaches readiness, exactly one daemon and one engine exist, SIGTERM leaves no child and no stale socket, and the temporary store is removed by the harness after the run. Each half reports separately because staging is non-atomic and a partial prefix cannot prove lifecycle behaviour. ABI compatibility stays unverified unless observed.
- **4c, release paperwork.** Licensing review for a sidecar payload, supported engine range, and a statement that Cargo alone is not enough. No distribution claim before 4b passes.

## 5. Launcher build profile

Current decision is debug; this spike tested whether that remains the right default.

Unit: one bounded spike, no product change. Build both profiles and run the existing switching probe against the same synthetic fixture. That probe measures input to readable reconstructed terminal state, not compositor first paint.

Status: measured. `pyte==0.8.2` is available in the E2E tools environment, and the probe now accepts `--binary`, so no patched copy is needed and no test depends on a fixed profile.

Fixture: 200 messages per chat, 100 columns, fake daemon, one run per profile. Warm values exclude the first cold visit; "first visit" is the single cold BOB layout. These numbers are not directly comparable to the earlier 31/213 ms figures, which came from a different probe fixture.

| Fixture | Profile | Warm chat next | First visit | Overlay median |
| --- | --- | --- | --- | --- |
| medium, body-repeat 10 | debug | 22.2 ms | 53.7 ms | 8-12 ms |
| medium, body-repeat 10 | release | 8.7 ms | 27.8 ms | 9-10 ms |
| long, body-repeat 100 | debug | 20.1 ms | 351.8 ms | 8-11 ms |
| long, body-repeat 100 | release | 9.5 ms | 61.0 ms | 9-10 ms |

Boundary: synthetic PTY reconstruction. It is not a live-account or compositor benchmark; there was one run per profile, and terminal scheduling contributes to the ~9-12 ms overlay floor.

Decision: keep debug as the launcher default for now, and propose an explicit opt-in release launcher as a separate unit. Release roughly halves warm chat switching (22.2 to 8.7 ms medium; 20.1 to 9.5 ms long) and cuts the long cold visit from 351.8 to 61.0 ms. No launcher or product change was made in this spike; the probe default stays debug.

Exit criteria met: measurements recorded, decision restated, no test depends on a fixed profile.

## 6. Rich messaging

Every phase needs an evidence gate first: the sidecar payload must actually carry the variant, and the domain must represent it honestly.

- **6a, inbound reactions: accepted.** Built-in reactions are decoded, mapped to typed events, applied transactionally without fabricating targets or rewriting message content, preserved by fixture history/context/save/restart, and rendered as aggregate counts in the real TUI PTY at 100/44/24 columns. The transport capability remains false because no outbound operation is supported.
- **6b, edits and unsent display: accepted inbound-only.** [The plan](edit-unsend-plan.md) records part-identity preservation, typed mapping, transactional application, display, fixture parity and PTY evidence. Edits replace only an evidenced part group with nonempty text; whole-body unsends retain content and show `Unsent · retained locally`. Partial multipart unsend, attachment/mention replacement, and edits after unsend are skipped explicitly. No outbound operation or capability flag was added.
- **6c, attachment opening: accepted.** [Evidence](attachment-opening-evidence.md) records the API gate, the sidecar-only token registry, the private bounded writer, typed IPC v8, the explicit `/attachment` chooser, and the external open action. One authorized live probe downloaded 325,019 bytes with mode `0600`; a later live TUI run saved 165,257 bytes through the chooser. Limits: tokens die with the sidecar generation, the chooser lists only loaded transcript parts, the live multi-candidate chooser and live `o` open are unexercised, and opening must never imply success. No automatic download or engine-database read is allowed.
- **6d, outbound attachment send: synthetic slice implemented.** [The evidence record](attachment-send-evidence.md) names the pinned upstream `sendAttachment` path and the matching local bindings. IPC v9 adds `send_attachment`, the daemon validates one local file and records one outbound part, the sidecar uploads and sends it, and the TUI exposes `/attach <absolute path>`. Rust, Dart and real-TUI PTY tests pass. Remote delivery, echo/dedupe, live limits and failure semantics remain unproven; no live send is authorized. Outbound reactions remain unsupported by decision. Any edits or unsend still need their own confirmation and uncertainty handling, and automatic retry stays forbidden.

Sequencing: 6a, 6b and 6c are accepted at their documented boundaries. 6d is implemented synthetically and its live boundary remains blocked.

## Review points

Each unit above ends with parent verification and an independent review. Plans for 6a and 6b must name the exact files before implementation starts, and neither may add IPC fields without a version decision.
