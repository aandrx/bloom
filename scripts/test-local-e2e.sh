#!/usr/bin/env bash
set -euo pipefail
umask 077
root="$(cd "$(dirname "$0")/.." && pwd)"
spike="$root/spike/matrix"
cleanup() { "$spike/cleanup.sh"; }
trap cleanup EXIT INT TERM
"$spike/setup.sh"
export BLOOM_E2E_ACCOUNTS="$spike/data/accounts.env"
set -a
source "$BLOOM_E2E_ACCOUNTS"
set +a
cd "$root"
cargo test -p bloom-core --test matrix_e2e -- --ignored --nocapture
