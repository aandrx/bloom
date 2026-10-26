# Next iteration plan: recovery, packaging, rich messaging, GPUI and multi-service

Status: proposed overarching plan for review. It does not authorize a live engine run, a real install, account access, or connector work. Each track still needs its own scoped unit plan and acceptance evidence.

This plan follows the original roadmap rather than replacing it. It records what is already accepted, then sequences the remaining work the user selected: live recovery validation, packaging diagnostics, richer messaging, and a decision on GPUI and multi-service.

## Current baseline

- Text messaging, local history, paging, search, unread navigation, session undo, durable encrypted drafts, inbound reactions, inbound text edits and whole-body unsends, and generation-scoped engine recovery are implemented.
- The synthetic universal gate passes: workspace and timezone tests, strict Clippy, formatting, guards, 68 fixture, 13 input PTY, 8 messaging PTY, 17 reading PTY, 8 launcher, and 58 Dart tests.
- Historical ingestion remains blocked at the engine boundary.
- Packaging phases 1 to 3 are accepted; phase 4 is open because a daemon-owned sidecar occasionally emitted an unparsable frame.
- The launcher stays on the debug profile.
- GPUI and multi-service are deferred design tracks, not active implementation.

## Track A: live engine recovery validation

Goal: prove that a real Dart sidecar child is reaped and replaced after an unexpected death, without a second registration owner, duplicate send, or surviving child.

This is the only track that needs live account state. It is a receive/recovery test, not a send or history test.

### Authorization required

Run only after the user explicitly approves this statement:

> Authorize one live engine recovery validation: start one daemon against my real OpenBubbles registration and state, allow it to consume pushes into a temporary Bloom store, kill only that engine child once with SIGKILL, observe recovery and exactly one replacement owner, then stop and delete the temporary store. No sends, conversation sync beyond startup, history import, renewal, registration change, or credential access are authorized.

Without that statement, stop at a synthetic or preflight-only unit.

### Preflight

1. Close OpenBubbles completely. Confirm no other owner:

   ```sh
   pgrep -af 'bloomd|bloom_engine'
   ```

   It must print nothing.
2. Resolve paths without printing account data:

   ```sh
   flatpak info --show-location app.openbubbles.OpenBubbles
   ```

   Confirm the engine library exists at `<location>/files/bluebubbles/lib/librust_lib_bluebubbles.so` and the state directory is `~/.var/app/app.openbubbles.OpenBubbles/data/bluebubbles`.
3. Resolve the sidecar script and Dart executable, and confirm `dart pub get` has already succeeded in the sidecar tree.
4. Build the daemon and TUI from the current tree. Use a fresh temporary `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CONFIG_HOME`, and `BLOOM_SOCKET_PATH`; never reuse the personal Bloom store.
5. Record the exact environment, binary hashes, engine version if reported, and start/stop timestamps in the private evidence directory.

### Probe design

Add an ignored live test or a small scripted probe that:

1. Starts exactly one `bloomd` with the temporary socket/store and the real state/library/script.
2. Waits for `ready`, an active session, and a non-empty conversation list.
3. Finds the engine child by parent PID, records its PID and start time, and verifies exactly one child.
4. Sends `SIGKILL` to that child only.
5. Asserts the daemon stays alive, the socket still answers, and subscribers see a disconnected session.
6. Waits a bounded interval for a replacement child with a different PID and verifies the old PID is gone and reaped.
7. Verifies the conversation list and session become active again, and that no second daemon or extra engine exists.
8. Optionally waits for one inbound event to prove recovery is usable; never prints content.
9. Stops the daemon with SIGTERM, asserts no child or socket remains, and deletes the temporary store without reading it.

Forbidden: sending a message, forcing a sync, importing history, copying the store, printing handles/IDs/content, touching registration files, or retrying an uncertain operation.

### Exact commands

Run from the repository root after reading the authorization statement. Do not set the token before you have decided to authorize the run.

