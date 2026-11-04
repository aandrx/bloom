# Final publication scan

Status: technical scan passed at `650752c` on 2026-10-28. The repository was
subsequently published source-only with the owner's approval. The scan does not
replace legal review, and the support-ticket retention risk remains.

## Scope

The scan ran against the clean current checkout and its full reachable Git
history. It did not scan ignored build directories, real account state, the
external engine library, GitHub caches, or pre-rewrite objects that only GitHub
retains.

## Secret scan

Tool: Gitleaks 8.30.1, downloaded from the official release and verified against
the release checksum. Command:

```sh
gitleaks git . --log-opts='--all' --no-banner --redact=100
```

The scan covered the full reachable history. It returned five
`generic-api-key` matches, all previously reviewed synthetic test tokens:

- `crates/bloom-ipc/src/client.rs`
- `crates/bloom-domain/src/lib.rs`
- `ext/bloom-engine-sidecar/dart/test/payload_codec_test.dart`
- `ext/bloom-engine-sidecar/dart/test/protocol_test.dart`
- `scripts/test-tui-reading-e2e.py`

No new finding was reported. Reachable author and committer identities use only
the GitHub noreply address. Scanner limitations remain: binary image contents
and external GitHub metadata were not inspected, and absence of findings is not
proof that no secret exists.

## Privacy and file hygiene

- `git ls-files` contained only `.env.example` among sensitive-looking names.
- No tracked `.so`, database, key, credential, or log file was found.
- Current absolute home paths were limited to generic synthetic fixtures under
  `/home/person` and `/home/someone`.
- Current email domains were synthetic example domains.
- The three largest tracked blobs are revisions of the generated
  `frb_generated.dart` file at about 1.14 MB. No engine binary is tracked.
- The root MIT license, upstream Apache-2.0 license, and NOTICE hashes were
  recorded. Documentation links resolve, the source guard passes, the third-party
  notice bundle validates, and the refresh-helper regressions pass.
- `git fsck --full` reported no corruption.

## CI

Run `41681524228` passed all three jobs at `650752c`.

## Residual risks

GitHub may still serve pre-rewrite commit SHAs containing the earlier local path
and personal email. That risk was disclosed in the release gate and accepted as
part of retaining this repository. It was not purged or verified absent.
No final claim is made about third-party redistribution rights, the external
SSPL/closed engine, or repository visibility.

The technical scan is complete. Publication still needs qualified licensing
review and explicit owner approval to change visibility.
