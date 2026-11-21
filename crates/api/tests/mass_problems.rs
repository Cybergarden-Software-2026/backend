//! Shared curation against the same PostgreSQL schema and importer as the dashboard.

#[path = "../../importer/tests/common/mod.rs"]
mod common;

use std::sync::Arc;

use axum::{Router, body::Body, http::Request};
use common::{Env, Record, records, s, update};
use dashboard_api::{AppState, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const ROUTE: &str = "/api/v1/problems/clusters";

fn api(env: &Env) -> Router {
    router(AppState::with_clock(
        Arc::clone(&env.settings),
        env.pool().clone(),
        env.clock.as_clock(),
    ))
}

async fn request(app: &Router, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn get(app: &Router) -> Value {
    let (status, body) = request(app, "GET", ROUTE, None).await;
    assert_eq!(status, 200, "{body}");
    body
}

async fn edit(app: &Router, action: Value) -> (u16, Value) {
    request(app, "PATCH", ROUTE, Some(action)).await
}

fn with_incidents(spec: &[(&str, &str, &str, &str, &str)]) -> Vec<Record> {
    records()
        .into_iter()
        .zip(spec)
        .map(|(mut record, &(id, service, header, date, state))| {
            update(
                &mut record,
                &[
                    ("sysId", s(id)),
                    ("id", json!(format!("INC-{id}"))),
                    ("service", s(service)),
                    ("header", s(header)),
                    ("submitDate", s(date)),
                    ("stateEnum", s(state)),
                    ("stateId", if state == "Research" { json!(20) } else { json!(0) }),
                ],
            );
            record
        })
        .collect()
}

async fn import(env: &mut Env, records: &[Record]) {
    env.write_json(records);
    let outcome = env.tick().await;
    assert!(outcome.accepted(), "{outcome:?}");
}

#[tokio::test]
async fn clusters_use_complete_link_and_preserve_demo_rules() {
    let Some(mut env) = Env::new().await else { return };
    let app = api(&env);
    assert_eq!(request(&app, "GET", ROUTE, None).await.0, 503);
    let first = "2026-11-18T12:00:00Z";
    let second = "2026-11-19T12:00:00Z";
    let third = "2026-11-20T12:00:00Z";
    import(
        &mut env,
        &with_incidents(&[
            ("c", "Процессинг", "Зависла выплата", third, "Assigned"),
            ("a", "Процессинг", "Терминал не отвечает", first, "Assigned"),
            ("r", "Процессинг", "Завис возврат", second, "Research"),
            ("b", "Процессинг", "Не прошёл перевод", second, "Assigned"),
            ("d", "Сервис 1", "Таймаут авторизации карт", first, "Assigned"),
            ("e", "Сервис 2", "Ошибка авторизации карт", first, "Assigned"),
            ("f", "Сервис 3", "Ошибка сбой проблема", first, "Assigned"),
            ("g", "Сервис 4", "Неисправность сбой проблема", first, "Assigned"),
        ]),
    )
    .await;
    let groups = get(&app).await;
    assert_eq!(
        groups["items"],
        json!([
            {"id":"a-b", "title":"Процессинг", "reason":"Один затронутый сервис; регистрации в пределах 24 часов", "sysIds":["a","b"], "incidentIds":["INC-a","INC-b"]},
            {"id":"d-e", "title":"Похожие симптомы", "reason":"Похожие симптомы в заголовках; регистрации в пределах 24 часов", "sysIds":["d","e"], "incidentIds":["INC-d","INC-e"]},
        ])
    );
    assert_eq!(groups["exclusions"], json!([]));
    env.finish().await;
}

#[tokio::test]
async fn curation_survives_a_new_router_and_import_and_checks_stale_revisions() {
    let Some(mut env) = Env::new().await else { return };
    let date = "2026-11-19T12:00:00Z";
    let mut records = with_incidents(&[
        ("a", "Один сервис", "Один заголовок", date, "Assigned"),
        ("b", "Один сервис", "Второй заголовок", date, "Assigned"),
        ("c", "Другой сервис", "Третий заголовок", date, "Research"),
        ("d", "Остальной сервис", "Четвёртый заголовок", date, "Assigned"),
        ("e", "Остальной сервис", "Пятый заголовок", date, "Assigned"),
    ]);
    import(&mut env, &records).await;
    let first_app = api(&env);
    let second_app = api(&env);
    let first = get(&first_app).await;
    let (status, reviewed) = edit(
        &first_app,
        json!({
            "expectedRevision":first["revision"], "action":"add", "groupId":"a-b", "sysIds":["c", "c"]
        }),
    )
    .await;
    assert_eq!(status, 200, "{reviewed}");
    assert_eq!(reviewed["items"][0]["id"], "manual:a-b");
    assert_eq!(reviewed["items"][0]["sysIds"], json!(["a", "b", "c"]));
    assert_eq!(get(&second_app).await, reviewed);
    let (status, stale) = edit(
        &second_app,
        json!({
            "expectedRevision":first["revision"], "action":"exclude", "groupId":"manual:a-b", "sysId":"c"
        }),
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(stale["currentRevision"], reviewed["revision"]);

    // A team change in the source does not invalidate mass-problem context.
    records[2].insert("assignedGroupName".into(), s("Другая команда"));
    import(&mut env, &records).await;
    let imported = get(&second_app).await;
    assert_eq!(imported["items"], reviewed["items"]);
    assert!(imported["revision"].as_i64() > reviewed["revision"].as_i64());
    let (left, right) = tokio::join!(
        edit(
            &first_app,
            json!({"expectedRevision":imported["revision"], "action":"exclude", "groupId":"manual:a-b", "sysId":"c"})
        ),
        edit(
            &second_app,
            json!({"expectedRevision":imported["revision"], "action":"exclude", "groupId":"manual:a-b", "sysId":"b"})
        ),
    );
    let (excluded, conflict) = if left.0 == 200 {
        (left, right)
    } else {
        (right, left)
    };
    assert_eq!((excluded.0, conflict.0), (200, 409));
    assert_eq!(excluded.1["exclusions"].as_array().unwrap().len(), 1);
    assert_eq!(get(&second_app).await, excluded.1);
    let (status, restored) = edit(&second_app, json!({
        "expectedRevision":excluded.1["revision"], "action":"restore", "sysId":excluded.1["exclusions"][0]["sysId"]
    })).await;
    assert_eq!(status, 200, "{restored}");
    assert_eq!(restored["items"][0]["sysIds"], json!(["a", "b", "c"]));
    assert_eq!(restored["exclusions"], json!([]));
    let (_, dashboard) = request(&first_app, "GET", "/api/v1/snapshot?view=web", None).await;
    assert_eq!(dashboard["revision"], restored["revision"]);

    let (status, moved) = edit(
        &second_app,
        json!({
            "expectedRevision":restored["revision"], "action":"add", "groupId":"d-e", "sysIds":["c"]
        }),
    )
    .await;
    assert_eq!(status, 200, "{moved}");
    assert_eq!(moved["items"][0]["id"], "manual:d-e");
    assert_eq!(moved["items"][0]["sysIds"], json!(["d", "e", "c"]));
    assert_eq!(moved["items"][1]["id"], "manual:a-b");
    assert_eq!(moved["items"][1]["sysIds"], json!(["a", "b"]));

    let (status, hidden) = edit(
        &first_app,
        json!({
            "expectedRevision":moved["revision"], "action":"exclude", "groupId":"manual:d-e", "sysId":"c"
        }),
    )
    .await;
    assert_eq!(status, 200, "{hidden}");
    assert_eq!(hidden["exclusions"][0]["sysId"], "c");
    records[2].insert("header".into(), s("Инцидент с новой тематикой"));
    import(&mut env, &records).await;
    let changed = get(&second_app).await;
    assert_eq!(changed["exclusions"], json!([]));
    assert_eq!(changed["items"][0]["sysIds"], json!(["d", "e"]));
    env.finish().await;
}

#[tokio::test]
async fn same_product_and_scenario_need_a_similar_description() {
    let Some(mut env) = Env::new().await else { return };
    let date = "2026-11-19T12:00:00Z";
    let mut records = with_incidents(&[
        ("a", "Кассы магазин 1", "Нет печати", date, "Assigned"),
        ("b", "Кассы магазин 2", "Не печатает", date, "Assigned"),
        ("c", "Кассы магазин 3", "Медленно", date, "Assigned"),
        ("d", "Кассы магазин 4", "Тоже касса", date, "Assigned"),
        ("e", "Кассы магазин 5", "Похоже, но", date, "Assigned"),
    ]);
    let set = |record: &mut Record, product: &str, scenario: &str, description: &str| {
        update(
            record,
            &[
                ("productName", s(product)),
                ("scenarioName", s(scenario)),
                ("shortDescription", Value::Null),
                ("description", s(description)),
            ],
        );
    };
    let receipts = "Кассовые терминалы магазина перестали печатать фискальные чеки покупателям";
    set(&mut records[0], "Розница", "Печать чека", receipts);
    set(
        &mut records[1],
        "Розница",
        "Печать чека",
        "Терминалы магазина перестали печатать фискальные чеки после обновления",
    );
    // Тот же продукт и сценарий, но другое описание.
    set(
        &mut records[2],
        "Розница",
        "Печать чека",
        "Долго открывается отчёт смены у администратора",
    );
    // То же описание, но другой сценарий.
    set(&mut records[3], "Розница", "Оплата картой", receipts);
    // Тот же сценарий, но другой продукт.
    set(&mut records[4], "Склад", "Печать чека", receipts);
    import(&mut env, &records).await;
    let groups = get(&api(&env)).await;
    assert_eq!(
        groups["items"],
        json!([{
            "id": "a-b",
            "title": "Розница: Печать чека",
            "reason": "Один продукт и сценарий, похожее описание проблемы; регистрации в пределах 24 часов",
            "sysIds": ["a", "b"],
            "incidentIds": ["INC-a", "INC-b"],
        }])
    );
    env.finish().await;
}
