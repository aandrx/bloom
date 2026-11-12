# Demo mode

Status: implemented behind `--demo`. Demo mode runs the shipped `bloom`,
`bloomd`, and `bloom-tui` binaries against a synthetic, curated, isolated
session. It is not a separate build of the product: the same renderer, IPC,
store, and startup checklist run, but nothing outside the demo root is read,
written, or contacted.

## What demo mode is

- **Synthetic.** The daemon generates an encrypted SQLCipher database at startup
  from [`fixtures/demo/venue.json`](../fixtures/demo/venue.json). No binary store
  is committed, and no real message, account, or engine is involved.
- **Curated.** The fixture seeds five chats with natural multi-speaker
  exchanges, multiline text, attachments, read-only reactions, a parked draft,
  unread markers, and one long history for paging and search.
- **Isolated.** Every launch gets one fresh private root under `/tmp`, a fixed
  public demo key, and fixed paths. Real configuration, keychain, socket, store,
  and `HOME`/XDG destinations are never consulted.

The design record is in the [demo mode plan](demo-mode-plan.md).

## Running it

From the repository:

```sh
scripts/bloom --demo
```

The script builds the `demo` profile (which inherits `release`) and hands over to
the installed-style launcher. It never sources `.env`, so a developer checkout
cannot leak real socket, store, engine, or opener settings into the session.

To build and launch by hand:

```sh
cargo build --profile demo -p bloomd -p bloom-tui
target/demo/bloom --demo
```

`--demo` is mutually exclusive with `--release` and `--debug`. The launcher
creates the session root, starts the demo daemon, writes the demo preferences,
and starts the TUI. A direct `bloom-tui --demo` requires a launcher-created
session; a missing or malformed session fails startup and never falls back to
normal mode.

## What it shows

- The five-check startup checklist: `Daemon connected`, `Engine ready`,
  `Metadata refresh`, `Persistence acknowledged`, and `Local view and draft`.
  The demo daemon satisfies all five against the synthetic store.
- A restrained `Demo` indicator in the header status slot, outside the
  transcript.
- Five seeded chats: `Weekend crew`, `Design systems`, `Jules Lee`,
  `Dana Okafor`, and `Harbour log`.
- Three persistent unread badges (`Design systems`, `Dana Okafor`,
  `Harbour log`) that survive a restart of the TUI within the same session.
- A parked draft in `Design systems`, restored as a durable draft.
- Named synthetic attachments (`tide-notes.txt`, `spacing-spec.pdf`,
  `deck-v3.pdf`, `route-map.png`) with metadata only.
- Read-only inbound reactions.
- Searchable history, including the 205-message `Harbour log` for paging and
  deep search.

Sends and `/sync` are unavailable in demo mode: the demo daemon rejects both
with an unsupported-operation error. Preference changes apply only inside the
demo session and are discarded when it exits.

## Isolation guarantees

- One fresh root per launch, mode `0700`:

  ```text
  /tmp/bloom-demo-<random>/
    demo-session.json     synthetic-only identity marker
    runtime/bloomd.sock
    data/store.db
    config/bloom/tui.json
    state/bloom/
    cache/attachments/
    home/
  ```

- The store opens only through `EncryptedStore::open_demo()`, which uses a fixed
  public 32-byte demo key. `OsKeyring` is never constructed, and the real
  `bloomd.sqlite.v1` key namespace is never consulted.
- The demo daemon dispatches before real configuration or discovery: no engine
  configuration, no Flatpak or Dart lookup, no supervisor, no recovery loop, and
  no registration.
- Every `BLOOM_*` variable except `BLOOM_DEMO_ROOT` is ignored, including
  `BLOOM_SOCKET_PATH`, legacy `BLOOM_SOCKET`, `BLOOM_STORE_PATH`, engine and
  opener settings, and inherited `HOME`/XDG destinations. Nothing is removed
  from the environment; it is simply never read.
- The real socket is never probed or reused, and the real store and preferences
  are never opened. Diagnostics are pinned to the demo state directory.
- Normal mode never consults `BLOOM_DEMO_ROOT`, so an inherited value is inert
  unless `--demo` was requested.
- The root is validated in every entry point. A non-fresh, malformed, or
  unexpected root fails startup rather than being trusted or repaired.

## Ephemeral storage

The demo key is public, so nothing typed into a demo session is confidential.
Treat the session as throwaway:

- The session root is removed when the launcher and its children exit.
- A crashed or killed session leaves its root behind. The next launch uses a new
  random root and never adopts, reuses, or repairs a stale one.
- Stale roots are swept manually, never automatically:

  ```sh
  rm -rf /tmp/bloom-demo-*
  ```

## Boundaries

- No live engine, no real account or registration, no remote traffic, and no
  historical import. Demo mode is not proof of live delivery, renewal, or
  recovery behavior.
- Isolation prevents accidental real-state access. It is not a defense against a
  hostile host or a compromised process.
- For screenshots, recordings, or demos on public venues, disclose that the
  content is synthetic.
