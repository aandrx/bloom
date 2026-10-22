# Multi-service Bloom: feasibility research and proposed plan

**Status: proposal for review, not implemented or authorized live integration.** Researched 2026-10-16. The current product remains the iMessage-backed TUI described in [status](status.md). This proposal may change the work order in [implementation-plan](implementation-plan.md) if accepted. No application source was changed, and no account credentials, registration state or live messages were accessed for this research.

## 1. Decision summary

A shared local messaging hub is technically reasonable. A flat universal chat list is not the right primary navigation for every service. Recommended direction:

- One restrained Bloom visual language, transcript/composer toolkit and local daemon.
- Provider-aware navigation: iMessage conversations; Slack workspace sections/channels/DMs; Discord DMs/server/category/channel trees.
- Explicit account, resource and thread routing; independent sync and failure state for each connection.
- Optional background reception plus bounded encrypted local storage.
- No automatic cross-service forwarding, identity merging, account-wide scraping or claim of complete archives.

**Do not yet commit to a fully supported Slack/Discord personal-client replacement.** Existing clients establish that unofficial approaches work, not that equivalent supported access is generally available. Slack policy and onboarding, Discord access scope, and storage/retention obligations are product decisions as well as engineering questions.

The previous recommendation to simply start with supported Slack OAuth was incomplete. Slack's API Terms restrict replacement/competitive uses. The previous bot-versus-unofficial Discord framing was also incomplete: the current Social SDK documents user DMs/history, but has important scope, approval, platform and contractual constraints. Neither finding is a legal determination that Bloom is categorically prohibited. They are reasons to clarify the intended use before promising a supported connector.

## 2. Evidence boundaries

Evidence categories used here:

- **Source-proven:** inspected pinned public code or an explicit statement in its README.
- **Documented:** a current official API, support, SDK or terms page states the behavior. Not a live test or permission granted specifically to Bloom.
- **Proposed:** a parent-selected design or test, not existing functionality.
- **Unverified:** requires additional research, platform confirmation, an isolated prototype or separately authorized live validation.

Public snapshots:

| Reference | Revision / observation |
| --- | --- |
| slk upstream | `e59099c3426936d4754a5bce64ca4577c3b1e704` |
| Discordo upstream | `3b28429a3726d03c09dc6d7d16f32adffd65a1b4` |
| Installed slk | Build metadata reports `v0.20.0+dirty`, revision `356d29e4d1f3b0c68d39d08fcad3608418d524d0`, modified source. Not necessarily identical to the inspected upstream snapshot. |
| Local availability | `slk` and official Slack executables found. Discordo was not found on PATH or in the limited executable/project locations checked; this does not disprove the user's installed/working setup. |

The parent inspected the public slk screenshot and Discordo preview/picker images, plus selected source. No authenticated client was opened. Screenshots are upstream examples, not current-machine or small-terminal verification. No upstream code was copied into Bloom. Temporary public clones are disposable; pinned links below preserve the evidence.

Research roles covered Slack access/policy, upstream navigation, and local architecture. The initial deep Discord agent hit its turn limit without producing a report; the parent independently checked official OAuth/RPC/Social SDK/terms sources. A later focused rerun completed and independently validated the main distinctions, identifying concrete 200-message/72-hour and both-players-played limitations on SDK DM history; the parent verified these in the official guide. Some agent reports ended at turn limits. Their suggestions were not accepted as proof: in particular, a 24-column thread pane at 100 columns and an unconditional supported-Slack-first recommendation were rejected. An independent read-only critic subsequently flagged overly bundled migration gates and wording implying legacy persistent drafts; both were corrected. The critic checked local planning/source consistency, not the external policy pages. A targeted follow-up found no remaining issues in the revised identity/migration sections and confirmed the separate gates and in-memory draft distinction.

