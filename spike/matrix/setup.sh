#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
umask 077
mkdir -p data/synapse
# Generated synthetic-only credentials and config; never print secret values.
if [[ ! -f data/accounts.env ]]; then
  alice="bloom-alice-$(date +%s)"
  bob="bloom-bob-$(date +%s)"
  alice_pw="$(openssl rand -hex 24)"
  bob_pw="$(openssl rand -hex 24)"
  printf 'ALICE_USER=%q\nALICE_PASSWORD=%q\nBOB_USER=%q\nBOB_PASSWORD=%q\n' "$alice" "$alice_pw" "$bob" "$bob_pw" > data/accounts.env
fi
if [[ ! -f data/synapse/homeserver.yaml ]]; then
  docker compose run --rm --no-deps synapse generate
fi
docker compose up -d
for _ in $(seq 1 90); do
  if curl -fsS http://127.0.0.1:8008/_matrix/client/versions >/dev/null; then break; fi
  sleep 2
done
curl -fsS http://127.0.0.1:8008/_matrix/client/versions >/dev/null || { echo 'Synapse did not become ready' >&2; exit 1; }
set -a
source data/accounts.env
set +a
for user in "$ALICE_USER" "$BOB_USER"; do
  password="$([[ "$user" == "$ALICE_USER" ]] && printf %s "$ALICE_PASSWORD" || printf %s "$BOB_PASSWORD")"
  docker compose exec -T synapse register_new_matrix_user -c /data/homeserver.yaml -u "$user" -p "$password" --no-admin http://127.0.0.1:8008 >/dev/null 2>&1 || { echo 'Account registration failed; details intentionally withheld' >&2; exit 1; }
done
chmod 600 data/accounts.env
printf 'Synapse ready at http://127.0.0.1:8008; two local accounts configured.\n'
