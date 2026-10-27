# Remaining plans: packaging tail, launcher profile, rich messaging

Status: planning only. Packaging phases 1 to 3 are accepted; everything below is unscoped work turned into bounded units.

## 4. Packaging leftovers

Phase 4 is parked with one open fault: the daemon intermittently rejected a sidecar frame (768 printable bytes, first byte `8`). Diagnostics log only structure, and the writer is identified below as an upstream MMCS debug print.

Entry criteria to resume: a reproduced fault with its redacted line, or a concrete new hypothesis that names what the next run would measure. "Run more attempts" alone is not a lead. Exit criteria for each unit:

- **4a, frame-fault root cause: writer identified.** Reproduced live on 2026-10-27 during the 4b rerun: `len=768 first=0x38 printable=100% digits=474 letters=294 spaces=0`. The writer is upstream `rustpush`'s MMCS Ford branch at `src/icloud/mmcs.rs:1114`: `println!("{}", encode_hex(&data));`. `encode_hex` emits two lowercase characters per byte, so 768 characters are 384 bytes of decrypted Ford data with no spaces; the observed 474 digits and 294 letters match hex (about 62.5% digits expected, versus about 16% for base64). It runs when an MMCS `AuthorizeGetResponse` contains a `ford_reference`; `get_mmcs` is shared by attachment, shared-stream and CloudKit downloads. The line predates the installed app build in `5363f4f` (2025-12-25), and the installed library contains the `src/icloud/mmcs.rs` and `FordChunk` strings. Bloom already skips it and does not log the payload; the fake-sidecar regression now emits an exact 768-character hex line before valid frames. Upstream must remove the print. 4a is done as a root-cause/regression unit; the upstream data leak remains.
- **4b, gate rerun in two verifiable halves.** Live rerun on 2026-10-27 after the license, bundle, preflight and release-launcher changes. Staged-prefix half: the doctor reported nine checks with the sidecar resolved from the installed prefix; the hidden-checkout missing sidecar, Dart and library cases each exited 1. Lifecycle half: the first launch had one daemon, one engine and one TUI at readiness; a second launcher reused the daemon and added one TUI, though that second TUI still showed `Connecting` after 60 seconds; quitting the second left one daemon/engine/TUI, quitting the first left zero and removed the socket; the temporary root was deleted. The 768-byte unmarked line recurred and containment skipped it. Result: passed for the first client and the two lifecycle halves; the second TUI's `Connecting` state remains a second-client caveat. ABI compatibility remains unverified.
- **4c, release paperwork: review complete, distribution blocked.** [Record](packaging-4c-release-paperwork.md). The Bloom MIT `LICENSE` and the upstream Apache-2.0 `LICENSE`/`NOTICE` are added and staged. The Rust/Dart third-party notice bundle is generated and staged (485 Rust and 52 Dart packages, with 48 Rust fallbacks). Distribution is still blocked by legal review of the fallback/compound notices, the SSPL/closed-engine review, and an exact-tag-only engine range; the sidecar preflight now fails closed with bounded `engine_incompatible` diagnostics. Cargo alone does not install Dart, sidecar packages, the engine or a registration. No distribution claim.

## 5. Launcher build profile

The launcher now defaults to release; this spike supplied the profile measurements behind that decision.

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

Decision: release is now the launcher default. The earlier debug default accumulated about 31 GB of build artifacts; release is a few hundred MB and roughly halves warm chat switching (22.2 to 8.7 ms medium; 20.1 to 9.5 ms long), while cutting the long cold visit from 351.8 to 61.0 ms. `./scripts/bloom --debug` remains available for backtraces and fast iteration. The installed `bloom` command still uses the cargo profile it was built with; the flag applies to the repository launcher.

Exit criteria met: measurements recorded, decision restated, no test depends on a fixed profile.

Live release check (2026-10-27): a `./scripts/bloom --release` launch against the real registration produced a release daemon, one engine child, a release TUI and an active session, with no debug binaries. A second release TUI reused the running daemon, reached readiness, and exited 0; the owning daemon and TUI were left running. After the owning TUI was quit, the launcher, release daemon, engine and TUI exited, the socket was removed, and no debug or replacement process remained. No message was sent.

## 6. Rich messaging

Every phase needs an evidence gate first: the sidecar payload must actually carry the variant, and the domain must represent it honestly.

- **6a, inbound reactions: accepted.** Built-in reactions are decoded, mapped to typed events, applied transactionally without fabricating targets or rewriting message content, preserved by fixture history/context/save/restart, and rendered as aggregate counts in the real TUI PTY at 100/44/24 columns. The transport capability remains false because no outbound operation is supported.
- **6b, edits and unsent display: accepted inbound-only.** [The plan](edit-unsend-plan.md) records part-identity preservation, typed mapping, transactional application, display, fixture parity and PTY evidence. Edits replace only an evidenced part group with nonempty text; whole-body unsends retain content and show `Unsent · retained locally`. Partial multipart unsend, attachment/mention replacement, and edits after unsend are skipped explicitly. No outbound operation or capability flag was added.
- **6c, attachment opening: accepted.** [Evidence](attachment-opening-evidence.md) records the API gate, the sidecar-only token registry, the private bounded writer, typed IPC v8, the explicit `/attachment` chooser, and the external open action. One authorized live probe downloaded 325,019 bytes with mode `0600`; a later live TUI run saved 165,257 bytes through the chooser. Limits: tokens die with the sidecar generation, the chooser lists only loaded transcript parts, the live multi-candidate chooser and live `o` open are unexercised, and opening must never imply success. No automatic download or engine-database read is allowed.
- **6d, outbound attachment send: synthetic slice implemented.** [The evidence record](attachment-send-evidence.md) names the pinned upstream `sendAttachment` path and the matching local bindings. IPC v9 adds `send_attachment`, the daemon validates one local file and records one outbound part, the sidecar uploads and sends it, and the TUI exposes `/attach <absolute path>`. Rust, Dart and real-TUI PTY tests pass. A live engine upload of a 33-byte file succeeded on a self-pair; the engine rejected the self-target send with `no_valid_targets`, so live delivery still needs an explicitly named third-party chat and a fresh authorization. Echo/dedupe, live limits and failure semantics remain unproven. Outbound reactions remain unsupported by decision. Any edits or unsend still need their own confirmation and uncertainty handling, and automatic retry stays forbidden.

Sequencing: 6a, 6b and 6c are accepted at their documented boundaries. 6d is implemented synthetically and its live boundary remains blocked.

## Review points

Each unit above ends with parent verification and an independent review. Plans for 6a and 6b must name the exact files before implementation starts, and neither may add IPC fields without a version decision.