```sh
cd $HOME/Documents/Projects/bloom
cargo build -p bloomd

STATE="$HOME/.var/app/app.openbubbles.OpenBubbles/data/bluebubbles"
FLATPAK="$(flatpak info --show-location app.openbubbles.OpenBubbles)"
LIB="$FLATPAK/files/bluebubbles/lib/librust_lib_bluebubbles.so"
SCRIPT="$PWD/ext/bloom-engine-sidecar/dart/bin/bloom_engine.dart"
DART="$(command -v dart || printf '%s' "$HOME/.cache/dart-sdk/dart-sdk/bin/dart")"

test -d "$STATE"
test -f "$LIB"
test -f "$SCRIPT"
test -x "$DART"
```

Close OpenBubbles, then run the read-only preflight. It starts nothing and refuses if any competing owner exists:

```sh
BLOOM_ENGINE_STATE="$STATE" \
BLOOM_ENGINE_LIBRARY="$LIB" \
BLOOM_ENGINE_SCRIPT="$SCRIPT" \
BLOOM_DART_EXECUTABLE="$DART" \
scripts/live-recovery-probe --preflight
```

After that prints `preflight: no competing owner; required paths exist`, authorize exactly one run by setting the guard token. This token is a script guard, not a substitute for your authorization statement:

```sh
export BLOOM_LIVE_RECOVERY_ACK=one-shot-authorized
scripts/live-recovery-probe \
  --bloomd target/debug/bloomd \
  --engine-state "$STATE" \
  --engine-library "$LIB" \
  --engine-script "$SCRIPT" \
  --dart "$DART" \
  --evidence-dir "$HOME/bloom-live-recovery-evidence"
```

The script starts one daemon with an isolated socket/store, waits for a child, kills only that verified child, requires a different reaped-and-replaced PID, checks the session and conversation listing, stops the daemon, rejects a surviving engine child, and deletes the temporary store. It prints only structural results and keeps `recovery-summary.json` in the evidence directory.

After it exits, clear the token and verify cleanup:

```sh
unset BLOOM_LIVE_RECOVERY_ACK
pgrep -x bloomd || true
ps -eo pid,args | grep -E '[b]loom_engine\.dart' || true
```

Both process checks must print nothing. Read only `recovery-summary.json`; do not open or share the temporary daemon log, which may contain live activity metadata.

The probe script itself was exercised against a disposable fake daemon that spawned, killed and replaced a dummy `bloom_engine.dart` child. That validates its process checks, IPC queries, replacement wait and cleanup logic without touching live state; it is not live-engine evidence.

### Acceptance and abort rules

- Pass only if one owner exists at every sampled moment, the replacement is a different reaped-and-replaced child, the daemon never exits, and no process or socket survives cleanup.
- Abort immediately on two engine children, a second daemon, a missing registration, any request to sign in, or any ambiguous ownership state.
- Treat renewal, ABI compatibility, and recovery of the real registration across restarts as unverified unless directly observed.
- If the probe fails, preserve only allowlisted logs and the structural outcome; never attach raw engine logs.

### Track A result

Passed on the first authorized run. A temporary daemon started with the real registration and isolated socket/store; the first engine child was killed after its parent and command line were verified; the daemon stayed alive, the old PID was reaped, a different replacement PID appeared after 0.42 seconds, and the replacement reached an active session. The conversation listing returned zero conversations, so this proves child replacement and session recovery, not message transfer or archive readiness. Cleanup removed the daemon, both engine PIDs and the temporary store; no owner remained.

Limits: one run, no sends, no inbound message round trip, no conversation listing content, no renewal or ABI check, and no restart of the daemon itself. Treat repeated recovery, crash-loop exhaustion against the real engine, and delivery after recovery as separate unverified gates.

## Track B: packaging phase 4 diagnostics

Goal: explain or structurally contain the intermittent daemon-owned unparsable frame, then finish the two-part install gate.

### Observed facts

