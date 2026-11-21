# План переноса backend на Rust (axum + tokio + sqlx)

> История переноса с Python-версии (FastAPI) на Rust: этапы R1–R7, решения и найденные расхождения.
> Пути вида `backend/…` и `backend-rust/…` относятся к исходной раскладке, где обе версии лежали рядом;
> в этом репозитории `backend-rust/` — корень, SQL — `migrations/`, выгрузка для тестов — `testdata/`.

Решение от 20.11: Rust-версия пишется **рядом** с Python (`backend/`), Python остаётся рабочей и эталонной
до полной замены. ИИ-модуль не переносится (подключается отдельно, как и в Python-версии).

## 1. Критерий готовности — паритет с Python

Rust-сервис считается готовым, когда на том же входе даёт тот же результат:

1. та же схема БД — **без новой миграции**: `backend/migrations/sql/0001_initial_up.sql` встраивается
   в `sqlx`-мигратор как есть (одна и та же база может обслуживаться любой из версий; журналы
   `alembic_version` ↔ `_sqlx_migrations` синхронизируются, см. README.md);
2. тот же JSON-контракт: API.md (`/snapshot`, `/events/stream`, `/incidents…`, `/events`) и маршруты ТЗ
   (`/dashboard`, `/notifications…`) — поля, camelCase, формат времени `2026-11-20T14:30:00.000Z`,
   формат ошибок `{error, code, message, requestId}`, коды 400/404/409/503;
3. те же переменные окружения (`DATABASE_URL`, `JSON_SOURCE_PATH`, `IMPORT_*`, `COORDINATOR_GROUP_NAME`,
   `ITSM_*`, `SSE_*`, …) — `.env` общий;
4. black-box-тесты паритета (раздел 6) проходят против обеих версий.

## 2. Стек

| Задача | Crate |
| --- | --- |
| HTTP, SSE | `axum` (`axum::response::sse`), `tower-http` (CORS, trace, request-id) |
| Async | `tokio` (full) |
| PostgreSQL, миграции | `sqlx` (postgres, runtime-tokio, tls-rustls, chrono, uuid, json, migrate) |
| JSON | `serde`, `serde_json` (`preserve_order` — исходная запись отдаётся как есть) |
| Время | `chrono` (UTC), `chrono-tz` для `SOURCE_NAIVE_TIMEZONE` |
| Конфигурация | свой загрузчик (`crates/config`): правила pydantic-settings + разбор `.env` как у python-dotenv (dotenvy не принимает значения с пробелами без кавычек) |
| Хэши | `sha2`, `hex` |
| Ошибки | `thiserror` (домен), `anyhow` (процессы) |
| Логи | `tracing`, `tracing-subscriber` |
| Тесты | встроенные `#[test]`/`#[tokio::test]`, `reqwest` для e2e (разбор SSE — свой, 20 строк) |

## 3. Структура

Один workspace, бинарники как процессы Python-версии:

```
backend-rust/
├── Cargo.toml                 # workspace
├── crates/
│   ├── config/                # Settings ← app/config.py; .env как у python-dotenv
│   ├── domain/                # чистая логика, без БД и async — порт модулей «без зависимостей»
│   │   ├── constants.rs       # ← app/core/constants.py
│   │   ├── pycompat.rs        # поведение Python для паритета: strip, repr(float), json.dumps, ==
│   │   ├── pydatetime.rs      # ← datetime.fromisoformat (C-реализация CPython)
│   │   ├── pyjson.rs          # ← позиции и тексты ошибок json.loads (C-сканер CPython 3.12), R4
│   │   ├── normalizer.rs      # ← app/importer/normalizer.py (таблица FIELDS, даты, статус)
│   │   ├── validator.rs       # ← app/importer/validator.py
│   │   ├── change_detector.rs # ← app/importer/change_detector.py (план импорта)
│   │   ├── grouping.rs        # ← app/services/grouping.py (+ отметка «Решён»)
│   │   ├── sla.rs             # ← app/services/sla.py
│   │   ├── cursors.rs         # ← app/services/cursors.py (R3: нужен тестам этапа 7)
│   │   ├── notification_rules.rs
│   │   ├── incident_query.rs  # фильтры/сортировка/курсоры /incidents
│   │   └── projection.rs      # строки → JSON-ответы
│   ├── store/                 # sqlx: пул, migrate (общий SQL), import_repo (SQL импортёра, R4)
│   ├── api/                   # axum: маршруты, ошибки, SSE + broadcaster (api::sse, R6)
│   └── importer/              # reader (stat + устойчивое чтение), service (транзакция), worker (цикл + lock)
├── bin/
│   ├── api.rs                 # порт 8080 (при запуске рядом с Python — 8081); --healthcheck
│   ├── migrate.rs             # аналог `alembic upgrade head` (сервис migrate в compose)
│   └── importer.rs            # аналог `python -m app.importer.worker` (R4)
│   (миграции: include_str!("../backend/migrations/sql/…") — тот же SQL, без симлинка и копии)
├── tests/parity/              # black-box тесты паритета
├── Dockerfile                 # multi-stage: cargo-chef → distroless/cc-debian13; контекст — корень Hackathon/
└── README.md
```

