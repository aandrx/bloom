# Public-release gate

Status: **started, not passed. Repository visibility remains private.**

This gate covers a source-only public repository. It does not authorize a binary
release, engine redistribution, a packaged install, or any live account operation.
Making the repository public is a distribution decision and must wait for the
blockers below.

## Scope and evidence

- Repository: `aandrx/bloom`, private, default branch `main`.
- History scanned: 215 commits, 8.55 MB of Git data, after the history rewrite.
- CI: `Check Bloom` green at `c6034a0` before the history rewrite (source guard,
  engine sidecar, Rust tests, real-terminal synthetic suites, second timezone,
  Clippy).
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

- **Current machine-local path: resolved.** The four affected files
  (`docs/attachment-opening-evidence.md`, `docs/next-iteration-plan.md`,
  `scripts/tui-review.sh`, and `scripts/tui-shot.sh`) now use repository-relative
  commands or `$HOME` defaults. No machine-local home path remains in current
  tracked text. Local development keeps the same defaults; `TUI_SHOT_TOOLS` and
  `BLOOM_BROWSER` provide explicit overrides.
- **History paths and commit identity: resolved.** The history was rewritten
  with `git-filter-repo` on 2026-10-28. All 215 commits now use the GitHub noreply
  identity `aandrx <91489736+aandrx@users.noreply.github.com>` for author and
  committer, and no machine-local home path remains in any commit. The current tree was
  unchanged by the rewrite. A pre-rewrite bundle was retained outside the
  repository until the force-pushed remote was verified, then deleted. The
  fetched remote `main` contains only the noreply identity and no machine-local
  home path.
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
| Machine-local paths removed from current files | Passed |
| History paths removed | Passed by rewrite |
| Personal commit email removed | Passed by rewrite |
| Design previews reviewed | Open |
| 4c legal and SSPL/closed-engine review | Blocked externally |
| ABI and engine support statement | Open |
| Public support/security policy | Open |
| Source-only publication scope approved | Open |

## Decision

**Do not make the repository public yet.** The technical history is clean of real
credentials, machine-local paths and personal commit email, and no engine binary
is vendored. Design-preview review, the 4c legal/SSPL review, ABI support and a
public security/support policy remain unresolved. The gate can be resumed without
another live engine run.

## Next actions

1. Verify the rewritten history on a fresh clone and confirm the remote has no
   old objects or refs.
2. Verify the cleaned capture scripts in a fresh checkout; `tui-shot.sh` already
   produced a synthetic PNG locally with its `$HOME` defaults.
3. Review the design previews for incidental personal content.
4. Complete the 4c legal/SSPL/engine review and define the source-only publication
   scope.
5. Add a public security/support policy and a final public-readiness note.
6. Re-run this gate, then decide whether to change GitHub visibility.
