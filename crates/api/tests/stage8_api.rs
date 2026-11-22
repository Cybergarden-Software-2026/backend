//! Порт `backend/tests/integration/test_stage8_api.py` (REST API на PostgreSQL) и части
//! `test_stage7_notifications.py` про `NotificationService` (список с курсором, отметки
//! прочтения). Нужен `TEST_DATABASE_URL`.

#[path = "../../importer/tests/common/mod.rs"]
mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use common::*;
use dashboard_api::{AppState, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const DEMO_SYS_ID: &str = "179429233402440192";

struct Api {
    e: Env,
    app: axum::Router,
}

struct Res {
    status: u16,
    content_type: Option<String>,
    raw: Vec<u8>,
}

impl Res {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.raw).unwrap()
    }
}

impl Api {
    async fn new() -> Option<Self> {
        Self::with_vars(&[]).await
    }

    async fn with_vars(extra: &[(&str, &str)]) -> Option<Self> {
        let mut vars = vec![
            ("ITSM_BASE_URL", "https://itsm.example"),
            ("ITSM_URL_TEMPLATE", "{base_url}/incidents/{sysId}"),
        ];
        vars.extend_from_slice(extra);
        let e = Env::with_vars(&vars).await?;
        let app = router(AppState::with_clock(
            Arc::clone(&e.settings),
            e.pool().clone(),
            e.clock.as_clock(),
        ));
        Some(Self { e, app })
    }

    async fn request(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> Res {
        self.send(method, path, headers, Body::empty()).await
    }

    async fn send(&self, method: &str, path: &str, headers: &[(&str, &str)], body: Body) -> Res {
        let mut b = Request::builder().method(method).uri(format!("/api/v1{path}"));
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let res = self.app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
        let status = res.status().as_u16();
        let content_type = res
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_owned());
        let raw = res.into_body().collect().await.unwrap().to_bytes().to_vec();
        Res {
            status,
            content_type,
            raw,
        }
    }

    async fn get(&self, path: &str) -> Res {
        self.request("GET", path, &[]).await
    }

    async fn assign(&self, sys_id: &str, team: &str, revision: i64) -> Res {
        self.send(
            "POST",
            &format!("/incidents/{sys_id}/assignment"),
            &[("Content-Type", "application/json")],
            Body::from(json!({"team":team,"expectedRevision":revision}).to_string()),
        )
        .await
    }

    async fn json(&self, path: &str) -> Value {
        let r = self.get(path).await;
        assert_eq!(r.status, 200, "{path}: {}", String::from_utf8_lossy(&r.raw));
        r.json()
    }

    async fn imported(&mut self, recs: &[Record]) {
        self.e.write_json(recs);
        assert!(self.e.tick().await.accepted());
    }

    async fn finish(self) {
        self.e.finish().await;
    }
}

macro_rules! api {
    () => {
        match Api::new().await {
            Some(api) => api,
            None => return,
        }
    };
}

// ================================================================ до первого снимка

#[tokio::test]
async fn status_503_before_first_snapshot() {
    let api = api!();
    for path in [
        "/snapshot",
        "/dashboard",
        "/incidents",
        "/events",
        &format!("/incidents/{DEMO_ID}"),
    ] {
        let r = api.request("GET", path, &[("X-Request-ID", "req-1")]).await;
        assert_eq!(r.status, 503, "{path}");
        let body = r.json();
        assert_eq!(body["code"], "SOURCE_NOT_READY");
        assert_eq!(body["requestId"], "req-1");
        assert_eq!(body["error"], body["message"]);
        assert_eq!(body["source"]["status"], "initializing");
    }
    api.finish().await;
}

