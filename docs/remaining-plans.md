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

Unit: one bounded spike, no product change. Build both profiles, run the existing switching probe in an isolated prefix, and record input-to-reconstructed-screen numbers with the same fixture. That probe does not measure compositor first paint, so neither the plan nor the report may call it first-render latency.

Status: blocked. The release build succeeds, but the probe needs `pyte==0.8.2`, which is not installed and cannot be fetched offline. The probe also hardcodes `target/debug/bloom-tui`, so a release run needs an isolated copy with that path patched. Recorded, not silently skipped. Then either keep debug with the numbers restated, or propose a release launcher behind an explicit opt-in. Exit criteria: measurements recorded, decision restated, no test depends on a fixed profile.

## 6. Rich messaging

Every phase needs an evidence gate first: the sidecar payload must actually carry the variant, and the domain must represent it honestly.

- **6a, inbound reactions: accepted.** Built-in reactions are decoded, mapped to typed events, applied transactionally without fabricating targets or rewriting message content, preserved by fixture history/context/save/restart, and rendered as aggregate counts in the real TUI PTY at 100/44/24 columns. The transport capability remains false because no outbound operation is supported.
- **6b, edits and unsent display: accepted inbound-only.** [The plan](edit-unsend-plan.md) records part-identity preservation, typed mapping, transactional application, display, fixture parity and PTY evidence. Edits replace only an evidenced part group with nonempty text; whole-body unsends retain content and show `Unsent · retained locally`. Partial multipart unsend, attachment/mention replacement, and edits after unsend are skipped explicitly. No outbound operation or capability flag was added.
- **6c, attachment opening: API gate passed, end-to-end gate open.** [Evidence](attachment-opening-evidence.md) shows `api.downloadMmcs`/`downloadAttachment` in the pinned bridge, an `ApsConnection` in the restored sidecar state, and upstream implementations that write a caller path and stream progress. Bloom discards the MMCS fields and has no token/path/opening contract. C1b added the sidecar-only token registry and private bounded writer. C1c adds token delivery, the sidecar `attachment_fetch` command, daemon waiting/path validation and typed IPC v8. C1d adds the explicit `/attachment` save action and PTY result, without external opening. A live inbound attachment download is still required; opening must never imply success, and no automatic download or database access is allowed.
- **6d, outbound rich sends.** Outbound reactions are explicitly unnecessary for this project and remain unsupported. Any future edits, unsend, or attachments would still need an independently proven engine send path, capability negotiation, per-type confirmation, and uncertainty handling. Automatic retry stays forbidden.

Sequencing: 6a is complete. If rich-message correctness resumes, 6b is the next bounded design unit. 6c stays blocked. 6d is not scheduled.

## Review points

Each unit above ends with parent verification and an independent review. Plans for 6a and 6b must name the exact files before implementation starts, and neither may add IPC fields without a version decision.
