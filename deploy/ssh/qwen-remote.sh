#!/usr/bin/env bash
# Called under the project lock by deploy-remote.sh and undeploy-remote.sh.
set -euo pipefail

die() { printf '%s\n' "$*" >&2; exit 1; }

[[ $# == 1 && ( $1 == setup || $1 == remove ) && $EUID == 0 ]] ||
  die 'Usage as root: qwen-remote.sh setup|remove'

state=/var/lib/underctrl-ssh
marker=$state/.managed
unit=underctrl-ssh-qwen
unit_dir=/etc/systemd/system
unit_header='# Managed by underctrl-ssh-v1'
network=$unit
bridge=br-uc-sshlm
model_endpoint=http://127.0.0.1:8080
unset DOCKER_CONTEXT
export DOCKER_HOST=unix:///var/run/docker.sock

[[ -d "$state" && ! -L "$state" && -f "$marker" && ! -L "$marker" &&
   $(stat -c %u "$state") == 0 && $(stat -c %u "$marker") == 0 &&
   $(cat "$marker") == underctrl-ssh-v1 ]] || die "Invalid installation marker: $marker"

qwen_ready() {
  command -v python3 >/dev/null &&
    curl --noproxy '*' -fsS --max-time 3 "$model_endpoint/health" >/dev/null &&
    curl --noproxy '*' -fsS --max-time 3 "$model_endpoint/v1/models" |
    python3 -c 'import json, sys; sys.exit(not any(item["id"] == "qwen-local" for item in json.load(sys.stdin)["data"]))'
}

inspect_network() {
  docker network inspect --format "$1" "$network"
}

check_network() {
  local identity
  identity=$(inspect_network '{{.Driver}} {{.Internal}} {{index .Options "com.docker.network.bridge.name"}} {{index .Labels "io.underctrl.installation"}}')
  [[ "$identity" == "bridge true $bridge ssh" ]] || die "Unexpected Docker network: $network"
}

check_units() {
  local ext path fragment drop_ins
  for ext in socket service; do
    path=$unit_dir/$unit.$ext
    if [[ -e "$path" || -L "$path" ]]; then
      [[ -f "$path" && ! -L "$path" && $(head -n 1 "$path") == "$unit_header" ]] ||
        die "Unexpected systemd unit file: $path"
    fi
    fragment=$(systemctl show -p FragmentPath --value "$unit.$ext")
    drop_ins=$(systemctl show -p DropInPaths --value "$unit.$ext")
    [[ -z "$fragment" || "$fragment" == "$path" ]] || die "Unexpected systemd unit: $fragment"
    [[ -z "$drop_ins" ]] || die "Unexpected systemd drop-ins: $drop_ins"
  done
}

stop_units() {
  if [[ -f "$unit_dir/$unit.socket" ]]; then
    systemctl stop "$unit.socket" >&2
  fi
  if [[ -f "$unit_dir/$unit.service" ]]; then
    systemctl stop "$unit.service" >&2
  fi
}

# Bring up the bridge only when the existing host API is ready.
if [[ $1 == setup ]] && ! qwen_ready 2>/dev/null; then
  printf '%s\n' 'Qwen недоступна на 127.0.0.1:8080 под именем qwen-local. Будут использованы правила приложения.' >&2
  exit 0
fi

networks=$(docker network ls --format '{{.Name}}')
network_exists=false
if grep -Fxq -- "$network" <<< "$networks"; then
  check_network
  network_exists=true
fi
check_units

if [[ $1 == remove ]]; then
  if $network_exists && [[ $(inspect_network '{{len .Containers}}') != 0 ]]; then
    die "Docker network $network still has connected containers."
  fi
  if [[ -f "$unit_dir/$unit.socket" ]]; then
    systemctl disable --now "$unit.socket" >&2
  fi
  if [[ -f "$unit_dir/$unit.service" ]]; then
    systemctl stop "$unit.service" >&2
  fi
  if [[ -f "$unit_dir/$unit.socket" || -f "$unit_dir/$unit.service" ]]; then
    rm -f -- "$unit_dir/$unit.socket" "$unit_dir/$unit.service"
    systemctl daemon-reload
  fi
  if $network_exists; then
    docker network rm "$network" >&2
  fi
  exit 0
fi

proxy=/usr/lib/systemd/systemd-socket-proxyd
[[ -x "$proxy" ]] || proxy=/lib/systemd/systemd-socket-proxyd
[[ -x "$proxy" ]] || die 'systemd-socket-proxyd is unavailable.'

if ! $network_exists; then
  docker network create --driver bridge --internal \
    --opt "com.docker.network.bridge.name=$bridge" \
    --label io.underctrl.installation=ssh "$network" >&2
  check_network
fi

gateway=$(inspect_network '{{(index .IPAM.Config 0).Gateway}}')
[[ "$gateway" =~ ^([0-9]{1,3}\.){3}[0-9]{1,3}$ ]] || die "No IPv4 gateway in network $network."

tmpdir=$(mktemp -d)
trap 'rm -f -- "$tmpdir/$unit.socket" "$tmpdir/$unit.service"; rmdir -- "$tmpdir"' EXIT
cat >"$tmpdir/$unit.socket" <<EOF
$unit_header
[Unit]
Description=UnderCtrl private Qwen listener
DefaultDependencies=no
Wants=docker.service
After=docker.service
Before=shutdown.target
Conflicts=shutdown.target

[Socket]
ListenStream=$gateway:18080
BindToDevice=$bridge
FreeBind=yes

[Install]
WantedBy=multi-user.target
EOF
cat >"$tmpdir/$unit.service" <<EOF
$unit_header
[Unit]
Description=UnderCtrl proxy to the host Qwen API
Requires=$unit.socket
After=$unit.socket

[Service]
ExecStart=$proxy 127.0.0.1:8080
DynamicUser=yes
NoNewPrivileges=yes
EOF
systemd-analyze verify "$tmpdir/$unit.socket" "$tmpdir/$unit.service" >&2
if ! cmp -s "$tmpdir/$unit.socket" "$unit_dir/$unit.socket" ||
   ! cmp -s "$tmpdir/$unit.service" "$unit_dir/$unit.service"; then
  stop_units
  install -m 0644 "$tmpdir/$unit.socket" "$tmpdir/$unit.service" "$unit_dir/"
  systemctl daemon-reload
fi
systemctl enable --now "$unit.socket" >&2
printf 'http://%s:18080/v1\n' "$gateway"