- The only reproduced fault was one line of 768 printable ASCII bytes, first byte `0x38` (`8`), no `{`, no NUL, reported by the daemon.
- Direct staged-sidecar captures, including 300 seconds of steady state and a `--sync-chats` run, produced only valid JSON.
- Eight staged-prefix daemon attempts did not reproduce it.
- The fault therefore appears only when the daemon owns the child, or only under an event/timing condition not yet captured.

### Hypotheses, in test order

1. **Foreign writer on the child's stdout.** The linked engine library or a Dart/FFI component writes a diagnostic line to fd 1, interleaving with protocol frames. Test: run the daemon with a fd-level capture (`strace -f -e write` or a byte-timestamping wrapper) that records write sizes and call origins without payload; correlate the 768-byte write with a frame sequence and source. Evidence that would close it: a write outside the sidecar's frame path with that shape.
2. **Protocol framing assumption.** A partial or interleaved frame is being split into a line that happens to be printable. Test: record raw write boundaries and line lengths for a large frame and concurrent writers; compare with the daemon's line reader. Evidence: the 768 bytes are a tail/prefix of a larger frame.
3. **Daemon-owned environment difference.** The child behaves differently under daemon cwd/env/stdin than under direct capture. Test: run the sidecar with the daemon's exact environment, cwd, stdin behavior, and timing, then with a controlled trigger. Evidence: reproduction requires one of those differences.

Do not run unbounded soak loops. Stop after a reproduced fault or a named hypothesis is eliminated.

### Containment design

If the writer cannot be eliminated, add a protocol integrity envelope:

- The sidecar prefixes every protocol line with a reserved marker and protocol revision.
- The daemon accepts only marked lines as frames; an unmarked complete line is structurally logged and skipped with a bounded count rather than closing the engine.
- A malformed marked frame still fails closed.
- A version handshake makes old/new sidecar pairing explicit.
- A fake sidecar regression emits junk lines between valid frames and proves they are skipped, while a malformed marked frame still terminates the connection.

This is a protocol change and needs its own version decision and tests. It is not permission to hide repeated corruption indefinitely: repeated skips must surface a bounded diagnostic and a session failure after a documented threshold.

### Gate rerun

After either a root-cause fix or containment:

1. Staged-prefix checks: doctor passes, resolved sidecar lives under the prefix, missing dependency cases fail cleanly.
2. Lifecycle checks: socket appears, TUI reaches readiness, exactly one daemon and one engine, SIGTERM leaves no child or stale socket, temporary store deleted by the harness.
3. Report the two halves separately. ABI and licensing remain unverified until their own review.

### Track B result

The 768-byte line is consistent with a foreign stdout writer: the installed engine library contains multiple Rust logger implementations that can write to stdout. Rather than print or rewrite their payload, the sidecar now marks every protocol line, and the daemon skips a bounded number of unmarked non-JSON lines with a redacted structural log. Unmarked JSON remains compatible; a malformed marked frame still fails closed. Synthetic tests cover both paths.

This is containment, not a proven root cause. The live staged-prefix rerun passed: doctor resolved the prefix sidecar, one daemon and one engine started, a second launcher reused the daemon, cleanup removed the socket and processes, and hidden-checkout missing sidecar/Dart/library cases failed cleanly. The original unparsable line did not recur and the containment path was not exercised live. The fault remains intermittent; ABI and licensing review remain open.

## Track C: remaining rich messaging

Inbound reactions, text edits and whole-body unsends are accepted. Remaining work is gated by transport evidence, not UI demand.

### C1. Inbound attachment usefulness

- Already present: reported filename metadata and safe placeholders.
- Missing: opening or downloading the file, previews, and content search.
- Gate: a verified local path or download API that does not imply success before bytes exist. If the only route is the blocked archive/object path, keep display-only and document the block.
- Acceptance: no fabricated filename or "downloaded" claim; unsupported parts remain explicit; no plaintext extraction or URL inference.

### C2. Outbound edits, unsend and reactions

