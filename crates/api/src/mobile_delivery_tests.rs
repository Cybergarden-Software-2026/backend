#[path = "../../importer/tests/common/mod.rs"]
mod common;

use super::*;
use serde_json::json;

async fn pair(env: &common::Env, name: &str) {
    sqlx::query(
        "INSERT INTO coordinator_accounts (login, display_name, password_hash, key_hash)
         VALUES ($1, $1, 'test', sha256(convert_to($1, 'UTF8')))",
    )
    .bind(name)
    .execute(env.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO mobile_devices (account_login, platform, secret_hash, fcm_token)
         VALUES ($1, 'android', sha256(convert_to($1, 'UTF8')), $1)",
    )
    .bind(name)
    .execute(env.pool())
    .await
    .unwrap();
}

#[tokio::test]
async fn imported_critical_transition_queues_only_devices_paired_at_that_moment() {
    let Some(mut env) = common::Env::new().await else {
        return;
    };
    pair(&env, "early").await;
    pair(&env, "another").await;
    pair(&env, "disabled").await;
    sqlx::query("UPDATE coordinator_accounts SET disabled_at = now() WHERE login = 'disabled'")
        .execute(env.pool())
        .await
        .unwrap();

    let mut records = common::records();
    env.write_json(&records);
    env.tick().await;
    assert!(claim(env.pool()).await.unwrap().is_none());

    common::update(
        common::demo(&mut records),
        &[
            ("priorityId", json!(4)),
            ("priority", json!("Наивысший")),
            ("assignedGroupName", json!(common::COORD)),
        ],
    );
    env.write_json(&records);
    env.tick().await;
    env.write_json(&records);
    env.tick().await;
    pair(&env, "late").await;

    assert_eq!(
        env.strings(
            "SELECT array_agg(DISTINCT n.type ORDER BY n.type) FROM notifications n
             JOIN mobile_push_deliveries q ON q.notification_id = n.id"
        )
        .await,
        ["critical.raised"]
    );
    assert_eq!(
        env.strings(
            "SELECT array_agg(d.account_login ORDER BY d.account_login) FROM mobile_devices d
             JOIN mobile_push_deliveries q ON q.device_id = d.id"
        )
        .await,
        ["another", "early"]
    );
    env.finish().await;
}

async fn event(env: &common::Env, key: &str) {
    sqlx::query(
        "INSERT INTO notifications(type, incident_id, sys_id, title, dedup_key)
         VALUES ('critical.raised', 'INC1', 'sys1', 'Incident', $1) ON CONFLICT DO NOTHING",
    )
    .bind(key)
    .execute(env.pool())
    .await
    .unwrap();
}

#[tokio::test]
async fn a_header_bell_has_one_event_id_on_android_web_and_redelivery() {
    let Some(env) = common::Env::new().await else {
        return;
    };
    pair(&env, "phone").await;
    pair(&env, "browser").await;
    sqlx::query("UPDATE mobile_devices SET platform = 'web' WHERE account_login = 'browser'")
        .execute(env.pool())
        .await
        .unwrap();
    sqlx::query("SELECT enqueue_mobile_bell('waiting', 'INC2', 'sys2', now())")
        .execute(env.pool())
        .await
        .unwrap();

    let first = claim(env.pool()).await.unwrap().unwrap();
    let second = claim(env.pool()).await.unwrap().unwrap();
    assert_ne!(first.device_id, second.device_id);
    assert_eq!(first.event.notification_id, second.event.notification_id);
    for job in [&first, &second] {
        assert_eq!(job.event.bell_kind.as_deref(), Some("waiting"));
        assert_eq!(job.event.sys_id, "sys2");
        assert_eq!(job.event.incident_id, "INC2");
    }
    finish(env.pool(), &first, Ok(())).await.unwrap();
    finish(env.pool(), &second, Err(SendError::Retry { after: None }))
        .await
        .unwrap();
    assert!(claim(env.pool()).await.unwrap().is_none());
    sqlx::query("UPDATE mobile_push_deliveries SET available_at = now()")
        .execute(env.pool())
        .await
        .unwrap();
    let retry = claim(env.pool()).await.unwrap().unwrap();
    assert_eq!(retry.event.notification_id, second.event.notification_id);
    assert_eq!(retry.device_id, second.device_id);
    assert_eq!(retry.event.bell_kind.as_deref(), Some("waiting"));
    finish(env.pool(), &retry, Ok(())).await.unwrap();
    assert!(claim(env.pool()).await.unwrap().is_none());
    env.finish().await;
}

