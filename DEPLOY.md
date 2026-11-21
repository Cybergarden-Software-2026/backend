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

Python, Rust и PostgreSQL на сервер ставить не нужно. Python 3 нужен только для `tools/demo.py`
(сценарий демонстрации), если его запускать на сервере.

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
