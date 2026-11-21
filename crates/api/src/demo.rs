//! Демо-маршруты веб-дашборда: `POST /api/v1/demo` и `PUT /api/v1/incidents`.
//!
//! Повторяют локальный адаптер `frontend-web/tools/demo-api`: оба маршрута переписывают файл
//! источника (`JSON_SOURCE_PATH`) атомарной заменой и сразу импортируют его, ответ — полный
//! веб-снимок. Импортёр видит тот же файл и повторно его не меняет.
//!
//! Маршруты выключены по умолчанию (`DEMO_API_ENABLED=false`): тогда `POST /demo` отвечает 404,
//! а `PUT /incidents` — 405. Каталог тестового источника должен быть доступен API на запись.

use std::path::Path;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, JsonRejection};
use axum::extract::{Json, State};
use axum::http::StatusCode;
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use dashboard_domain::constants::{NormalizedState, PRIORITY_CRITICAL};
use dashboard_domain::normalizer::resolve_state;
use dashboard_domain::validator::parse_and_validate;
use dashboard_importer::ImportService;
use dashboard_importer::reader::{self, ReadOptions};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::dashboard::build_snapshot_with_view;
use crate::response::PyJson;
use crate::{ApiResult, AppError, AppState};

/// Сценарии `POST /demo` (кнопки «Симуляция» фронтенда).
pub const DEMO_ACTIONS: &[&str] = &[
    "coordinator-wait",
    "mass",
    "unassigned",
    "critical",
    "assign",
    "deadline",
    "resolve",
    "reopen",
    "research",
    "close",
    "reset",
];

#[derive(Deserialize)]
pub struct DemoRequest {
    action: String,
}

/// `POST /demo` `{"action": "..."}`: меняет тестовую выгрузку по сценарию.
pub async fn simulate(
    State(state): State<AppState>,
    body: Result<Json<DemoRequest>, JsonRejection>,
) -> ApiResult<PyJson> {
    if !state.settings.demo_api_enabled {
        return Err(AppError::http(StatusCode::NOT_FOUND));
    }
    let Json(input) = body.map_err(|_| AppError::bad_request("Ожидается action в JSON."))?;
    if input.action == "reset" {
        let seed = state.settings.demo_seed_path.clone().ok_or_else(|| {
            AppError::new(
                StatusCode::CONFLICT,
                "DEMO_SEED_NOT_CONFIGURED",
                "Для сброса настройте DEMO_SEED_PATH на сервере.",
            )
        })?;
        let content = read_file(seed).await.map_err(|e| {
            AppError::new(
                StatusCode::CONFLICT,
                "DEMO_SEED_UNAVAILABLE",
                format!("Не удалось прочитать эталонную выгрузку: {e}"),
            )
        })?;
        return replace_source(&state, content).await;
    }
    let content = read_file(state.settings.json_source_path.clone())
        .await
        .map_err(|e| AppError::conflict(format!("Не удалось прочитать источник: {e}")))?;
    let mut items: Vec<Value> = serde_json::from_slice(&content)
        .map_err(|_| AppError::conflict("Источник должен содержать JSON-массив инцидентов."))?;
    let now = (state.clock)();
    apply_action(
        &mut items,
        &input.action,
        now,
        &state.settings.coordinator_group_name,
    )?;
    let mut content = serde_json::to_vec_pretty(&items).map_err(|_| AppError::internal())?;
    content.push(b'\n');
    replace_source(&state, content).await
}

/// `PUT /incidents`: полная замена тестовой выгрузки массивом из тела запроса.
pub async fn replace_incidents(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult<PyJson> {
    if !state.settings.demo_api_enabled {
        return Err(AppError::http(StatusCode::METHOD_NOT_ALLOWED));
    }
    let body = body.map_err(|_| {
        AppError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "PAYLOAD_TOO_LARGE",
            "Размер запроса превышает допустимый.",
        )
    })?;
    replace_source(&state, body.to_vec()).await
}

async fn read_file(path: std::path::PathBuf) -> std::io::Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || std::fs::read(path))
        .await
        .map_err(std::io::Error::other)?
}