- Outbound reactions are out of scope by user decision.
- Outbound edits/unsend are optional. Gate: the pinned engine's send API accepts the corresponding message type through the sidecar, proven with a synthetic or authorized test. If it does not, stop.
- If the gate passes: add an explicit capability, one confirmation path, durable intent, no automatic retry, and no claim of exactly-once delivery. Keep edits text-only and unsend whole-message until per-part tombstones exist.

### C3. Replies, typing and read receipts

- Gate each separately. A domain field or inbound enum is not proof of remote support.
- Replies require transport identity and rendering; typing/read receipts require authoritative inbound events and an explicit privacy decision. Do not add outbound read receipts merely because a type exists.

### C4. Group management

- Existing group text is conditional on recipient-list capability.
- Creating, renaming, adding or removing participants is a separate transport workflow and is not planned until a real API path is established.

Priority: C1 evidence first, C2 gate second, C3/C4 deferred.

## Track D: GPUI decision

Recommendation: keep the TUI as the supported client and evaluate GPUI as a second daemon client, not a replacement.

### Why not pivot now

- GPUI is pre-1.0 with early Linux support; its API and platform behavior are moving targets.
- The TUI is tested, works over SSH, and is the current daily surface.
- A GUI earns its cost on transcript layout, inline attachments, notifications, selection and pointer interaction, not on duplicating existing terminal structure.
- The daemon/IPC boundary already supports another client, but shared UI state (`App`) is ratatui-shaped and must be extracted deliberately.

### Bounded spike

1. Build a disposable window that renders one long synthetic transcript with wrapping, selection, scroll, theme switching, and an attachment placeholder.
2. Verify Linux Wayland and X11, input methods, high-DPI, frame time on a long transcript, and packaging impact.
3. Compare against the previews, not a port of terminal geometry.
4. Decide: proceed, defer, or reject. A successful spike is not a production port.

If it proceeds, split into: shared client-state extraction, IPC client reuse, transcript surface, composer, attachments/notifications, and packaging. Keep the TUI passing throughout.

## Track E: multi-service direction

Recommendation: keep multi-service as a research and spine track, not an immediate connector build. It is the stronger differentiator, but it changes identity, storage, IPC, auth and lifecycle at once.

Sequence from the existing research:

1. **Gate A: access decision.** Per platform, settle personal versus distribution use, supported versus unofficial access, credential provisioning, and retention/permission obligations. No public claim before this.
2. **Gate B: synthetic navigation.** Multiple fake instances with colliding names and IDs; terminal captures at 60 to 180 columns; account/channel/thread context always visible.
3. **Gate C: connector spine.** Instance-scoped identity, routing, permissions, checkpoints, fairness, and no wrong-account sends, proven with fake services.
4. **D1 to D5.** Identity/storage groundwork, registry routing, scoped IPC, hierarchy UI, independent lifecycle, each separately accepted.
5. **Gate E.** One narrowly authorized provider pilot with synthetic content and explicit user/workspace approval.

Do not start GPUI and multi-service together. Finish recovery and packaging first, then choose one: GPUI if the product bottleneck is presentation; multi-service if it is reach.

## Sequence

1. A: live recovery validation, after explicit authorization.
2. B: packaging fault reproduction and containment, then the two-half gate.
3. C1/C2: attachment evidence and outbound mutation gate.
4. D: bounded GPUI spike and decision.
5. E: Gate A/B multi-service research and synthetic spine.

Tracks B and C can run without live credentials. Track A is the only live gate and must not be folded into them. Track D and E are decisions with prototypes, not open-ended ports.

## Validation checklist for the next scoped plans

- Each unit names exact files, commands, and non-goals.
- Each gate has evidence that can fail, not only a completion summary.
- Live work has a written authorization statement, owner checks, isolated store/socket, and cleanup proof.
- Packaging work does not bundle the engine or claim Cargo-only installation.
- GPUI work does not fork the daemon or duplicate protocol semantics.
- Multi-service work does not reuse a single-account store or guess identities.
- Any new protocol change has a version decision and old-peer rejection test.
- No plan claims success from a partial agent run.