## 4. Соответствие ключевых решений (переносятся без изменений)

Из `backend/docs/ANALYSIS.md`, разделы 6 и 9:

* статус: `stateEnum` → fallback по коду `{0,2,7,10,20}`; противоречие → побеждает `stateEnum` (`statusMismatch`);
* дата-заглушка `0001-01-01…` → null; нераспознаваемая непустая дата отклоняет снимок (C8);
* сравнение по нормализованным `tracked`-значениям (пробелы, `""`/null, формат даты не дают событий);
* группы по API.md (Research требует внимания), отметка «Решён» по C16, окно 24 ч, событие `expiry`;
* уведомления `critical.raised`, `coordinator.assigned`, `critical.resolved`, `source.error/recovered`,
  ключ `{incidentId}:{type}:{incidentRevision}`, первичная загрузка без событий и уведомлений;
* источник → `error` после 2 неудач подряд;
* revision = id `dashboard_events`, чтение снимка в `REPEATABLE READ READ ONLY`;
* SSE: режим `snapshot` (по умолчанию) и `notify`; при каждом подключении — свежий опрос журнала;
  снимок строится один раз на ревизию; `retry: 2000`, `: heartbeat`.

Известные ловушки, найденные в Python-версии (повторить тесты):
`IncidentItem` должен сериализоваться с `sysId`, а не `sys_id`; фоновая задача broadcaster создаётся
только из async-контекста; кэш снимка не должен отдавать старую ревизию переподключившемуся клиенту.

## 5. Этапы

| # | Этап | Проверка |
| --- | --- | --- |
| R1 ✅ | Workspace, конфиг, мигратор на общем SQL, `/health` | миграция на пустой БД ✅, обмен с alembic ✅, раскладка Dockerfile ✅; `docker build` — проверить локально |
| R2 ✅ | `domain`: normalizer, validator, change_detector | порт unit-тестов этапа 5 Python (44 записи, все кейсы отказа) ✅; паритет на корпусе от Python ✅ |
| R3 ✅ | `domain`: grouping (группы, сортировка, `describe_resolution`), sla, cursors; `notification_rules` уже в R2 | порт unit-тестов этапов 6–7 ✅, сценарий демо на плане импорта ✅; паритет на корпусе от Python ✅ |
| R4 ✅ | `store` + `importer`: reader, транзакция импорта, advisory lock, expiry | тесты reader из этапа 5 ✅; интеграционные тесты этапов 5–7 на PostgreSQL ✅; паритет БД с Python ✅ |
| R5 ✅ | `api`: REST + ошибки | паритет-тесты REST против обеих версий ✅ (на эталоне — байт в байт; вживую — Python- и Rust-импортёр) |
| R6 ✅ | SSE + broadcaster | e2e-тест «изменение файла → браузер < 5 с» ✅, переподключение ✅, reset ✅; паритет потоков с Python ✅ (на эталоне — байт в байт; вживую — Python- и Rust-импортёр) |
| R7 ✅ | Docker, compose-профиль `rust`, README | `docker compose --profile rust up` ✅, `tools/demo.py` против Rust ✅, `parity` в контейнере 6/6 с обоими импортёрами ✅ (на заменённых базовых образах — см. журнал; на настоящих — собрать локально) |

## 6. Тесты паритета

