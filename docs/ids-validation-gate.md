# Direct iMessage validation gate

**State: blocked.** A successful offline build is not proof of independent iMessage
registration. Bloom has not used real Apple credentials or completed a live send,
receive, or renewal. The OpenBubbles Flatpak working while the Mac sleeps proves
that app's behavior, not Bloom's independent transport.

## Verified boundary

- The isolated harness at `~/.cache/bloom-direct-spike` decodes Base64 `OABS`,
  one sharing byte, and protobuf hardware fields into rustpush `MacOSConfig`.
  Its tests are synthetic and offline. Hardware info is not IDS validation data.
- The pinned OpenBubbles rustpush fork is `a7fab473e7a33325a760635285db2860de8e1cb0`.
  Its public `open-absinthe` dependency is
  [OpenAbsinthe-Stub](https://github.com/OpenBubbles/OpenAbsinthe-Stub), whose
  README says production is closed source. `ValidationCtx::new`,
  `key_establishment`, and `sign` are unfinished in the public stub. The Mac
  generator calls them before registration can submit `validation-data`.
- [pypush](https://github.com/JJTech0130/pypush) is a research lead, not an
  approved Bloom dependency. Its [license](https://github.com/JJTech0130/pypush/blob/main/LICENSE)
  is SSPL and its README directs licensing questions to Beeper. The published
  validation approach emulates an obfuscated macOS binary. Redistribution
  rights for that binary and a current renewal path are not established.
- Anisette and Mac Hardware Info solve different inputs. A remote validation
  endpoint or a permanently running Mac would change the product requirement
  and must not be substituted silently.

## What would open Gate 0

1. Establish a lawful, locally runnable implementation and redistribution
   terms, including every required binary and Apple asset. Keep any SSPL code
   outside the MIT core and default dependency graph.
2. In an isolated, credential-safe harness, generate fresh IDS validation
   data locally for the actual rustpush registration path. Do not accept a
   hardcoded test blob, a hosted generator, or a compile-only result.
3. With a dedicated test identity and explicit user authorization, demonstrate
   independent registration, two-way send/receive, restart, and scheduled
   renewal with the Mac off. Verify the results and redacted logs.

Until all three pass, Bloom's local daemon and TUI may progress against the fake
transport, but direct iMessage must remain disabled and marked unverified.

A separate, narrower route exists for users who accept the installed local
OpenBubbles app as the engine and want only a different interface. That is not a
Gate 0 fix and does not give Bloom its own registration; see `engine-sidecar.md`.
