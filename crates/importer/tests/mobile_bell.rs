mod common;

use chrono::{DateTime, Duration, Utc};
use common::{Env, Record, update};
use dashboard_importer::assignment;
use dashboard_store::{bell, import_repo};
use serde_json::{Value, json};

async fn register(env: &Env, login: &str, platform: &str) {
    sqlx::query(
        "INSERT INTO coordinator_accounts(login, display_name, password_hash, key_hash)
         VALUES ($1, $1, 'test', sha256(convert_to($1, 'UTF8')))",
    )
    .bind(login)
    .execute(env.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO mobile_devices(account_login, platform, fcm_token, secret_hash)
         VALUES ($1, $2, $1, sha256(convert_to($1, 'UTF8')))",
    )
    .bind(login)
    .bind(platform)
    .execute(env.pool())
    .await
    .unwrap();
}

async fn start(env: &Env) -> DateTime<Utc> {
    // Both subscriptions precede the simulated clock. Stay inside the real one-hour delivery window.
    register(env, "android", "android").await;
    register(env, "iphone", "web").await;
    let now = sqlx::query_scalar::<_, DateTime<Utc>>("SELECT date_trunc('second', now())")
        .fetch_one(env.pool())
        .await
        .unwrap();
    let anchor = now - Duration::minutes(2);
    let paired = anchor - Duration::minutes(1);
    sqlx::query("UPDATE mobile_devices SET created_at = $1, fcm_registered_at = $1")
        .bind(paired)
        .execute(env.pool())
        .await
        .unwrap();
    env.clock.advance((anchor - env.clock.now()).num_seconds() as f64);
    anchor
}

fn incident(id: &str, now: DateTime<Utc>, team: &str) -> Record {
    let mut row = common::records().remove(0);
    update(
        &mut row,
        &[
            ("id", json!(format!("INC{id}"))),
            ("sysId", json!(id)),
            ("stateEnum", json!("Assigned")),
            ("stateId", json!(0)),
            ("priorityId", json!(1)),
            ("assignedGroupName", json!(team)),
            ("lastAssignmentToTeamDateTime", Value::Null),
            ("submitDate", json!(now - Duration::minutes(1))),
            ("deadline", Value::Null),
        ],
    );
    row
}

async fn assert_events(env: &Env, expected: &[(&str, &str)]) {
    let events: Vec<(String, String, Vec<String>)> = sqlx::query_as(
        "SELECT coalesce(q.bell_incident_id, n.incident_id), coalesce(q.bell_kind, n.type),
                array_agg(d.account_login ORDER BY d.account_login)
         FROM mobile_push_deliveries q JOIN mobile_devices d ON d.id = q.device_id
         LEFT JOIN notifications n ON n.id = q.journal_notification_id
         GROUP BY q.notification_id, 1, 2 ORDER BY q.notification_id",
    )
    .fetch_all(env.pool())
    .await
    .unwrap();
    assert_eq!(events.len(), expected.len(), "{events:?}");
    for ((id, kind, recipients), &(expected_id, expected_kind)) in events.iter().zip(expected) {
        assert_eq!((id.as_str(), kind.as_str()), (expected_id, expected_kind));
        assert_eq!(
            recipients,
            &["android", "iphone"],
            "one job per registered device for {id}"
        );
    }
}

