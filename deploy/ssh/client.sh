#!/usr/bin/env bash
set -euo pipefail
umask 077

die() { printf '%s\n' "$*" >&2; exit 1; }

action=${1:?missing action}
shift
case "$action" in
  deploy|undeploy) ;;
  *) die "Unknown action: $action" ;;
esac
if [[ ${1:-} == --help || ${1:-} == -h ]]; then
  printf 'Использование: ./%s.sh [user@host | ssh://user@host:port]\n' "$action"
  exit 0
fi
(( $# <= 1 )) || die "Использование: ./$action.sh [SSH-адрес]"
target=${1:-}
if [[ -z "$target" ]]; then
  read -r -p 'SSH-адрес (user@host или ssh://user@host:port): ' target
fi
[[ -n "$target" && "$target" != -* && "$target" != *[[:space:]]* ]] || die 'Некорректный SSH-адрес.'

helpers=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
backend=$(cd -- "$helpers/../.." && pwd -P)
work=$(mktemp -d /tmp/underctrl-ssh.XXXXXXXX)
control=$work/control
transfer=
session=(-S "$control" -o ControlMaster=no -o BatchMode=yes -o ConnectTimeout=5)
cleanup() {
  if [[ -n "$transfer" && -S "$control" ]]; then
    ssh "${session[@]}" -T -- "$target" "rm -rf -- '$transfer'" >/dev/null 2>&1 || true
  fi
  if [[ -S "$control" ]]; then
    ssh -S "$control" -O exit -- "$target" >/dev/null 2>&1 || true
  fi
  rm -rf -- "$work"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -- "$work/bundle"
cp -- "$helpers/$action-remote.sh" "$helpers/qwen-remote.sh" "$work/bundle/"
if [[ "$action" == deploy ]]; then
  cp -- "$helpers/compose.ssh.yml" "$helpers/compose.qwen.yml" "$work/bundle/"
  # Linked worktrees can be elsewhere on disk. Look next to the main backend clone.
  backend_common=$(git -C "$backend" rev-parse --path-format=absolute --git-common-dir)
  frontend=${UNDERCTRL_FRONTEND_REPO:-$(dirname -- "$(dirname -- "$backend_common")")/frontend-web}
  if [[ ! -e "$frontend" && -z ${UNDERCTRL_FRONTEND_REPO:-} ]]; then
    frontend=$work/frontend-web
    git clone --quiet --depth 1 --single-branch --branch main \
      git@github.com:Cybergarden-Software-2026/frontend-web.git "$frontend"
  fi
  backend_revision=$(git -C "$backend" rev-parse --verify 'HEAD^{commit}')
  frontend_revision=$(git -C "$frontend" rev-parse --verify 'refs/remotes/origin/main^{commit}')
  git -C "$backend" archive --format=tar "$backend_revision" > "$work/bundle/backend.tar"
  git -C "$frontend" archive --format=tar "$frontend_revision" > "$work/bundle/frontend.tar"
  printf '%s\n%s\n' "$backend_revision" "$frontend_revision" > "$work/bundle/revisions"
  printf 'Развёртывание на %s\nbackend %s\nfrontend-web %s\n' "$target" "$backend_revision" "$frontend_revision"
else
  printf 'Удаление UnderCtrl со всеми данными проекта на %s\n' "$target"
fi

ssh -M -N -f -S "$control" -o ControlPersist=60 \
  -o ServerAliveInterval=15 -o ServerAliveCountMax=3 -- "$target"
uploaded=$(ssh "${session[@]}" -T -- "$target" 'umask 077; mktemp -d /tmp/underctrl-ssh-upload.XXXXXXXX')
[[ "$uploaded" =~ ^/tmp/underctrl-ssh-upload\.[A-Za-z0-9]{8}$ ]] || die 'Сервер вернул неожиданный временный путь.'
transfer=$uploaded

COPYFILE_DISABLE=1 tar -C "$work/bundle" -cf - . |
  ssh "${session[@]}" -T -- "$target" "umask 077; tar -xf - -C '$transfer'"

# SSH handles credentials itself. A terminal is also required if sudo asks for a password.
runner="'$transfer/$action-remote.sh'"
if [[ "$action" == deploy ]]; then
  runner+=" '$transfer'"
fi
ssh "${session[@]}" -tt -- "$target" \
  "if [ \"\$(id -u)\" -eq 0 ]; then exec /bin/bash $runner; else exec sudo -- /bin/bash $runner; fi"

if [[ "$action" == deploy ]]; then
  printf '\nЧтобы открыть дашборд, запустите в отдельном терминале:\n'
  printf 'ssh -N -o ExitOnForwardFailure=yes -L 8005:127.0.0.1:8005 -- %q\n' "$target"
  printf 'Затем откройте http://127.0.0.1:8005/\n'
fi
