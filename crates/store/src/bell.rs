//! Durable state of the header bell. Registry changes and clock transitions are serialized by
//! the import_state row lock. A queued push and the state that suppresses its replay commit together.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use dashboard_domain::constants::NormalizedState;
use dashboard_domain::header::{Header, header_team};
use sqlx::{PgConnection, PgPool, Row};

use crate::import_repo;

const BATCH_SIZE: i64 = 200;

struct Observation<'a> {
    pk: i64,
    incident_id: &'a str,
    sys_id: &'a str,
    header: Header<'a>,
    assignment: Option<DateTime<Utc>>,
    deadline: Option<DateTime<Utc>>,
}

impl Observation<'_> {
    async fn enqueue(
        &self,
        conn: &mut PgConnection,
        kind: &str,
        at: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT enqueue_mobile_bell($1, $2, $3, $4)")
            .bind(kind)
            .bind(self.incident_id)
            .bind(self.sys_id)
            .bind(at)
            .execute(conn)
            .await?;
        Ok(())
    }

    fn next_check(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.header
            .next_change(now.timestamp_millis())
            .and_then(DateTime::from_timestamp_millis)
    }

    async fn save(&self, conn: &mut PgConnection, now: DateTime<Utc>) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO mobile_bell_state AS b (incident_pk, kind, team, assigned_at, deadline, next_check_at)
             VALUES($1, $2, $3, $4, $5, $6)
             ON CONFLICT(incident_pk) DO UPDATE SET initialized = true, kind = EXCLUDED.kind, team = EXCLUDED.team,
                 assigned_at = EXCLUDED.assigned_at, deadline = EXCLUDED.deadline, next_check_at = EXCLUDED.next_check_at
             WHERE (b.initialized, b.kind, b.team, b.assigned_at, b.deadline, b.next_check_at)
                IS DISTINCT FROM (true, EXCLUDED.kind, EXCLUDED.team, EXCLUDED.assigned_at, EXCLUDED.deadline, EXCLUDED.next_check_at)",
        )
        .bind(self.pk)
        .bind(self.header.kind_at(now.timestamp_millis()))
        .bind(self.header.team)
        .bind(self.assignment)
        .bind(self.deadline)
        .bind(self.next_check(now))
        .execute(conn)
        .await?;
        Ok(())
    }
}

#[derive(Default)]
pub struct ChangedIncidents {
    /// A critical.raised journal entry already enqueues one push per device.
    pub critical_raised: HashSet<i64>,
    /// The web tracker keys cards by sysId. Changing it makes a new card.
    pub changed_sys_id: HashSet<i64>,
    pub initial: bool,
}

