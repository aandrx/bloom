# Bloom Matrix local proof

**Status: excluded legacy research proof, not Bloom's product transport or next iteration.** Start with [the current handoff](../../docs/handoff.md); do not run this separate Docker stack as the normal Bloom setup.

A provisional local end-to-end encryption proof using the official Rust `matrix-sdk` 0.19.1 and Synapse. It verifies encrypted event delivery and local crypto-store persistence, not key backup or recovery and not production security. Use synthetic accounts only.

Requires Docker Compose, Rust, and curl. From this directory, run the complete isolated proof with `./run-proof.sh`. It creates a fresh ignored `data/`, starts Synapse, provisions synthetic accounts, runs the Rust proof, and always stops Synapse and removes only this spike's `data/` on exit. For manual setup, use `./setup.sh`, then `cargo run`, and finally `./cleanup.sh`.

`setup.sh` starts Synapse bound exclusively to `127.0.0.1:8008`, waits for readiness, then creates two normal local accounts with `register_new_matrix_user` (public registration is not enabled). Passwords and all generated Synapse/client data are stored under ignored `data/`; output never includes credentials. The restored Bob session (including tokens) and sync token are created with owner-only permissions (0600 on Unix), including in the manual flow. The harness requires setup-created credentials in `data/accounts.env`.

The proof verifies independent Alice/Bob SDK clients and SQLite crypto stores, private invite/join access, encrypted delivery with a direct homeserver ciphertext check (encrypted event type, ciphertext content, and no unique plaintext marker) plus Bob's decrypted exact-event checks, offline encrypted catch-up after reopening Bob's stored session and crypto store, readable decrypted message history, and rejection of invalid credentials. It stops Synapse on exit. Cryptographic key backup/recovery is not tested or claimed. `cleanup.sh` stops the service and deletes all local state, including homeserver signing key/database and client stores.
