//! Exercise the production importer with a loopback relay and isolated PostgreSQL.

mod common;

use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use common::{COORD, DEMO_ID, Env, demo, records};
use dashboard_importer::{assignment, run_importer};
use dashboard_store::import_repo;
use serde_json::{Value, json};
use sqlx::Row;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

const UUID: &str = "b3226f3f-e47b-41c4-b0c1-0d02d4cfaf2a";
const HEADER: &str = "Недоступен вход";
const DESCRIPTION: &str = "Пользователи видят ошибку при входе.";
const SERVICE: &str = "Личный кабинет";
const TOKEN: &str = "test-relay-token";

fn submission(id: i64, form_id: &str) -> Value {
    json!({
        "id": id, "form_id": form_id, "answer_id": UUID,
        "received_at": "2026-11-20T12:34:56.000+00:00",
        "payload": {
            "form_id": form_id, "answer_id": UUID,
            "answers": {
                "title": HEADER, "description": DESCRIPTION, "service": SERVICE,
                "priority": "P1", "priorityId": 4
            }
        }
    })
}

async fn relay_reply(mut stream: TcpStream, first_request: &mut bool) -> i64 {
    let mut reader = BufReader::new(&mut stream);
    let mut request = String::new();
    assert_ne!(reader.read_line(&mut request).await.unwrap(), 0);
    let target = request
        .strip_prefix("GET /v1/submissions?")
        .unwrap()
        .split(' ')
        .next()
        .unwrap();
    let after = target
        .split('&')
        .find_map(|part| part.strip_prefix("after="))
        .unwrap()
        .parse::<i64>()
        .unwrap();
    let mut authorized = false;
    loop {
        let mut header = String::new();
        assert_ne!(reader.read_line(&mut header).await.unwrap(), 0);
        if header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            authorized |=
                name.eq_ignore_ascii_case("authorization") && value.trim() == format!("Bearer {TOKEN}");
        }
    }
    assert!(authorized);

    let (status, body) = match after {
        0 if std::mem::take(first_request) => ("503 Service Unavailable", json!({})),
        0 => (
            "200 OK",
            json!({"items": [submission(7, "incident-web")], "next_after": 7, "has_more": true}),
        ),
        7 => (
            "200 OK",
            // The legacy form is ignored. The repeated answer UUID is already imported.
            json!({
                "items": [submission(8, "legacy-yandex-form"), submission(9, "incident-web")],
                "next_after": 9, "has_more": false
            }),
        ),
        9 => ("200 OK", json!({"items": [], "next_after": 9, "has_more": false})),
        _ => panic!("unexpected relay cursor {after}"),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.to_string().len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
    after
}

fn run_relay(listener: TcpListener) -> (JoinHandle<()>, mpsc::UnboundedReceiver<i64>) {
    let (sender, receiver) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let mut first_request = true;
        while let Ok((stream, _)) = listener.accept().await {
            let after = relay_reply(stream, &mut first_request).await;
            if sender.send(after).is_err() {
                break;
            }
        }
    });
    (server, receiver)
}

async fn next_request(requests: &mut mpsc::UnboundedReceiver<i64>) -> i64 {
    timeout(Duration::from_secs(6), requests.recv())
        .await
        .expect("relay request timeout")
        .expect("relay closed")
}