`tests/parity/` — Python (`httpx`, уже есть в образе тестов): поднимают общую БД, публикуют один и тот же
JSON через `tools/demo.py`, запускают импортёр выбранной версии и сравнивают ответы `/snapshot`, `/dashboard`,
`/incidents/{id}`, `/events`, `/notifications` и SSE-события. Поля, зависящие от времени (`serverTime`,
`computedAt`, `remainingSeconds`, `at`, `createdAt`), сравниваются по формату, не по значению.
Переменная `TARGET=python|rust` выбирает адрес сервиса.

## 7. Запуск рядом

`backend/docker-compose.yml`, профиль `rust`: `migrate-rust` (после `migrate`), `api-rust` (порт 8081),
`importer-rust`; профиль `parity` — black-box тесты из `tests/parity` в образе тестов Python.
Одновременно пишет в базу только один импортёр (advisory lock общий — ключ `0x4D545301`, как в
Python); второй ждёт. Команды — README обеих версий.

## 8. Журнал

**R1 (20.11)** — сделано: workspace (`config`, `domain`, `store`, `api`, `importer`), бинарники `api` и
`migrate`, `/health`, `/health/ready`, `X-Request-ID`, CORS, `AppError` с форматом
`{error, code, message, requestId}`, 404/405/500 в этом формате, Dockerfile. 28 тестов (`cargo test`;
с `TEST_DATABASE_URL` — ещё и миграции на PostgreSQL).

Решения и находки:
* `sqlx::migrate!` не подходит к именам `0001_initial_up/down.sql` (две «версии 1») — мигратор
  собирается из списка `store::migrate::MIGRATIONS`, SQL встраивается `include_str!`;
* общая база: пустая → Rust пишет и `_sqlx_migrations`, и `alembic_version=0001`; от alembic →
  `Migrator::skip` без выполнения SQL; после `alembic downgrade` главный журнал — `alembic_version`;
  неизвестная Rust ревизия alembic — ошибка. Проверено с настоящим alembic 1.20 в обе стороны;
* в `backend/migrations/env.py` добавлен `include_object`: `alembic check`/autogenerate не видят
  `_sqlx_migrations` (иначе предлагали удалить таблицу);
* advisory lock миграций `0x4D545302` (импортёр — `0x4D545301`);
* новые переменные только у Rust: `API_HOST`, `API_PORT`;
* `docker build` в облачной среде не прогнан (реестры образов закрыты прокси); шаги Dockerfile
  (`cargo chef prepare/cook`, `cargo build --release --locked` в раскладке `/src`) проверены без Docker.

**R2 (20.11)** — сделано: `domain::{constants, normalizer, validator, change_detector}`, а также
части `grouping` (`normalize_group_name`, `next_resolution`) и весь `notification_rules`: без них план
импорта не даёт типов `critical`/`assigned`, отметки «Решён» и уведомлений, которые проверяют тесты
этапа 5. Тесты: порт `test_stage5_importer.py` (27, кроме `reader` — он в R4 вместе с импортёром) и
`stage5_parity.rs` — сверка с настоящим Python на корпусе (3,3 тыс. строк дат, 464 записи, 196 снимков,
300 сценариев плана). Всего в workspace 76 тестов.

Решения и находки:
* `content_hash` совпадает с Python байт в байт (общая БД): свой сериализатор как
  `json.dumps(sort_keys=True, ensure_ascii=False, separators=(",", ":"))`, включая `repr(float)`
  (`1e+16`, `1e-05`); пробелы — по `str.isspace` (Rust `trim` не считает пробелом `\x1c`–`\x1f`);
  сравнение значений — по `==` Python (`1 == 1.0 == True`);
* `datetime.fromisoformat` перенесён с C-реализации CPython, а не с `_pydatetime`: они расходятся
  (C принимает `12:40,5`, `12:30:45.`, требует `Z` в конце). Совпадение на корпусе — полное;
* `int()` Python принимает любые десятичные цифры Unicode (`"١٢"` → 12) — повторено таблицей блоков Nd;
* отличия, оставленные намеренно: там, где Python падает необработанным исключением (`"--5"`, `"²"`
  в целом поле, дата за 9999 годом после перевода в UTC), Rust отклоняет снимок обычной ошибкой поля;
  текст синтаксической ошибки JSON в скобках — от `serde_json`; целые вне `i64` и `1e400` в JSON
  Rust не принимает (Python отклонил бы их уже при записи в БД / хэшировал бы `Infinity`);