The parent also compiled and ran a small isolated Rust experiment against the actual `bloom-domain` crate, using synthetic IDs only. It verified that a legacy conversation ID round-trips unchanged, different opaque transport namespaces distinguish otherwise identical conversation IDs, and channel-qualified native keys distinguish equal Slack-style timestamps. It confirmed that an unscoped `Handle` still compares equal across account contexts. This proves a useful identity primitive, not working multi-account routing or a storage migration. The scratch crate lives outside the repo and is disposable.

## 3. Feasibility matrix

| Goal | Assessment | Remaining gate |
| --- | --- | --- |
| Shared Bloom renderer and provider-aware sidebar | High technical confidence from existing source/UI patterns | Synthetic UI prototype and interaction review |
| Independent optional background connections | Existing daemon is a foundation, not yet a multi-account manager | Lifecycle recovery, instance routing, fairness and fixture tests |
| iMessage current integration | Implemented; current live reliability still unverified | Preserve all current safety boundaries and regression suites |
| Supported Slack user-context integration | Documented primitives exist; intended replacement-client use is policy-sensitive | Slack use-case clarification, workspace authorization, exact scope/event/onboarding proof |
| slk-like broad Slack client using internal protocol | Source-proven approach, unofficial and unsanctioned | Explicit decision on policy, credential handling, maintenance and distribution risk; not the default supported route |
| Supported Discord bot connector | Feasible for the bot's permitted resources | Permissions/intents and scoped pilot; not the user's existing personal inbox |
| Discord Social SDK personal DMs | Official DM APIs exist, but history is bounded to 200 messages/72 hours and requires both players to have played the game | Not a general personal archive; non-game eligibility/terms, production access, Linux runtime and exact live coverage still require verification |
| Social SDK as full Discord server browser | Not established; linked-channel integration is not arbitrary server-message access | Do not equate guild/channel listing with permission to read all messages |
| RPC or OAuth scope as full personal Discord client | Approval-restricted/limited; ordinary user install is not proof | Explicit platform approval and operation-by-operation verification |
| Discordo-like user-account client | Source-proven technical route, with explicit self-bot policy warning | Informed unsupported-route decision; cannot be advertised as approved personal OAuth |
| Always-complete offline history | Not established for any proposed integration | Provider retention, replay gaps, rate limits, permissions and coverage reconciliation |
| Hard local storage budget | Feasible with deliberate retention design | WAL/temp space, replay/tombstones, fairness and data-loss disclosure |

## 4. Slack findings

### Actual slk approach