async fn stop_importer(stop: watch::Sender<bool>, importer: JoinHandle<Result<(), sqlx::Error>>) {
    stop.send(true).unwrap();
    timeout(Duration::from_secs(6), importer)
        .await
        .expect("importer shutdown timeout")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn form_incident_survives_retry_restart_and_replacement_of_itsm_export() {
    if std::env::var_os("TEST_DATABASE_URL").is_none() {
        eprintln!("skip: TEST_DATABASE_URL is not set");
        return;
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1/submissions", listener.local_addr().unwrap());
    let Some(mut env) = Env::with_vars(&[
        ("ENVIRONMENT", "test"),
        ("INCIDENT_FORM_RELAY_URL", &endpoint),
        ("INCIDENT_FORM_READ_TOKEN", TOKEN),
        ("INCIDENT_FORM_POLL_INTERVAL_SECONDS", "1"),
    ])
    .await
    else {
        return;
    };
    let itsm_incident = demo(&mut records()).clone();
    env.write_json(&[itsm_incident]);
    let initial = env.tick().await;
    assert_eq!((initial.created, initial.revision), (1, Some(1)));

    let (relay, mut requests) = run_relay(listener);
    let (stop, running) = watch::channel(false);
    let importer = tokio::spawn(run_importer(Arc::clone(&env.settings), running));
    assert_eq!(next_request(&mut requests).await, 0); // 503
    assert_eq!(next_request(&mut requests).await, 0); // Retried from the same cursor.
    assert_eq!(next_request(&mut requests).await, 7); // Read the next page immediately.
    timeout(Duration::from_secs(6), async {
        loop {
            let mut conn = env.pool().acquire().await.unwrap();
            if import_repo::forms_cursor(&mut conn).await.unwrap() == 9 {
                break;
            }
            drop(conn);
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("form cursor did not reach 9");
    stop_importer(stop, importer).await;
    while requests.try_recv().is_ok() {}

    let report = sqlx::query(
        "SELECT id, sys_id, raw, submit_date, normalized_state, priority_id, assigned_group_name
         FROM incidents WHERE header = $1",
    )
    .bind(HEADER)
    .fetch_one(env.pool())
    .await
    .unwrap();
    let id: String = report.get("id");
    let sys_id: String = report.get("sys_id");
    let raw: Value = report.get("raw");
    assert_eq!(raw["description"], DESCRIPTION);
    assert_eq!(raw["service"], SERVICE);
    assert_eq!(
        report.get::<chrono::DateTime<Utc>, _>("submit_date"),
        Utc.with_ymd_and_hms(2026, 11, 20, 12, 34, 56).unwrap()
    );
    assert_eq!(report.get::<String, _>("normalized_state"), "Assigned");
    assert_eq!(report.get::<i16, _>("priority_id"), 4);
    assert_eq!(report.get::<String, _>("assigned_group_name"), COORD);
    assert_eq!(
        env.i64("SELECT count(*) FROM incidents").await,
        2,
        "only one form incident plus the imported ITSM incident"
    );
    let created_revisions: Vec<i64> =
        sqlx::query_scalar("SELECT revision FROM change_events WHERE sys_id = $1 AND 'created' = ANY(types)")
            .bind(&sys_id)
            .fetch_all(env.pool())
            .await
            .unwrap();
    assert_eq!(created_revisions, [2]);

    let mut tx = env.pool().begin().await.unwrap();
    import_repo::lock_state(&mut tx).await.unwrap();
    sqlx::query("INSERT INTO teams (name) VALUES ('Team Form') ON CONFLICT DO NOTHING")
        .execute(&mut *tx)
        .await
        .unwrap();
    let previous = import_repo::stored_by_sys_id(&mut tx, &sys_id)
        .await
        .unwrap()
        .unwrap();
    assignment::record_assignment(&mut tx, &env.settings, &previous, "Team Form", Utc::now(), None)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    env.write_json(&[]);
    let replacement = env.tick().await;
    assert_eq!((replacement.removed, replacement.revision), (1, Some(4)));
    assert_eq!(
        env.i64("SELECT count(*) FROM incidents WHERE is_present").await,
        1
    );
    let itsm_present: bool = sqlx::query_scalar("SELECT is_present FROM incidents WHERE id = $1")
        .bind(DEMO_ID)
        .fetch_one(env.pool())
        .await
        .unwrap();
    assert!(!itsm_present);
    let preserved_assignment: (String, String) = sqlx::query_as(
        "SELECT i.assigned_group_name, a.team FROM incidents i
         JOIN assignment_overrides a ON a.incident_id = i.id
         WHERE i.id = $1 AND i.is_present",
    )
    .bind(&id)
    .fetch_one(env.pool())
    .await
    .unwrap();
    assert_eq!(preserved_assignment, ("Team Form".into(), "Team Form".into()));

    let (stop, running) = watch::channel(false);
    let restarted = tokio::spawn(run_importer(Arc::clone(&env.settings), running));
    assert_eq!(next_request(&mut requests).await, 9);
    stop_importer(stop, restarted).await;
    assert_eq!(env.i64("SELECT max(id) FROM dashboard_events").await, 4);
    assert_eq!(env.i64("SELECT count(*) FROM incidents").await, 2);
    relay.abort();
    env.finish().await;
}