* тесты берут оригинал `Hackathon/character_problems.json`: `backend/data/character_problems.json`
  сейчас в состоянии демо-шага 1 (у `INC000021938167` priorityId 4 и группа координаторов), поэтому
  unit-тесты Python этапа 5 на нём не проходят до `python tools/demo.py reset`;
* фикстура собрана Python 3.13 из облачной среды, а в образе тестов — 3.12 (в R4 пересобрана 3.12.13,
  нашлось одно расхождение — см. журнал R4).

**R3 (20.11)** — сделано: `domain::grouping` целиком (трейт `Groupable` вместо duck typing,
`compute_groups`, `build_groups` + `Membership`, сортировка, `has_resolution_mark`,
`describe_resolution` → `ResolutionInfo`, `assignee_of`, `group_counts`), `domain::sla`
(`SlaStatus`, `compute_sla`, `format_duration`), `domain::cursors` (`parse_cursor` →
`InvalidCursor`, код `INVALID_CURSOR`). Тесты: `stage6.rs` (25 — весь `test_stage6_grouping_sla.py`),
`stage7.rs` (11 — весь `test_stage7_notifications.py`, включая полный сценарий демо через
имитацию реестра поверх плана импорта), `stage67_parity.rs` — сверка с настоящим Python на корпусе
(200 снимков: 7,7 тыс. проверок групп/порядка/членства/«Решён»/SLA/исполнителя; 200 цепочек
`next_resolution`; 524 длительности; 513 курсоров). Всего в workspace 119 тестов.

Решения и находки:
* `GroupResult` хранит ссылки (`Vec<&I>`), поэтому `store` в R4 сможет строить группы прямо по
  строкам из БД без копий; `Groupable::assigned_employee` по умолчанию `None` (как `getattr(…, None)`);
* порядок внутри группы — `(отметка «Решён», нет срока, срок, submitDate, id)`; `id` сравнивается
  как `str` Rust — по байтам UTF-8, что равно порядку кодовых точек Python; сортировка устойчива в обоих;
* `remaining_seconds` = `int(timedelta.total_seconds())`: микросекунды → `float` → отбрасывание к нулю,
  поэтому дедлайн −0,5 с — ещё `warning` «Осталось меньше минуты» с 0, а не `overdue` (повторено,
  совпадает с Python, пока |Δ| < 285 лет); `warning` — по точному остатку, не по секундам;
* `assignee_of`: пустой `assignedEmployee` — тоже «нет» (`or` Python);
* `cursors` вынесен из `incident_query` (R5) сюда, потому что его проверяют тесты этапа 7. Отличие,
  оставленное намеренно: `"²"` и подобные — `isdigit()` пропускает, `int()` падает (500 в Python), Rust
  отвечает 400 `INVALID_CURSOR`; арабские и прочие десятичные цифры (`"١٢"` → 12) — как в Python;
* мутационная проверка: перевёрнутый порядок `id` или `floor` вместо `trunc` дают 254 расхождения
  в `stage67_parity` — тест чувствителен к деталям порядка и округления.

**R4 (21.11)** — сделано: `store::import_repo` (состояние источника с `FOR UPDATE`, реестр, журнал,
ChangeEvent, пополевая история, уведомления с `ON CONFLICT (dedup_key) DO NOTHING`, истечение «Решён»),
`importer::{reader, service, worker}`, бинарник `importer` (SIGINT/SIGTERM), `IMPORTER_EMBEDDED=true`
в `api`, `importer` в Dockerfile; `domain::pyjson`. Тесты: `reader.rs` (7 — тесты `reader` из
`test_stage5_importer.py` + not_a_file/too_large), `stage5_import.rs` (17 — весь
`test_stage5_import.py` + ошибка БД, пустой первичный снимок, `run_importer` целиком),
`stage67_import.rs` (8 — весь `test_stage6_resolution.py` и импортная часть
`test_stage7_notifications.py`), `import_parity.rs` — три сценария по 121 шагу через настоящий
Python-импортёр (3.12) и Rust: итог каждого шага, md5 реестра, `import_state`, максимальные id и
полный дамп шести таблиц совпадают **точно, без нормализации**; `pyjson_parity.rs` — 8 026 отклонённых
текстов JSON совпадают с CPython 3.12 слово в слово. Всего в workspace 153 теста.
Вживую: второй импортёр (Rust или Python) ждёт общий lock, после остановки первого подхватывает базу,
записанную другой версией, и продолжает с той же ревизии.

