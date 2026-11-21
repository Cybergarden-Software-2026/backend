#!/usr/bin/env bash
set -euo pipefail
umask 077

repo=/home/cyber/cybergarden/backend
state=/var/lib/underctrl-backend-deploy
config=/etc/underctrl-backend-deploy

exec 9>/run/lock/underctrl-backend-deploy.lock
flock -n 9 || exit 0

git_as_cyber() {
  runuser -u cyber -- git -C "$repo" "$@"
}

compose() {
  local image=$1
  shift
  UNDERCTRL_BACKEND_IMAGE="$image" docker compose \
    --project-name mts-incident-dashboard \
    --project-directory "$repo" --env-file "$repo/.env" \
    -f "$config/docker-compose.yml" -f "$config/compose.vserver.yml" \
    -f "$config/compose.image.yml" "$@"
}

check_backend() {
  local container
  container=$(compose "$image" ps --quiet api) || return 1
  [[ -n "$container" ]] && python3 "$config/check-backend.py" "$container"
}

wait_for_backend() {
  local attempt
  for ((attempt = 0; attempt < 45; attempt++)); do
    if check_backend; then return 0; fi
    sleep 2
  done
  return 1
}

git_as_cyber fetch --quiet --no-tags --no-write-fetch-head origin \
  '+refs/heads/main:refs/remotes/origin/main'
revision=$(git_as_cyber rev-parse --verify 'refs/remotes/origin/main^{commit}')
image="underctrl-backend:$revision"
compose "$image" config --quiet

api=$(compose "$image" ps --all --quiet api)
importer=$(compose "$image" ps --all --quiet importer)
if [[ -z "$api" || -z "$importer" ]]; then
  echo 'Start the existing Compose deployment before enabling this service.' >&2
  exit 1
fi
running_image=$(docker container inspect --format '{{.Image}}' "$api")
importer_image=$(docker container inspect --format '{{.Image}}' "$importer")
if [[ "$running_image" != "$importer_image" ]]; then
  echo 'API and importer do not share a runtime image; check the existing Compose project.' >&2
  exit 1
fi
if [[ "$(docker container inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$api")" == "$revision" &&
      "$(docker container inspect --format '{{.State.Running}}' "$importer")" == true ]] &&
   check_backend; then
  exit 0
fi

test_image="underctrl-backend-test:$revision"
test_prefix="underctrl-backend-check-$(openssl rand -hex 6)"
test_network="$test_prefix-net"
test_db="$test_prefix-db"
test_runner="$test_prefix-runner"
previous=underctrl-backend:previous
temporary_backup=
backup=
switch_started=0
cleanup() {
  local status=$?
  trap - EXIT
  if (( status != 0 && switch_started )); then
    echo 'Deployment failed, restoring the previous API and importer images.' >&2
    if compose "$previous" up -d --no-build --no-deps api importer &&
       wait_for_backend; then
      echo 'Previous containers passed readiness and snapshot checks.' >&2
    else
      echo 'Previous containers did not recover. Check the Compose logs and the database backup.' >&2
    fi
  fi
  if (( status != 0 )) && [[ -n "$backup" && -s "$backup" ]]; then
    printf 'Database backup: %s\n' "$backup" >&2
  fi
  docker container rm --force "$test_runner" "$test_db" >/dev/null 2>&1 || true
  docker network rm "$test_network" >/dev/null 2>&1 || true
  docker image rm "$test_image" >/dev/null 2>&1 || true
  if [[ -n "$temporary_backup" ]]; then
    rm -f -- "$temporary_backup"
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 143' TERM
trap 'exit 130' INT

echo "Building and testing backend $revision"
git_as_cyber archive --format=tar "$revision" |
  docker build --pull --target test --tag "$test_image" -

password=$(openssl rand -hex 24)
docker network create --internal "$test_network" >/dev/null
docker run -d --rm --name "$test_db" --network "$test_network" \
  --network-alias postgres --env "POSTGRES_PASSWORD=$password" \
  --tmpfs /var/lib/postgresql/data postgres:16-alpine >/dev/null
ready=0
for ((attempt = 0; attempt < 45; attempt++)); do
  if docker exec "$test_db" pg_isready -h 127.0.0.1 -U postgres -d postgres >/dev/null; then
    ready=1
    break
  fi
  sleep 2
done
if (( ! ready )); then
  echo 'Test PostgreSQL did not become ready.' >&2
  exit 1
fi
docker run --rm --name "$test_runner" --network "$test_network" \
  --env "TEST_DATABASE_URL=postgresql://postgres:$password@postgres:5432/postgres" \
  "$test_image"
docker container rm --force "$test_db" >/dev/null
docker network rm "$test_network" >/dev/null

git_as_cyber archive --format=tar "$revision" |
  docker build --target runtime --label "org.opencontainers.image.revision=$revision" \
    --tag "$image" -

install -d -m 0700 "$state/backups"
backup="$state/backups/$revision.dump"
if [[ ! -s "$backup" ]]; then
  temporary_backup=$(mktemp "$state/backups/.pending.XXXXXXXX")
  compose "$image" exec -T db sh -c \
    'exec pg_dump --format=custom -U "$POSTGRES_USER" -d "$POSTGRES_DB"' >"$temporary_backup"
  test -s "$temporary_backup"
  mv -T -- "$temporary_backup" "$backup"
  temporary_backup=
fi

docker image tag "$running_image" "$previous"
compose "$image" run --rm --no-deps -T migrate
switch_started=1
compose "$image" up -d --no-build --no-deps api importer
if ! wait_for_backend ||
   [[ "$(docker container inspect --format '{{.State.Running}}' "$(compose "$image" ps --quiet importer)")" != true ]]; then
  echo 'The new backend did not pass readiness and snapshot checks.' >&2
  exit 1
fi

echo "Deployed backend $revision on 127.0.0.1:8001"
