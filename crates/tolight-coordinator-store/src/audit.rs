//! Bounded online Operator audit reads. No cleanup, controls, or serve lease.
use super::{OffsetDateTime, Result, Store, controls::SnapshotAction, db};
use serde::Serialize;
use tolight_core::{mentions::valid_agent_id, models::ApiError};

#[derive(Debug, Serialize)]
pub struct AuditPage {
    pub schema: &'static str,
    pub order: &'static str,
    pub limit: usize,
    pub truncated: bool,
    pub records: Vec<SnapshotAction>,
}

impl Store {
    /// Read one statement snapshot, newest first. The extra row makes truncation
    /// explicit; this is not an offline recovery snapshot or a mutable cursor.
    pub async fn operator_audit(
        &self,
        since: Option<OffsetDateTime>,
        agent_id: Option<&str>,
        limit: usize,
    ) -> Result<AuditPage> {
        if !(1..=10_000).contains(&limit) {
            return Err(ApiError::new("bad_request").with("field", "limit"));
        }
        if agent_id.is_some_and(|id| !valid_agent_id(id)) {
            return Err(ApiError::new("bad_request").with("field", "agent_id"));
        }
        let mut records: Vec<SnapshotAction> = sqlx::query_as(
            "SELECT action_id,kind,target_agent_id,target_application_id,target_event_id,target_public_key,reason_code,detail,created_at FROM operator_actions WHERE ($1::timestamptz IS NULL OR created_at >= $1) AND ($2::text IS NULL OR target_agent_id=$2) ORDER BY created_at DESC,action_id DESC LIMIT $3",
        )
        .bind(since)
        .bind(agent_id)
        .bind((limit + 1) as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        let truncated = records.len() > limit;
        records.truncate(limit);
        Ok(AuditPage {
            schema: "tolight-operator-audit/1",
            order: "created_at_desc_action_id_desc",
            limit,
            truncated,
            records,
        })
    }
}