#[tokio::test]
async fn form_incident_can_be_viewed_and_assigned_before_first_itsm_snapshot() {
    let api = api!();
    const ANSWER: &str = "4ef12276-4b5e-435e-921c-8cb07d84ae9e";
    let sys_id = format!("web-form-{ANSWER}");
    let raw = json!({
        "id": "INC-WEB-42",
        "sysId": sys_id,
        "header": "Сообщение из формы",
        "stateEnum": "Assigned",
        "priorityId": 2,
        "submitDate": "2026-11-20T12:00:00Z"
    });
    let mut tx = api.e.pool().begin().await.unwrap();
    sqlx::query(
        "INSERT INTO incidents (id, sys_id, header, normalized_state, priority_id, submit_date, raw, content_hash)
         VALUES ('INC-WEB-42', $1, 'Сообщение из формы', 'Assigned', 2, '2026-11-20T12:00:00Z', $2, 'form')",
    )
    .bind(&sys_id)
    .bind(raw)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("INSERT INTO form_incidents VALUES ('incident-web', $1, 'INC-WEB-42')")
        .bind(ANSWER)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::raw_sql(
        "INSERT INTO teams (name) VALUES ('Team A');
         UPDATE import_state SET last_change_at = '2026-11-20T12:00:00Z' WHERE id = 1;
         INSERT INTO dashboard_events (event_type, reason, payload)
         VALUES ('dashboard.changed', 'import', '{}');",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let snapshot = api.json("/snapshot?view=web").await;
    assert_eq!(snapshot["revision"], 1);
    assert_eq!(snapshot["source"]["status"], "initializing");
    assert_eq!(snapshot["items"][0]["header"], "Сообщение из формы");
    assert_eq!(snapshot["items"][0]["dashboard"]["itsmUrl"], Value::Null);

    let assignment = api.assign(&sys_id, "Team A", 1).await;
    assert_eq!(assignment.status, 200, "{:?}", assignment.json());
    assert_eq!(assignment.json()["items"][0]["assignedGroupName"], "Team A");
    api.finish().await;
}

// ================================================================ /snapshot (API.md §3)

#[tokio::test]
async fn snapshot_contract() {
    let mut api = api!();
    api.imported(&records()).await;
    let r = api.get("/snapshot").await;
    assert_eq!(r.status, 200);
    assert!(r.content_type.as_deref().unwrap().starts_with("application/json"));
    let snap = r.json();
    for key in [
        "revision",
        "serverTime",
        "updatedAt",
        "items",
        "events",
        "source",
        "itsmUrlTemplate",
    ] {
        assert!(snap.get(key).is_some(), "{key}");
    }
    assert_eq!(
        (snap["revision"].clone(), snap["eventCursor"].clone()),
        (json!(1), json!("1"))
    );
    assert!(snap["serverTime"].as_str().unwrap().ends_with('Z'));
    assert_eq!(snap["itsmUrlTemplate"], "https://itsm.example/incidents/{sysId}");
    assert_eq!(snap["source"]["error"], Value::Null);
    assert_eq!(snap["source"]["path"], "character_problems.json");
    assert_eq!(snap["events"], json!([])); // первичная загрузка

    // API.md §10.1: выгрузка без потери Research, null-дедлайнов и текста solution
    let items = snap["items"].as_array().unwrap();
    assert_eq!(items.len(), 44);
    for source in records() {
        let mut item = items.iter().find(|i| i["id"] == source["id"]).unwrap().clone();
        item.as_object_mut().unwrap().shift_remove("dashboard");
        assert_eq!(item, Value::Object(source));
    }
    let counts: Vec<(Value, Value)> = snap["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| (g["key"].clone(), g["count"].clone()))
        .collect();
    assert_eq!(
        counts,
        [("critical", 0), ("coordinators", 2), ("high", 3), ("research", 5)]
            .map(|(k, c)| (json!(k), json!(c)))
    );
    let demo = items.iter().find(|i| i["id"] == DEMO_ID).unwrap();
    assert_eq!(
        demo["dashboard"]["itsmUrl"],
        format!("https://itsm.example/incidents/{DEMO_SYS_ID}")
    );
    api.finish().await;
}

#[tokio::test]
async fn deadline_change_bumps_revision_and_events_are_stable() {
    let mut api = api!();
    let mut recs = records();
    api.imported(&recs).await;
    demo(&mut recs).insert("deadline".into(), s("2026-11-25T12:00:00Z"));
    api.imported(&recs).await;
    let first = api.json("/snapshot").await;
    let second = api.json("/snapshot").await;
    assert_eq!(first["revision"], 2);
    let ids = |s: &Value| -> Vec<Value> {
        s["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["eventId"].clone())
            .collect()
    };
    assert_eq!(ids(&first), ids(&second));
    let events = first["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(
        (event["types"].clone(), event["fields"].clone()),
        (json!(["deadline"]), json!(["deadline"]))
    );
    assert_eq!(event["previous"]["deadline"], "2026-11-10T12:32:14Z");
    assert_eq!(event["current"]["deadline"], "2026-11-25T12:00:00Z");
    let dash = api.json("/dashboard").await;
    assert_eq!(dash["eventCursor"], first["revision"].to_string()); // одна последовательность
    api.finish().await;
}

#[tokio::test]
async fn source_error_keeps_items() {
    let mut api = api!();
    api.imported(&records()).await;
    api.e.write_text("[{");
    api.e.tick().await;
    api.e.tick().await;
    let snap = api.json("/snapshot").await;
    assert_eq!(snap["items"].as_array().unwrap().len(), 44);
    assert_eq!(snap["source"]["status"], "error");
    assert!(
        snap["source"]["error"]
            .as_str()
            .unwrap()
            .contains("некорректный JSON")
    );
    api.finish().await;
}

// ================================================================ /dashboard (ARCHITECTURE.md)

#[tokio::test]
async fn dashboard_demo_step_1() {
    let mut api = api!();
    let mut recs = records();
    api.imported(&recs).await;
    update(
        demo(&mut recs),
        &[
            ("priorityId", json!(4)),
            ("priority", s("Наивысший")),
            ("assignedGroupName", s(COORD)),
        ],
    );
    api.imported(&recs).await;
    let dash = api.json("/dashboard").await;
    let group = |key: &str| -> Vec<Value> {
        dash["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["key"] == key)
            .unwrap()["incidentIds"]
            .as_array()
            .unwrap()
            .clone()
    };
    assert!(group("critical").contains(&s(DEMO_ID)) && group("coordinators").contains(&s(DEMO_ID)));
    assert!(!group("high").contains(&s(DEMO_ID)));
    let card = &dash["incidents"][DEMO_ID];
    assert_eq!(card["incidentRevision"], 2);
    assert_eq!(card["status"], "in_progress");
    assert_eq!(card["priorityId"], 4);
    assert_eq!(card["groups"], json!(["critical", "coordinators"]));
    api.finish().await;
}

// ================================================================ /incidents

#[tokio::test]
async fn incident_by_sys_id_and_by_number() {
    let mut api = api!();
    api.imported(&records()).await;
    let by_sys = api.json(&format!("/incidents/{DEMO_SYS_ID}")).await;
    let by_id = api.json(&format!("/incidents/{}", DEMO_ID.to_lowercase())).await;
    assert_eq!(by_sys, by_id);
    assert_eq!(by_sys["incident"]["id"], DEMO_ID);
    assert_eq!(by_sys["normalizedState"], "WorkInProgress");
    assert_eq!(by_sys["revision"], 1);
    assert_eq!(by_sys["details"]["incidentRevision"], 1);
    assert!(
        by_sys["details"]["itsmUrl"]
            .as_str()
            .unwrap()
            .ends_with(DEMO_SYS_ID)
    );
    api.finish().await;
}

#[tokio::test]
async fn unknown_incident_404() {
    let mut api = api!();
    api.imported(&records()).await;
    for key in ["INC000000000000", "123", "nonsense"] {
        let r = api.get(&format!("/incidents/{key}")).await;
        assert_eq!(r.status, 404, "{key}");
        assert_eq!(r.json()["code"], "INCIDENT_NOT_FOUND");
    }
    api.finish().await;
}

#[tokio::test]
async fn incident_list_filters_and_cursor() {
    let mut api = api!();
    let mut recs = records();
    api.imported(&recs).await;
    assert_eq!(
        api.json("/incidents?group=research").await["items"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    let r = api.json("/incidents?priorityId=3&state=WorkInProgress").await;
    let ids: Vec<&Value> = r["items"].as_array().unwrap().iter().map(|i| &i["id"]).collect();
    assert_eq!(ids, [&s(DEMO_ID)]);

    let page1 = api.json("/incidents?limit=20").await;
    let c1 = page1["nextCursor"].as_str().unwrap().to_owned();
    let page2 = api.json(&format!("/incidents?limit=20&cursor={c1}")).await;
    let c2 = page2["nextCursor"].as_str().unwrap().to_owned();
    let page3 = api.json(&format!("/incidents?limit=20&cursor={c2}")).await;
    let len = |p: &Value| p["items"].as_array().unwrap().len();
    assert_eq!(len(&page1) + len(&page2) + len(&page3), 44);
    assert_eq!(page3["nextCursor"], Value::Null);

    demo(&mut recs).insert("header".into(), s("Изменено"));
    api.imported(&recs).await;
    let stale = api.get(&format!("/incidents?limit=20&cursor={c1}")).await;
    assert_eq!(stale.status, 409);
    assert_eq!(stale.json()["code"], "CURSOR_EXPIRED");
    assert_eq!(stale.json()["currentRevision"], 2);
    api.finish().await;
}

#[tokio::test]
async fn incident_list_bad_params() {
    let mut api = api!();
    api.imported(&records()).await;
    for (query, code) in [
        ("priorityId=9", "VALIDATION_ERROR"),
        ("group=vip", "VALIDATION_ERROR"),
        ("sort=color", "INVALID_SORT"),
        ("cursor=%25%25%25", "INVALID_CURSOR"),
    ] {
        let r = api.get(&format!("/incidents?{query}")).await;
        assert_eq!(r.status, 400, "{query}");
        assert_eq!(r.json()["code"], code, "{query}");
    }
    api.finish().await;
}

#[tokio::test]
async fn rules_brief_tracks_this_incidents_actions_and_current_outcome() {
    let mut api = api!();
    let mut recs = records();
    recs.truncate(2);
    update(
        &mut recs[0],
        &[
            ("shortDescription", s("  ")),
            ("description", s("Очередь чеков остановилась")),
            ("stateEnum", s("Research")),
            ("stateId", json!(20)),
            ("solution", s("Перезапустили очередь")),
            ("assignedGroupName", s("")),
        ],
    );
    api.imported(&recs).await;
    let path = format!("/incidents/{}/analysis", recs[0]["sysId"].as_str().unwrap());
    let summary = api.json(&path).await["summary"].as_str().unwrap().to_owned();
    assert_eq!(
        summary,
        "Очередь чеков остановилась. В поле решения указано «Перезапустили очередь». Идёт исследование причин."
    );

    update(
        &mut recs[0],
        &[("stateEnum", s("WorkInProgress")), ("stateId", json!(2))],
    );
    update(&mut recs[1], &[("assignedGroupName", s("Другая команда"))]);
    api.imported(&recs).await;
    let analysis = api.json(&path).await;
    assert_eq!(analysis["provider"], "rules");
    assert_eq!(
        analysis["summary"],
        "Очередь чеков остановилась. В поле решения указано «Перезапустили очередь». Инцидент снова в работе, результат пока не подтверждён."
    );

    update(&mut recs[0], &[("solution", s(""))]);
    api.imported(&recs).await;
    let analysis = api.json(&path).await;
    assert_eq!(
        analysis["summary"],
        "Очередь чеков остановилась. Ранее в карточке указали «Перезапустили очередь». Инцидент снова в работе, результат пока не подтверждён."
    );

    update(
        &mut recs[0],
        &[
            ("stateEnum", s("Closed")),
            ("stateId", json!(10)),
            ("solution", s("Обновили обработчик")),
        ],
    );
    api.imported(&recs).await;
    let analysis = api.json(&path).await;
    assert_eq!(
        analysis["summary"],
        "Очередь чеков остановилась. В поле решения указано «Обновили обработчик». Инцидент закрыт."
    );
    api.finish().await;
}

#[tokio::test]
async fn web_queue_filters_before_paging_and_analysis_uses_completed_evidence() {
    let mut api = api!();
    let mut recs = records();
    recs.truncate(4);
    for (idx, r) in recs.iter_mut().enumerate() {
        update(
            r,
            &[
                ("header", s("Ошибка камеры")),
                ("priorityId", json!(4)),
                ("service", s("scan")),
                ("stateEnum", s("Assigned")),
                ("stateId", json!(0)),
                ("assignedGroupName", s("")),
                ("deadline", s("2026-11-20T12:30:00Z")),
                ("id", s(&format!("INC{idx}"))),
            ],
        );
    }
    update(
        &mut recs[2],
        &[("stateEnum", s("Research")), ("stateId", json!(20))],
    );
    update(
        &mut recs[3],
        &[
            ("stateEnum", s("Closed")),
            ("stateId", json!(10)),
            ("assignedGroupName", s("Команда А")),
            ("completionDate", s("2026-11-20T12:00:00Z")),
        ],
    );
    api.imported(&recs).await;

    let path = "/incidents?group=critical&pageSize=1&page=99&priority=4&status=Assigned&team=__unassigned__&sla=soon&q=sCan&sort=deadline";
    let page = api.json(path).await;
    assert_eq!(page["total"], 2);
    assert_eq!(page["totalPages"], 2);
    assert_eq!(page["page"], 2);
    assert_eq!(page["pageSize"], 1);
    assert_eq!(page["items"][0]["id"], "INC1");
    let empty = api.json("/incidents?page=2&group=critical&sla=closed").await;
    assert_eq!(empty["page"], 1);
    assert_eq!(empty["totalPages"], 1);
    assert_eq!(empty["items"], json!([]));
    let research = api.json("/incidents?group=all&sla=research&sort=newest").await;
    assert_eq!(research["items"][0]["id"], "INC2");

    let sys_id = recs[0]["sysId"].as_str().unwrap();
    let analysis = api.json(&format!("/incidents/{sys_id}/analysis")).await;
    assert_eq!(analysis["sysId"], sys_id);
    assert_eq!(analysis["revision"], page["revision"]);
    assert_eq!(analysis["provider"], "rules");
    assert_eq!(analysis["candidates"][0]["team"], "Команда А");
    assert_eq!(analysis["candidates"][0]["evidenceIds"], json!(["INC3"]));
    assert_eq!(api.get("/incidents/unknown/analysis").await.status, 404);
    api.finish().await;
}

#[tokio::test]
async fn assignments_are_atomic_and_reconciled_with_subsequent_imports() {
    let mut api = api!();
    let mut recs = records();
    recs.truncate(4);
    let initial_team = "Duty";
    for (row, (team, state, state_id)) in recs.iter_mut().zip([
        (initial_team, "Assigned", 0),
        ("Team A", "WorkInProgress", 2),
        ("Team B", "Closed", 10),
        ("AutoClosedMi", "Closed", 10),
    ]) {
        update(
            row,
            &[
                ("assignedGroupName", s(team)),
                ("stateEnum", s(state)),
                ("stateId", json!(state_id)),
            ],
        );
    }
    update(
        &mut recs[0],
        &[
            ("assignedEmployee", s("Duty operator")),
            ("assignedEmployeeLogin", s("operator")),
        ],
    );
    api.imported(&recs).await;
    let id = recs[0]["sysId"].as_str().unwrap().to_owned();
    let closed = recs[2]["sysId"].as_str().unwrap();
    let teams = api.json("/teams").await;
    assert_eq!(teams["revision"], 1);
    let items = teams["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    for (item, (name, count)) in items.iter().zip([("Duty", 1), ("Team A", 1), ("Team B", 0)]) {
        assert_eq!(item["name"], name);
        assert_eq!(item["activeCount"], count);
        assert_eq!(item["phoneIsTest"], true);
        assert!(item["phone"].as_str().unwrap().starts_with("+7819"));
    }
    let duty_phone = items[0]["phone"].clone();
    assert_eq!(api.assign(closed, "Team A", 1).await.status, 409);
    assert_eq!(api.assign(&id, "Unknown", 1).await.status, 400);
    assert_eq!(api.assign("missing", "Team B", 1).await.status, 404);

    let (left, right) = tokio::join!(api.assign(&id, "Team A", 1), api.assign(&id, "Team B", 1),);
    let (ok, conflict, winning_team) = if left.status == 200 {
        (left, right, "Team A")
    } else {
        (right, left, "Team B")
    };
    assert_eq!(ok.status, 200, "{}", ok.json());
    assert_eq!(conflict.status, 409);
    assert_eq!(conflict.json()["currentRevision"], 2);
    let snapshot = ok.json();
    assert_eq!(snapshot["revision"], 2);
    assert_eq!(snapshot["items"].as_array().unwrap().len(), recs.len());
    let assigned = snapshot["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["sysId"] == id)
        .unwrap();
    assert_eq!(assigned["assignedGroupName"], winning_team);
    assert_eq!(assigned["assignedEmployee"], "");
    assert_eq!(assigned["assignedEmployeeLogin"], "");
    assert_eq!(assigned["stateEnum"], "Assigned");
    assert_ne!(
        assigned["lastAssignmentToTeamDateTime"],
        recs[0]["lastAssignmentToTeamDateTime"]
    );
    let events = snapshot["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["revision"] == 2)
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["previous"]["assignedGroupName"], initial_team);
    assert_eq!(events[0]["current"]["assignedGroupName"], winning_team);
    assert!(events[0]["types"].as_array().unwrap().contains(&s("team")));

    // Reimport with fresh source data while the source assignment itself is unchanged.
    recs[1].insert("header".into(), s("Source-side update"));
    api.imported(&recs).await;
    let snapshot = api.json("/snapshot").await;
    let retained = snapshot["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["sysId"] == id)
        .unwrap();
    assert_eq!(retained["assignedGroupName"], winning_team);
    assert_eq!(
        retained["lastAssignmentToTeamDateTime"],
        assigned["lastAssignmentToTeamDateTime"]
    );

    update(
        &mut recs[0],
        &[
            ("assignedGroupName", s("External")),
            ("assignedEmployee", s("External operator")),
        ],
    );
    api.imported(&recs).await;
    let snapshot = api.json("/snapshot").await;
    let external = snapshot["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["sysId"] == id)
        .unwrap();
    assert_eq!(external["assignedGroupName"], "External");
    assert_eq!(external["assignedEmployee"], "External operator");
    let teams = api.json("/teams").await;
    assert_eq!(teams["revision"], snapshot["revision"]);
    let items = teams["items"].as_array().unwrap();
    assert!(
        items
            .iter()
            .any(|item| item["name"] == "Duty" && item["activeCount"] == 0 && item["phone"] == duty_phone)
    );
    assert!(
        items.iter().any(|item| item["name"] == "External"
            && item["activeCount"] == 1
            && item["phoneIsTest"] == true)
    );
    api.finish().await;
}

// ================================================================ /events

#[tokio::test]
async fn events_journal_filters_and_paging() {
    let mut api = api!();
    let mut recs = records();
    api.imported(&recs).await;
    for n in 0..3 {
        demo(&mut recs).insert("deadline".into(), s(&format!("2026-11-2{}T12:00:00Z", n + 1)));
        api.imported(&recs).await;
    }
    demo(&mut recs).insert("priorityId".into(), json!(4));
    api.imported(&recs).await;

    let all = api.json("/events").await["items"].as_array().unwrap().clone();
    let types: Vec<&Value> = all.iter().map(|e| &e["types"]).collect();
    assert_eq!(
        types,
        [
            &json!(["critical", "priority"]),
            &json!(["deadline"]),
            &json!(["deadline"]),
            &json!(["deadline"])
        ]
    );
    let ids: Vec<i64> = all.iter().map(|e| e["eventId"].as_i64().unwrap()).collect();
    assert!(ids.windows(2).all(|w| w[0] > w[1]));

    let page = api.json("/events?limit=2").await;
    let page_ids: Vec<i64> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["eventId"].as_i64().unwrap())
        .collect();
    assert_eq!(page_ids, ids[..2]);
    assert_eq!(page["nextCursor"], ids[1].to_string());
    let rest = api.json(&format!("/events?beforeId={}", ids[1])).await;
    let rest_ids: Vec<i64> = rest["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["eventId"].as_i64().unwrap())
        .collect();
    assert_eq!(rest_ids, ids[2..]);
    assert_eq!(rest["nextCursor"], Value::Null);

    let count = |v: Value| v["items"].as_array().unwrap().len();
    assert_eq!(count(api.json("/events?type=critical").await), 1);
    assert_eq!(count(api.json(&format!("/events?afterId={}", ids[2])).await), 2);
    assert_eq!(
        count(api.json(&format!("/events?incidentSysId={DEMO_SYS_ID}")).await),
        4
    );
    assert_eq!(count(api.json("/events?incidentSysId=1").await), 0);
    let history = api.json(&format!("/incidents/{DEMO_SYS_ID}/events")).await;
    let history_ids: Vec<i64> = history["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["eventId"].as_i64().unwrap())
        .collect();
    assert_eq!(history_ids, ids);

    assert_eq!(api.get("/events?type=bogus").await.status, 400);
    assert_eq!(
        api.get("/events?beforeId=x").await.json()["code"],
        "INVALID_CURSOR"
    );
    api.finish().await;
}

// ================================================================ /notifications

#[tokio::test]
async fn notifications_api() {
    let mut api = api!();
    let mut recs = records();
    api.imported(&recs).await;
    update(
        demo(&mut recs),
        &[("priorityId", json!(4)), ("assignedGroupName", s(COORD))],
    );
    api.imported(&recs).await;

    let page = api.json("/notifications").await;
    let types: Vec<&Value> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| &n["type"])
        .collect();
    assert_eq!(types, [&s("coordinator.assigned"), &s("critical.raised")]);
    assert_eq!(page["unreadCount"], 2);
    assert_eq!(page["nextCursor"], Value::Null);
    assert_eq!(page["items"][0]["incidentId"], DEMO_ID);
    assert_eq!(page["items"][0]["isRead"], false);
    let target = page["items"][0]["id"].as_i64().unwrap();

    for _ in 0..2 {
        // идемпотентно
        let r = api
            .request(
                "PUT",
                &format!("/notifications/{target}/read"),
                &[("X-User-Id", "alice")],
            )
            .await;
        assert_eq!(r.status, 204);
        assert!(r.raw.is_empty());
    }
    let alice = api
        .request("GET", "/notifications", &[("X-User-Id", "alice")])
        .await
        .json();
    let default = api.json("/notifications").await;
    assert_eq!(
        (alice["unreadCount"].clone(), default["unreadCount"].clone()),
        (json!(1), json!(2))
    );
    let read = alice["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == target)
        .unwrap();
    assert_ne!(read["readAt"], Value::Null);

    assert_eq!(
        api.request("PUT", "/notifications/999999/read", &[]).await.status,
        404
    );
    assert_eq!(api.get("/notifications?cursor=abc").await.status, 400);
    assert_eq!(
        api.request("GET", "/notifications", &[("X-User-Id", "bad user!")])
            .await
            .status,
        400
    );
    let page1 = api.json("/notifications?limit=1").await;
    let page2 = api
        .json(&format!(
            "/notifications?limit=1&cursor={}",
            page1["nextCursor"].as_str().unwrap()
        ))
        .await;
    assert!(page1["items"][0]["id"].as_i64() > page2["items"][0]["id"].as_i64());
    assert_eq!(page2["nextCursor"], Value::Null);
    api.finish().await;
}

/// `test_notification_list_cursor_and_reads` (этап 7): курсор, отметки у каждого своё,
/// повторная отметка не меняет время прочтения.
#[tokio::test]
async fn notification_list_cursor_and_reads() {
    let mut api = api!();
    let mut recs = records();
    api.imported(&recs).await;
    for group in [COORD, "ОГЭ Приморье", COORD, "ОГЭ Приморье", COORD] {
        demo(&mut recs).insert("assignedGroupName".into(), s(group)); // 3 назначения
        api.imported(&recs).await;
    }
    let alice = [("X-User-Id", "alice")];
    let page1 = api.request("GET", "/notifications?limit=2", &alice).await.json();
    let items = page1["items"].as_array().unwrap();
    assert!(items.iter().all(|n| n["type"] == "coordinator.assigned") && items.len() == 2);
    assert!(items[0]["id"].as_i64() > items[1]["id"].as_i64()); // новые первыми
    assert_eq!(page1["unreadCount"], 3);
    assert_eq!(page1["nextCursor"], items[1]["id"].to_string());
    let page2 = api
        .request(
            "GET",
            &format!(
                "/notifications?limit=2&cursor={}",
                page1["nextCursor"].as_str().unwrap()
            ),
            &alice,
        )
        .await
        .json();
    assert_eq!(page2["items"].as_array().unwrap().len(), 1);
    assert_eq!(page2["nextCursor"], Value::Null);

    let target = items[0]["id"].as_i64().unwrap();
    let read_at = format!("SELECT read_at FROM notification_reads WHERE notification_id = {target}");
    for _ in 0..2 {
        api.request("PUT", &format!("/notifications/{target}/read"), &alice)
            .await;
    }
    let first_read = api.e.time(&read_at).await;
    api.e.clock.advance(60.0);
    api.request("PUT", &format!("/notifications/{target}/read"), &alice)
        .await;
    assert_eq!(api.e.time(&read_at).await, first_read);

    let alice_page = api.request("GET", "/notifications", &alice).await.json();
    let bob_page = api
        .request("GET", "/notifications", &[("X-User-Id", "bob")])
        .await
        .json();
    assert_eq!(
        (alice_page["unreadCount"].clone(), bob_page["unreadCount"].clone()),
        (json!(2), json!(3))
    );
    let n = alice_page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == target)
        .unwrap();
    assert_eq!(n["isRead"], true);
    assert!(
        bob_page["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["isRead"] == false)
    );
    let missing = api.request("PUT", "/notifications/999999/read", &alice).await;
    assert_eq!(missing.json()["code"], "NOTIFICATION_NOT_FOUND");
    api.finish().await;
}

#[tokio::test]
async fn source_notifications_listed() {
    let mut api = api!();
    api.imported(&records()).await;
    api.e.write_text("{broken");
    api.e.tick().await;
    api.e.tick().await;
    api.imported(&records()).await;
    let page = api.json("/notifications").await;
    let types: Vec<&Value> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| &n["type"])
        .collect();
    assert_eq!(types, [&s("source.recovered"), &s("source.error")]);
    assert_eq!(page["items"][1]["incidentId"], Value::Null);
    api.finish().await;
}

#[tokio::test]
async fn shift_statistics_uses_only_historical_dashboard_assignments() {
    let Some(mut api) = Api::with_vars(&[("SOURCE_NAIVE_TIMEZONE", "Europe/Moscow")]).await else {
        return;
    };
    let range = "/statistics/shift?from=2026-11-20T12:00:01Z&to=2026-11-20T12:00:06Z";
    let empty = api.json(range).await;
    assert_eq!(empty["transfers"], json!([]));
    assert_eq!(empty["averageMs"], Value::Null);
    assert_eq!(empty["kpiPercent"], Value::Null);

    let mut recs = records();
    recs.truncate(4);
    for (record, (team, priority)) in recs
        .iter_mut()
        .zip([("", 4), (COORD, 3), ("Team B", 2), ("Team A", 1)])
    {
        update(
            record,
            &[
                ("stateEnum", s("Assigned")),
                ("stateId", json!(0)),
                ("priorityId", json!(priority)),
                ("assignedGroupName", s(team)),
                ("submitDate", s("2026-11-20 14:50:02")),
                ("lastAssignmentToTeamDateTime", s("2026-11-20 14:50:02")),
            ],
        );
    }
    update(&mut recs[0], &[("header", s("Название во время назначения"))]);
    api.imported(&recs).await;
    let revision = api.json("/snapshot").await["revision"].as_i64().unwrap();
    for (index, incident) in recs.iter().take(3).enumerate() {
        let key = incident["sysId"].as_str().unwrap();
        let res = api.assign(key, "Team A", revision + index as i64).await;
        assert_eq!(res.status, 200, "{}", res.json());
        api.e.clock.advance(1.0);
    }

    // A subsequent ITSM import changes the title and priority and assigns a
    // different incident. The report keeps the previous values and excludes imports.
    update(
        &mut recs[0],
        &[("header", s("Новое название")), ("priorityId", json!(2))],
    );
    update(&mut recs[3], &[("assignedGroupName", s("External"))]);
    api.imported(&recs).await;

    let stats = api.json(range).await;
    assert_eq!(stats["source"], "api");
    assert_eq!(stats["coordinator"]["id"], "shift");
    assert_eq!(stats["transferred"], 3);
    assert_eq!(stats["critical"], 1);
    assert_eq!(stats["measured"], 2);
    assert_eq!(stats["kpiMet"], 1);
    assert_eq!(stats["kpiPercent"].as_f64(), Some(50.0));
    assert_eq!(stats["averageMs"].as_f64(), Some(599_500.0));
    let transfers = stats["transfers"].as_array().unwrap();
    assert_eq!(transfers[0]["durationMs"], Value::Null);
    assert_eq!(transfers[0]["kpiMet"], Value::Null);
    assert_eq!(transfers[0]["reassignmentMs"], 601_000);
    assert_eq!(transfers[1]["durationMs"], 600_000);
    assert_eq!(transfers[1]["kpiMet"], false);
    assert_eq!(transfers[1]["reassignmentMs"], Value::Null);
    assert_eq!(transfers[2]["durationMs"], 599_000);
    assert_eq!(transfers[2]["kpiMet"], true);
    assert_eq!(transfers[2]["reassignmentMs"], Value::Null);
    assert_eq!(transfers[2]["title"], "Название во время назначения");
    assert_eq!(transfers[2]["team"], "Team A");

    let bounded = api
        .json("/statistics/shift?from=2026-11-20T15:00:02%2B03:00&to=2026-11-20T12:00:03Z")
        .await;
    assert_eq!(bounded["transferred"], 1);
    assert_eq!(bounded["transfers"][0]["id"], transfers[1]["id"]);
    api.finish().await;
}

#[tokio::test]
async fn openapi_lists_all_routes() {
    let api = api!();
    let spec = api.json("/openapi.json").await;
    for path in [
        "/api/v1/snapshot",
        "/api/v1/dashboard",
        "/api/v1/incidents",
        "/api/v1/incidents/{key}",
        "/api/v1/incidents/{key}/events",
        "/api/v1/events",
        "/api/v1/notifications",
        "/api/v1/notifications/{notification_id}/read",
    ] {
        assert!(spec["paths"].get(path).is_some(), "{path}");
    }
    assert_eq!(spec["info"]["title"], api.e.settings.app_name.as_str());
    api.finish().await;
}

// ================================================================ демо-режим фронтенда

async fn demo_send(api: &Api, method: &str, path: &str, body: &Value) -> Res {
    api.send(
        method,
        path,
        &[("Content-Type", "application/json")],
        Body::from(body.to_string()),
    )
    .await
}

fn source_json(api: &Api) -> Value {
    serde_json::from_slice(&std::fs::read(&api.e.source).unwrap()).unwrap()
}

#[tokio::test]
async fn demo_routes_are_disabled_by_default() {
    let api = api!();
    let res = demo_send(&api, "POST", "/demo", &json!({"action":"deadline"})).await;
    assert_eq!(res.status, 404);
    let res = demo_send(&api, "PUT", "/incidents", &json!([])).await;
    assert_eq!(res.status, 405);
    assert_eq!(res.json()["code"], "METHOD_NOT_ALLOWED");
    api.finish().await;
}

#[tokio::test]
async fn demo_import_replaces_the_source_and_scenarios_publish_new_revisions() {
    let Some(mut api) = Api::with_vars(&[("DEMO_API_ENABLED", "true")]).await else {
        return;
    };
    let mut recs = records();
    api.imported(&recs).await;
    let first = api.json("/snapshot?view=web").await["revision"].as_i64().unwrap();

    recs.truncate(5);
    let body = Value::Array(recs.iter().cloned().map(Value::Object).collect());
    let res = demo_send(&api, "PUT", "/incidents", &body).await;
    assert_eq!(res.status, 200, "{}", String::from_utf8_lossy(&res.raw));
    let replaced = res.json();
    let second = replaced["revision"].as_i64().unwrap();
    assert!(second > first);
    assert_eq!(replaced["items"].as_array().unwrap().len(), 5);
    assert_eq!(source_json(&api), body);

    let rejected = demo_send(&api, "PUT", "/incidents", &json!([{"id":"INC1"}])).await;
    assert_eq!(rejected.status, 400);
    assert_eq!(rejected.json()["code"], "SNAPSHOT_REJECTED");
    assert_eq!(
        source_json(&api),
        body,
        "rejected payload must not touch the source"
    );

    let res = demo_send(&api, "POST", "/demo", &json!({"action":"mass"})).await;
    assert_eq!(res.status, 200, "{}", String::from_utf8_lossy(&res.raw));
    let mass = res.json();
    assert!(mass["revision"].as_i64().unwrap() > second);
    assert_eq!(mass["items"].as_array().unwrap().len(), 8);
    assert_eq!(source_json(&api).as_array().unwrap().len(), 8);

    let res = demo_send(&api, "POST", "/demo", &json!({"action":"deadline"})).await;
    assert_eq!(res.status, 200, "{}", String::from_utf8_lossy(&res.raw));

    let unknown = demo_send(&api, "POST", "/demo", &json!({"action":"explode"})).await;
    assert_eq!(unknown.status, 400);
    assert_eq!(unknown.json()["code"], "UNKNOWN_DEMO_ACTION");

    let reset = demo_send(&api, "POST", "/demo", &json!({"action":"reset"})).await;
    assert_eq!(reset.status, 409);
    assert_eq!(reset.json()["code"], "DEMO_SEED_NOT_CONFIGURED");
    api.finish().await;
}

#[tokio::test]
async fn demo_reset_restores_the_seed() {
    let seed = std::env::temp_dir().join(format!("demo_seed_{}.json", uuid_like()));
    let mut recs = records();
    recs.truncate(3);
    let seed_body = Value::Array(recs.iter().cloned().map(Value::Object).collect());
    std::fs::write(&seed, seed_body.to_string()).unwrap();
    let seed_str = seed.to_string_lossy().into_owned();
    let Some(mut api) = Api::with_vars(&[
        ("DEMO_API_ENABLED", "true"),
        ("DEMO_SEED_PATH", seed_str.as_str()),
    ])
    .await
    else {
        let _ = std::fs::remove_file(&seed);
        return;
    };
    api.imported(&records()).await;
    let res = demo_send(&api, "POST", "/demo", &json!({"action":"reset"})).await;
    assert_eq!(res.status, 200, "{}", String::from_utf8_lossy(&res.raw));
    assert_eq!(res.json()["items"].as_array().unwrap().len(), 3);
    assert_eq!(source_json(&api), seed_body);
    let _ = std::fs::remove_file(&seed);
    api.finish().await;
}

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    format!(
        "{}_{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    )
}