async fn observe(
    conn: &mut PgConnection,
    ids: &[i64],
    now: DateTime<Utc>,
    change: Option<&ChangedIncidents>,
) -> Result<(), sqlx::Error> {
    if ids.is_empty() {
        return Ok(());
    }
    let rows = sqlx::query(
        "SELECT i.pk, i.id, i.sys_id, i.is_present, i.normalized_state, i.priority_id,
                i.raw ->> 'assignedGroupName' AS team, i.submit_date, i.deadline, i.last_assignment_to_team_at,
                b.initialized, b.kind, b.team AS previous_team, b.assigned_at AS previous_assignment,
                b.deadline AS previous_deadline, b.next_check_at
         FROM incidents i LEFT JOIN mobile_bell_state b ON b.incident_pk = i.pk
         WHERE i.pk = ANY($1) AND ($2 OR b.initialized IS NOT TRUE OR b.next_check_at <= $3)
         ORDER BY i.pk",
    )
    .bind(ids)
    .bind(change.is_some())
    .bind(now)
    .fetch_all(&mut *conn)
    .await?;
    for row in rows {
        let pk: i64 = row.try_get("pk")?;
        if !row.try_get::<bool, _>("is_present")? {
            sqlx::query("DELETE FROM mobile_bell_state WHERE incident_pk = $1")
                .bind(pk)
                .execute(&mut *conn)
                .await?;
            continue;
        }
        let team = row.try_get::<Option<&str>, _>("team")?.and_then(header_team);
        let assignment: Option<DateTime<Utc>> = row.try_get("last_assignment_to_team_at")?;
        let deadline: Option<DateTime<Utc>> = row.try_get("deadline")?;
        let header = Header {
            state: row
                .try_get::<&str, _>("normalized_state")?
                .parse::<NormalizedState>()
                .map_err(|err| sqlx::Error::ColumnDecode {
                    index: "normalized_state".into(),
                    source: Box::new(err),
                })?,
            priority: row.try_get("priority_id")?,
            team,
            submitted: row.try_get::<DateTime<Utc>, _>("submit_date")?.timestamp_millis(),
            assignment: assignment.map(|at| at.timestamp_millis()),
            deadline: deadline.map(|at| at.timestamp_millis()),
        };
        let obs = Observation {
            pk,
            incident_id: row.try_get("id")?,
            sys_id: row.try_get("sys_id")?,
            header,
            assignment,
            deadline,
        };
        let previous_kind: Option<&str> = row.try_get("kind")?;
        let mut observed_at = now;
        if let Some(change) = change {
            let kind = obs.header.kind_at(now.timestamp_millis());
            if let Some(kind) = kind
                && !change.initial
                && !change.critical_raised.contains(&pk)
                && (previous_kind != Some(kind)
                    || change.changed_sys_id.contains(&pk)
                    || team != row.try_get::<Option<&str>, _>("previous_team")?
                    || assignment != row.try_get("previous_assignment")?
                    || deadline != row.try_get("previous_deadline")?)
            {
                obs.enqueue(conn, kind, now).await?;
            }
        } else {
            let mut next: Option<DateTime<Utc>> = row.try_get("next_check_at")?;
            let mut previous = previous_kind;
            if row.try_get::<Option<bool>, _>("initialized")? == Some(false)
                && let Some(baseline_at) = next
            {
                // An old importer may have replaced this row while a timer waited for the lock.
                observed_at = observed_at.max(baseline_at);
                previous = obs.header.kind_at(baseline_at.timestamp_millis());
                next = obs.next_check(baseline_at);
            }
            // At most three boundaries per immutable source record. Use each crossing's time
            // so a late wakeup cannot address devices that registered after the event.
            while let Some(at) = next.filter(|at| *at <= now) {
                let kind = obs.header.kind_at(at.timestamp_millis());
                if let Some(kind) = kind
                    && previous != Some(kind)
                {
                    obs.enqueue(conn, kind, at).await?;
                }
                previous = kind;
                next = obs.next_check(at);
            }
        }
        obs.save(conn, observed_at).await?;
    }
    Ok(())
}

/// Account for elapsed timers against the previous version before the importer overwrites it.
pub async fn before_changes(
    conn: &mut PgConnection,
    ids: &[i64],
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    observe(conn, ids, now, None).await
}

pub async fn after_changes(
    conn: &mut PgConnection,
    ids: &[i64],
    now: DateTime<Utc>,
    change: &ChangedIncidents,
) -> Result<(), sqlx::Error> {
    observe(conn, ids, now, Some(change)).await
}

/// Check indexed due times even if neither the source nor the snapshot revision changes.
pub async fn process_due(pool: &PgPool, now: DateTime<Utc>) -> Result<usize, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT incident_pk FROM mobile_bell_state WHERE next_check_at <= $1
         ORDER BY next_check_at, incident_pk LIMIT $2",
    )
    .bind(now)
    .bind(BATCH_SIZE)
    .fetch_all(&mut *tx)
    .await?;
    if !ids.is_empty() {
        import_repo::lock_state(&mut tx).await?;
        before_changes(&mut tx, &ids, now).await?;
    }
    tx.commit().await?;
    Ok(ids.len())
}