Решения и находки:
* SQLAlchemy пишет `None` в JSONB как JSON `null`, а не SQL NULL (`none_as_null=False`): так в
  `change_events.previous/current` и `incident_changes.old/new_value`; Rust делает так же. `to_jsonb()`
  этого не различает, поэтому в дамп паритета добавлен счётчик SQL NULL (`sql_nulls`) — мутационная
  проверка: SQL NULL вместо JSON null и `Z` вместо `+00:00` в `dedup_key` тест ловит;
* `dedup_key` и `payload.errorSince` источника — `datetime.isoformat()` (`+00:00`, микросекунды только
  если не ноль); часы Rust усечены до микросекунд, как у PostgreSQL и Python;
* порядок записи как у unit of work SQLAlchemy: сначала UPDATE существующих, потом INSERT новых (pk
  новых — в порядке плана), ChangeEvent, история (один `INSERT … SELECT FROM unnest WITH ORDINALITY`),
  уведомления, затем `source.recovered`;
* ошибка БД в транзакции → `Ошибка сохранения снимка: <имя класса как в Python>`. Диалект asyncpg
  переводит по иерархии asyncpg: 23 → `IntegrityError`, 42 → `ProgrammingError`, XX → `InternalError`,
  всё прочее (в том числе 22003 — smallint вне диапазона) → `DBAPIError` (проверено на Python);
* **поправка к R2**: позиции синтаксических ошибок JSON у `serde_json` не совпадали с Python не только
  текстом в скобках — колонки считаются в байтах (кириллица) и при обрыве файла (реальный случай
  недописанной записи) указывают на последний символ. `domain::pyjson` повторяет C-сканер CPython 3.12
  и вызывается только после отказа `serde_json`; где CPython текст принимает, а `serde_json` нет
  (одиночный суррогат, `1e400`, вложенность > 128), остаётся описание `serde_json`. Тексты 3.13
  отличаются (висячая запятая), поэтому эталоны собираются Python 3.12, как в образе;
* **поправка к R2**: фикстуры `domain` пересобраны Python 3.12.13 (последняя 3.12, как в образе) —
  нашлось расхождение 3.12/3.13 в `fromisoformat`: если целые секунды смещения равны 0, 3.12 даёт
  `timezone.utc` и теряет доли (`+00:00:00.5` → UTC), 3.13 — нет. `pydatetime` повторяет 3.12;
  при переходе образа на 3.13 условие убрать и пересобрать фикстуры (записано в коде);
* `str(Path)` для `import_state.source_path` повторён (`./data/x.json` → `data/x.json`);
* список/прочтение уведомлений из `test_stage7_notifications.py` (`NotificationService`) — это API,
  переносится в R5 вместе с маршрутами `/notifications`;
* `docker build` по-прежнему не прогнан в облачной среде.

**R5 (21.11)** — сделано: `domain::{projection, incident_query}` (+ в `pycompat`: `int()` и разбор
целых pydantic, `quote`, base64 как `binascii`, `dumps` без сортировки ключей), `store::read_repo`
(согласованное чтение `REPEATABLE READ READ ONLY`, поиск по ключу, журнал, уведомления и отметки),
`api::{dashboard, notifications, params, docs, response}`: все маршруты `/api/v1` Python-версии, кроме
SSE; `AppState::clock` (как `get_clock`); 307 на адрес без `/` (`redirect_slashes`); OpenAPI,
`/api/docs`, `/redoc`, `/docs/oauth2-redirect` — из `crates/api/static/` (снято с FastAPI). Тесты:
`domain/tests/stage8.rs` (15 — весь `test_stage8_projection.py`), `api/tests/stage8_api.rs` (14 — весь
`test_stage8_api.py` + обе проверки `NotificationService` из `test_stage7_notifications.py`),
`api/tests/api_parity.rs` — два сценария по 43 шага через Python-импортёр и настоящий FastAPI на тех же
часах: 2 237 ответов (снимки, фильтры, сортировки, цепочки курсоров, 404/409/503, ошибки параметров,
отметки прочтения, 307/405, документация) совпадают **байт в байт** — статус, `content-type`,
`location`, `x-request-id` и тело, включая `serverTime` и SLA; `tests/parity/test_rest_parity.py` —
black-box сравнение двух работающих версий на одной базе по шагам `tools/demo.py` (прогнано и с
Python-, и с Rust-импортёром). Всего в workspace 196 тестов.

