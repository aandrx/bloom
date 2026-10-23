# Participant-context message search

Status: accepted under shipp with synthetic evidence. Participant scope is implemented end to end, not live-account verified.

Final parent workspace all-target tests, Tokyo-timezone TUI tests, strict workspace Clippy, formatting, source guard, and rebuilt runtime suites pass. Runtime counts are 28 fixture, 14 reading/navigation, 12 input, 4 messaging, and 8 launcher tests. The actual PTY covers keyboard and SGR mouse selection, a hit behind 61 newer out-of-scope matches, All chats, cancel, literal query preservation, and recipient-safe draft restoration. Parent inspected the final 24-column capture with both complete shared-prefix identities visible.

Scoped independent reviews covered the backend, state/request ownership, integrated picker, and presentation correction. The last source-only rereview found no confirmed remaining defects in the wrapping, selection window, geometry, and hitboxes it inspected. Parent owns runtime acceptance. Failing-then-passing regressions cover static-fixture alias identity, hidden-highlight commit, pointer ownership, and long-handle ambiguity. Identities taller than the available viewport remain subject to the explicit resize warning; no universal tiny-terminal guarantee is claimed.

## Outcome and baseline

Search stored message text in chats containing one explicitly selected known participant. This is conversation membership, not sender filtering and not proof of who belonged to a group when an old message was sent.

Before this unit, the actual synthetic PTY opened Search with Ctrl+F, but Ctrl+P had no participant picker and no participant filter was sent to the daemon. Client-side filtering of the newest 50 global hits would miss older relevant matches.

## Identity and query contract

`ParticipantFilter` contains `transport: TransportId` and `handle: Handle`. `Handle` alone has only kind and value; it is not transport-qualified. Match the exact stored handle within conversations of the selected transport. Names and local nicknames are labels and picker search terms, never identity keys. Do not merge phone formatting variants, handle kinds, or transports as part of this feature.

The filter means that the canonical conversation's current locally stored participants include the exact handle. Include group messages even when another person sent them. Resolve retired conversation aliases to the canonical conversation before testing membership. If metadata is unavailable or no conversation matches, a filtered search returns no hits; it must not fall back to an unfiltered search.

The text query remains literal and case-insensitive under the existing rules. Do not add `from:` or other query syntax. Require a nonempty text query; selecting a participant alone does not enumerate all of their messages.

Apply membership filtering before collecting hits and before the result limit. Preserve the global scan bound of the newest 20,000 stored rows; this is not exhaustive archive search. Unfiltered search preserves its existing ordering, snippets, aliases, and limit semantics.

## Store and IPC unit

IPC version 5 adds `participant: Option<ParticipantFilter>` to `SearchMessages`, defaulting to `None` when absent in v5 JSON. Strict version-byte rejection prevents an older daemon silently ignoring the filter.

Keep existing `Client::search_messages(query, limit)` and store `search_messages` as unfiltered wrappers. Add `search_messages_with_participant(query, limit, participant)` for the explicit scope. Dispatch forwards the typed filter; the real service invokes the filtered store query. A default synthetic handler may delegate an absent filter to its existing method, but a present unsupported filter returns `Unsupported`, never unfiltered results.

Compute canonical matching conversation IDs from current stored metadata, then filter scanned messages before collecting hits. Use full transport-scoped conversation IDs and exact handles. Reuse the existing alias mapping and avoid decoding conversation metadata per message. No schema, dependency, engine, or registration changes are needed. Update both synthetic daemons to v5 and the same filtering/scan-bound semantics.

## TUI unit

Inside Search, Ctrl+P opens a nested **Chats with** picker. Candidates come from known non-self conversation participants, deduplicated by transport and exact handle. Show the handle and transport as well as any label; exact identity takes precedence over optional nickname text when width is tight.

- The first candidate is **All chats**, which clears the scope.
- A separate picker query filters labels, raw handles, and transport names. Typing or pasting there does not edit the body query or recipient draft.
- Up/Down and PageUp/PageDown choose a candidate. Enter or a painted row click commits scope and closes the picker. Mouse hitboxes retain the painted candidate identity, not an index into a later changed list.
- Esc, Ctrl+P, or Ctrl+C closes the picker without changing scope or the body query. It does not close Search. Search retains its existing outer cancel and draft-restoration behavior.
- Committing a changed scope clears old hits and invalidates the old request, but preserves the body query. The next Enter runs that query with the selected filter.
- The search view displays its scope and the Ctrl+P control. Below 70 columns the picker owns the full frame. Complete identities wrap across measured rows, keeping shared-prefix handles distinguishable; optional labels yield before identity text. An identity taller than the available viewport shows a resize warning and has no partially painted mouse target. Clearing scope does not clear the body query. Closing Search discards its session-only scope.
- Capture the filter in `Action::Search` with the request ID and query before spawning work. Late replies from another query or scope must not appear or open a hit.

Selecting or highlighting a participant is not conversation navigation, cannot change a send recipient, and does not mark any chat read. Opening a result still uses the already accepted direct-anchor or bounded-context path.

## Acceptance

Store and wire tests cover duplicate labels, exact transport and handle kind/value separation, direct and group membership regardless of sender, canonical aliases, missing metadata, no body match, and a filtered hit hidden behind more than 50 newer global text hits. Unfiltered behavior and the global 20,000-row scan bound remain explicit. Test request forwarding, unsupported-handler behavior, missing optional field, and strict version rejection.

TUI tests cover literal body-query preservation, scope selection and clearing, picker cancel, paste and mouse ownership, narrow identity rendering, stale rows, stale replies after scope change, duplicate names, transport separation, and recipient-owned draft/cursor/paste provenance. The passing actual PTY flow selects a participant, runs a scoped query, opens its result, clears or cancels the picker, and restores the original recipient's literal draft before explicitly sending it to that recipient. A second PTY case inspects both wrapped shared-prefix handles at 24 columns and selects the exact identity by mouse.

Verify each unit with scoped tests first, inspect the actual diff, then run the workspace/timezone/lint/format/source gates and rebuilt synthetic runtime suites. Independently review implementation before acceptance. No live account or engine access is authorized. The launcher stays on debug; attachment search and backend recovery remain outside this unit.
