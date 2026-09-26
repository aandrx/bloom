#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
umask 077
cleanup() { ./cleanup.sh; }
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
./setup.sh
cargo run
