# Public-release gate

Status: **started, not passed. Repository visibility remains private.**

This gate covers a source-only public repository. It does not authorize a binary
release, engine redistribution, a packaged install, or any live account operation.
Making the repository public is a distribution decision and must wait for the
blockers below.

## Scope and evidence

- Repository: `aandrx/bloom`, private, default branch `main`.
- History scanned: 213 commits, 8.53 MB of Git data.
- CI: `Check Bloom` green at `aac3b77` (source guard, engine sidecar, Rust tests,
  real-terminal synthetic suites, second timezone, Clippy).
- Engine binary: not tracked, not vendored, and absent from the staged prefix.
- Submodules: none. Only `main` and its remote-tracking ref exist.
- Largest tracked files are generated Dart bindings around 1.14 MB; design preview
  PNGs are also tracked. No database, key, private-key, log, or `.so` file is
  tracked.
- Current file contents use synthetic example addresses and `+1555...` numbers.
  Historical test fixtures use generic `/home/person`-style paths.

## Secret and credential audit

Tool: `gitleaks 8.30.1`, full Git history (`--log-opts=--all`), default rules.

Result: **5 findings, all false positives.** Every finding is a
`generic-api-key` match on the synthetic test token
`0123456789abcdef0123456789abcdef` (or the equivalent fixture token) in:

- `crates/bloom-ipc/src/client.rs`
- `crates/bloom-domain/src/lib.rs`
- `ext/bloom-engine-sidecar/dart/test/payload_codec_test.dart`
- `ext/bloom-engine-sidecar/dart/test/protocol_test.dart`
- `scripts/test-tui-reading-e2e.py`

No real credential, private key, API token, password, or environment secret was
found. The scanner does not inspect binary image contents or external GitHub
metadata, and this is not proof that no secret exists.

Deleted-file review found one removed source file, `apps/bloom-tui/src/auth.rs`;
it was application code, not a credential store. No tracked or deleted `.env`,
database, private-key, cookie, keychain, or log file was found.

## Privacy and history findings

- **Current machine-local path:** four current files contain the developer's
  absolute home path: `docs/attachment-opening-evidence.md`,
  `docs/next-iteration-plan.md`, `scripts/tui-review.sh`, and
  `scripts/tui-shot.sh`. The values are intentionally omitted here. Replace them
  with `$HOME` or repository-relative paths before publication.
- **History retains that path:** earlier commits also contain the same local path.
  Current-file cleanup does not remove it from already-published history.
- **Commit metadata:** all commits use one personal author/committer email. It is
  visible in public Git history and must either be accepted or removed with a
  history rewrite.
- **Design previews:** the tracked PNG previews were not content-reviewed for
  incidental personal data. Review them before publication or mark them as
  synthetic references.
- No real phone number, account handle, registration identifier, message content,
  or credential was found in current tracked text. Historical fixtures remain
  synthetic.

## Licensing and distribution

- Root `LICENSE`: MIT, copyright "Bloom contributors".
- Vendored generated Dart bindings: Apache-2.0 `LICENSE` and `NOTICE` present.
- Third-party notices: generated Rust/Dart bundle present, including 48 Rust
  fallback notices.
- The [B-4c release paperwork review](packaging-4c-release-paperwork.md) remains
  the authority: legal review of fallback/compound notices, the SSPL/closed-engine
  boundary, and the exact-tag-only engine range is still incomplete.
- The installed OpenBubbles engine is external and unchanged. No engine binary may
  be published, and ABI compatibility remains unverified.

## Gate checklist

| Check | Status |
| --- | --- |
| Full-history secret scan | Passed with five synthetic false positives |
| No engine binary or credential files tracked | Passed |
| CI green | Passed at `aac3b77` |
| Current text uses only synthetic account fixtures | Passed |
| Machine-local paths removed from current files | Open |
| History privacy decision (path and author email) | Open |
| Design previews reviewed | Open |
| 4c legal and SSPL/closed-engine review | Blocked externally |
| ABI and engine support statement | Open |
| Public support/security policy | Open |
| Source-only publication scope approved | Open |

## Decision

**Do not make the repository public yet.** The technical history is clean of real
credentials and vendored engine binaries, but privacy cleanup/history rewrite and
the 4c legal review are unresolved. The gate can be resumed without another live
engine run.

## Next actions

1. Replace current home paths with portable environment/repository-relative
   values; rerun the affected script checks.
2. Decide whether to rewrite history for the local path and personal author email.
   A rewrite changes every hash and requires an explicit force-push decision.
3. Review the design previews for incidental personal content.
4. Complete the 4c legal/SSPL/engine review and define the source-only publication
   scope.
5. Add a public security/support policy and a final public-readiness note.
6. Re-run this gate, then decide whether to change GitHub visibility.
