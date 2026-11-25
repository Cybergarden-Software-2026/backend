#!/usr/bin/env bash
set -euo pipefail
umask 077

die() { printf 'Deployment failed: %s\n' "$*" >&2; exit 1; }

[[ $EUID == 0 && $# == 1 && $1 == /* ]] || die 'Run through sudo with the absolute upload directory.'
bundle=$1
project=underctrl-ssh
state=/var/lib/$project
marker=$state/.managed
marker_value=underctrl-ssh-v1
image_label=io.underctrl.installation

for tool in docker curl flock openssl python3 tar; do
    command -v "$tool" >/dev/null || die "Required command is missing: $tool."
done
unset DOCKER_CONTEXT DOCKER_TLS DOCKER_TLS_VERIFY DOCKER_CERT_PATH DOCKER_API_VERSION
unset COMPOSE_PROJECT_NAME COMPOSE_FILE COMPOSE_PROFILES
unset POSTGRES_USER POSTGRES_DB POSTGRES_PASSWORD
export DOCKER_HOST=unix:///var/run/docker.sock
docker info >/dev/null || die 'The local Docker engine is unavailable.'
engine_version=$(docker version --format '{{.Server.Version}}')
if [[ ! $engine_version =~ ^v?([0-9]+)\. ]] || (( 10#${BASH_REMATCH[1]} < 28 )); then
    die "Docker Engine 28 or newer is required for loopback-only publishing (found $engine_version)."
fi
docker compose version >/dev/null || die 'Install Docker Compose v2.24.4 or newer.'
docker buildx version >/dev/null || die 'Install the Docker Buildx plugin.'

for file in revisions backend.tar frontend.tar compose.ssh.yml; do
    [[ -f "$bundle/$file" && ! -L "$bundle/$file" ]] || die "Missing upload file: $file."
done
mapfile -t revisions < "$bundle/revisions"
[[ ${#revisions[@]} == 2 && ${revisions[0]} =~ ^[0-9a-f]{40}$ &&
    ${revisions[1]} =~ ^[0-9a-f]{40}$ ]] || die 'Expected two Git commit IDs in the upload metadata.'
release_id=${revisions[0]}-${revisions[1]}

exec 9>"/run/$project.lock"
flock -n 9 || die 'Another deployment or removal is already running.'

if [[ -e $state || -L $state ]]; then
    [[ -d $state && ! -L $state && -O $state && -f $marker && ! -L $marker && -O $marker ]] ||
        die "Unrecognized state directory: $state."
    [[ $(stat -c %a -- "$state") == 700 && $(stat -c %a -- "$marker") == 600 ]] ||
        die "Unsafe permissions on $state or its marker."
    cmp -s "$marker" <(printf '%s\n' "$marker_value") || die "Unrecognized marker: $marker."
else
    existing_containers=$(docker container ls -aq --filter "label=com.docker.compose.project=$project")
    existing_volumes=$(docker volume ls -q --filter "label=com.docker.compose.project=$project")
    existing_networks=$(docker network ls -q --filter "label=com.docker.compose.project=$project")
    if [[ -n $existing_containers || -n $existing_volumes || -n $existing_networks ]] ||
        docker volume inspect "${project}_pgdata" >/dev/null 2>&1 ||
        docker network inspect "${project}_default" >/dev/null 2>&1; then
        die "Docker already has resources named for $project without the installation marker."
    fi
    state_stage=$(mktemp -d "/var/lib/.$project.XXXXXXXX")
    trap 'rm -rf -- "$state_stage"' EXIT
    printf '%s\n' "$marker_value" > "$state_stage/.managed"
    mv -T -- "$state_stage" "$state"
    trap - EXIT
fi

install -d -m 0700 "$state/releases" "$state/backups"
install -d -m 0755 "$state/data"
release=$state/releases/$release_id
previous_id=
previous=
if [[ -e $state/current ]]; then
    read -r previous_id < "$state/current"
    [[ $previous_id =~ ^[0-9a-f]{40}-[0-9a-f]{40}$ ]] || die "Invalid release ID in $state/current."
    previous=$state/releases/$previous_id
    [[ -d $previous && ! -L $previous ]] || die "Current release directory is missing: $previous."
fi

stage=
cutover=0
qwen_url=
compose() {
    local release_dir=$1 id=${1##*/}
    shift
    local -a files=(-f "$release_dir/backend/docker-compose.yml" -f "$release_dir/compose.ssh.yml")
    if [[ -n $qwen_url ]]; then
        files+=(-f "$state/qwen.compose.yml")
    fi
    UNDERCTRL_BACKEND_IMAGE="underctrl-ssh-backend:${id%-*}" \
    UNDERCTRL_FRONTEND_IMAGE="underctrl-ssh-frontend:${id#*-}" \
    UNDERCTRL_ENV_FILE="$state/.env" UNDERCTRL_DATA_DIR="$state/data" UNDERCTRL_QWEN_URL="$qwen_url" \
        docker compose --project-name "$project" --project-directory "$release_dir/backend" \
        --env-file "$state/.env" "${files[@]}" "$@"
}

check_dashboard() {
    local attempt
    for ((attempt = 0; attempt < 60; attempt++)); do
        if curl --noproxy '*' --fail --silent --max-time 3 http://127.0.0.1:8005/healthz >/dev/null &&
            curl --noproxy '*' --fail --silent --max-time 3 http://127.0.0.1:8005/ >/dev/null &&
            curl --noproxy '*' --fail --silent --max-time 5 \
                'http://127.0.0.1:8005/api/v1/snapshot?view=web' |
                python3 -c 'import json, sys
snapshot = json.load(sys.stdin)
sys.exit(not (isinstance(snapshot["items"], list) and snapshot["source"]["status"] == "ok"))' 2>/dev/null; then
            return
        fi
        sleep 2
    done
    return 1
}

check_qwen_proxy() {
    local worker pid
    command -v nsenter >/dev/null &&
        worker=$(compose "$release" ps --quiet ai-worker) && [[ -n $worker ]] &&
        pid=$(docker container inspect --format '{{.State.Pid}}' "$worker") &&
        [[ $pid =~ ^[1-9][0-9]*$ ]] &&
        nsenter --target "$pid" --net -- curl --noproxy '*' --fail --silent --max-time 3 \
            "${qwen_url%/v1}/health" >/dev/null
}

cleanup() {
    local status=$?
    trap - EXIT
    if (( status != 0 && cutover )); then
        if [[ -n $previous ]]; then
            printf 'Trying the previous release. Any applied database migrations remain in place.\n' >&2
            if ! compose "$previous" up -d --no-build --remove-orphans || ! check_dashboard; then
                printf 'Previous release could not be verified. Review the stack and backups in %s.\n' "$state" >&2
            fi
        else
            compose "$release" stop || true
        fi
    fi
    if [[ -n $stage ]]; then rm -rf -- "$stage"; fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if [[ ! -d $release ]]; then
    stage=$(mktemp -d "$state/releases/.stage.XXXXXXXX")
    mkdir -- "$stage/backend" "$stage/frontend"
    (
        umask 022
        tar --no-same-owner --no-same-permissions -xf "$bundle/backend.tar" -C "$stage/backend"
        tar --no-same-owner --no-same-permissions -xf "$bundle/frontend.tar" -C "$stage/frontend"
    )
    [[ -f $stage/backend/docker-compose.yml && -f $stage/backend/Dockerfile &&
        -f $stage/frontend/Dockerfile ]] || die 'The archives do not contain the backend and frontend sources.'
    install -m 0600 "$bundle/compose.ssh.yml" "$stage/compose.ssh.yml"
    mv -T -- "$stage" "$release"
    stage=
fi

if [[ ! -f $state/.env ]]; then
    password=$(openssl rand -hex 32)
    printf 'POSTGRES_USER=dashboard\nPOSTGRES_DB=dashboard\nPOSTGRES_PASSWORD=%s\nDATABASE_URL=postgresql://dashboard:%s@db:5432/dashboard\nCOORDINATOR_ACCOUNTS_ENABLED=false\nLLM_ENABLED=false\n' \
        "$password" "$password" > "$state/.env.pending"
    mv -T -- "$state/.env.pending" "$state/.env"
    unset password
fi
if [[ ! -e $state/data/character_problems.json ]]; then
    install -m 0644 "$release/backend/data/character_problems.json" "$state/data/character_problems.json.pending"
    mv -T -- "$state/data/character_problems.json.pending" "$state/data/character_problems.json"
fi

export BUILDX_CONFIG=$state/buildx
builder_node=${project}0
if ! docker buildx inspect "$project" >/dev/null 2>&1; then
    if docker container inspect "buildx_buildkit_$builder_node" >/dev/null 2>&1 ||
        docker volume inspect "buildx_buildkit_${builder_node}_state" >/dev/null 2>&1; then
        die "Existing BuildKit resources for $project have no installation-scoped builder configuration."
    fi
    docker buildx create --name "$project" --node "$builder_node" --driver docker-container "$DOCKER_HOST"
fi

build_image() {
    local image=$1 revision=$2 context=$3
    shift 3
    local label
    if label=$(docker image inspect --format "{{index .Config.Labels \"$image_label\"}}" "$image" 2>/dev/null); then
        [[ $label == ssh ]] || die "Refusing to replace an existing Docker image with the tag $image."
    else
        docker buildx build --builder "$project" --load \
            --label "$image_label=ssh" --label "org.opencontainers.image.revision=$revision" \
            --tag "$image" "$@" "$context"
    fi
}

build_image "underctrl-ssh-backend:${revisions[0]}" "${revisions[0]}" "$release/backend" --target runtime
build_image "underctrl-ssh-frontend:${revisions[1]}" "${revisions[1]}" "$release/frontend"

if [[ -f $bundle/qwen-remote.sh ]]; then
    [[ ! -L $bundle/qwen-remote.sh && -f $bundle/compose.qwen.yml && ! -L $bundle/compose.qwen.yml ]] ||
        die 'The Qwen upload is missing its Compose override.'
    qwen_url=$(bash "$bundle/qwen-remote.sh" setup)
    if [[ -n $qwen_url ]]; then
        [[ $qwen_url =~ ^http://([0-9]{1,3}\.){3}[0-9]{1,3}:18080/v1$ ]] || die 'The Qwen proxy returned an unexpected URL.'
        install -m 0600 "$bundle/compose.qwen.yml" "$state/qwen.compose.yml.pending"
        mv -T -- "$state/qwen.compose.yml.pending" "$state/qwen.compose.yml"
    fi
fi

compose "$release" config --quiet

if [[ -n $previous && $release_id != "$previous_id" ]]; then
    backup=$(mktemp "$state/backups/postgres.XXXXXXXX")
    if compose "$previous" up -d --no-build --wait --wait-timeout 90 db &&
        compose "$previous" exec -T db sh -c 'exec pg_dump -Fc -U "$POSTGRES_USER" "$POSTGRES_DB"' > "$backup" &&
        compose "$previous" exec -T db pg_restore --list < "$backup" >/dev/null; then
        mv -- "$backup" "$backup.dump"
    else
        rm -f -- "$backup"
        die 'Could not create and verify a PostgreSQL backup before changing releases.'
    fi
fi

cutover=1
compose "$release" up -d --no-build --remove-orphans
if [[ -n $qwen_url ]] && ! check_qwen_proxy; then
    printf '%s\n' 'Qwen proxy is unreachable from the worker. Using the dashboard rules.' >&2
    qwen_url=
    compose "$release" up -d --no-build --remove-orphans
    if ! bash "$bundle/qwen-remote.sh" remove; then
        printf '%s\n' 'The dormant project Qwen proxy could not be cleaned up.' >&2
    fi
fi
if ! check_dashboard; then
    compose "$release" ps --all >&2
    die 'The dashboard or its API did not become ready.'
fi
printf '%s\n' "$release_id" > "$state/current.pending"
mv -T -- "$state/current.pending" "$state/current"
cutover=0
printf 'UnderCtrl is ready on server loopback at http://127.0.0.1:8005/.\n'
