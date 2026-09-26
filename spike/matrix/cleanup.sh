#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
docker compose down --remove-orphans >/dev/null 2>&1 || true
if [[ -d data ]]; then
  if rm -rf data 2>/dev/null; then
    exit 0
  fi
  docker run --rm --user 0:0 --entrypoint /bin/sh -v "$PWD/data:/cleanup" matrixdotorg/synapse:v1.127.0 -c 'rm -rf /cleanup/* /cleanup/.[!.]* /cleanup/..?*'
  rmdir data 2>/dev/null || true
fi