Решения и находки:
* тело ответа пишется как `JSONResponse` Starlette — `json.dumps(ensure_ascii=False, separators=…)`
  (`pycompat::dumps`, `api::response::PyJson`), а не `serde_json`: иначе `1e+16` ≠ `1e16`; порядок полей
  — как у моделей pydantic, `IncidentItem` — `sysId`, `id`, `dashboard`, затем исходные поля в порядке
  JSONB (ключ исходной записи `sys_id` остаётся в extras — проверено на pydantic);
* `format_utc`: миллисекунды отбрасываются; год < 1000 без нулей (`strftime("%Y")` glibc);
* ошибки параметров повторяют FastAPI: порядок path → query → header, внутри — порядок объявления;
  `msg`/`type` как у pydantic; при повторе query-ключа берётся последнее значение. Разбор `int`
  pydantic снят пробой: пробелы Unicode по краям, `+`, `_` между цифрами, `"3.0"`/`"3.00"`, но не
  `"3."`, не `"1e2"` и не арабские цифры (а `int()` в курсорах — принимает); числа вне `i64` → `le`;
* зависимость пользователя (`X-User-Id` → `INVALID_USER`) срабатывает раньше проверки параметров —
  как sub-dependency FastAPI;
* `decode_cursor` `/incidents` повторяет нестрогий `urlsafe_b64decode` (посторонние символы
  пропускаются, `=` дополняется по числу символов, не байтов) и `int()` (`"7: 1_0"` → 10);
* ключ `/incidents/{key}`: `^INC\d+$` с `re.IGNORECASE` совпадает и с `ı`/`İ`, `\d` — любые
  десятичные цифры Unicode; затем `upper()` (мутационная проверка: без `ı` — 20 расхождений);
* `axum::nest` отрезает префикс у `Uri` — сегменты пути берутся из `OriginalUri`;
* отличия, оставленные намеренно: `PUT /notifications/{id}/read` с id вне `bigint` — 404 (Python —
  500 от asyncpg); `%2F` в ключе Starlette декодирует до маршрутизации (другой маршрут), axum — нет;
  `/events/stream` — 404, `/events?since=…` и `Accept: text/event-stream` — 501 до R6;
* OpenAPI не генерируется из Rust-кода: контракт общий, `static/openapi.json` снимается с FastAPI
  тем же генератором, что и эталон паритета (подставляется только `info.title` из `APP_NAME`);
* эталон хранит большие тела как sha256 (2,1 МБ вместо 26 МБ); мутационная проверка: округление
  миллисекунд — 600 расхождений, обратный порядок дедлайнов в `priority` — 138;
* `docker build` по-прежнему не прогнан в облачной среде; `tests/fixtures/` исключены из контекста.

**R6 (21.11)** — сделано: `api::sse` — формат событий, `Broadcaster` (порт `RevisionBroadcaster`:
опрос журнала, `refresh` при каждом подключении, ожидание изменения, одна сборка снимка на ревизию),
потоки `snapshot_stream`/`notify_stream`, `plan_notify_start`, `DbJournal` (порт
`make_db_broadcaster`); маршрут `/events/stream` и ветка SSE в `/events` (`since` или
`Accept: text/event-stream`); `AppState::broadcaster()` (как `get_broadcaster`: создаётся при первом
подключении, до проверки параметров) и `AppState::with_clock`; `pycompat::dumps_pydantic`. Тесты:
`api/tests/stage9_sse.rs` (19 — весь `test_stage9_sse.py` + опоздавший опрос, остановка сервера,
отключение клиента, обрыв при ошибке БД, завершение фоновой задачи), `api/tests/stage9_sse_e2e.rs`
(9 — весь интеграционный `test_stage9_sse.py` на настоящем сервере: снимок < 5 с, heartbeat,
переподключение, ожидание первого импорта, notify, reset, `/events?since=`, ошибка источника; +
плавная остановка при открытых потоках и ошибки параметров), `api/tests/sse_parity.rs` — 29 шагов
через Python-импортёр и настоящий uvicorn: 836 потоков (заголовки, первые события, закрытие)
совпадают **байт в байт**, 4 015 чисел — с `model_dump_json`; `tests/parity/test_sse_parity.py` —
black-box сравнение двух работающих версий (вживую вместе с REST — 6/6, и с Python-, и с
Rust-импортёром; шаги демо доходят до клиентов обеих версий < 5 с). Всего в workspace 227 тестов.