#[tokio::test]
async fn claims_retry_and_finish_only_the_current_lease_and_fcm_token() {
    let Some(env) = common::Env::new().await else {
        return;
    };
    pair(&env, "phone").await;
    event(&env, "first").await;
    event(&env, "first").await;

    let (left, right) = tokio::join!(claim(env.pool()), claim(env.pool()));
    let jobs: Vec<_> = [left.unwrap(), right.unwrap()].into_iter().flatten().collect();
    assert_eq!(jobs.len(), 1);
    let job = &jobs[0];
    assert!(job.event.bell_kind.is_none());
    assert_eq!(job.event.incident_id, "INC1");
    assert_eq!(job.event.sys_id, "sys1");
    assert!(job.ttl <= Duration::from_secs(3600));

    finish(
        env.pool(),
        job,
        Err(SendError::Retry {
            after: Some(Duration::from_secs(200)),
        }),
    )
    .await
    .unwrap();
    assert!(claim(env.pool()).await.unwrap().is_none());
    assert!(
        env.boolean("SELECT available_at > now() + interval '190 seconds' FROM mobile_push_deliveries")
            .await
    );
    sqlx::query("UPDATE mobile_push_deliveries SET available_at = now()")
        .execute(env.pool())
        .await
        .unwrap();
    let reclaimed = claim(env.pool()).await.unwrap().unwrap();
    finish(env.pool(), job, Err(SendError::InvalidRegistration))
        .await
        .unwrap();
    assert_eq!(
        env.string("SELECT fcm_token FROM mobile_devices")
            .await
            .as_deref(),
        Some("phone")
    );

    sqlx::query("UPDATE mobile_devices SET fcm_token = 'refreshed', fcm_target_type = 'fid'")
        .execute(env.pool())
        .await
        .unwrap();
    finish(env.pool(), &reclaimed, Err(SendError::InvalidRegistration))
        .await
        .unwrap();
    let refreshed = claim(env.pool()).await.unwrap().unwrap();
    assert_eq!(refreshed.token, "refreshed");
    assert_eq!(refreshed.target_type, "fid");
    finish(env.pool(), &refreshed, Ok(())).await.unwrap();
    assert_eq!(env.i64("SELECT count(*) FROM mobile_push_deliveries").await, 0);

    event(&env, "second").await;
    let removed = claim(env.pool()).await.unwrap().unwrap();
    finish(env.pool(), &removed, Err(SendError::InvalidRegistration))
        .await
        .unwrap();
    assert!(env.string("SELECT fcm_token FROM mobile_devices").await.is_none());
    event(&env, "no-token").await;
    assert_eq!(env.i64("SELECT count(*) FROM mobile_push_deliveries").await, 0);
    env.finish().await;
}

#[tokio::test]
async fn expired_revoked_and_disabled_registrations_stop_receiving() {
    let Some(env) = common::Env::new().await else {
        return;
    };
    pair(&env, "expired").await;
    pair(&env, "revoked").await;
    pair(&env, "disabled").await;
    event(&env, "queued").await;
    sqlx::raw_sql(
        "UPDATE mobile_push_deliveries SET expires_at = now()
         WHERE device_id IN (SELECT id FROM mobile_devices WHERE account_login = 'expired');
         DELETE FROM mobile_devices WHERE account_login = 'revoked';
         UPDATE coordinator_accounts SET disabled_at = now() WHERE login = 'disabled';",
    )
    .execute(env.pool())
    .await
    .unwrap();
    assert!(claim(env.pool()).await.unwrap().is_none());
    // Later inserts also purge expired rows while the HTTP worker is offline.
    event(&env, "next").await;
    assert_eq!(
        env.i64("SELECT count(*) FROM mobile_push_deliveries WHERE expires_at <= now()")
            .await,
        0
    );
    env.finish().await;
}
