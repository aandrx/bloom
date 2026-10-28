# Contributing and getting help

Bloom is an experimental Linux terminal client for an existing OpenBubbles
registration. There is no guaranteed support schedule or stable release series.
See [current status](docs/status.md) for implemented behavior and limits.

## Report a bug

For a non-security bug, include the commit, operating system, terminal, expected
behavior, and a minimal synthetic reproduction. Use [SECURITY.md](SECURITY.md)
for suspected vulnerabilities.

Do not attach your registration state, encrypted store, credentials, raw engine
frames, or real conversation screenshots. Engine logs and terminal captures can
contain personal data. Prefer a synthetic reproduction over a redacted live log.

## Keep local development private

Keep using the same checkout for development. `.env`, `target/`, and the sidecar's
`.dart_tool/` directory are ignored. The repository launcher sources `.env` as
shell code, so use only a file you trust. It can override local runtime paths
without putting those paths in Git. Do not store credentials in `.env` or commit
local captures.

`./scripts/bloom` defaults to release. `./scripts/bloom --debug` remains available
for local debugging. Both can start the real engine and consume messages. They
are not safe commands for a synthetic UI preview.

Capture tools retain their `$HOME` defaults. Set `TUI_SHOT_TOOLS` to the capture
virtual environment directory or `BLOOM_BROWSER` to your browser helper script
when your development tools live elsewhere. These optional helpers are not
product runtime dependencies.

## Verify a change without an account

From the repository root, run the synthetic Rust checks:

```sh
cargo build -p bloom-tui
cargo test --workspace --all-targets
TZ=Asia/Tokyo cargo test -p bloom-tui
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
./scripts/check-sources
```

Use a virtual environment with `pyte==0.8.2` for the Python fixture and PTY tests.
See [the safe test setup](docs/handoff.md#safe-test-setup) and CI in
`.github/workflows/check.yml` for the complete commands and Dart checks. Do not
run ignored real-engine tests as part of routine verification.

## Submit a change

Keep the change scoped, preserve unrelated work, and add a regression for a bug
when practical. Describe what you ran and which behavior remains unverified.
Never turn an uncertain send into an automatic retry or add another owner of the
same registration to reproduce an issue.

Keep the engine external and preserve licenses and attribution for copied code.
Do not hand-edit generated upstream bindings. Synthetic test results do not
establish live delivery, account renewal, or compatibility with other engines.
