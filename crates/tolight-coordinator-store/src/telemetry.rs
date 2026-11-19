//! Read-only operational metrics; no Owner data leaves the store.
use super::{Result, RoomState, Store, db};
use sqlx::Row;
use time::OffsetDateTime;

pub struct AgentTelemetry {
    pub agent_id: String,
    pub attempts: i64,
    pub last_seen_seconds: f64,
}
pub struct RejectionTelemetry {
    pub agent_id: String,
    pub reason: String,
    pub count: i64,
    pub count_last_hour: i64,
}
pub struct TelemetrySnapshot {
    pub room: RoomState,
    pub messages_last_hour: i64,
    pub agents: Vec<AgentTelemetry>,
    pub rejections: Vec<RejectionTelemetry>,
    pub pending_count: i64,
    pub oldest_pending_seconds: f64,
    pub latest_pending_timestamp: f64,
    pub muted_count: i64,
}
impl Store {
    pub async fn telemetry(&self, now: OffsetDateTime) -> Result<TelemetrySnapshot> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        let room = super::room_in(&mut tx).await?;
        let messages_last_hour=sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE kind='agent_message' AND created_at > $1 - INTERVAL '1 hour'").bind(now).fetch_one(&mut *tx).await.map_err(db)?;
        let agents=sqlx::query("SELECT a.agent_id,(SELECT COUNT(*) FROM message_attempts m WHERE m.agent_id=a.agent_id AND m.created_at > $1 - INTERVAL '24 hours') AS attempts,CASE WHEN a.last_seen_at IS NULL THEN -1::float8 ELSE GREATEST(EXTRACT(EPOCH FROM ($1 - a.last_seen_at))::float8,0) END AS last_seen_seconds FROM agents a ORDER BY a.agent_id").bind(now).fetch_all(&mut *tx).await.map_err(db)?.into_iter().map(|row| AgentTelemetry{agent_id:row.get("agent_id"),attempts:row.get("attempts"),last_seen_seconds:row.get("last_seen_seconds")}).collect();
        let rejections=sqlx::query("SELECT agent_id,reason_code,COUNT(*) AS count,COUNT(*) FILTER (WHERE created_at > $1 - INTERVAL '1 hour') AS count_last_hour FROM message_attempts WHERE outcome='rejected' AND created_at > $1 - INTERVAL '24 hours' GROUP BY agent_id,reason_code ORDER BY agent_id,reason_code").bind(now).fetch_all(&mut *tx).await.map_err(db)?.into_iter().map(|row| RejectionTelemetry{agent_id:row.get("agent_id"),reason:row.get("reason_code"),count:row.get("count"),count_last_hour:row.get("count_last_hour")}).collect();
        let pending=sqlx::query("SELECT COUNT(*) AS count,COALESCE(GREATEST(EXTRACT(EPOCH FROM ($1-MIN(state_changed_at)))::float8,0),0) AS oldest,COALESCE(EXTRACT(EPOCH FROM MAX(state_changed_at))::float8,0) AS latest FROM agents WHERE state='PENDING'").bind(now).fetch_one(&mut *tx).await.map_err(db)?;
        let muted_count=sqlx::query_scalar("SELECT COUNT(*) FROM agents WHERE state='ACTIVE' AND (muted_indefinitely OR muted_until > $1)").bind(now).fetch_one(&mut *tx).await.map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(TelemetrySnapshot {
            room,
            messages_last_hour,
            agents,
            rejections,
            pending_count: pending.get("count"),
            oldest_pending_seconds: pending.get("oldest"),
            latest_pending_timestamp: pending.get("latest"),
            muted_count,
        })
    }
}