/// Проверяет выгрузку, записывает её в источник и сразу импортирует. Отклонённая выгрузка
/// источник не меняет.
async fn replace_source(state: &AppState, content: Vec<u8>) -> ApiResult<PyJson> {
    let tz = state.settings.source_naive_timezone;
    let checked = tokio::task::spawn_blocking(move || {
        let verdict = parse_and_validate(&content, tz).map(|_| ());
        (content, verdict)
    })
    .await
    .map_err(|_| AppError::internal())?;
    let content = match checked {
        (content, Ok(())) => content,
        (_, Err(rejected)) => {
            return Err(AppError::validation(rejected.summary()).with_code("SNAPSHOT_REJECTED"));
        }
    };

    let path = state.settings.json_source_path.clone();
    let options = ReadOptions {
        retry_delay: state.settings.import_stable_read_delay,
        ..ReadOptions::default()
    };
    let read = tokio::task::spawn_blocking(move || {
        write_atomically(&path, &content).map(|()| reader::read_snapshot_file(&path, None, &options))
    })
    .await
    .map_err(|_| AppError::internal())?
    .map_err(|e| {
        tracing::error!(error = %e, "demo: cannot write source file");
        AppError::new(
            StatusCode::CONFLICT,
            "SOURCE_NOT_WRITABLE",
            format!("Не удалось записать источник: {e}"),
        )
    })?;

    let service = ImportService::with_clock(
        state.pool.clone(),
        Arc::clone(&state.settings),
        Arc::clone(&state.clock),
    );
    let outcome = service.process(read).await.map_err(|e| {
        tracing::error!(error = %e, "demo: import failed");
        AppError::internal()
    })?;
    if !outcome.accepted() {
        return Err(AppError::validation(
            outcome
                .message
                .unwrap_or_else(|| "Выгрузка не принята.".to_owned()),
        )
        .with_code("SNAPSHOT_REJECTED"));
    }
    Ok(PyJson(build_snapshot_with_view(state, true).await?))
}

/// Запись во временный файл того же каталога и `rename`: читатель видит файл целиком.
fn write_atomically(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "source.json".to_owned());
    let temporary = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&temporary, content)?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

// ------------------------------------------------------------------ сценарии

fn state_of(item: &Value) -> NormalizedState {
    resolve_state(
        item.get("stateEnum").and_then(Value::as_str),
        item.get("stateId").and_then(Value::as_i64),
    )
    .state
}