Решения и находки:
* `data:` снимка — `snapshot.model_dump_json()`, а не `json.dumps`, как у `/snapshot`: pydantic-core
  2.50 пишет отрицательный показатель без нуля (`1e-6`), а показатель −5 — десятичной записью
  (`0.00001`); `float_pydantic` сверен на 300 тыс. случайных чисел. Запись зависит от версии
  pydantic-core (2.33–2.41: ещё и `1e16` вместо `1e+16`) — эталон снят с 2.50.5 (как в образе,
  см. `backend/compose_log.txt`); при обновлении pydantic пересобрать `gen_sse_parity.py`;
* заголовки как у `StreamingResponse`: `text/event-stream; charset=utf-8`, `cache-control`,
  `x-accel-buffering`, `connection: keep-alive`, `transfer-encoding: chunked`;
* broadcaster — `watch`-канал вместо `asyncio.Event`; результаты опросов применяются в порядке их
  начала (иначе в многопоточном tokio опоздавший ответ откатил бы курсор, и поток notify отправил
  бы ложный `stream.reset`); фоновая задача держит `Weak` и завершается вместе с broadcaster;
* поток — задача tokio с каналом ёмкостью 1 (не уходит вперёд читателя, как генератор), сразу
  завершается при отключении клиента; ошибка БД обрывает соединение (как исключение в генераторе);
  `serve` при остановке закрывает все потоки — иначе плавная остановка axum ждала бы их вечно
  (бинарник `api` с открытым потоком останавливается за ~1,5 с);
* `DbJournal` держит копию состояния без broadcaster (`AppState::detached`) — без цикла `Arc`;
  голова журнала читается одним запросом (max, min, причина последнего события);
* **поправка к R2**: `float_repr` при «ничьей» двух кратчайших записей выбирал не ту цифру
  (`{:e}` Rust: `664742861425528.25` → `…528.3`, `repr` Python — `…528.2`, к чётной). Теперь берётся
  корректно округлённая запись `{:.p$e}` той же длины. Касалось `content_hash`, REST и SSE;
  проверено на 20 тыс. чисел против CPython (старая версия ошибалась на «ничьих»);
* **поправка к R4/R5**: `serde_json` без `float_roundtrip` разбирал длинные десятичные записи с
  ошибкой в 1 ulp (`0.000…0001` → `1.0000000000000001e-34`), а PostgreSQL отдаёт малые числа JSONB
  именно так; кроме того, целые из 309 цифр (`1e308` после JSONB) не читались вовсе — строка
  реестра не декодировалась, и `/snapshot` и импортёр падали бы на такой записи. Включён
  `float_roundtrip` для всего workspace: читается всё, что помещается в `f64`. Мутационная проверка:
  без него и с записью чисел как у `json.dumps` — по 78 расхождений в `sse_parity`;
* отличия, оставленные намеренно: целые значения ≥ 2⁶⁴ в исходных записях (`1e22`) asyncpg +
  `json.loads` возвращают как `int`, и Python пишет их цифрами, Rust — как float (`1e+22`); числа
  больше `f64::MAX` (только целые из 310+ цифр, записанные Python-импортёром) Rust не прочитает;
  курсор notify `"²"` и подобные `isdigit()` пропускает, а `int()` падает — Python обрывает поток
  после `retry:`, Rust отвечает `stream.reset` (`invalid_cursor`);
* `docker build` по-прежнему не прогнан в облачной среде.

