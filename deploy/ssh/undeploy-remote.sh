#!/usr/bin/env bash
set -euo pipefail
umask 077

die() { printf '%s\n' "$*" >&2; exit 1; }

[[ $# == 0 && $EUID == 0 ]] || die 'Run undeploy-remote.sh as root without arguments.'

project=underctrl-ssh
state=/var/lib/underctrl-ssh
marker=$state/.managed
builder_config=$state/buildx
builder_container=buildx_buildkit_${project}0
builder_cache=${builder_container}_state
qwen_helper=$(dirname -- "$0")/qwen-remote.sh
compose_label="label=com.docker.compose.project=$project"
installation_label='label=io.underctrl.installation=ssh'

exec 9>/run/underctrl-ssh.lock
flock -n 9 || die 'Another UnderCtrl deployment or removal is in progress.'

unset DOCKER_CONTEXT DOCKER_HOST DOCKER_TLS_VERIFY DOCKER_CERT_PATH
docker_local() { docker --host unix:///var/run/docker.sock "$@"; }

list_builder_resources() {
    docker_local container ls --all --format '{{.Names}}' |
        awk -v name="$builder_container" '$0 == name' &&
    docker_local volume ls --format '{{.Name}}' |
        awk -v name="$builder_cache" '$0 == name'
}

remove_list() {
    local list=$1 entry
    local -a resources=()
    shift
    [[ -n $list ]] || return 0
    while IFS= read -r entry; do
        resources+=("$entry")
    done <<<"$list"
    docker_local "$@" "${resources[@]}"
}

managed=false
if [[ -e $state || -L $state ]]; then
    [[ -d $state && ! -L $state && $(stat -c '%u:%a' -- "$state") == 0:700 &&
       -f $marker && ! -L $marker && $(stat -c '%u:%a' -- "$marker") == 0:600 ]] &&
      cmp -s -- "$marker" <(printf '%s\n' 'underctrl-ssh-v1') ||
      die "Cannot verify ownership of $state. Review the installation before removing it."
    managed=true
fi

docker_local info >/dev/null
containers=$(docker_local container ls --all --quiet --filter "$compose_label")
networks=$(docker_local network ls --quiet --filter "$compose_label")
volumes=$(docker_local volume ls --quiet --filter "$compose_label")
images=$(docker_local image ls --filter "$installation_label" --format '{{.Repository}}:{{.Tag}}' |
    awk -F: '$1 ~ /^underctrl-ssh-(backend|frontend)$/ && $2 ~ /^[0-9a-f]+$/ && length($2) == 40')
builder_resources=$(list_builder_resources)

if [[ $managed == false ]]; then
    qwen_networks=$(docker_local network ls --filter "$installation_label" --format '{{.Name}}' |
        awk '$0 == "underctrl-ssh-qwen"')
    if [[ -n $containers$networks$volumes$images$qwen_networks$builder_resources ||
          -e /etc/systemd/system/underctrl-ssh-qwen.socket ||
          -L /etc/systemd/system/underctrl-ssh-qwen.socket ||
          -e /etc/systemd/system/underctrl-ssh-qwen.service ||
          -L /etc/systemd/system/underctrl-ssh-qwen.service ]]; then
        die "Found UnderCtrl resources without the ownership marker $marker. Review them manually."
    fi
    legacy=$(docker_local container ls --all --quiet --filter 'label=com.docker.compose.project=mts-incident-dashboard')
    if [[ -n $legacy || -e /home/cyber/cybergarden || -e /etc/underctrl-backend-deploy ||
          -e /etc/systemd/system/underctrl-backend-deploy.timer ]]; then
        die 'Found a legacy UnderCtrl installation. Use its existing decommissioning procedure.'
    fi
    printf '%s\n' 'No managed UnderCtrl SSH installation found.'
    exit 0
fi

[[ -f $qwen_helper ]] || die "Missing the removal helper $qwen_helper."
mounts=$(findmnt --raw --noheadings --output TARGET |
    awk -v state="$state" '$0 == state || index($0, state "/") == 1')
[[ -z $mounts ]] || die "Unmount the filesystems under $state before removing the installation."

builder_managed=false
if [[ -e $builder_config || -L $builder_config ]]; then
    [[ -d $builder_config && ! -L $builder_config && -O $builder_config ]] ||
      die "Cannot verify ownership of $builder_config."
    if BUILDX_CONFIG=$builder_config docker_local buildx inspect "$project" >/dev/null 2>&1; then
        builder_managed=true
    fi
fi
[[ -z $builder_resources || $builder_managed == true ]] ||
  die 'Existing BuildKit resources have no installation-scoped builder configuration.'

remove_list "$containers" container rm --force --volumes
bash "$qwen_helper" remove
remove_list "$networks" network rm
remove_list "$volumes" volume rm
remove_list "$images" image rm
if [[ $builder_managed == true ]]; then
    BUILDX_CONFIG=$builder_config docker_local buildx rm "$project"
    builder_resources=$(list_builder_resources)
    [[ -z $builder_resources ]] || die 'Buildx did not remove its builder container or cache volume.'
fi

rm -rf --one-file-system -- "$state"
printf '%s\n' 'UnderCtrl SSH installation removed.'
