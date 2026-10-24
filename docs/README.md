# Documentation map

Start with [fresh-session handoff](handoff.md). The root [AGENTS.md](../AGENTS.md) is the automatic safety/continuation entry point for coding agents. Source/tests and Git status remain authoritative; no historical note grants permission to access live registration or messages.

## Current documents

| Document | Purpose |
| --- | --- |
| [Project README](../README.md) | Product intent, prerequisites, installation and limitations |
| [Handoff](handoff.md) | Fresh-session context, implementation map, safe tests and next decisions |
| [Status](status.md) | Implemented/partial/missing features and evidence boundaries |
| [System design](system-design.md) | Current Rust daemon/TUI, Dart child, store, identity and lifecycle architecture |
| [Implementation roadmap](implementation-plan.md) | Active ordered work and acceptance gates, not completed features |
| [TUI user guide](tui.md) | Controls, preferences, screen behavior, setup and troubleshooting |
| [TUI internals](tui-internals.md) | Redraw, state/ownership, geometry, palettes, terminal lifecycle and test tooling |
| [Local filename search](filename-search.md) | Body-first filename search, display rules, v6 compatibility, verification status, and limits |
| [Attachment metadata](attachment-metadata.md) | Allowlisted ingestion and local metadata tokens, not file content or download identity |
| [Reading/navigation UI iteration](ux-polish-review.md) | Accepted current-chat interaction/design scope, reproductions and evidence; integrations deferred |
| [Switching latency follow-up](tui-switching-performance.md) | Measured local switching lag, bounded rendering correction and repeatable synthetic PTY probe |
| [Previous completed iteration](iteration-review.md) | Accepted reliability/UI fixes, synthetic verification, parent critique and remaining limits |
| [Sidecar package guide](../ext/bloom-engine-sidecar/README.md) | Runtime prerequisites, version/licensing boundary and current child protocol |

## Research, history and deferred work

| Document | Status / how to use |
| --- | --- |
| [Packaging plan](packaging-plan.md) | Standalone install layout, doctor diagnostics, phases, licensing boundary; design only |
| [Historical ingestion plan](historical-ingestion-plan.md) | Design-only gate for importing past messages; blocked at the engine boundary |
| [Engine recovery plan](engine-recovery-plan.md) | Proposed generation-scoped engine restart, invariants, phased units, risks; not implemented |
| [Attachment filename baseline](attachment-search-gap.md) | Historical ingestion gap, accepted prerequisites, and limits of old unsupported records |
| [Multi-service research and proposed plan](multi-service-research.md) | Deferred Slack/Discord feasibility, policy/access limits, service-native navigation and staged acceptance gates; not implemented connectors |
| [Engine-sidecar evidence](engine-sidecar.md) | Historical research/authorized live experiments plus updated integration boundaries; not current-checkout live proof |
| [IDS validation gate](ids-validation-gate.md) | Independent registration remains blocked; existing-engine reuse does not open this gate |
| [GPUI bookmark](gpui-surface.md) | Deferred surface exploration; no working GUI implementation or scheduled port |
| [Earlier product audit](product-audit.md) | Earlier completed iteration; baseline findings/counts/scores are historical |
| [Earlier UX upgrade](tui-ux-upgrade.md) | Codex references and past design decisions; do not redispatch its phases |
| [Earlier reliability follow-up](tui-reliability-next.md) | Historical investigation and durable-send design constraints; not an active task queue |
| [Matrix proof](../spike/matrix/README.md) | Excluded synthetic research stack, not the product's messaging route |

`design/mockup/index.html` and `design/previews/` are static illustrative references. Current Mermaid sources cover [architecture](diagrams/architecture.mmd), [runtime ordering](diagrams/runtime-flow.mmd), [roadmap](diagrams/waves.mmd), [maintainer process](diagrams/process.mmd) and [session lifecycle versus reported labels](diagrams/session-states.mmd). They replace obsolete direct-transport/relay wave diagrams and are not executable features. Generated bindings and dependency metadata have their own attribution/update tooling; they are not manually maintained product documentation.

## Updating documentation

When behavior changes, update the user guide, status, current design/internals, handoff and active roadmap as appropriate. Record acceptance evidence/limits in the iteration review. Preserve historical records with status labels rather than making old experiments sound like new production guarantees. Link to reusable commands and files, not transient agent handles or machine-specific session memory. Test counts are dated checkpoints; rerun the suites instead of relying on them.