**R7 (21.11)** — сделано: профиль `rust` в `backend/docker-compose.yml` — `migrate-rust`, `api-rust`
(8081, healthcheck `api --healthcheck`), `importer-rust` (один образ `mts-incident-dashboard-rust`,
контекст `..`, `backend-rust/Dockerfile.dockerignore`); профиль `parity` — `tests/parity` в образе
тестов Python против `api` и `api-rust`; README обеих версий (запуск рядом, только Rust,
переключение импортёра, паритет, ошибки прав). Всего в workspace 227 тестов (без изменений).

Проверка. Реестры образов в облачной среде закрыты, поэтому базовые образы заменены локальными
с теми же именами, собранными из Ubuntu 24.04 (debootstrap): `rust:1-slim-trixie` (rustup stable),
`gcr.io/distroless/cc-debian13:nonroot` (пользователь `nonroot` 65532), `python:3.12-slim`
(python3.12 + venv), `postgres:16-alpine` (postgresql-16 + упрощённый entrypoint: `POSTGRES_*`,
`initdb.d`, UTF8). Dockerfile и compose — без изменений (кроме `# syntax=`: фронтенд из Docker Hub
тоже недоступен, использован встроенный BuildKit). На этом стенде:
* `docker compose --profile rust --profile parity build` — Rust-образ собран через cargo-chef
  (контекст 0,8 МБ), Python-образы — как обычно;
* `--profile rust up` на пустой базе: `migrate` → `migrate-rust` (`adopted_from_alembic=[1]`) →
  обе API healthy, обе отдают ревизию 1; второй импортёр ждёт lock;
* `docker compose stop importer` → `importer-rust` подхватывает базу; `tools/demo.py`
  reset → step1…step5 → broken → restore → reset через Rust-API: группы, 2 оповещения на step1,
  без новых на step2/step5, `error`/`source.error` на broken, `recovered` на restore — как в README;
* `parity` — 6/6 и с Rust-, и с Python-импортёром (~35 с);
* `--profile rust up db api-rust importer-rust` на пустой базе — только Rust-сервисы (+ db, migrate).

Решения и находки:
* `migrate-rust` зависит от `migrate`: alembic не берёт advisory lock миграций, и параллельный
  запуск двух миграторов на пустой базе мог бы столкнуться на `CREATE TABLE`;
* `HEALTHCHECK` Rust-образа (`api --healthcheck`) наследовался `importer-rust` и `migrate-rust`
  (импортёр становился бы `unhealthy`) — в compose отключён;
* логи Rust писали ANSI-цвета и в `docker compose logs` — теперь только в терминал (`bin/common.rs`);
* Linux-хост: файл выгрузки с правами 600 Rust-образ (uid 65532) не читает (`Permission denied`,
  у Python-образа то же — строка `chmod -R a+rX` в README уже была); `parity` пишет в `data/`
  от `root`, на Linux — `PARITY_USER="$(id -u):$(id -g)"`. На macOS (Docker Desktop) права
  отображаются на пользователя хоста — проблемы нет;
* **поправка к R6**: `test_notify_stream_cursors` ждал блок потока 10 с, а при `SSE_HEARTBEAT_SECONDS=15`
  из `.env.example` курсор на голове журнала получает первым `: heartbeat` — тест падал по тайм-ауту.
  Тайм-аут теперь `SSE_HEARTBEAT_SECONDS + 5` (`parity` читает `.env`), 14 потоков проверяются
  параллельно; `PYTHON_URL` по умолчанию — `http://localhost:8080` (порт compose);
* не проверено: настоящие базовые образы (trixie, distroless без shell, alpine). Риски небольшие —
  бинарники связаны только с glibc (TLS — rustls), healthcheck без shell, — но первый
  `docker compose --profile rust up --build` нужно выполнить локально.

## 9. Как начать в новом чате

Все этапы R1–R7 выполнены. Осталось локально, на Mac с Docker Desktop:

```bash
cd backend
docker compose --profile rust up -d --build
docker compose --profile rust --profile parity run --rm --build parity   # ожидается 6 passed
```

Если что-то не так — написать в новом чате проекта: «Rust-порт по `backend-rust/PLAN.md`: проверка
R7 на настоящих образах» и приложить вывод (`docker compose --profile rust logs`).
