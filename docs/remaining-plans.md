# Remaining plans: packaging tail, launcher profile, rich messaging

Status: planning only. Packaging phases 1 to 3 are accepted; everything below is unscoped work turned into bounded units.

## 4. Packaging leftovers

Phase 4 is parked with one open fault: the daemon intermittently rejected a sidecar frame (768 printable bytes, first byte `8`). Diagnostics now log only structure.

Entry criteria to resume: a reproduced fault with its redacted line, or a concrete new hypothesis that names what the next run would measure. "Run more attempts" alone is not a lead. Exit criteria for each unit:

- **4a, frame-fault root cause.** Keep the redacted summary. Reproduce with a bounded loop, then classify the line (digit, letter, space split) and map it to a writer by source inspection, not by printing payload. Done when the writer is named and a regression exists, or the fault is proven environmental and documented.
- **4b, gate rerun in two verifiable halves.** First, staged-prefix checks: doctor passes, the resolved sidecar lives under the prefix, and the dependency-break cases fail cleanly. Second, lifecycle checks in the same run: socket appears, TUI reaches readiness, exactly one daemon and one engine exist, SIGTERM leaves no child and no stale socket, and the temporary store is removed by the harness after the run. Each half reports separately because staging is non-atomic and a partial prefix cannot prove lifecycle behaviour. ABI compatibility stays unverified unless observed.
- **4c, release paperwork.** Licensing review for a sidecar payload, supported engine range, and a statement that Cargo alone is not enough. No distribution claim before 4b passes.

## 5. Launcher build profile

Current decision is debug. The isolated probe measured first render at about 31 ms (medium) and 213 ms (long) in debug against 4 ms and 22 ms in release.

Unit: one bounded spike, no product change. Build both profiles, run the existing switching probe in an isolated prefix, and record input-to-reconstructed-screen numbers with the same fixture. That probe does not measure compositor first paint, so neither the plan nor the report may call it first-render latency. Then either keep debug with the numbers restated, or propose a release launcher behind an explicit opt-in. Exit criteria: measurements recorded, decision restated, no test depends on a fixed profile.

## 6. Rich messaging

Every phase needs an evidence gate first: the sidecar payload must actually carry the variant, and the domain must represent it honestly.

- **6a, inbound reactions.** Gate: a captured reaction payload variant. Then map counts and actors into the domain, persist them, and display them on the message. Read-only; no sending. Tests: mapping, dedupe, removal.
- **6b, edits and unsent display.** Gate: edit and unsend variants. Display edited text with an edited marker and unsent rows as removed, without rewriting history silently. Read-only.
- **6c, attachment opening.** Gate: a verified local path and permission model. Opening must never imply download success. This depends on the same blocked source as historical ingestion; do not start it before that gate.
- **6d, outbound rich sends.** Reactions, edits, unsend, and attachments each need capability negotiation, per-type confirmation, and uncertainty handling. Automatic retry stays forbidden.

Parallelism: 6a and 6b can run together once their payload gates pass. 6c stays blocked. 6d starts only after 6a and 6b are accepted.

## Review points

Each unit above ends with parent verification and an independent review. Plans for 6a and 6b must name the exact files before implementation starts, and neither may add IPC fields without a version decision.
