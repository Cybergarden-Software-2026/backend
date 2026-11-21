#!/usr/bin/env bash
set -euo pipefail
set +x
umask 077

if (( $# > 1 )); then
  echo 'Usage: create-test-coordinators.sh [credentials.json]' >&2
  exit 2
fi

credentials=${1:-${XDG_STATE_HOME:-$HOME/.local/state}/underctrl/test-coordinators.json}
if [[ -e "$credentials" || -L "$credentials" ]]; then
  printf 'Existing credentials file left unchanged: %s\n' "$credentials"
  exit 0
fi

docker_args=(sudo docker)
if (( EUID == 0 )) || docker info >/dev/null 2>&1; then
  docker_args=(docker)
fi
api=$("${docker_args[@]}" ps --quiet \
  --filter label=com.docker.compose.project=mts-incident-dashboard \
  --filter label=com.docker.compose.service=api)
if [[ -z "$api" || "$api" == *$'\n'* ]]; then
  echo 'Expected one running API container in the mts-incident-dashboard project.' >&2
  exit 1
fi

mkdir -p -- "$(dirname -- "$credentials")"
pending=$(mktemp -- "$credentials.pending.XXXXXXXX")
trap '[[ ! -s "$pending" ]] && rm -f -- "$pending"' EXIT

if ! "${docker_args[@]}" exec "$api" /usr/local/bin/api coordinator create-test >"$pending"; then
  if [[ -s "$pending" ]]; then
    printf 'Review the protected output from the failed command: %s\n' "$pending" >&2
  fi
  exit 1
fi

if cmp -s -- "$pending" <(printf '[]\n'); then
  rm -f -- "$pending"
  echo 'All three test coordinator accounts already exist.'
  exit 0
fi

if python3 - "$pending" "$credentials" <<'LINK_CREDENTIALS'
import os
import sys

os.link(sys.argv[1], sys.argv[2])
LINK_CREDENTIALS
then
  rm -f -- "$pending"
  printf 'New test coordinator credentials saved to %s\n' "$credentials"
else
  printf 'New test coordinator credentials retained in %s\n' "$pending" >&2
  exit 1
fi
