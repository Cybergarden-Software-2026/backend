# Backend дашборда координатора ИТ-инцидентов (Rust)

HTTP API и импортёр для дашборда координатора массовых инцидентов: импортёр следит за JSON-выгрузкой
инцидентов, сохраняет снимки в PostgreSQL, считает группы, SLA и уведомления; API отдаёт снимок,
списки и журнал событий (REST) и обновления в реальном времени (SSE).

Стек: axum + tokio, sqlx (PostgreSQL 16), serde. Портирован с Python-версии (FastAPI) с полным
паритетом ответов — история и решения в [`docs/PORTING.md`](docs/PORTING.md). Контракт для фронтенда —
API.md (маршруты `/api/v1/snapshot`, `/events/stream`, `/incidents…`, `/events`, `/dashboard`,
`/notifications…`); схема OpenAPI — `/api/v1/openapi.json`, документация — `/api/docs`.
Развёртывание на сервере — [`DEPLOY.md`](DEPLOY.md).

## Быстрый старт (Docker)

```bash
cp .env.example .env            # задать POSTGRES_PASSWORD (и тот же пароль в DATABASE_URL)
docker compose up -d --build    # db → migrate → api + importer
curl localhost:8080/health/ready
curl -s localhost:8080/api/v1/snapshot | head -c 300
```

| Сервис | Что делает |
| --- | --- |
| `db` | PostgreSQL 16, данные в томе `pgdata` |
| `migrate` | применяет миграции и завершается (`/usr/local/bin/migrate`) |
| `api` | HTTP API на порту 8080, healthcheck `api --healthcheck` |
| `importer` | раз в `IMPORT_POLL_INTERVAL_SECONDS` читает `data/character_problems.json` |

Один образ (`cargo-chef` → `distroless/cc-debian13:nonroot`) с тремя бинарниками: `api`, `migrate`,
`importer`. Образ работает от `nonroot` (uid 65532): файл выгрузки должен быть читаем всеми
(`chmod -R a+rX data`). Одновременно работает один импортёр (advisory lock PostgreSQL), второй ждёт.

## Источник данных

Импортёр читает `data/character_problems.json` (каталог `data/` смонтирован в контейнер как `/data`).
Файл нужно публиковать **атомарно** — во временный файл в той же папке, затем `rename`, — иначе
импортёр может прочитать недописанный файл (такой снимок отклоняется, на экране остаётся последний
принятый, источник после двух неудач подряд — `error`).

Сценарий демонстрации меняет `data/character_problems.json` по шагам (скрипт без зависимостей):

```bash
python3 tools/demo.py status
python3 tools/demo.py step1     # INC000021938167: приоритет 3 → 4 + координаторы → «Критические», 2 оповещения
python3 tools/demo.py step2     # новый дедлайн → таймер пересчитан
python3 tools/demo.py step3     # Research / 20 + completionDate → «Исследование», отметка «Решён»
python3 tools/demo.py step4     # переназначение инженерам → уходит из «На координаторах»
python3 tools/demo.py step5     # повторное сохранение того же файла → без дублей
python3 tools/demo.py broken    # невалидный JSON → последний снимок + предупреждение
python3 tools/demo.py restore   # корректный снимок → обновления возобновились
python3 tools/demo.py reset     # исходная выгрузка
```

Исходная выгрузка для тестов — `testdata/character_problems.json` (её демо не трогает).

## Примеры запросов

```bash
curl -s localhost:8080/api/v1/snapshot | jq '{revision, source, groups: [.groups[] | {key, count}]}'
curl -s "localhost:8080/api/v1/incidents?group=critical&sort=deadline" | jq '.items[].id'
curl -s localhost:8080/api/v1/incidents/INC000021938167 | jq '.details | {status, sla, groups, itsmUrl}'
curl -s "localhost:8080/api/v1/events?type=critical&limit=10" | jq '.items[] | {eventId, types, at}'
curl -N localhost:8080/api/v1/events/stream                               # SSE: полный снимок
curl -N "localhost:8080/api/v1/events/stream?mode=notify&since=1"         # SSE: только сигналы
curl -s -H 'X-User-Id: alice' localhost:8080/api/v1/notifications | jq
```

## Структура

```
Cargo.toml          workspace + корневой пакет с бинарниками
bin/api.rs          HTTP API (API_HOST:API_PORT, по умолчанию 0.0.0.0:8080); --healthcheck
bin/migrate.rs      миграции
bin/importer.rs     импортёр JSON (SIGINT/SIGTERM — плавная остановка)
crates/config       Settings из окружения и .env
crates/domain       чистая логика: нормализация, валидация, план импорта, группы, SLA,
                    уведомления, курсоры, проекции в JSON-ответы
crates/store        пул sqlx, миграции, SQL импортёра, согласованное чтение для API
crates/importer     reader (устойчивое чтение файла), service (транзакция), worker (цикл + lock)
crates/api          axum: маршруты /api/v1, ошибки, SSE + broadcaster, OpenAPI (static/)
migrations/         SQL схемы (встраивается в бинарник migrate)
data/               файл выгрузки для docker compose
testdata/           исходная выгрузка для тестов
tools/demo.py       сценарий демонстрации
```

## Запуск без Docker

Нужны Rust (stable, `rustup`) и PostgreSQL 14+.

```bash
cp .env.example .env    # host "db" → localhost в DATABASE_URL, JSON_SOURCE_PATH=data/character_problems.json
cargo run --release --bin migrate
cargo run --release --bin api          # http://localhost:8080/api/docs
cargo run --release --bin importer
```

`.env` читается из текущего каталога; переменные окружения важнее файла.
`IMPORTER_EMBEDDED=true` запускает импортёр внутри процесса `api`.

## Конфигурация

Все переменные — в [`.env.example`](.env.example). Ошибки конфигурации выводятся все сразу, код
выхода 2. Дополнительно: `RUST_LOG` — тонкая настройка логов (без неё уровень берётся из `LOG_LEVEL`).
`DATABASE_URL` в формате Python-версии (`postgresql+asyncpg://…`) тоже принимается.

## Миграции

SQL — в `migrations/` (`0001_initial_up.sql` / `_down.sql`), встраивается в `migrate` через
`include_str!`; список версий — `crates/store/src/migrate.rs` (`MIGRATIONS`). Параллельные запуски
сериализуются advisory lock `0x4D545302`. Мигратор ведёт `_sqlx_migrations` и, для совместимости с
базами Python-версии, `alembic_version`: базу, созданную Python-версией, он принимает без выполнения SQL.

Новая миграция: SQL-файл в `migrations/` и запись в `MIGRATIONS` со следующей версией.

## Тесты

```bash
cargo test                                                     # без БД
TEST_DATABASE_URL=postgresql://user:pass@localhost:5432/postgres cargo test   # + PostgreSQL
cargo clippy --all-targets
```

С `TEST_DATABASE_URL` тесты создают и удаляют свои временные базы (`rust_migrate_*`, `rust_import_*`),
роли нужно право `CREATEDB`. Тесты `*_parity.rs` сверяют результаты с эталонами, снятыми с Python-версии
(`crates/*/tests/fixtures/*.json`: даты, хэши, тексты ошибок, планы импорта, дампы таблиц, ответы REST
и SSE байт в байт). Эталоны заморожены: генераторы остались в Python-версии.