[slk's pinned README](https://github.com/gammons/slk/blob/e59099c3426936d4754a5bce64ca4577c3b1e704/README.md) describes workspace rail, live connections, native sidebar sections, configurable sections, thread panel and activity feed. It also explicitly calls its browser protocol unofficial and unsanctioned. It reports MIT licensing; check all reused dependencies separately.

[Auth storage](https://github.com/gammons/slk/blob/e59099c3426936d4754a5bce64ca4577c3b1e704/internal/slack/auth.go#L12-L31), [token minting](https://github.com/gammons/slk/blob/e59099c3426936d4754a5bce64ca4577c3b1e704/internal/slack/mint.go#L15-L25), [onboarding](https://github.com/gammons/slk/blob/e59099c3426936d4754a5bce64ca4577c3b1e704/cmd/slk/onboarding_browser.go#L22-L33), and [internal websocket setup](https://github.com/gammons/slk/blob/e59099c3426936d4754a5bce64ca4577c3b1e704/internal/slack/client.go#L401-L426) establish session/internal-protocol use, not ordinary supported OAuth. Bloom should not silently extract credentials from the installed Slack app to reproduce it.

### Supported mechanisms and unresolved parity

Slack documents [user and bot tokens](https://docs.slack.dev/authentication/tokens/), [conversation history](https://docs.slack.dev/reference/methods/conversations.history/), [thread replies](https://docs.slack.dev/reference/methods/conversations.replies/), and the [Events API](https://docs.slack.dev/apis/events-api/). User authorization and membership govern visibility; bot membership is not equivalent to a user's private channels/DMs. Read, write, thread, event and read-marker permissions must be verified individually.

[PKCE](https://docs.slack.dev/authentication/using-pkce/) explicitly supports native public clients without bundling a client secret. The documented desktop redirect path cannot request bot scopes; custom URI redirects require PKCE and rotating tokens. Token refresh/expiry, revoked consent and rejected callbacks belong in the prototype.

[Socket Mode](https://docs.slack.dev/apis/events-api/using-socket-mode/) avoids a public inbound HTTP endpoint but separately requires an app-level token and app configuration. A user's OAuth login does not automatically provision that token. Current docs say Socket Mode apps cannot be listed in the public Slack Marketplace. A personal/internal app can have different operational setup from a distributed product; this is not an exception to other terms.

**Unverified:** exact user-event coverage for personal DMs/MPIMs with the proposed app configuration; a safe, approved public local-only onboarding story for app-level credentials; supported access to native custom sidebar sections, exact unread/mention state and search parity. Default to local favorites/sections and explicitly local read tracking until remote equivalents are proven. Do not label local counters as Slack's authoritative unread state.

Events are not a lossless archive. Reconnect must reconcile with bounded history/replies requests. Current [rate-limit guidance](https://api.slack.com/changelog/2025-05-terms-rate-limit-update-and-faq) and method docs distinguish app/distribution categories; affected non-Marketplace commercial apps can face one request/minute and 15 results/page, while internal/Marketplace cases have different allowances. Check the actual installation category and current limits before choosing a backfill schedule. Workspace [retention](https://slack.com/help/articles/203457187-Customize-data-retention-in-Slack) bounds what can be retrieved.

### Policy changes the recommendation

Parent verified [Slack API Terms](https://slack.com/terms-of-service/api), effective October 10, 2025. API Access(C) restricts using the APIs to "replicate or compete with the Services." This creates a substantial intended-use question for a replacement-style unified client. Obtain clarification before treating public Slack integration as a supported commitment; workspace admin consent alone does not settle Slack's terms.

Keep the clauses distinct:

- Commercial distribution has additional agreement/Marketplace requirements and a stated single-third-party exception. Free open-source distribution is not automatically the same category as commercial distribution.
- Third-party organizational data has consent/minimization and bulk-export restrictions.
- The explicit ban on persistent archives/indexes in the Data Access/Real-Time Search section applies to those APIs and that context. It must not be misreported as a blanket prohibition on every Web API cache.
- OAuth capability, retention permission, workspace approval and product/distribution permission are different questions.

This is technical/policy risk analysis, not legal advice or a definitive ruling on Bloom. Synthetic development is unblocked. A narrowly authorized internal experiment and a supported distributable personal client are different milestones.

## 5. Discord findings

### Discordo: useful source, unofficial account route

[Discordo's pinned README](https://github.com/ayn2op/discordo/blob/3b28429a3726d03c09dc6d7d16f32adffd65a1b4/README.md) documents password/QR/token login and warns about normal-account automation. It is GPL-3.0, not MIT. [Discord's self-bot guidance](https://support.discord.com/hc/en-us/articles/115002192352-Automated-User-Accounts-Self-Bots) describes account termination risk. A working private client does not establish supported platform access or eliminate operational/account risk.

Use its hierarchy/interaction concepts as reference. Do not assume copying its code preserves Bloom's licensing model; an external process is not automatically a resolution of every licensing obligation. Its exact reconnect/history implementation has not been fully audited here.

### Official routes are more nuanced than bot versus nothing

[OAuth scope documentation](https://docs.discord.com/developers/topics/oauth2) distinguishes:

- `guilds`: basic guild information, not permission to read every guild's channels/messages.
- `dm_channels.read`: approved-partner scope, not generally available inbox access.
- `messages.read`: local RPC context, not a general REST/Gateway personal token grant.
- `rpc`: approved-partner access. [RPC docs](https://docs.discord.com/developers/topics/rpc) describe tester/approval restrictions; the special RPC-token bypass excludes `messages.read`. RPC does have channel-message methods/events; the constraint is approved access and scope, not absence of message capabilities.
- User installation of commands does not itself grant general personal message read/send access.

An official bot can operate in authorized guilds and conversations, subject to permissions/intents, but uses its own identity and cannot be presented as a mirror of the user's existing personal inbox. Verify guild content intent and channel/thread permissions in a pilot; do not derive access from visible tree nodes alone.

### Social SDK: a real avenue, not proven full-client support

The current [DM guide](https://docs.discord.com/developers/discord-social-sdk/development-guides/sending-direct-messages) documents user-action sends, create/update/delete callbacks, DM conversation summaries and recent DM history. It is therefore inaccurate to claim Discord offers no official user-context messaging capability. This is worth a targeted eligibility inquiry.

However:

- [Platform compatibility](https://docs.discord.com/developers/discord-social-sdk/core-concepts/platform-compatibility) lists Linux as **experimental**, with glibc 2.31+ guidance and standalone C++ support. Do not rely on older landing-page wording or assume Rust/TUI/headless support was tested.
- [Communication feature access](https://docs.discord.com/developers/discord-social-sdk/core-concepts/communication-features) is game-oriented, with development limits and production requirements including account linking, Rich Presence/game joins and a friends list. No confirmation exists that Bloom, a non-game messaging hub, qualifies.
- The DM guide explicitly limits history retrieval to **200 messages and up to 72 hours**. **Both players must have played the game**; otherwise channel lookup can return a 404. This is a material constraint on ordinary personal-inbox/history use, not merely an unknown pagination detail. Do not generalize that history condition into a claim that every live send has the same prerequisite; exact live coverage remains unverified.
- The documented pre-approval DM-send limit is **100 per two hours per application**. Production-rate access has separate requirements; a working development prototype does not establish approval or daily-driver capacity.
- [Linked channels](https://docs.discord.com/developers/discord-social-sdk/development-guides/linked-channels) bridge a game lobby to a selected text channel. Linking requires Manage Channels, View Channel and Send Messages, and has channel/lobby restrictions. It is not a general read-access mechanism for every server the user belongs to.
- Linking a private channel can expose its messages to lobby members without the same Discord permissions. Do not use channel linking as an invisible workaround for Bloom's personal navigation.
- [Social SDK Terms](https://support-dev.discord.com/hc/en-us/articles/30225844245271-Discord-Social-SDK-Terms), section 2(b)(vii), restrict competitive uses; section 3 covers integration requirements and section 5 data handling. A replacement messaging client is an unresolved use-case/contract question, not an SDK entitlement.

**Recommendation:** ask Discord about a non-game personal messaging hub, eligible scopes, permitted local caching and Linux support before investing in SDK integration. Keep bot-scoped, SDK-limited and unofficial user-client options separate. No account route was selected or authorized during research.

## 6. Navigation and visual direction

### Observed references

Parent visually inspected [slk's public screenshot](https://github.com/gammons/slk/blob/e59099c3426936d4754a5bce64ca4577c3b1e704/docs/assets/screenshot.png). It shows a compact workspace rail, grouped channel/DM sidebar, central conversation and a separate thread view/composer. The useful lesson is hierarchy and parallel context, not copying its exact palette or modal editing rules. Its screenshot is wide; it does not prove a good 80-column layout.

Parent also inspected Discordo's [preview](https://github.com/ayn2op/discordo/blob/3b28429a3726d03c09dc6d7d16f32adffd65a1b4/.github/preview.png) and [picker](https://github.com/ayn2op/discordo/blob/3b28429a3726d03c09dc6d7d16f32adffd65a1b4/.github/picker.png). The expandable guild/category/channel tree and contextual quick picker preserve where a channel belongs. Its right member list and heavy framing are optional reference details, not requirements for Bloom.

Source confirms the [Discord tree](https://github.com/ayn2op/discordo/blob/3b28429a3726d03c09dc6d7d16f32adffd65a1b4/internal/ui/chat/guildstree/model.go#L75-L142), [chat layout](https://github.com/ayn2op/discordo/blob/3b28429a3726d03c09dc6d7d16f32adffd65a1b4/internal/ui/chat/model.go), [slk sidebar](https://github.com/gammons/slk/blob/e59099c3426936d4754a5bce64ca4577c3b1e704/internal/ui/sidebar/model.go) and [slk thread region](https://github.com/gammons/slk/blob/e59099c3426936d4754a5bce64ca4577c3b1e704/internal/ui/view_thread.go#L30-L57). Source-reviewed behavior is not the same as parent-tested interaction.

### Recommended: progressive native hierarchy

Use a small service/account switcher and one contextual navigation pane, not three unrelated applications or a single giant global tree.

```text
iMessage                  Slack / Workspace A       Discord / Account A
  Recent conversations      Favorites                 Direct Messages
  Direct chats              Channels                  Server One
  Group chats                 # general                 Category
                            Direct Messages               # general
                            Threads                     # help
                                                      Server Two
```

These labels are proposed navigation, not a promise that each provider exposes identical remote folders. Local custom sections can fill gaps, clearly distinguished from synced organization.

- iMessage keeps its lightweight list; no empty server/workspace chrome.
- Slack groups resources by the selected workspace, with optional local sections and contextual thread access.
- Discord retains server/category/channel hierarchy and a separate DM branch. Unsupported voice/forum operations are labeled or omitted; a displayed node must not imply a supported action.
- A unified Recent/Unread/Mentions view is secondary. Every result carries provider/account/container context and opens the native destination. Only show unread/mention semantics actually available from the source or explicitly labeled local tracking.
- Quick jump searches destinations across services with full breadcrumbs. Expansion/selection/focus/history are keyed by stable IDs, not row positions or names.
- Members/presence pane is optional and deferred; do not reserve it by default.

### Responsive proposal, to validate rather than freeze

- Around 80 columns: contextual navigation plus one transcript where both remain usable; otherwise a focused transcript with navigation overlay.
- Around 100 columns: retain one readable transcript. Do not force a second 24-column thread transcript merely because it technically fits.
- Wide terminals: allow side-by-side channel/thread only if measured minimum widths preserve both composers; initially explore a threshold around 140 columns, not an unconditional rule.
- Short terminals: collapse navigation chrome and preserve target/header, draft, cursor and a meaningful close/back action.
- Single-pane thread mode shows its parent breadcrumb and an explicit return action. Return restores parent scroll and draft.

Use one Bloom palette, spacing system, composer behavior, popup toolkit and predictable focus model. Do not silently switch to slk's Vim modes on Slack or Discordo's shortcuts on Discord. Preserve text-entry safety; account shortcuts must not consume normal composer characters. Selection and focus remain distinct, visible without color. Test long names, Unicode, collapsed unread parents, permissions changes and thousands of lazily loaded nodes.

## 7. Required architecture, with current source gaps

| Boundary | Current implementation | Proposed change |
| --- | --- | --- |
| Service routing | `apps/bloomd/src/service.rs` stores `Arc<Engine>` and sends via `Engine::send_text` | Registry keyed by stable connector instance; route by explicit target, never displayed title |
| Connector lifecycle | `main.rs` creates one engine; `supervisor.rs`/`session.rs` are single-session | Independent lifecycle, bounded queues, fair fan-in and generation-scoped recovery per instance |
| IDs | Domain conversation/message IDs are opaque transport plus value; current engine uses one hardcoded namespace; handles unscoped | Make each namespace a stable instance identity; keep provider family as separate metadata and preserve native message uniqueness |
| Conversation shape | `ConversationKind::Direct/Group` only | Containers, channels, DMs and explicit thread-root references; optional lazy member lists |
| IPC | Bare-address new chat, limit-only history, global session events | Explicit account/target, cursor history, scoped health/sync/events and resource permissions |
| Store | Encrypted schema v1, serialized IDs as keys | Add instance metadata/hierarchy/checkpoints/coverage transactionally; preserve existing serialized IDs wherever possible rather than automatically rekeying them |
| Local UI state | Draft owner is conversation or new-chat address; contact labels use unscoped handles | Include account and thread/compose target; exact legacy preference mapping |

The existing `Transport` trait is a useful seam but is not the service's current routing boundary. Update the canonical runtime path, not an unused abstraction alongside it.

### Parent-selected contracts

- **Connection identity:** a stable local instance ID distinct from provider name, credentials, remote login and workspace/guild. Prefer using the existing opaque `TransportId` as that namespace, with a separate provider/instance descriptor, rather than adding a redundant scope to every existing ID. The existing `openbubbles` namespace can describe the deterministic legacy instance unchanged. Audit and remove assumptions that the namespace always equals a provider family. Reauthorization preserves an instance only when remote identity is verified. One login's many containers do not automatically become independent accounts; user/bot sender identities remain explicit.
- **Resource identity:** typed, instance-scoped resources and messages preserving native uniqueness. Slack timestamps must remain exact strings and be scoped to their conversation/channel; never float-convert them or assume a timestamp alone is globally unique.
- **Compose target:** account + conversation + optional thread/reply context. New-DM selection includes account. Drafts, pending attempts, recovery and async navigation bind to that complete target.
- **Permissions:** operation support and current per-resource permission intersect. Loss of access freezes the active target with an explanation and preserves drafts; it never redirects to a similarly named destination.
- **Payloads:** normalize common semantics but preserve necessary typed provider fields. Do not dump arbitrary raw provider packets into storage/logs. Unsupported parts get explicit placeholders/permalinks, not silent stripping.
- **Sends:** durable local intent/request key before side effects, provider idempotency where actually supported, explicit accepted/failed/uncertain states. A repeated local request key does not justify remote resend after an unknown outcome. Do not claim exactly-once delivery.
- **Isolation:** prefer a shared Rust connector interface; retain bounded versioned process adapters where runtime/library/failure boundaries warrant them. Preserve the external iMessage engine boundary. Do not impose a process on every simple connector or claim a process automatically settles licensing.
- **IPC compatibility:** select an explicit coordinated protocol bump for a breaking migration and report client/daemon mismatch clearly. Do not silently reinterpret v1 fields; retaining an old-client compatibility layer is not a default requirement for this local product.

## 8. Sync, background operation and storage

Offer app-only and optional background modes. Background means reception continues without a TUI, not a promise of 24/7 operation or complete offline recovery. Login autostart is a separate opt-in. Keep iMessage registration exclusively owned; Slack/Discord connections must not weaken that rule.

Per connection, expose credential state, connection/reconnect state, last completed reconciliation, known gaps and cache coverage separately. A recent inbound message is not proof that every channel is synchronized. Use push events first, bounded history reconciliation where available, and selected/recent/unread resources before cold histories. Do not prefetch entire large servers or workspaces at startup. One rate-limited service must not freeze another.

Commit an opaque provider cursor/checkpoint only with corresponding durable event/page processing. Cursor expiry, lost events, deleted content and permission changes require explicit recovery/coverage rules. Disconnect or revocation may require removing locally retained data according to service/workspace/user policy; a disk quota is not authorization to retain data indefinitely.

A configurable local budget such as 1 GiB is a proposal, not a selected default. Account for DB, WAL, indexes and bounded checkpoint/compaction headroom; budget attachment cache and owned logs separately, and state that external engine state is outside Bloom's cache quota. A strict total-disk ceiling needs explicit reserved headroom and an exhaustion policy. SQLite `max_page_count` alone is not useful retention.

Prune with high/low watermarks and per-account fairness. Preserve active/recent context and needed uncertainty/dedup/delete metadata without unbounded tombstones. Record eviction coverage so replay does not casually resurrect pruned or deleted content. Pinning/protection cannot silently defeat a configured hard budget: warn and require an explicit policy when protected data itself exceeds it. Do not automatically delete the only known message copy while describing storage as a recoverable cache. Implement after provider-specific retention constraints are known.

## 9. Staged research and implementation plan

### Gate A: choose the actual access product

Before committing to connector implementation, make a decision record for each platform:

1. Personal/internal experiment or supported public distribution? Free/open-source and commercial distribution have different obligations.
2. Existing user's full inbox, explicitly bot-scoped integration, or a documented SDK subset?
3. Is the proposed use permitted/approved, which resources can it actually access, and can credentials be provisioned without extracting an official app session or shipping a secret?
4. Which data can be cached, for how long, and what must disappear on revocation/deletion?

Ask Slack about the replacement-client clause, local user PKCE + event setup, retention and intended distribution. Ask Discord about non-game Social SDK eligibility, permitted DM/server scope, approval/limits and Linux. Existing source licenses do not grant platform permission. If supported full-client access is unavailable, explicitly choose reduced functionality, defer the connector, or discuss an isolated unofficial personal experiment and its risks. Do not quietly substitute bots for the user's requested personal client.

**Deliverable:** per-platform capability/permission matrix and credential/onboarding diagram. **No-go for a supported public claim:** unanswered access/policy/onboarding requirements. This does not block synthetic core/UI research.

### Gate B: validate navigation without accounts

Build a disposable synthetic prototype, not production connector code. Populate fixtures with iMessage DMs/groups, two Slack workspaces with colliding names/IDs, and Discord DMs plus many servers/categories/channels. Include threads, local sections, inaccessible resources, long names, read-only channels and many hidden children.

Compare contextual sidebar versus always-visible rail/tree; parent review actual terminal captures at 60x18, 80x24, 100x30, 140x40 and 180x50, light/dark/monochrome. Check keyboard/mouse paths and thread navigation, not only screenshots. Stress large trees without materializing every message/member.

**Gate:** iMessage remains simple; no composer retargeting; clear account/channel/thread context; narrow mode usable; two transcript panes only when genuinely readable. Pick the layout based on this evidence, not the proposed breakpoint alone.

### Gate C: prove the connector spine with fake services

Define the instance/resource/target/event/permission/checkpoint contracts before assigning disjoint writers. Use a real daemon/IPC/encrypted-store fixture with multiple synthetic connector instances, not merely a fake daemon that bypasses the code being changed.

Acceptance includes identical remote message IDs across accounts/channels, noisy-source fairness, independent auth/crash/rate-limit failure, replay/duplicates/deletes, checkpoint interruption, permission loss, delayed stale-generation events and manual reconciliation. UI tests switch accounts/threads during composition and while a send result is pending.

**Gate:** no wrong-account sends, guessed merges, cross-posts, lost draft ownership, premature checkpoint or false healthy status. Retain current iMessage/launcher/PTY suites.

### Gates D1-D5: integrate in independently accepted steps

Do not bundle encrypted-store, routing, wire, UI and lifecycle migrations under one pass condition. Each step gets its own commit boundary, fixture verification and rollback/compatibility decision. Existing iMessage regressions must pass after every step.

**D1 - Identity/storage groundwork.** Prove instance namespacing, native composite keys and exact legacy mappings in fixtures. Prefer preserving existing serialized IDs: `TransportId` is already opaque and can distinguish instances. Add instance metadata and hierarchy/checkpoint schema as needed, transactionally, rather than requiring a full primary-key rewrite. Map existing unscoped contact preferences only to the legacy iMessage instance. Preserve the existing keychain identity, history IDs and aliases; missing keys still fail closed. Gate: synthetic v1 store/preferences survive upgrade; injected failure leaves the old schema readable. Specify a bounded encrypted pre-migration backup/recovery path before any real-store upgrade; never silently downgrade a new schema.

**D2 - Registry routing.** Route the current iMessage adapter through the connector registry, preserving its behavior, with no new live platform. Gate: existing engine-sidecar/store/sending suites plus fake multi-instance tests pass; foreign-instance events/targets are rejected and identities never guessed.

**D3 - Scoped IPC.** Add resource listing, target/permission/health events, cursors and request-status contracts. Use an explicit coordinated client/daemon protocol version transition and clear mismatch errors. Gate: actual synthetic Unix-socket tests prove version rejection, scoped reconnect snapshots and pending-send uncertainty; do not rely only on serialization tests.

**D4 - Hierarchy UI.** Add contextual navigation and thread targets behind fixture-backed tests. Gate: actual PTY tests prove draft/focus/scroll ownership through account/thread switches, modal panels, permission loss and late results. Current drafts are in-memory only: this tests runtime ownership, not migration of nonexistent stored drafts. Persistent preference migration belongs to D1 and is verified again through the UI.

**D5 - Independent lifecycle/background operation.** Add per-instance recovery, generation rejection, optional service lifetime, signal drain and credential revocation behavior. Gate: synthetic production-entry subprocess tests prove that a failed connector cannot stall another, reused services survive UI exit, owned processes are reaped correctly and no uncertain send is replayed.

No step may reset/rekey/delete existing data to make a migration pass. This sequence is a proposed implementation plan, not permission to perform upgrades or start a real engine.

### Gate E: one narrowly authorized provider pilot

Select the pilot only after Gate A. A supported Slack internal-app pilot is a candidate, not a foregone conclusion. A Discord bot pilot demonstrates bot resources, not personal-client parity. An SDK pilot proves only the explicitly approved subset.

First use mock HTTP/WebSocket/OAuth servers to exercise actual connector parsing, pagination, reconnect, token expiry/rotation, forbidden resources, rate limits and event/history overlap. Then, with explicit user/workspace authorization, use a test account/workspace/server and clearly synthetic content:

- bounded read-only resource discovery and recent history;
- one DM/channel/thread send with recipient confirmation;
- receive/edit/delete and sender identity;
- disconnect/resume and gap reconciliation;
- revoke permissions while a draft exists;
- stop/restart with no duplicate uncertain send;
- verify what the official client shows, rather than trusting only API acceptance.

Keep public/private channel, DM, group DM and thread results separate. Do not log credentials or private payloads. **Gate:** publish an observed capability matrix including failures; expand only proven operations.

### Gate F: daily-driver sync/retention and distribution

Once a connector is proven: optimize demand-driven backfill, local pagination/search, unread/mention behavior and background-service UX. Add size/age/attachment quotas with protected-data exhaustion tests, provider deletion/revocation handling and replay-after-eviction tests. Then assess packaging, licenses, API-policy obligations, app approval and operational support for distribution. No phase claims complete offline archives without coverage evidence.

## 10. Immediate recommendation and outstanding questions

Proceed first with a **synthetic multi-service navigation prototype and a precise capability/access decision record**, not simultaneous real Slack/Discord connector implementations. These two tracks can run independently: UI/domain feasibility is strong even while platform approval is uncertain.

Questions the next research/approval step must settle:

- Is the target your private machine only, or eventual public distribution? Does a reduced supported integration satisfy the intended product?
- Does your Slack workspace permit a user-context app, and what does Slack say about the exact use case? Can local event provisioning and custom-section/read-state requirements be met?
- Does Discord offer Bloom an eligible non-game SDK/RPC access path? If not, would bot-only scope be useful, or should personal Discord remain explicitly unofficial/deferred?
- What local retention and deletion behavior is acceptable when upstream history is unavailable or service policy requires removal?

Do not turn these questions into permission to launch live clients, extract sessions or activate registration. The next engineering step is a separately scoped prototype/contract task after review of this proposal.
