#!/usr/bin/env bash
set -euo pipefail

source_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
live_repo=/home/cyber/cybergarden/backend
if [[ ! -s "$live_repo/.env" ]]; then
  echo 'Start the manual Compose deployment in /home/cyber/cybergarden/backend before installing.' >&2
  exit 1
fi

exec 9>/run/lock/underctrl-backend-deploy.lock
flock -n 9 || { echo 'A backend deployment is running. Try again afterwards.' >&2; exit 1; }

config=/etc/underctrl-backend-deploy
install -d -m 0755 "$config"
install -m 0644 "$source_dir/docker-compose.yml" "$source_dir/compose.vserver.yml" \
  "$source_dir/deploy/compose.image.yml" "$source_dir/deploy/check-backend.py" "$config/"
install -m 0755 "$source_dir/deploy/deploy-backend.sh" /usr/local/sbin/underctrl-backend-deploy
install -m 0644 "$source_dir/deploy/underctrl-backend-deploy.service" \
  "$source_dir/deploy/underctrl-backend-deploy.timer" /etc/systemd/system/
systemctl daemon-reload
