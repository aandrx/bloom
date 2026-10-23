# Local filename search

Status: accepted under shipp at the synthetic local-store, IPC, and real-TUI boundary. IPC v6 implementation, rebuilt checks, completed runtime rerun, and scoped/final source reviews are complete. The legacy timeout and other limits below remain explicit. Changes are uncommitted; no deployment or live-account operation occurred.

## Search contract

Search reads Bloom's stored text and explicitly reported `AttachmentRef.file_name()` values. It does not read file contents or query the engine or phone.

- Queries retain the existing trimmed literal lowercase/span algorithm. There is no regex syntax, full case-folding, Unicode normalization, or cross-part concatenation. `from:` is literal text.
- Within each message, all text parts are checked first in stored order. A body match returns its unchanged snippet. Otherwise the first matching nonempty filename supplies the hit.
- Each message yields at most one hit. Body priority does not apply across messages. Newer filename hits can displace older body hits at the result limit.
- Missing or empty names and unsupported parts do not supply filenames. IDs, MIME types, byte lengths, URLs, and placeholders are never name sources.
- Search keeps the newest-20,000 global-row scan, descending timestamp/encoded-ID order, canonical conversation remapping, and exact current participant membership. Membership applies before counting hits, without moving the global scan boundary. The TUI requests at most 50 hits.

Old unsupported placeholders have no name to recover. No metadata backfill, remote import, new table/index, or plaintext metadata copy is part of this feature.

## Snippets and transcript

Body snippets retain their existing representation. Filename-only snippets contain `Filename: ` plus the existing contextual window escaped with Rust `str::escape_debug()`. Matching uses the raw name first. The window keeps 30 original characters on either side and flattens CR/LF as before. Literal truncation ellipses survive escaping.

The hit DTO remains conversation ID, message ID, timestamp, and snippet. Its prefix is presentation only, not a structured origin, offset map, identity, or action. Opening any hit uses the existing exact message-context lookup.

The transcript and loaded-message preview show a nonempty reported name as `[attachment: NAME]`, with the full name escaped through `str::escape_debug()`. Missing/empty names and unsupported parts retain `[attachment]`. Raw parts, surrounding text/order, and send data remain unchanged. Cached/unloaded conversation summaries are not backfilled.

Printable accents and wide characters remain visible. Controls, bidi marks, quotes, and zero-width joiners can appear as literal escapes. Apostrophes also escape on the pinned Rust toolchain. These are display strings, not filesystem paths. No file-opening or download action is added, and transport attachment capabilities remain false.

Filename escaping does not establish global body-text sanitization. Existing search body snippets use the UI sanitizer; full transcript bodies follow their existing path.

## IPC v6

Both binaries and synthetic daemons use version 6. Framing rejects unequal versions before header/payload decoding. Explicit tests reject a lone v5 byte on both request and response readers. The payload shape did not change.

This version change guarantees the expanded coverage to a new UI rather than silently accepting an older body-only daemon. No existing documented bump rule mandated a schema change. The Dart sidecar protocol is unchanged.

A reused older daemon produces `unsupported IPC protocol version`. Users must upgrade both peers and preserve session drafts before stopping an older owner through its launcher. This task does not authorize restarting an actual engine owner or accessing an account.

## Component evidence

All execution below is synthetic and was checked under shipp. Source reviewers did not run these checks themselves.

- The metadata mapper, borrowed getter, and fixture RPC/save/restart prerequisites are recorded in [attachment metadata](attachment-metadata.md). The [baseline record](attachment-search-gap.md) explains unrecoverable old placeholders.
- Parent ran 118 daemon unit tests, strict daemon Clippy, formatting/source/diff checks, and inspected the one-file matcher diff. Independent review found no confirmed correctness/security defect. Tests cover per-message body priority, literal queries, original Unicode spans, safe excerpts, canonical membership, more than 50 newer out-of-scope hits, and the unchanged global scan boundary.
- Parent ran 61 fixture tests. Both fake daemons embed 64 escaped-name cases. Actual pinned-Rust comparisons covered all 468 Python `Mc` scalars checked. Other Unicode classes or versions are not exhaustively equivalent. Test corrections restored legacy assertion placement, matched message/conversation transports, and covered filename-only boundary queries.
- Parent ran the TUI normally and in Tokyo time, strict TUI Clippy, formatting/source checks, and rebuilt the binary. Eight new render regressions pass within 119 render tests. A reused `RenderCache` repaints same-ID filename changes without a cache implementation change. Assertions exclude sidebar previews.
- Before the UI change, the rebuilt real TUI found a filename-only hit but opened generic `[attachment]` metadata. Afterward, parent inspected safe named-context captures at 100/44/24 columns. Wide Unicode and escaped RLO/ESC/C1 were visible. Captures: `/tmp/bloom-filename-ui-verified-9d4mlhk2`.
- Parent ran the full 15-case reading suite. The new case opens an old Bob filename hit while Alice owns a literal multiline draft, restores Alice's exact cursor, inserts at that position, and sends one exact text message only to Alice. A strengthened focused rerun also rejects any additional outbound row. Captures: `/tmp/bloom-filename-reading-gates.GL7CkK/artifacts` and `/tmp/bloom-filename-owner-final.4uvKfb`.
- Scoped UI and PTY reviews found no blockers. One PTY review ended with a provider error and supplied no verdict; the completed retry is the acceptance evidence. The narrow apostrophe follow-up also found no blockers.
- Parent's final v6 workspace run passes 22 IPC tests, 118 daemon unit tests, 13 fake-sidecar integration tests, the other workspace all-target tests, Tokyo TUI tests, strict workspace Clippy, formatting, and both binary builds. Ignored tests were not run.
- The completed v6 runtime rerun passes 61 fixture, 15 reading, 12 input, 4 messaging, and 8 launcher tests. Parent inspected the final named-context and draft/send captures at `/tmp/bloom-filename-v6-runtime.HvkPUh/reading`.
- Final cross-component source review found no confirmed new defect. Parent also checked the documentation diff, source guard, and working diff. Reviews were source-only; execution claims belong to the parent.

The initial 12-row PTY case could not show the long metadata and anchor together. The test now uses the established 30-row viewport. Earlier probe failures also exposed bad transcript extraction and an invalid narrow-header subtitle assumption. Those were test corrections, not renderer fixes.

## Final gate and limits

Final Rust/static/build log: `/tmp/bloom-filename-v6-final.yZXQ5I/gates.log`. That initial job stopped after an RPC timeout in the existing unread-navigation case. The filename case passed. The unchanged unread case then passed a focused run, and the complete runtime rerun passed all suites. Runtime log: `/tmp/bloom-filename-v6-runtime.HvkPUh/gates.log`. The timeout's cause remains unresolved; neither a fix nor a timeout increase is claimed.

No live or ignored engine test, actual account operation, file-content capability, oversized-metadata UI claim, performance claim, or universal terminal/font claim follows from this evidence. Filename and MIME scalars still lack a separate per-field size cap. A review noted a pre-existing unbounded `waitpid` in the shared PTY cleanup helper; no cleanup hang occurred in these runs. The static fake also does not validate incoming version bytes, a pre-existing test-fidelity limit. Production framing rejects mismatches in both directions. These limits were not expanded into unrelated repairs.
