# Public-release gate

Status: preparation continues in the existing `aandrx/bloom` repository. Visibility
remains private. The owner chose to keep this repository rather than recreate it.
No further history rewrite, force push, or visibility change is part of this work.

## Publication scope

The candidate is an experimental source repository, not a binary release or a
standalone installation package. It includes Bloom source, synthetic tests,
generated bindings with their notices, and illustrative design references.
The external engine, registration data, credentials, local databases, and live
captures must stay outside Git.

Source publication still requires review of the material actually included.
A process boundary or an experimental label does not establish redistribution
rights. The [B-4c review](packaging-4c-release-paperwork.md) explicitly requires
review of both Bloom binaries and sidecar sources that interact with the external
engine. That source-publication question remains open. Binary packaging also
needs its own staging and runtime checks. Neither publication scope requires
implementing every unfinished product feature, but neither bypasses the applicable
license and external-engine review.

## Recorded audit evidence

The initial Gitleaks 8.30.1 scan covered 213 commits and 8.53 MB of Git data.
A scan after the first rewrite covered 215 commits and 8.55 MB. Both used default
rules and `--log-opts=--all`, with five findings on synthetic test tokens in:

- `crates/bloom-ipc/src/client.rs`
- `crates/bloom-domain/src/lib.rs`
- `ext/bloom-engine-sidecar/dart/test/payload_codec_test.dart`
- `ext/bloom-engine-sidecar/dart/test/protocol_test.dart`
- `scripts/test-tui-reading-e2e.py`

These were reviewed as false positives. The scans found no real credential. They
are dated evidence, not a scan of every later commit or proof of absence.
Binary image contents and external GitHub metadata were outside scanner coverage.

The current baseline is `2748896`, with 218 reachable commits and a successful
`Check Bloom` run, `40733496753`. A final history scan is still required after
publication preparation. Earlier short commit IDs in historical documents may
refer to the pre-rewrite history.

## Privacy and local development

Current documents and capture scripts use repository-relative commands or `$HOME`
defaults. The reachable history uses the GitHub noreply author and committer
identity. Earlier checks found no tracked engine binary or database. The
[design preview review](design-preview-review.md) inspected all 12 tracked PNGs.
No visible private identifiers or ancillary PNG metadata were found. The visible
text matches inline mockup fixtures. Asset authorship and permissions remain
unverified, especially for the custom background whose input is not tracked.
Labeling an image synthetic is not a substitute for establishing rights.

GitHub still served three old commits by exact SHA after the history rewrite.
Those commits can expose the earlier local path and personal commit email. Normal
branch history is clean, but a force push does not prove those old objects were
purged, and it does not erase prior CI logs, caches, or existing clones.

Keeping this repository does not require a purge or recreation as a technical
prerequisite. Retained old metadata is a disclosed privacy risk for the owner's
final visibility decision, not a credential finding. Do not claim it was erased.
No additional history changes are planned.

The existing checkout remains the development checkout. There is no separate
public build mode and no need to move local account state:

- `.env` remains ignored. Use it for trusted local path overrides, not secrets.
- `target/` and the sidecar's `.dart_tool/` remain ignored.
- `./scripts/bloom` keeps its release default and `--debug` option. Both use the
  real engine when run, so neither is a synthetic preview command.
- `TUI_SHOT_TOOLS` and `BLOOM_BROWSER` keep local capture tools configurable.
- Use the isolated synthetic tests in [CONTRIBUTING.md](../CONTRIBUTING.md) for
  development without account access.

## Publication checklist

| Check | Status |
| --- | --- |
| Existing repository retained | Owner decision recorded |
| Current local paths and reachable commit email cleanup | Previously verified; preserve it |
| Old GitHub objects and metadata | Residual privacy risk disclosed; not purged |
| Final full-history secret scan | Open after preparation changes |
| Design previews and source provenance | Pixels, metadata, source, and reachable history reviewed; owner confirmation and custom-artwork origin remain open |
| Included source licenses and notices | Files present; review still open |
| External-engine relationship | Source and binary obligations still under review; engine must remain external |
| Experimental engine support statement | Added to README; no broad compatibility promise |
| Security and support guidance | Added in SECURITY.md and CONTRIBUTING.md |
| Private vulnerability reporting | Unverified; API returned 404, not enabled by this task |
| Fresh-checkout synthetic verification | Open |
| Latest baseline CI | Passed at `2748896`; rerun after changes |
| Final owner approval to change visibility | Required; not granted by preparation request |

## Source publication versus product release

Unverified live delivery, renewal, and additional engine versions remain product
limitations. They must be stated accurately but do not by themselves prevent an
experimental source repository. The pinned bindings and initialization content
hash check do not establish a general engine compatibility matrix.

A packaged release remains separate. It needs the unresolved notice and external
engine review in B-4c, packaging verification, and accurate runtime prerequisites.
No engine library may be bundled under Bloom's MIT license.

## Next actions

1. Resolve the design review's provenance questions with owner confirmation of
   fixture content and artwork origins. Record the custom background's source
   and permission, or replace that preview with documented project-owned artwork.
2. Review licensing of included bindings, notices, and other copied material.
   Separate source-publication questions from binary-package obligations. Obtain
   qualified review where the rights or external-engine obligations are unclear.
3. Establish and verify a private vulnerability reporting route before visibility
   changes. Do not post a vulnerability or private evidence in a public issue.
4. Test a fresh checkout with synthetic fixtures and rerun the final history scan.
5. Present the remaining risks and scope for the owner's visibility approval.
