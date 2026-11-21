# Развёртывание на сервере

Всё работает в Docker: PostgreSQL и бинарники backend — в контейнерах, на сервер их ставить не нужно.

## 1. Сервер

| Параметр | Требование |
| --- | --- |
| ОС | Linux с Docker (Ubuntu 22.04/24.04, Debian 12/13) |
| Архитектура | x86_64 или arm64 |
| CPU | от 2 vCPU |
| RAM | 1 ГБ для работы; **3–4 ГБ для сборки образа** (release-сборка Rust) — или swap, или сборка на другой машине |
| Диск | 10–15 ГБ (слои сборки — несколько ГБ, чистятся `docker builder prune`) |
| Сеть | исходящий доступ к Docker Hub, gcr.io, crates.io, static.rust-lang.org (для сборки) |

Образ лучше собирать на самом сервере: собранный на Mac (arm64) не запустится на x86-сервере без
`docker buildx build --platform linux/amd64`.

## 2. Что установить

### Обязательно

| ПО | Версия | Зачем |
| --- | --- | --- |
| Docker Engine | ≥ 23 | контейнеры; BuildKit по умолчанию |
| Docker Compose (плагин `docker compose`) | ≥ 2.20 | `service_completed_successfully`, `healthcheck: disable` |
| git | любая | забрать репозиторий |

```bash
curl -fsSL https://get.docker.com | sh
sudo usermod -aG docker $USER         # затем перелогиниться
sudo systemctl enable --now docker    # автозапуск Docker после перезагрузки
docker version && docker compose version
```

### Если API доступен из интернета

