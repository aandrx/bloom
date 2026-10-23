# Deep message-search navigation

Status: accepted under shipp with synthetic evidence. Exact context lookup and the separate TUI view are implemented; this is not live-account verification.

Parent checks pass on the final source: workspace all-target tests, Tokyo-timezone TUI tests, strict workspace Clippy, formatting, source guard, and rebuilt runtime suites. Runtime counts are 18 fixture, 12 input, 4 messaging, 12 reading/navigation, and 8 launcher tests. Parent inspected the final returned-Latest capture: the newest arrival is visible, the unread badge is cleared, and the literal draft remains on its original recipient.

The stale badge was reproduced failing in the actual PTY before correction. Marker advancement now requests one coalesced follow-up frame; a unit test proves unchanged markers do not create a redraw loop. Scoped independent reviews checked framing, context isolation, cancellation, and redraw follow-ups. The last source-only rereview reported no findings in its narrow scope; runtime evidence belongs to the parent runs.

## Reproduced limit

Before this change, the actual synthetic PTY found a unique hit 1,420 older messages back, but opening it stopped with "That message is outside the history Bloom keeps loaded." Normal paging retains the newest 200 messages plus at most 1,000 older messages. Search used to accumulate intermediate pages and could not reach every result.

Keep that normal paging budget. Search needs an exact lookup, not a larger cache.

## Exact context API

IPC version 4 adds `MessageContext { conversation_id, message_id, limit }` and a distinct `MessageContext { messages }` response. Valid limits are 1 through 200. Invalid limits return `InvalidRequest`; a missing target or a target in another logical conversation returns `NotFound`.

The store resolves aliases and obtains the current target timestamp by exact message ID. It returns the target plus up to `(limit - 1) / 2` older rows and the remaining quota of newer rows, oldest first. Rows use history's `(created_at, stable JSON id)` order. A short side need not be backfilled. Returned identities are canonical.

Queries are bounded and use the existing history index. No timestamp arithmetic, full-conversation materialization, engine access, or new dependencies are needed. A fabricated timestamp-plus-one cursor is rejected because tied timestamps can crowd out the target and maximum timestamps cannot be incremented.

## TUI ownership

Normal loaded history remains in `App.messages`. A separate session-only search context owns at most 200 messages and a request generation, conversation ID, target ID, and loading state. It is a fixed context preview, not an unbounded second history reader.

- A hit already in normal loaded history can use its existing reading anchor without a context request.
- An unloaded hit starts one context request. This includes a cold thread whose normal snapshot has not arrived yet; the exemption applies only to rows already loaded when the hit is opened. The client does not wait for repeated snapshots or page through intermediate history.
- The renderer and its layout-cache key read the same active-message accessor. Loading context does not paint unrelated latest messages as the target.
- Normal snapshots, older-page replies, and live arrivals update normal history only. They cannot replace or extend the active context.
- A context reply applies only to its current generation and selected conversation. It must include the requested target and stay within the size and identity contract.
- A context frame never advances the local unread marker, even if its short content clamps to scroll zero.
- End and the measured Latest control leave context, invalidate pending context replies, clear context viewport geometry, and show normal latest history. The control remains available even at the last row of a short context.
- Conversation navigation, `/unread`, and a committed replacement hit invalidate the old context request. Opening and cancelling an overlay preserves a ready context and recipient-owned composition.
- Canonical alias changes restart the same target lookup under a new generation and canonical ID. Replies for the retired request are ignored.
- New-message composition must not expose or retarget the previous chat's context. Same-recipient draft text, cursor, and literal-paste provenance stay intact when opening a hit or returning to latest.

An oversized response returns a same-request Internal error without writing a partial frame. The original size check and bounded fallback share one write deadline; partial I/O errors remain fatal. Neither daemon nor fixture truncates message bodies to fit.

The view must say that it is search context and explain how to return to Latest. Missing targets and failed requests produce a bounded notice and leave a recoverable normal transcript. They do not silently select another message or conversation.

## Acceptance

Backend tests cover exact target inclusion in a 2,000-row history, more than 200 equal-timestamp neighbors, escaped IDs, maximum timestamps, limit 1, missing targets, wrong conversations, canonical aliases, bounded ordering, invalid limits, and strict protocol version rejection. Synthetic daemons implement the same context contract.

TUI state and render tests cover late success and error replies after Latest, selection, unread, new-chat, or replacement navigation; reconciliation and arrivals during a context view; alias restart; missing targets; read-marker suppression; short contexts; disabled context paging; both Latest input paths; and draft ownership. A cold-thread regression covers the one-request context path before normal history arrives.

The actual PTY regression opens a hit 1,420 rows back, keeps it visible through reconciliation and a new arrival, then returns to newest via End without changing the literal draft. It verifies prompt badge clearing without another key and sends the preserved draft to its original synthetic recipient. Separate PTY cases cover an already-loaded hit and an unloaded hit. The final checks and review boundaries are recorded above.

No live account or engine access is authorized. The launcher stays on debug. Remote archives, participant and attachment search, durable drafts, and engine recovery remain outside this unit.