fn group_of(item: &Value) -> &str {
    item.get("assignedGroupName")
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn priority_of(item: &Value) -> Option<i64> {
    item.get("priorityId").and_then(Value::as_i64)
}

fn iso(at: DateTime<Utc>) -> Value {
    Value::String(at.to_rfc3339_opts(SecondsFormat::Secs, true))
}

fn set(item: &mut Value, fields: Value) {
    if let (Some(target), Value::Object(fields)) = (item.as_object_mut(), fields) {
        target.extend(fields);
    }
}

fn find(items: &[Value], predicate: impl Fn(&Value) -> bool) -> Option<usize> {
    items.iter().position(predicate)
}

/// Меняет выгрузку так же, как `IncidentStore.simulation` в `tools/demo-api` (кроме `reset`).
pub fn apply_action(
    items: &mut Vec<Value>,
    action: &str,
    now: DateTime<Utc>,
    coordinator: &str,
) -> ApiResult<()> {
    let minutes = |m: i64| iso(now + TimeDelta::minutes(m));
    if action == "mass" {
        let base = items
            .first()
            .cloned()
            .ok_or_else(|| AppError::bad_request("Нет исходных данных"))?;
        let stamp = now.timestamp_millis();
        let headers = [
            "Ошибки при оплате картой",
            "Отказ авторизации платежа",
            "Платежи не проходят",
        ];
        for (index, header) in headers.into_iter().enumerate() {
            let mut demo = base.clone();
            set(
                &mut demo,
                json!({
                    "sysId": format!("demo-mass-{stamp}-{index}"),
                    "id": format!("DEMO-MASS-{stamp}-{}", index + 1),
                    "header": header,
                    "description": "Демонстрация: сбой в общем сервисе обработки платежей. Требуется проверка связи инцидентов.",
                    "shortDescription": "Не проходит оплата через общий платёжный сервис.",
                    "service": "ДЕМО · Сервис обработки платежей",
                    "serviceId": "demo-payments",
                    "productName": "Демонстрационный платёжный сервис",
                    "stateEnum": "Assigned",
                    "stateId": 0,
                    "state": "Назначен",
                    "priorityId": 3,
                    "priority": "Высокий",
                    "submitDate": iso(now),
                    "deadline": minutes(20),
                    "completionDate": null,
                    "assignedGroupName": "",
                    "assignedEmployee": "",
                    "assignedEmployeeLogin": "",
                    "lastAssignmentToTeamDateTime": null,
                    "solution": "",
                }),
            );
            items.push(demo);
        }
        return Ok(());
    }
    if !DEMO_ACTIONS.contains(&action) || action == "reset" {
        return Err(AppError::bad_request("Неизвестный сценарий").with_code("UNKNOWN_DEMO_ACTION"));
    }
    if items.is_empty() {
        return Err(AppError::bad_request(
            "Для симуляции необходим хотя бы один инцидент",
        ));
    }
    let active = |i: &Value| state_of(i).is_active();
    let attention = |i: &Value| state_of(i).is_attention();
    let fallback = find(items.as_slice(), |i| active(i) && group_of(i) == coordinator)
        .or_else(|| find(items.as_slice(), active))
        .unwrap_or(0);
    let pick = |found: Option<usize>| found.unwrap_or(fallback);

    match action {
        "critical" => {
            let i = pick(find(items.as_slice(), |i| {
                active(i) && priority_of(i) != Some(PRIORITY_CRITICAL)
            }));
            set(
                &mut items[i],
                json!({
                    "priorityId": PRIORITY_CRITICAL,
                    "priority": "Наивысший",
                    "assignedGroupName": "",
                    "assignedEmployee": "",
                    "assignedEmployeeLogin": "",
                    "lastAssignmentToTeamDateTime": iso(now),
                    "deadline": minutes(12),
                }),
            );
        }
        "assign" => {
            let i = pick(find(items.as_slice(), |i| {
                group_of(i) != coordinator && attention(i)
            }));
            set(
                &mut items[i],
                json!({"assignedGroupName": coordinator, "lastAssignmentToTeamDateTime": iso(now)}),
            );
        }
        "resolve" => {
            let i = pick(find(items.as_slice(), |i| {
                priority_of(i) == Some(PRIORITY_CRITICAL) && active(i)
            }));
            set(
                &mut items[i],
                json!({
                    "stateId": 7,
                    "stateEnum": "Resolved",
                    "state": "Завершен",
                    "completionDate": iso(now),
                    "solution": "Проблема устранена в сценарии демонстрации. Запись допускает возврат в работу.",
                }),
            );
        }
        "close" => {
            let i = pick(find(items.as_slice(), |i| {
                state_of(i) == NormalizedState::Resolved
            }));
            let completion = items[i]
                .get("completionDate")
                .filter(|v| !v.is_null())
                .cloned()
                .unwrap_or_else(|| iso(now));
            set(
                &mut items[i],
                json!({"stateId": 10, "stateEnum": "Closed", "state": "Закрыт", "completionDate": completion}),
            );
        }
        "reopen" => {
            let i = pick(
                find(items.as_slice(), |i| state_of(i) == NormalizedState::Resolved)
                    .or_else(|| find(items.as_slice(), |i| state_of(i) == NormalizedState::Research)),
            );
            set(
                &mut items[i],
                json!({
                    "stateId": 2,
                    "stateEnum": "WorkInProgress",
                    "state": "Выполняется",
                    "completionDate": null,
                    "deadline": minutes(45),
                }),
            );
        }
        "research" => set(
            &mut items[fallback],
            json!({
                "stateId": 20,
                "stateEnum": "Research",
                "state": "Исследование ситуации",
                "completionDate": iso(now),
                "assignedGroupName": coordinator,
            }),
        ),
        "coordinator-wait" => {
            let i = pick(find(items.as_slice(), |i| {
                attention(i) && priority_of(i) != Some(PRIORITY_CRITICAL)
            }));
            set(
                &mut items[i],
                json!({
                    "assignedGroupName": coordinator,
                    "assignedEmployee": "",
                    "assignedEmployeeLogin": "",
                    "lastAssignmentToTeamDateTime": minutes(-9),
                }),
            );
        }
        "unassigned" => set(
            &mut items[fallback],
            json!({
                "assignedGroupName": "",
                "assignedEmployee": "",
                "assignedEmployeeLogin": "",
                "lastAssignmentToTeamDateTime": minutes(-20),
                "deadline": minutes(60),
            }),
        ),
        "deadline" => set(&mut items[fallback], json!({"deadline": minutes(5)})),
        _ => unreachable!("action checked against DEMO_ACTIONS"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const COORD: &str = "КЦ ИТ Координаторы Массовых Инцидентов";

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-21T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn sample() -> Vec<Value> {
        vec![
            json!({"sysId":"1","id":"INC1","stateEnum":"Assigned","stateId":0,"priorityId":2,"assignedGroupName":COORD,"completionDate":null}),
            json!({"sysId":"2","id":"INC2","stateEnum":"Resolved","stateId":7,"priorityId":4,"assignedGroupName":"Team A","completionDate":"2026-11-20T10:00:00Z"}),
            json!({"sysId":"3","id":"INC3","stateEnum":"WorkInProgress","stateId":2,"priorityId":4,"assignedGroupName":"Team B","completionDate":null}),
        ]
    }

    #[test]
    fn mass_adds_three_related_incidents() {
        let mut items = sample();
        apply_action(&mut items, "mass", now(), COORD).unwrap();
        assert_eq!(items.len(), 6);
        assert!(items[3..].iter().all(|i| i["serviceId"] == "demo-payments"));
        assert_eq!(
            items[5]["id"],
            format!("DEMO-MASS-{}-3", now().timestamp_millis())
        );
        assert_eq!(items[3]["deadline"], "2026-11-21T12:20:00Z");
    }

    #[test]
    fn scenarios_pick_the_same_incidents_as_the_demo_adapter() {
        let mut items = sample();
        apply_action(&mut items, "critical", now(), COORD).unwrap();
        assert_eq!(items[0]["priorityId"], 4);
        assert_eq!(items[0]["assignedGroupName"], "");

        let mut items = sample();
        apply_action(&mut items, "close", now(), COORD).unwrap();
        assert_eq!(items[1]["stateEnum"], "Closed");
        assert_eq!(items[1]["completionDate"], "2026-11-20T10:00:00Z");

        let mut items = sample();
        apply_action(&mut items, "reopen", now(), COORD).unwrap();
        assert_eq!(items[1]["stateEnum"], "WorkInProgress");
        assert!(items[1]["completionDate"].is_null());

        let mut items = sample();
        apply_action(&mut items, "resolve", now(), COORD).unwrap();
        assert_eq!(items[2]["stateEnum"], "Resolved");

        let mut items = sample();
        apply_action(&mut items, "assign", now(), COORD).unwrap();
        assert_eq!(items[2]["assignedGroupName"], COORD);

        let mut items = sample();
        apply_action(&mut items, "deadline", now(), COORD).unwrap();
        assert_eq!(items[0]["deadline"], "2026-11-21T12:05:00Z");
    }

    #[test]
    fn unknown_and_empty_inputs_are_rejected() {
        let mut items = sample();
        let err = apply_action(&mut items, "explode", now(), COORD).unwrap_err();
        assert_eq!(err.body()["code"], "UNKNOWN_DEMO_ACTION");
        let mut empty = Vec::new();
        assert!(apply_action(&mut empty, "deadline", now(), COORD).is_err());
        assert!(apply_action(&mut empty, "mass", now(), COORD).is_err());
    }
}
