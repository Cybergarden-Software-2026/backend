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

### Личные учётки координаторов на vserver

1. Выложите backend с `COORDINATOR_ACCOUNTS_ENABLED=false`, затем frontend с выбором способа входа. Установите обновлённую службу автодеплоя из слитого коммита по инструкции в следующем разделе. С установкой станет доступен скрипт подготовки учёток.
2. После применения миграции `0006` запустите на vserver команду под своей учётной записью администратора. Скрипт сам использует `sudo` для Docker.

   ```bash
   /usr/local/sbin/underctrl-create-test-coordinators
   ```

   Скрипт создаёт `test-coordinator-1`, `test-coordinator-2` и `test-coordinator-3` и печатает путь к JSON с логинами, паролями и отдельными ключами. По умолчанию файл появляется в `$HOME/.local/state/underctrl/test-coordinators.json` с правами `0600`. Можно передать другой путь первым аргументом. Скрипт сохраняет существующие учётки и файлы с ранее выданными данными.
3. Откройте `sudoedit /home/cyber/cybergarden/backend/.env`. Установите `COORDINATOR_ACCOUNTS_ENABLED=true`. В `CORS_ORIGINS` добавьте `https://cybergarden.sosmooth.ru`, сохранив остальные нужные адреса. Режим личных учёток проверяет личные пароли и ключи.
4. Пересоздайте `api`, чтобы Docker применил `.env`. Для установленного автодеплоя используйте блок ниже. При ручном управлении используйте функцию `dc` из предыдущего раздела и команду `dc up -d --no-build --no-deps --force-recreate api`.

```bash
sudo flock -n /run/lock/underctrl-backend-deploy.lock bash -euo pipefail <<'RECREATE_API'
repo=/home/cyber/cybergarden/backend
config=/etc/underctrl-backend-deploy
api_container=$(docker ps -q --filter label=com.docker.compose.project=mts-incident-dashboard \
  --filter label=com.docker.compose.service=api)
test -n "$api_container"
api_image=$(docker container inspect --format '{{.Config.Image}}' "$api_container")
UNDERCTRL_BACKEND_IMAGE="$api_image" docker compose \
  --project-name mts-incident-dashboard --project-directory "$repo" --env-file "$repo/.env" \
  -f "$config/docker-compose.yml" -f "$config/compose.vserver.yml" -f "$config/compose.image.yml" \
  up -d --no-build --no-deps --force-recreate api
RECREATE_API
```

После запуска откройте сайт по HTTPS и проверьте ответы без браузерной сессии.

```bash
curl -fsS https://cybergarden.sosmooth.ru/api/v1/auth/coordinator/session
curl -sS -o /dev/null -w '%{http_code}\n' 'https://cybergarden.sosmooth.ru/api/v1/snapshot?view=web'
```

Первый запрос должен вернуть `required: true` и `mode: "accounts"`, второй — `401`. На сайте выберите «Логин и пароль» или вход по личному ключу и используйте данные одной из тестовых учёток. Фронтенд использует существующий прокси `/api/v1`. Cookie привязана к HTTPS-хосту, через который координатор выполнил вход.

Для новой рабочей учётки оператор может запустить команду `api coordinator create LOGIN [DISPLAY_NAME]` внутри контейнера `api` через `sudo docker exec`. Сохраните стандартный вывод в новый файл с `umask 077` и включённым `set -o noclobber`. Для отключения учётки и отзыва её сессий используйте `api coordinator disable LOGIN` через тот же контейнер.

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

При изменении конфигурации Compose или скриптов автодеплоя повторите установку файлов
из проверенного коммита `main`. Автоматический цикл использует параметры
Compose, установленные владельцем сервера в `/etc/underctrl-backend-deploy`.

## ИИ-анализ: ai-worker и локальная Qwen

`ai-worker` заранее готовит выжимку и подсказку команды для «Помощь ИИ» и
проверяет автоматические группы массовых сбоев (название группы от модели): берёт
задачи из `analysis_jobs`, вызывает `llama-server` по OpenAI-совместимому
`/v1/chat/completions` и сохраняет проверенный результат. `/analysis` отдаёт
его с `provider: "llm"`, а без готового результата — разбор по правилам.
API запрос модели не ждёт, остановка worker или модели на API не влияет.

Сеть: worker подключён к `default` (PostgreSQL) и к внутренней сети
`qwen`; API, импортёр, БД и веб-фронтенд в неё не входят. Хостовый
`llama-server` слушает `127.0.0.1:8080`, в сеть `qwen` его выводит
отдельный systemd-проброс на шлюз этой сети. Реальный адрес модели в образ и
репозиторий не попадает.

**Однократная настройка vserver (до установки нового Compose).** Без сети
`qwen` `compose.vserver.yml` не поднимется.

1. Сеть и шлюз:

   ```bash
   sudo docker network create --driver bridge --internal \
     --opt com.docker.network.bridge.name=br-uc-qwen qwen
   sudo docker network inspect -f '{{(index .IPAM.Config 0).Gateway}}' qwen
   ```

2. Проброс `underctrl-qwen-worker.socket`/`.service` по образцу
   `underctrl-qwen-bridge.*`: `ListenStream=<шлюз>:18080`,
   `BindToDevice=br-uc-qwen`, получатель `systemd-socket-proxyd` —
   `127.0.0.1:8080`; затем `systemd-analyze verify`, `systemctl daemon-reload`,
   `systemctl enable --now underctrl-qwen-worker.socket`.

3. Параметры только для worker — `/etc/underctrl-backend-deploy/ai-worker.env`
   (права `0600`, владелец `root`):

   ```dotenv
   LLM_ENABLED=true
   LLM_BASE_URL=http://<шлюз qwen>:18080/v1
   LLM_MODEL=qwen-local
   # первые 8 символов SHA-256 загруженного GGUF; при замене весов — обновить
   LLM_MODEL_REVISION=
   ```

4. Включить постановку задач и выдачу результатов: в
   `~/cybergarden/backend/.env` добавить `LLM_ENABLED=true` и те же
   `LLM_MODEL`/`LLM_MODEL_REVISION` (адрес модели туда не добавлять).
   Значения должны совпадать у API, импортёра и worker, иначе API не найдёт
   готовые результаты.

5. Обновить установленные файлы деплоя штатным установщиком (раздел выше).
   Следующий запуск таймера поднимет `ai-worker` из того же образа, что API
   и импортёр. При первом старте worker сам ставит задачи для всех текущих
   карточек; дальше их ставит импортёр при изменении полей инцидента.

Проверка: `sudo docker compose -p mts-incident-dashboard logs ai-worker`
показывает идентификатор задачи, длительность и результат (без текста
инцидентов). Состояние очереди:

```bash
sudo docker compose -p mts-incident-dashboard exec -T db sh -c \
  'psql -U "$POSTGRES_USER" -d "$POSTGRES_DB" -c "SELECT status, count(*) FROM analysis_jobs GROUP BY 1"'
```

Запрос к модели из сетевого пространства worker и отключение старого
проброса `underctrl-qwen-bridge` — по шагам из `QWEN_BACKEND_HANDOFF.md`
(раздел «Отдельная сеть для worker»). Откат: при провале проверок
автодеплой возвращает прежние API и импортёр; если прежний образ ещё без
`ai-worker`, worker останавливается.

Локально и в CI `LLM_ENABLED=false`: worker простаивает, тесты используют
имитацию модели и не обращаются к vserver.
