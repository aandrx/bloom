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

A fresh clone of `40db26a` contains 220 reachable commits. Gitleaks 8.30.1,
verified against the release checksum, scanned that complete reachable history
with default rules and full redaction. It returned the same five synthetic-token
findings at the previously reviewed commits, with no new findings. Reachable
author and committer identities are noreply-only. A final candidate-tree scan
still needs to include the subsequent preparation changes. Earlier short commit
IDs in historical documents may refer to the pre-rewrite history.

Fresh-checkout verification at `40db26a` passed workspace all-target tests, Tokyo
TUI tests, strict Clippy, formatting, source guard, notice validation, 15 staging
tests, and all five Python synthetic suites. CI run `41567860779` also passed all
three jobs at that commit, including the Dart checks. These checks used no real
engine or account state. Follow-up developer-tool changes need their own focused
verification before being accepted.

## Privacy and local development

Current documents and capture scripts use repository-relative commands or `$HOME`
defaults. The reachable history uses the GitHub noreply author and committer
identity. Earlier checks found no tracked engine binary or database. The
[design preview review](design-preview-review.md) inspected all 12 tracked PNGs.
No visible private identifiers or ancillary PNG metadata were found. The visible
text matches inline mockup fixtures. On 2026-10-28 the owner confirmed that the
sample content is fictional and that the preview artwork, including the input for
the custom background, is owner-held. This is owner confirmation, not an
independent legal opinion. Labeling an image synthetic is not a substitute for
establishing rights.

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
| Full reachable-history secret scan | Passed at `650752c`; five reviewed synthetic matches, no new findings |
| Design previews and source provenance | Pixels, metadata, source, and reachable history reviewed; owner confirmed fictional content and artwork ownership |
| Included source licenses and notices | Mechanical inventory and binding notices complete; fallback/MPL/Unicode attribution and external-engine legal review remain open |
| External-engine relationship | Source and binary obligations still under review; engine must remain external |
| Experimental engine support statement | Added to README; no broad compatibility promise |
| Security and support guidance | Added in SECURITY.md and CONTRIBUTING.md |
| Private vulnerability reporting | Policy and form prepared; enable and verify during separately authorized public launch |
| Fresh-checkout synthetic verification | Passed at `40db26a`; subsequent helper changes need focused tests |
| Latest baseline CI | Run `41681524228` passed at `650752c` |
| Final owner approval to change visibility | Required; not granted by preparation request |

## Reporting activation at publication

GitHub documents private vulnerability reporting for public repositories. It
cannot be verified as an outside reporting route while Bloom remains private.
The prepared `.github/VULNERABILITY_REPORT.yml` does not enable the feature.
After the owner separately authorizes public visibility, enable reporting in
Settings, Advanced Security, and verify the reporter entry point before
announcing the launch. An owner-approved private contact is an alternative, but
none has been supplied. No settings or visibility were changed during preparation.

Reference: [GitHub reporting configuration](https://docs.github.com/en/code-security/how-tos/report-and-fix-vulnerabilities/configure-vulnerability-reporting/configure-for-a-repository).

## Source publication versus product release

Unverified live delivery, renewal, and additional engine versions remain product
limitations. They must be stated accurately but do not by themselves prevent an
experimental source repository. The pinned bindings and initialization content
hash check do not establish a general engine compatibility matrix.

The [source license inventory](source-publication-licenses.md) records the checked
Apache attribution, root MIT coverage, spike metadata limits, and unresolved
external-engine interpretation. Missing license metadata in each subdirectory is
not by itself absence of root-license coverage. This inventory is not legal
clearance and does not invent copyright holders.

A packaged release remains separate. It needs the unresolved notice and external
engine review in B-4c, packaging verification, and accurate runtime prerequisites.
No engine library may be bundled under Bloom's MIT license.

## Next actions

1. Complete the licensing review in [source-publication licenses](source-publication-licenses.md).
   Resolve the fallback-attribution, MPL source-availability, Unicode/compound
   expression, and external-engine questions with qualified review. The mechanical
   inventory and binding modification notices are complete; they are not legal
   clearance.
2. Prepare the reporting launch step while private. After separately authorized
   public visibility, enable and verify reporting before announcement. Do not
   post a vulnerability or private evidence in a public issue.
3. Verify the remaining developer-tool changes and scan the final candidate tree.
   The fresh-checkout synthetic gate at `40db26a` is complete. The final scan at
   `650752c` is recorded in [public-release scan](public-release-scan.md); commit
   the report and rerun CI before publishing.
4. Present the remaining risks and scope for the owner's visibility approval.
