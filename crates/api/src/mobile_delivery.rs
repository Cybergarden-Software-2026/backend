//! Durable, at-least-once delivery of critical incident notifications.
//! A database claim expires if a process exits between the FCM request and its acknowledgement.

use std::time::{Duration, Instant};

use dashboard_config::Settings;
use sqlx::{PgPool, Row};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::fcm::{FcmClient, SendError};

const IDLE_POLL: Duration = Duration::from_secs(1);
const CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

struct Delivery {
    notification_id: i64,
    device_id: Uuid,
    attempts: i32,
    token: String,
    target_type: String,
    incident_id: String,
    sys_id: String,
    ttl: Duration,
}

async fn claim(pool: &PgPool) -> Result<Option<Delivery>, sqlx::Error> {
    let row = sqlx::query(
        "WITH ready AS (
             SELECT q.notification_id, q.device_id, d.fcm_token, d.fcm_target_type, n.incident_id, n.sys_id
             FROM mobile_push_deliveries q
             JOIN mobile_devices d ON d.id = q.device_id
             JOIN coordinator_accounts a ON a.login = d.account_login
             JOIN notifications n ON n.id = q.notification_id
             WHERE q.available_at <= now() AND q.expires_at > now()
               AND a.disabled_at IS NULL AND d.fcm_token IS NOT NULL
             ORDER BY q.available_at, q.notification_id, q.device_id
             LIMIT 1 FOR UPDATE OF q SKIP LOCKED
         )
         UPDATE mobile_push_deliveries q
         SET attempts = attempts + 1, available_at = now() + interval '2 minutes'
         FROM ready WHERE q.notification_id = ready.notification_id AND q.device_id = ready.device_id
         RETURNING q.notification_id, q.device_id, q.attempts, ready.fcm_token, ready.fcm_target_type,
                   ready.incident_id, ready.sys_id, floor(extract(epoch FROM q.expires_at - now()))::bigint AS ttl",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| Delivery {
        notification_id: r.get("notification_id"),
        device_id: r.get("device_id"),
        attempts: r.get("attempts"),
        token: r.get("fcm_token"),
        target_type: r.get("fcm_target_type"),
        incident_id: r.get("incident_id"),
        sys_id: r.get("sys_id"),
        ttl: Duration::from_secs(r.get::<i64, _>("ttl") as u64),
    }))
}

fn retry_delay(attempts: i32, after: Option<Duration>) -> i32 {
    let exponent = (attempts - 1).clamp(0, 4) as u32;
    let seconds = (60 * 2_u64.pow(exponent)).min(900);
    seconds.max(after.unwrap_or_default().as_secs()).min(3600) as i32
}

async fn finish(pool: &PgPool, job: &Delivery, result: Result<(), SendError>) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    // A delayed response may refer to a previous claim or a registration that has since rotated.
    let current: Option<(Option<String>, String)> = sqlx::query_as(
        "SELECT d.fcm_token, d.fcm_target_type FROM mobile_devices d JOIN mobile_push_deliveries q ON q.device_id = d.id
         WHERE q.notification_id = $1 AND d.id = $2 AND q.attempts = $3 FOR UPDATE OF d, q",
    )
    .bind(job.notification_id)
    .bind(job.device_id)
    .bind(job.attempts)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((current, current_type)) = current else {
        return Ok(());
    };

    let delay = if current.as_deref() != Some(job.token.as_str()) || current_type != job.target_type {
        // A refreshed token can receive this event immediately.
        current.map(|_| 0)
    } else {
        match result {
            Ok(()) => None,
            Err(SendError::InvalidRegistration) => {
                sqlx::query("UPDATE mobile_devices SET fcm_token = NULL WHERE id = $1")
                    .bind(job.device_id)
                    .execute(&mut *tx)
                    .await?;
                None
            }
            Err(SendError::Rejected) => {
                tracing::warn!(
                    notification_id = job.notification_id,
                    "FCM rejected the push message"
                );
                None
            }
            Err(SendError::Retry { after }) => {
                let delay = retry_delay(job.attempts, after);
                tracing::warn!(
                    notification_id = job.notification_id,
                    delay,
                    "FCM delivery deferred"
                );
                Some(delay)
            }
        }
    };

    if let Some(seconds) = delay {
        sqlx::query(
            "UPDATE mobile_push_deliveries SET available_at = now() + $3::integer * interval '1 second'
             WHERE notification_id = $1 AND device_id = $2",
        )
        .bind(job.notification_id)
        .bind(job.device_id)
        .bind(seconds)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query("DELETE FROM mobile_push_deliveries WHERE notification_id = $1 AND device_id = $2")
            .bind(job.notification_id)
            .bind(job.device_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await
}

async fn dispatch_one(pool: &PgPool, client: &FcmClient) -> Result<bool, sqlx::Error> {
    let Some(job) = claim(pool).await? else {
        return Ok(false);
    };
    let outcome = client
        .send(
            &job.token,
            &job.target_type,
            job.notification_id,
            &job.incident_id,
            &job.sys_id,
            job.ttl,
        )
        .await;
    finish(pool, &job, outcome).await?;
    Ok(true)
}

async fn run(pool: PgPool, client: FcmClient, mut stop: watch::Receiver<bool>) {
    let mut cleanup_at = Instant::now();
    while !*stop.borrow() {
        if Instant::now() >= cleanup_at {
            if let Err(error) = sqlx::query("DELETE FROM mobile_push_deliveries WHERE expires_at <= now()")
                .execute(&pool)
                .await
            {
                tracing::warn!(%error, "mobile push queue cleanup failed");
            }
            cleanup_at = Instant::now() + CLEANUP_INTERVAL;
        }
        match dispatch_one(&pool, &client).await {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => tracing::warn!(%error, "mobile push queue unavailable"),
        }
        tokio::select! {
            _ = tokio::time::sleep(IDLE_POLL) => {}
            _ = stop.changed() => break,
        }
    }
}

/// Start the dispatcher only on deployments configured for push delivery.
pub fn start(
    settings: &Settings,
    pool: PgPool,
    stop: watch::Receiver<bool>,
) -> anyhow::Result<Option<JoinHandle<()>>> {
    if !settings.mobile_push_enabled {
        return Ok(None);
    }
    let client = FcmClient::from_settings(settings)?;
    Ok(Some(tokio::spawn(run(pool, client, stop))))
}

#[cfg(test)]
#[path = "mobile_delivery_tests.rs"]
mod tests;