#[tokio::test]
async fn clock_crossings_emit_once_without_an_import_and_skip_late_devices() {
    let Some(mut env) = Env::new().await else { return };
    let start = start(&env).await;
    let mut soon = incident("101", start, "Team A");
    update(
        &mut soon,
        &[
            ("submitDate", json!(start - Duration::seconds(80))),
            ("deadline", json!(start + Duration::seconds(20))),
        ],
    );
    let mut waiting = incident("102", start, common::COORD);
    waiting.insert(
        "submitDate".into(),
        json!(start - Duration::minutes(8) + Duration::seconds(3)),
    );
    env.write_json(&[soon, waiting]);
    assert!(env.tick_after(0.0).await.accepted());
    let revision = env.i64("SELECT max(id) FROM dashboard_events").await;
    register(&env, "subscribed-after-crossing", "web").await;
    bell::process_due(env.pool(), start).await.unwrap();
    assert_events(&env, &[]).await;
    bell::process_due(env.pool(), start + Duration::milliseconds(1))
        .await
        .unwrap();
    bell::process_due(env.pool(), start + Duration::seconds(3))
        .await
        .unwrap();
    assert_events(&env, &[("INC101", "deadline")]).await;
    let now = start + Duration::milliseconds(3001);
    let (a, b) = tokio::join!(
        bell::process_due(env.pool(), now),
        bell::process_due(env.pool(), now)
    );
    a.unwrap();
    b.unwrap();
    // The first ticket leaves the header at its deadline. The second remains in waiting.
    bell::process_due(env.pool(), start + Duration::seconds(21))
        .await
        .unwrap();
    assert_events(&env, &[("INC101", "deadline"), ("INC102", "waiting")]).await;
    assert_eq!(env.i64("SELECT max(id) FROM dashboard_events").await, revision);
    env.finish().await;
}

#[tokio::test]
async fn an_import_preserves_both_elapsed_alerts_before_changing_the_incident() {
    let Some(mut env) = Env::new().await else { return };
    let start = start(&env).await;
    let mut row = incident("104", start, common::COORD);
    update(
        &mut row,
        &[
            ("submitDate", json!(start - Duration::seconds(470))),
            ("deadline", json!(start + Duration::seconds(125))),
        ],
    );
    env.write_json(&[row.clone()]);
    assert!(env.tick_after(0.0).await.accepted());
    // The previous record entered deadline at +6.001 s and waiting at +10.001 s.
    // The next import arrives before the clock worker has observed either boundary.
    row.insert("priorityId".into(), json!(4));
    env.write_json(&[row]);
    assert!(env.tick_after(20.0).await.accepted());
    bell::process_due(env.pool(), start + Duration::seconds(21))
        .await
        .unwrap();
    assert_events(
        &env,
        &[
            ("INC104", "deadline"),
            ("INC104", "waiting"),
            ("INC104", "critical.raised"),
        ],
    )
    .await;
    env.finish().await;
}

#[tokio::test]
async fn changes_and_local_team_assignment_emit_only_material_header_transitions() {
    let Some(mut env) = Env::new().await else { return };
    let start = start(&env).await;
    let mut row = incident("103", start, "Team A");
    row.insert("priorityId".into(), json!(4));
    env.write_json(&[row.clone()]);
    assert!(env.tick_after(0.0).await.accepted()); // First snapshot includes P1 and is silent.
    row.insert("header".into(), json!("Updated description of the same incident"));
    env.write_json(&[row.clone()]);
    assert!(env.tick().await.accepted());
    assert_events(&env, &[]).await;

    let mut tx = env.pool().begin().await.unwrap();
    import_repo::lock_state(&mut tx).await.unwrap();
    sqlx::query("INSERT INTO teams(name) VALUES ('Team B')")
        .execute(&mut *tx)
        .await
        .unwrap();
    let previous = import_repo::stored_by_sys_id(&mut tx, "103")
        .await
        .unwrap()
        .unwrap();
    assignment::record_assignment(
        &mut tx,
        &env.settings,
        &previous,
        "Team B",
        env.clock.now(),
        "operator",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_events(&env, &[("INC103", "critical")]).await;

    row.insert("deadline".into(), json!(start + Duration::hours(1)));
    env.write_json(&[row.clone()]);
    assert!(env.tick().await.accepted());
    env.write_json(&[row.clone()]);
    env.tick().await;
    assert_events(&env, &[("INC103", "critical"), ("INC103", "critical")]).await;
    env.write_json(&[]);
    assert!(env.tick().await.accepted());
    env.write_json(&[row]);
    assert!(env.tick().await.accepted());
    assert_events(
        &env,
        &[
            ("INC103", "critical"),
            ("INC103", "critical"),
            ("INC103", "critical.raised"),
        ],
    )
    .await;
    env.finish().await;
}