| ПО | Зачем |
| --- | --- |
| Caddy или nginx | обратный прокси и HTTPS (Let's Encrypt) |
| ufw (или другой файрвол) | открыть только 22, 80, 443 |

Для SSE (`/api/v1/events/stream`) у nginx: `proxy_buffering off;`, `proxy_http_version 1.1;`,
`proxy_read_timeout` больше `SSE_HEARTBEAT_SECONDS` (по умолчанию 15 с). Заголовок
`X-Accel-Buffering: no` API уже отдаёт.

```bash
sudo ufw allow 22,80,443/tcp && sudo ufw enable
```

Rust и PostgreSQL на сервер ставить не нужно. Python 3 нужен для проверки снимка при автодеплое и для `tools/demo.py`, если запускать сценарий демонстрации на сервере.

## 3. Настройка перед первым запуском

```bash
git clone <репозиторий> backend && cd backend
cp .env.example .env
```

1. **`.env`**: задать `POSTGRES_PASSWORD` и тот же пароль в `DATABASE_URL`; при необходимости —
   `CORS_ORIGINS` (адрес фронтенда, если он ходит в API не через общий прокси), `ITSM_BASE_URL`,
   `LOG_LEVEL`.
2. **Порт API** в `docker-compose.yml`: за обратным прокси на том же сервере — `"127.0.0.1:8080:8080"`.
   PostgreSQL наружу не публикуется.
3. **Файл выгрузки** `data/character_problems.json`:
   * определить, кто и как его обновляет (cron, scp, выгрузка из ITSM);
   * публиковать атомарно — во временный файл в той же папке, затем `rename` (как `tools/demo.py`);
   * права `644` (`chmod -R a+rX data`): контейнеры работают от uid 65532.

## 4. Запуск

```bash
docker compose up -d --build
docker compose ps                       # api — healthy, migrate — Exited (0)
curl -s localhost:8080/health/ready
docker compose logs -f importer         # «Снимок принят: revision=1 …»
```

## 5. Эксплуатация

| Задача | Команда |
| --- | --- |
| обновить | `git pull && docker compose up -d --build` |
| логи | `docker compose logs -f api importer` |
| бэкап базы | `docker compose exec -T db pg_dump -U dashboard dashboard > backup.sql` |
| восстановление | `docker compose exec -T db psql -U dashboard dashboard < backup.sql` |
| освободить диск | `docker image prune && docker builder prune` |

Данные PostgreSQL хранятся в томе `pgdata`: `docker compose down` их не удаляет, `down -v` — удаляет.
Пароль из `.env` применяется только при первом создании тома; потом — `ALTER USER` в PostgreSQL
или пересоздание тома.

ИИ-модуль в этот compose не входит и разворачивается отдельно.

## Запуск на vserver

Для этого варианта используйте Docker Engine 28 или новее и Docker Compose
2.24.4 или новее. Из корня репозитория с настроенным `.env` выполните:

```bash
dc() { sudo docker compose -f docker-compose.yml -f compose.vserver.yml "$@"; }
dc config --quiet
dc build api
dc up -d --no-build
dc ps --all
curl --noproxy '*' -fsS http://127.0.0.1:8001/health/ready
```

API слушает порт `8080` внутри контейнера и доступен хостовому Nginx на
`127.0.0.1:8001`. PostgreSQL доступен сервисам через сеть Compose. Хостовый
`127.0.0.1:8080` используется Qwen. В выводе `dc ps --all` ожидаются
`Exited (0)` для миграции и `healthy` для API после запуска. Готовность
проверяется ответом `"database":true` на `/health/ready`.

### Вход координатора на vserver

1. Обновите backend, пока `COORDINATOR_ACCESS_KEY` не задан, затем обновите frontend. До включения ключа `GET /api/v1/auth/coordinator/session` возвращает `required: false`.
2. Переустановите backend-службу автодеплоя по инструкции ниже. Обновлённая служба проверяет снимок через настоящую сессию, когда вход включён.
3. Сгенерируйте ключ командой `openssl rand -hex 32` на стороне администратора и задайте его в `~/cybergarden/backend/.env` как `COORDINATOR_ACCESS_KEY`. Добавьте `https://cybergarden.sosmooth.ru` в список `CORS_ORIGINS`, сохранив нужные адреса. Значение ключа передавайте координаторам отдельно.
4. Пересоздайте контейнер `api` с обновлённым `.env`. Если установлен автодеплой, выполните команды ниже из `~/cybergarden/backend`. Для ручного запуска используйте `dc up -d --no-build --no-deps --force-recreate api` из раздела выше. Откройте сайт по HTTPS. API покажет `required: true`, а запрос к снимку без сессии получит `401`.

```bash
sudo flock -n /run/lock/underctrl-backend-deploy.lock bash -euo pipefail <<'RECREATE_API'
api_container=$(docker ps -q --filter label=com.docker.compose.project=mts-incident-dashboard \
  --filter label=com.docker.compose.service=api)
test -n "$api_container"
api_image=$(docker container inspect --format '{{.Config.Image}}' "$api_container")
UNDERCTRL_BACKEND_IMAGE="$api_image" docker compose \
  --project-name mts-incident-dashboard --project-directory "$PWD" --env-file .env \
  -f /etc/underctrl-backend-deploy/docker-compose.yml \
  -f /etc/underctrl-backend-deploy/compose.vserver.yml \
  -f /etc/underctrl-backend-deploy/compose.image.yml \
  up -d --no-build --no-deps --force-recreate api
RECREATE_API
```

Фронтенд использует существующий прокси `/api/v1`. Nginx передаёт `Cookie` и `Set-Cookie` в стандартной конфигурации. Cookie входа привязана к HTTPS-хосту, через который координатор выполнил вход. Смена ключа и пересоздание `api` завершают ранее выданные сессии.

## Автодеплой из main на vserver

GitHub Actions проверяет PR в `main` и push в `main`. На сервере systemd раз
в минуту проверяет `main` существующим ключом чтения пользователя `cyber`.
Сервер архивирует точный коммит, прогоняет Rust-тесты в отдельном контейнере
с временной PostgreSQL и собирает runtime-образ только после успешных тестов.

Служба использует существующий проект `mts-incident-dashboard`, его том
`pgdata`, параметры из `~/cybergarden/backend/.env` и импортируемый каталог
`~/cybergarden/backend/data`. Перед миграцией сохраняется дамп в
`/var/lib/underctrl-backend-deploy/backups/`. После замены API и импортёра
служба проверяет готовность и снимок на `127.0.0.1:8001`. Если проверка
провалилась, она пробует вернуть предыдущие образы. Для такого отката
миграция должна быть совместима с предыдущей версией приложения.

После слияния автодеплоя в `main` выполните под `cyber`. Команды забирают
файлы установки во временный каталог, сохраняя рабочий каталог с выгрузкой.

```bash
(
  set -euo pipefail
  repo="$HOME/cybergarden/backend"
  git -C "$repo" fetch origin
  files=$(mktemp -d)
  trap 'rm -rf -- "$files"' EXIT
  git -C "$repo" archive origin/main deploy docker-compose.yml compose.vserver.yml |
    tar -xf - -C "$files"
  sudo "$files/deploy/install-backend-deploy.sh"
  sudo systemctl start underctrl-backend-deploy.service
  sudo systemctl enable --now underctrl-backend-deploy.timer
)
```

Журнал первого и последующих запусков:

```bash
sudo journalctl -u underctrl-backend-deploy.service -n 80 --no-pager
```

При изменении самой конфигурации Compose повторите установку файлов
из проверенного коммита `main`. Автоматический цикл использует параметры
Compose, установленные владельцем сервера в `/etc/underctrl-backend-deploy`.
