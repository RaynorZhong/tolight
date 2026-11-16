//! Private offline recovery and mode-checked participant self controls.
use super::*;
use tolight_core::models::AgentContext;

impl Store {
    pub async fn owner_leave(&self, context: &AgentContext, mode: &str) -> Result<()> {
        if !matches!(mode, "leave" | "withdraw") {
            return Err(ApiError::new("bad_request").with("field", "mode"));
        }
        self.leave_inner(
            &context.agent_id,
            Some(&context.public_key),
            "owner_request",
            true,
            Some(mode),
        )
        .await
    }
}

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Connection;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceAnchor {
    pub seq: i64,
    pub event_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRoom {
    pub paused: bool,
    pub applications_open: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
#[serde(deny_unknown_fields)]
pub struct SnapshotKey {
    pub public_key: String,
    pub agent_id: String,
    pub application_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
#[serde(deny_unknown_fields)]
pub struct SnapshotAction {
    pub action_id: String,
    pub kind: String,
    pub target_agent_id: Option<String>,
    pub target_application_id: Option<Uuid>,
    pub target_event_id: Option<String>,
    pub target_public_key: Option<String>,
    pub reason_code: Option<String>,
    pub detail: Option<Value>,
    pub created_at: DateTime<Utc>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlSnapshot {
    pub schema: String,
    pub export_id: Uuid,
    pub exported_at: DateTime<Utc>,
    pub restore_target: DateTime<Utc>,
    pub source_anchor: SourceAnchor,
    pub room: SnapshotRoom,
    pub agents: Vec<Agent>,
    pub agent_keys: Vec<SnapshotKey>,
    pub operator_actions: Vec<SnapshotAction>,
    pub retracted_event_ids: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct RetractionPlan {
    pub event_id: String,
    pub seq: Option<i64>,
    pub already_retracted: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct ActiveReview {
    pub application_id: Uuid,
    pub source: Agent,
    pub target: Option<Agent>,
}
#[derive(Clone, Debug, Serialize)]
pub struct RestorePlan {
    pub export_id: Uuid,
    pub snapshot_sha256: String,
    pub source_anchor: SourceAnchor,
    pub target_anchor: SourceAnchor,
    pub required_approvals: Vec<Uuid>,
    pub active_reviews: Vec<ActiveReview>,
    pub retractions: Vec<RetractionPlan>,
    pub identity_count: usize,
    pub already_applied: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct RestoreResult {
    pub export_id: Uuid,
    pub already_applied: bool,
    pub identities: usize,
    pub retractions: usize,
}

pub struct PreparedRestore {
    lease: ServeLease,
    snapshot: ControlSnapshot,
    plan: RestorePlan,
    expected: Value,
}
fn invalid_snapshot() -> ApiError {
    ApiError::new("bad_request").with("field", "control_snapshot")
}
fn path_error() -> ApiError {
    ApiError::new("bad_request").with("field", "private_snapshot_path")
}
fn io_error(_: std::io::Error) -> ApiError {
    ApiError::new("unavailable")
}

fn valid_control_fields(agent: &Agent) -> bool {
    let reason = |value: &Option<String>| {
        value
            .as_deref()
            .is_some_and(tolight_core::validation::is_reason_code)
    };
    let state_reason = match agent.state.as_str() {
        "PENDING" | "ACTIVE" => agent.state_reason.is_none(),
        "REJECTED" | "LEFT" => reason(&agent.state_reason),
        _ => false,
    };
    // Expired absolute mutes remain valid stored controls; expiry is decided
    // at use time. They must retain their reason without extending the deadline.
    let muted = agent.muted_indefinitely || agent.muted_until.is_some();
    state_reason
        && !(agent.muted_indefinitely && agent.muted_until.is_some())
        && if agent.state == "ACTIVE" {
            if muted {
                reason(&agent.mute_reason)
            } else {
                agent.mute_reason.is_none()
            }
        } else {
            !muted && agent.mute_reason.is_none()
        }
}

fn validate_snapshot(snapshot: &ControlSnapshot) -> Result<()> {
    if snapshot.schema != "tolight-control-snapshot/1"
        || snapshot.export_id.is_nil()
        || snapshot.restore_target > snapshot.exported_at
        || snapshot.source_anchor.seq < 0
        || (snapshot.source_anchor.seq == 0) != snapshot.source_anchor.event_id.is_none()
    {
        return Err(invalid_snapshot());
    }
    let mut ids = BTreeMap::new();
    let mut applications = BTreeSet::new();
    let mut names = BTreeSet::new();
    for agent in &snapshot.agents {
        let value = tolight_core::models::ApplicationInput {
            agent_id: agent.agent_id.clone(),
            display_name: agent.display_name.clone(),
            bio: agent.bio.clone(),
            owner_contact: agent.owner_contact.clone(),
            framework_label: agent.framework_label.clone().unwrap_or_default(),
        };
        let clean = tolight_core::validation::validate_application(&value)
            .map_err(|_| invalid_snapshot())?;
        if clean.display_name_key != agent.display_name_key
            || agent.application_revision < 1
            || !matches!(
                agent.state.as_str(),
                "PENDING" | "REJECTED" | "ACTIVE" | "LEFT"
            )
            || !applications.insert(agent.application_id)
            || !names.insert(agent.display_name_key.clone())
            || ids.insert(agent.agent_id.clone(), agent).is_some()
            || (agent.state == "ACTIVE" && agent.joined_at.is_none())
            || !valid_control_fields(agent)
        {
            return Err(invalid_snapshot());
        }
    }
    let mut keys = BTreeSet::new();
    let mut active = BTreeMap::new();
    for key in &snapshot.agent_keys {
        tolight_core::signing::decode_public_key(&key.public_key)
            .map_err(|_| invalid_snapshot())?;
        let agent = ids.get(&key.agent_id).ok_or_else(invalid_snapshot)?;
        if agent.application_id != key.application_id || !keys.insert(key.public_key.clone()) {
            return Err(invalid_snapshot());
        }
        if key.revoked_at.is_none()
            && (agent.state == "LEFT"
                || active
                    .insert(key.agent_id.clone(), key.public_key.clone())
                    .is_some())
        {
            return Err(invalid_snapshot());
        }
    }
    for agent in &snapshot.agents {
        if active.get(&agent.agent_id).map(String::as_str) != agent.public_key.as_deref() {
            return Err(invalid_snapshot());
        }
        if agent.state != "LEFT" && agent.public_key.is_none() {
            return Err(invalid_snapshot());
        }
    }
    let mut actions = BTreeSet::new();
    for action in &snapshot.operator_actions {
        if action.action_id.is_empty()
            || !actions.insert(&action.action_id)
            || action.kind.is_empty()
            || action.reason_code.as_deref().is_some_and(|value| {
                !matches!(action.kind.as_str(), "pause" | "resume")
                    && !tolight_core::validation::is_reason_code(value)
            })
        {
            return Err(invalid_snapshot());
        }
    }
    let retractions: BTreeSet<_> = snapshot.retracted_event_ids.iter().collect();
    if retractions.len() != snapshot.retracted_event_ids.len()
        || retractions.iter().any(|id| id.is_empty())
    {
        return Err(invalid_snapshot());
    }
    Ok(())
}

fn canonical_target(path: &Path) -> std::io::Result<PathBuf> {
    if path.exists() {
        return fs::canonicalize(path);
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("missing filename"))?;
    Ok(canonical_target(parent)?.join(name))
}
pub fn validate_private_path(path: &Path, public_root: &Path) -> Result<PathBuf> {
    let path = canonical_target(path).map_err(|_| path_error())?;
    let public = canonical_target(public_root).map_err(|_| path_error())?;
    if path.starts_with(public) {
        return Err(path_error());
    }
    Ok(path)
}

async fn schema_on(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    let version: Option<i64> =
        sqlx::query_scalar("SELECT version FROM schema_version WHERE singleton")
            .fetch_optional(&mut **tx)
            .await
            .map_err(db)?;
    let migration: Option<i64> =
        sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success")
            .fetch_one(&mut **tx)
            .await
            .map_err(db)?;
    let unknown: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM _sqlx_migrations WHERE NOT success OR version<>$1",
    )
    .bind(SCHEMA_VERSION)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    if version != Some(SCHEMA_VERSION) || migration != Some(SCHEMA_VERSION) || unknown != 0 {
        return Err(ApiError::new("schema_mismatch"));
    }
    Ok(())
}
async fn agents_on(tx: &mut Transaction<'_, Postgres>) -> Result<Vec<Agent>> {
    sqlx::query_as(agent_sql!("FROM agents a ORDER BY a.agent_id"))
        .fetch_all(&mut **tx)
        .await
        .map_err(db)
}
async fn keys_on(tx: &mut Transaction<'_, Postgres>) -> Result<Vec<SnapshotKey>> {
    sqlx::query_as("SELECT k.public_key,k.agent_id,a.application_id,k.created_at,k.revoked_at FROM agent_keys k JOIN agents a ON a.agent_id=k.agent_id ORDER BY k.public_key").fetch_all(&mut **tx).await.map_err(db)
}
async fn anchor_on(tx: &mut Transaction<'_, Postgres>) -> Result<SourceAnchor> {
    let seq: i64 = sqlx::query_scalar("SELECT last_seq FROM room_state WHERE singleton")
        .fetch_one(&mut **tx)
        .await
        .map_err(db)?;
    let event_id = sqlx::query_scalar("SELECT event_id FROM events WHERE seq=$1")
        .bind(seq)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?;
    if seq > 0 && event_id.is_none() {
        return Err(invalid_snapshot());
    }
    Ok(SourceAnchor { seq, event_id })
}
fn control_fingerprint(agents: &[Agent], room: &RoomState) -> Value {
    json!({"agents":agents.iter().map(|a|json!([a.agent_id,a.application_id,a.application_revision,a.state,a.public_key,a.muted_until,a.muted_indefinitely,a.mute_reason])).collect::<Vec<_>>(),"paused":room.paused,"applications_open":room.applications_open})
}

impl Store {
    pub async fn export_controls(
        &self,
        path: &Path,
        public_root: &Path,
        restore_target: DateTime<Utc>,
    ) -> Result<ControlSnapshot> {
        self.export_controls_observed(path, public_root, restore_target, |_| async { Ok(()) })
            .await
    }
    async fn export_controls_observed<F, Fut>(
        &self,
        path: &Path,
        public_root: &Path,
        restore_target: DateTime<Utc>,
        observer: F,
    ) -> Result<ControlSnapshot>
    where
        F: FnOnce(i32) -> Fut + Send,
        Fut: std::future::Future<Output = Result<()>> + Send,
    {
        let path = validate_private_path(path, public_root)?;
        if path.exists() || restore_target > Utc::now() {
            return Err(path_error());
        }
        let mut lease = self.acquire_serve_lock().await?;
        let mut tx = lease.connection.begin().await.map_err(db)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        schema_on(&mut tx).await?;
        let room = room_in(&mut tx).await?;
        let snapshot=ControlSnapshot {
            schema:"tolight-control-snapshot/1".into(),export_id:Uuid::new_v4(),exported_at:Utc::now(),restore_target,
            source_anchor:anchor_on(&mut tx).await?,room:SnapshotRoom{paused:room.paused,applications_open:room.applications_open},
            agents:agents_on(&mut tx).await?,agent_keys:keys_on(&mut tx).await?,
            operator_actions:sqlx::query_as("SELECT action_id,kind,target_agent_id,target_application_id,target_event_id,target_public_key,reason_code,detail,created_at FROM operator_actions ORDER BY created_at,action_id").fetch_all(&mut *tx).await.map_err(db)?,
            retracted_event_ids:sqlx::query_scalar("SELECT event_id FROM events WHERE retracted_at IS NOT NULL ORDER BY event_id").fetch_all(&mut *tx).await.map_err(db)?,
        };
        validate_snapshot(&snapshot)?;
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?;
        observer(pid).await?;
        tx.commit().await.map_err(db)?;
        lease.check().await?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).map_err(|_| path_error())?;
        let write = (|| -> std::io::Result<()> {
            let bytes = serde_json::to_vec_pretty(&snapshot)?;
            file.write_all(&bytes)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            Ok(())
        })();
        if write.is_err() {
            let _ = fs::remove_file(&path);
        }
        write.map_err(io_error)?;
        if let Err(error) = lease.check().await {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        Ok(snapshot)
    }

    pub async fn prepare_restore_controls(
        &self,
        bytes: &[u8],
        required_suite: &str,
    ) -> Result<PreparedRestore> {
        if bytes.len() > 64 * 1024 * 1024 {
            return Err(invalid_snapshot());
        }
        let snapshot: ControlSnapshot =
            serde_json::from_slice(bytes).map_err(|_| invalid_snapshot())?;
        validate_snapshot(&snapshot)?;
        let digest = format!("{:x}", Sha256::digest(bytes));
        let mut lease = self.acquire_serve_lock().await?;
        let mut tx = lease.connection.begin().await.map_err(db)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        schema_on(&mut tx).await?;
        let applied:Option<String>=sqlx::query_scalar("SELECT detail->>'snapshot_sha256' FROM operator_actions WHERE kind='restore_controls_completed' AND detail->>'source_export_id'=$1").bind(snapshot.export_id.to_string()).fetch_optional(&mut *tx).await.map_err(db)?;
        let agents = agents_on(&mut tx).await?;
        let room = room_in(&mut tx).await?;
        let mut plan = RestorePlan {
            export_id: snapshot.export_id,
            snapshot_sha256: digest.clone(),
            source_anchor: snapshot.source_anchor.clone(),
            target_anchor: anchor_on(&mut tx).await?,
            required_approvals: vec![],
            active_reviews: vec![],
            retractions: vec![],
            identity_count: snapshot.agents.len(),
            already_applied: false,
        };
        if let Some(applied) = applied {
            if applied != digest {
                return Err(invalid_snapshot());
            }
            plan.already_applied = true;
        } else {
            let sources: BTreeMap<_, _> = snapshot
                .agents
                .iter()
                .map(|a| (a.application_id, a))
                .collect();
            for target in &agents {
                if matches!(target.state.as_str(), "ACTIVE" | "LEFT")
                    && !sources.contains_key(&target.application_id)
                {
                    return Err(
                        ApiError::new("invalid_state").with("agent_id", target.agent_id.clone())
                    );
                }
            }
            for source in &snapshot.agents {
                let target = agents.iter().find(|a| a.agent_id == source.agent_id);
                if let Some(target) = target {
                    // A sealed identity must never be revived by an older export,
                    // even if an operator supplies a fresh ACTIVE approval.
                    if target.state == "LEFT" && source.state != "LEFT" {
                        return Err(ApiError::new("invalid_state")
                            .with("agent_id", source.agent_id.clone()));
                    }
                    if target.application_id != source.application_id
                        && matches!(target.state.as_str(), "ACTIVE" | "LEFT")
                    {
                        return Err(ApiError::new("invalid_state")
                            .with("agent_id", source.agent_id.clone()));
                    }
                    if matches!(source.state.as_str(), "PENDING" | "REJECTED")
                        && matches!(target.state.as_str(), "ACTIVE" | "LEFT")
                    {
                        return Err(ApiError::new("invalid_state"));
                    }
                }
                if source.state == "ACTIVE"
                    && !target.is_some_and(|a| {
                        a.application_id == source.application_id && a.state == "ACTIVE"
                    })
                {
                    if source.conformance_passed_at.is_none()
                        || source.conformance_suite_version.as_deref() != Some(required_suite)
                    {
                        return Err(ApiError::new("invalid_state").with("field", "conformance"));
                    }
                    plan.required_approvals.push(source.application_id);
                    plan.active_reviews.push(ActiveReview {
                        application_id: source.application_id,
                        source: source.clone(),
                        target: target.cloned(),
                    });
                }
            }
            let current_keys = keys_on(&mut tx).await?;
            for key in &current_keys {
                if sources.contains_key(&key.application_id)
                    && !snapshot.agent_keys.iter().any(|k| {
                        k.public_key == key.public_key && k.application_id == key.application_id
                    })
                {
                    return Err(ApiError::new("invalid_state").with("field", "public_key"));
                }
            }
            for id in &snapshot.retracted_event_ids {
                let target:Option<(i64,bool)>=sqlx::query_as("SELECT seq,retracted_at IS NOT NULL FROM events WHERE event_id=$1 AND kind='agent_message'").bind(id).fetch_optional(&mut *tx).await.map_err(db)?;
                plan.retractions.push(RetractionPlan {
                    event_id: id.clone(),
                    seq: target.map(|v| v.0),
                    already_retracted: target.is_none_or(|v| v.1),
                });
            }
        }
        let expected = control_fingerprint(&agents, &room);
        tx.commit().await.map_err(db)?;
        Ok(PreparedRestore {
            lease,
            snapshot,
            plan,
            expected,
        })
    }
}

impl PreparedRestore {
    pub fn plan(&self) -> &RestorePlan {
        &self.plan
    }

    pub async fn backend_pid(&mut self) -> Result<i32> {
        self.lease.backend_pid().await
    }
    pub async fn commit(self, approved: &[Uuid]) -> Result<RestoreResult> {
        self.commit_observed(approved, |_| async { Ok(()) }).await
    }
    async fn commit_observed<F, Fut>(
        mut self,
        approved: &[Uuid],
        observer: F,
    ) -> Result<RestoreResult>
    where
        F: FnOnce(i32) -> Fut + Send,
        Fut: std::future::Future<Output = Result<()>> + Send,
    {
        let snapshot = &self.snapshot;
        if self.plan.already_applied {
            return Ok(RestoreResult {
                export_id: snapshot.export_id,
                already_applied: true,
                identities: 0,
                retractions: 0,
            });
        }
        if self
            .plan
            .required_approvals
            .iter()
            .any(|id| !approved.contains(id))
        {
            return Err(ApiError::new("invalid_state").with("field", "approve_active"));
        }
        let mut tx = self.lease.connection.begin().await.map_err(db)?;
        schema_on(&mut tx).await?;
        sqlx::query("SELECT agent_id FROM agents ORDER BY agent_id FOR NO KEY UPDATE")
            .fetch_all(&mut *tx)
            .await
            .map_err(db)?;
        let current = agents_on(&mut tx).await?;
        let room = lock_room(&mut tx).await?;
        if control_fingerprint(&current, &room) != self.expected {
            return Err(ApiError::new("invalid_state").with("field", "review_snapshot"));
        }
        let source_ids: BTreeSet<_> = snapshot.agents.iter().map(|a| a.application_id).collect();
        for target in &current {
            if matches!(target.state.as_str(), "PENDING" | "REJECTED")
                && !source_ids.contains(&target.application_id)
            {
                sqlx::query("DELETE FROM agents WHERE application_id=$1 AND state IN ('PENDING','REJECTED')").bind(target.application_id).execute(&mut *tx).await.map_err(db)?;
            }
        }
        for source in &snapshot.agents {
            let before = current
                .iter()
                .find(|a| a.application_id == source.application_id);
            let mut restored = source.clone();
            restored.application_revision = source
                .application_revision
                .max(before.map(|a| a.application_revision).unwrap_or(0))
                .checked_add(1)
                .ok_or_else(invalid_snapshot)?;
            let joining = source.state == "ACTIVE" && !before.is_some_and(|a| a.state == "ACTIVE");
            let leaving = source.state == "LEFT" && !before.is_some_and(|a| a.state == "LEFT");
            if joining {
                restored.joined_at = Some(Utc::now());
                restored.state_changed_at = Utc::now();
            }
            let value = serde_json::to_value(&restored).map_err(|_| invalid_snapshot())?;
            sqlx::query(r#"INSERT INTO agents SELECT * FROM jsonb_populate_record(NULL::agents,$1)
ON CONFLICT(agent_id) DO UPDATE SET application_id=EXCLUDED.application_id,application_revision=EXCLUDED.application_revision,
display_name=EXCLUDED.display_name,display_name_key=EXCLUDED.display_name_key,bio=EXCLUDED.bio,framework_label=EXCLUDED.framework_label,
owner_contact=EXCLUDED.owner_contact,state=EXCLUDED.state,state_reason=EXCLUDED.state_reason,state_changed_at=EXCLUDED.state_changed_at,
registered_at=EXCLUDED.registered_at,joined_at=EXCLUDED.joined_at,muted_until=EXCLUDED.muted_until,muted_indefinitely=EXCLUDED.muted_indefinitely,
mute_reason=EXCLUDED.mute_reason,conformance_suite_version=EXCLUDED.conformance_suite_version,conformance_passed_at=EXCLUDED.conformance_passed_at,last_seen_at=EXCLUDED.last_seen_at"#)
                .bind(value).execute(&mut *tx).await.map_err(db)?;
            sqlx::query("DELETE FROM agent_keys WHERE agent_id=$1")
                .bind(&source.agent_id)
                .execute(&mut *tx)
                .await
                .map_err(db)?;
            for key in snapshot
                .agent_keys
                .iter()
                .filter(|k| k.application_id == source.application_id)
            {
                sqlx::query("INSERT INTO agent_keys(public_key,agent_id,created_at,revoked_at) VALUES($1,$2,$3,$4)").bind(&key.public_key).bind(&source.agent_id).bind(key.created_at).bind(if source.state=="LEFT" {Some(key.revoked_at.unwrap_or(snapshot.exported_at))}else{key.revoked_at}).execute(&mut *tx).await.map_err(db)?;
            }
            if joining || leaving {
                append_event(
                    &mut tx,
                    if joining {
                        "agent_joined"
                    } else {
                        "agent_left"
                    },
                    Some(&source.agent_id),
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .await?;
            }
            audit(&mut tx,"restore_controls_agent",Some(&restored),None,restored.public_key.as_deref(),None,Some(json!({"source_export_id":snapshot.export_id,"from":before.map(|a|a.state.as_str()),"to":restored.state}))).await?;
        }
        if room.paused != snapshot.room.paused {
            sqlx::query("UPDATE room_state SET paused=$1 WHERE singleton")
                .bind(snapshot.room.paused)
                .execute(&mut *tx)
                .await
                .map_err(db)?;
            append_event(
                &mut tx,
                if snapshot.room.paused {
                    "room_paused"
                } else {
                    "room_resumed"
                },
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await?;
        }
        sqlx::query("UPDATE room_state SET applications_open=$1,updated_at=GREATEST(updated_at,clock_timestamp()) WHERE singleton").bind(snapshot.room.applications_open).execute(&mut *tx).await.map_err(db)?;
        let mut retracted = 0;
        for id in &snapshot.retracted_event_ids {
            let target:Option<i64>=sqlx::query_scalar("SELECT seq FROM events WHERE event_id=$1 AND kind='agent_message' AND retracted_at IS NULL").bind(id).fetch_optional(&mut *tx).await.map_err(db)?;
            if let Some(seq) = target {
                sqlx::query("UPDATE events SET public_text=NULL,retracted_at=clock_timestamp() WHERE seq=$1").bind(seq).execute(&mut *tx).await.map_err(db)?;
                sqlx::query("DELETE FROM event_mentions WHERE seq=$1")
                    .bind(seq)
                    .execute(&mut *tx)
                    .await
                    .map_err(db)?;
                let reason = snapshot
                    .operator_actions
                    .iter()
                    .rev()
                    .find(|a| a.kind == "retract" && a.target_event_id.as_deref() == Some(id))
                    .and_then(|a| a.reason_code.as_deref())
                    .unwrap_or("other");
                append_event(
                    &mut tx,
                    "event_retracted",
                    None,
                    None,
                    None,
                    Some(seq),
                    Some(reason),
                    None,
                )
                .await?;
                audit(
                    &mut tx,
                    "restore_controls_retract",
                    None,
                    Some(id),
                    None,
                    Some(reason),
                    Some(json!({"source_export_id":snapshot.export_id})),
                )
                .await?;
                retracted += 1;
            }
        }
        for action in &snapshot.operator_actions {
            sqlx::query("INSERT INTO operator_actions SELECT * FROM jsonb_populate_record(NULL::operator_actions,$1) ON CONFLICT(action_id) DO NOTHING").bind(serde_json::to_value(action).map_err(|_|invalid_snapshot())?).execute(&mut *tx).await.map_err(db)?;
        }
        audit(&mut tx,"restore_controls_completed",None,None,None,None,Some(json!({"source_export_id":snapshot.export_id,"snapshot_sha256":self.plan.snapshot_sha256}))).await?;
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?;
        observer(pid).await?;
        tx.commit().await.map_err(db)?;
        Ok(RestoreResult {
            export_id: snapshot.export_id,
            already_applied: false,
            identities: snapshot.agents.len(),
            retractions: retracted,
        })
    }
}

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct Attempt {
    pub client_message_id: String,
    pub created_at: DateTime<Utc>,
    pub outcome: String,
    pub reason_code: Option<String>,
    pub reason_detail: Option<String>,
    pub event_seq: Option<i64>,
}
impl Store {
    pub async fn attempts(&self, id: &str) -> Result<Vec<Attempt>> {
        self.attempts_since(id, None).await
    }
    pub async fn attempts_since(
        &self,
        id: &str,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<Attempt>> {
        sqlx::query_as("SELECT client_message_id,created_at,outcome,reason_code,reason_detail,event_seq FROM message_attempts WHERE agent_id=$1 AND ($2::timestamptz IS NULL OR created_at >= $2) ORDER BY created_at,client_message_id").bind(id).bind(since).fetch_all(&self.pool).await.map_err(db)
    }
}

#[cfg(test)]
mod restore_tests {
    use super::*;
    use sqlx::PgPool;
    use tolight_core::signing::public_key_from_seed;
    async fn active(store: &Store, id: &str, seed: u8) -> AgentContext {
        let key = public_key_from_seed(&[seed; 32]);
        store
            .apply(
                ApplicationInput {
                    agent_id: id.into(),
                    display_name: id.into(),
                    display_name_key: id.into(),
                    bio: "本地恢复测试".into(),
                    owner_contact: "owner@example.invalid".into(),
                    framework_label: None,
                    public_key: key.clone(),
                },
                ApplyLimits::default(),
            )
            .await
            .unwrap();
        let context = AgentContext {
            agent_id: id.into(),
            public_key: key,
        };
        store
            .conformance_transaction(&context, |data| {
                data.agent.conformance_passed_at = Some(Utc::now());
                data.agent.conformance_suite_version =
                    Some(tolight_core::CONFORMANCE_SUITE_VERSION.into());
            })
            .await
            .unwrap();
        store
            .approve(&store.review(id).await.unwrap(), true)
            .await
            .unwrap();
        context
    }
    async fn export(store: &Store, dir: &Path) -> (ControlSnapshot, Vec<u8>) {
        let path = dir.join(format!("snapshot-{}.json", Uuid::new_v4()));
        let snapshot = store
            .export_controls(&path, &dir.join("public"), Utc::now() - Duration::hours(1))
            .await
            .unwrap();
        (snapshot, fs::read(path).unwrap())
    }
    #[sqlx::test(migrations = "./migrations")]
    async fn schema_one_snapshots_without_key_registration_projection_still_restore(pool: PgPool) {
        let store = Store::from_pool(pool);
        active(&store, "legacy-agent", 163).await;
        let expected = store
            .review("legacy-agent")
            .await
            .unwrap()
            .public_key_registered_at;
        let dir = tempfile::tempdir().unwrap();
        let (_, bytes) = export(&store, dir.path()).await;
        let mut legacy: Value = serde_json::from_slice(&bytes).unwrap();
        for agent in legacy["agents"].as_array_mut().unwrap() {
            assert!(
                agent
                    .as_object_mut()
                    .unwrap()
                    .remove("public_key_registered_at")
                    .is_some()
            );
        }
        let prepared = store
            .prepare_restore_controls(
                &serde_json::to_vec(&legacy).unwrap(),
                tolight_core::CONFORMANCE_SUITE_VERSION,
            )
            .await
            .unwrap();
        prepared.commit(&[]).await.unwrap();
        assert_eq!(
            store
                .review("legacy-agent")
                .await
                .unwrap()
                .public_key_registered_at,
            expected
        );
    }
    #[sqlx::test(migrations = "./migrations")]
    async fn stale_active_snapshot_cannot_revive_a_now_left_application_even_with_approval(
        pool: PgPool,
    ) {
        let store = Store::from_pool(pool.clone());
        let context = active(&store, "sealed-alice", 139).await;
        let application_id = store.review("sealed-alice").await.unwrap().application_id;
        let dir = tempfile::tempdir().unwrap();
        let (_, bytes) = export(&store, dir.path()).await;
        store.kick("sealed-alice", "spam").await.unwrap();
        let before_count: i64 = sqlx::query_scalar("SELECT count(*) FROM operator_actions")
            .fetch_one(&pool)
            .await
            .unwrap();
        let result = match store.prepare_restore_controls(&bytes, "1").await {
            Ok(prepared) => prepared.commit(&[application_id]).await.map(|_| ()),
            Err(error) => Err(error),
        };
        assert!(
            result.is_err(),
            "a fresh recovery approval resurrected a sealed LEFT application"
        );
        let agent = store.review("sealed-alice").await.unwrap();
        assert_eq!(agent.application_id, application_id);
        assert_eq!(agent.state, "LEFT");
        assert!(agent.public_key.is_none());
        assert!(
            store
                .read_snapshot(&context.agent_id, &context.public_key)
                .await
                .is_err()
        );
        assert_eq!(
            before_count,
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM operator_actions")
                .fetch_one(&pool)
                .await
                .unwrap()
        );
    }
    #[test]
    fn nonexistent_relative_private_paths_resolve_from_the_current_directory() {
        let name = format!("missing-private-{}.json", Uuid::new_v4());
        assert_eq!(
            canonical_target(Path::new(&name)).unwrap(),
            std::env::current_dir().unwrap().join(name)
        );
        assert!(
            validate_private_path(Path::new("private.json"), Path::new("missing-public-root"))
                .is_ok()
        );
    }
    #[sqlx::test(migrations = "./migrations")]
    async fn failed_or_unknown_migration_is_rejected_on_the_dedicated_connection(pool: PgPool) {
        let store = Store::from_pool(pool.clone());
        active(&store, "alice", 115).await;
        let dir = tempfile::tempdir().unwrap();
        let (_, bytes) = export(&store, dir.path()).await;
        sqlx::query("INSERT INTO _sqlx_migrations(version,description,success,checksum,execution_time) VALUES(2,'failed',FALSE,decode('00','hex'),0)").execute(&pool).await.unwrap();
        assert!(
            store
                .export_controls(
                    &dir.path().join("blocked.json"),
                    &dir.path().join("public"),
                    Utc::now()
                )
                .await
                .is_err()
        );
        assert!(!dir.path().join("blocked.json").exists());
        assert!(store.prepare_restore_controls(&bytes, "1").await.is_err());
    }
    #[sqlx::test(migrations = "./migrations")]
    async fn private_export_is_read_only_exclusive_and_rejects_public_aliases(pool: PgPool) {
        let store = Store::from_pool(pool.clone());
        active(&store, "alice", 101).await;
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("public")).unwrap();
        let path = dir.path().join("private.json");
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM operator_actions")
            .fetch_one(&pool)
            .await
            .unwrap();
        store
            .export_controls(&path, &dir.path().join("public"), Utc::now())
            .await
            .unwrap();
        assert!(
            store
                .export_controls(&path, &dir.path().join("public"), Utc::now())
                .await
                .is_err()
        );
        assert!(
            store
                .export_controls(
                    &dir.path().join("public/private.json"),
                    &dir.path().join("public"),
                    Utc::now()
                )
                .await
                .is_err()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            symlink(dir.path().join("public"), dir.path().join("alias")).unwrap();
            assert!(
                store
                    .export_controls(
                        &dir.path().join("alias/private.json"),
                        &dir.path().join("public"),
                        Utc::now()
                    )
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            count,
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM operator_actions")
                .fetch_one(&pool)
                .await
                .unwrap()
        );
        let _lease = store.acquire_serve_lock().await.unwrap();
        assert!(
            store
                .export_controls(
                    &dir.path().join("blocked.json"),
                    &dir.path().join("public"),
                    Utc::now()
                )
                .await
                .is_err()
        );
        assert!(!dir.path().join("blocked.json").exists());
    }
    #[sqlx::test(migrations = "./migrations")]
    async fn final_keys_mutes_and_room_switches_restore_even_when_anchor_matches(pool: PgPool) {
        let store = Store::from_pool(pool.clone());
        let old = active(&store, "alice", 102).await;
        let new = public_key_from_seed(&[103; 32]);
        store.set_key("alice", &new).await.unwrap();
        // Force sub-microsecond input on every platform. Recovery must match
        // the persisted source deadline, not the host clock's finer precision.
        let until =
            DateTime::from_timestamp((Utc::now() + Duration::hours(2)).timestamp(), 123_456_789)
                .unwrap();
        store.mute("alice", Some(until), "spam").await.unwrap();
        let expected_until = store.review("alice").await.unwrap().muted_until.unwrap();
        store.applications_open(false).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (snapshot, bytes) = export(&store, dir.path()).await;
        // Rewind only private controls: public event anchors still match, so anchor checking alone cannot recover them.
        sqlx::query("UPDATE agent_keys SET revoked_at=clock_timestamp() WHERE public_key=$1")
            .bind(&new)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE agent_keys SET revoked_at=NULL WHERE public_key=$1")
            .bind(&old.public_key)
            .execute(&pool)
            .await
            .unwrap();
        store.unmute("alice").await.unwrap();
        store.applications_open(true).await.unwrap();
        let prepared = store.prepare_restore_controls(&bytes, "1").await.unwrap();
        assert_eq!(prepared.plan.source_anchor, prepared.plan.target_anchor);
        assert!(prepared.plan.required_approvals.is_empty());
        prepared.commit(&[]).await.unwrap();
        let restored = store.review("alice").await.unwrap();
        assert_eq!(restored.public_key, Some(new));
        assert_eq!(restored.muted_until, Some(expected_until));
        assert!(!store.room_state().await.unwrap().applications_open);
        let later = public_key_from_seed(&[104; 32]);
        store.set_key("alice", &later).await.unwrap();
        store.unmute("alice").await.unwrap();
        let again = store.prepare_restore_controls(&bytes, "1").await.unwrap();
        assert!(again.plan.already_applied);
        again.commit(&[]).await.unwrap();
        assert_eq!(store.review("alice").await.unwrap().public_key, Some(later));
        assert!(store.review("alice").await.unwrap().muted_until.is_none());
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM operator_actions WHERE kind='restore_controls_completed' AND detail->>'source_export_id'=$1").bind(snapshot.export_id.to_string()).fetch_one(&pool).await.unwrap();
        assert_eq!(count, 1);
        let mut changed = bytes;
        changed.push(b' ');
        assert!(store.prepare_restore_controls(&changed, "1").await.is_err());
    }
    #[sqlx::test(migrations = "./migrations")]
    async fn recovered_active_identity_requires_fresh_explicit_approval_and_keeps_old_registration(
        pool: PgPool,
    ) {
        let store = Store::from_pool(pool.clone());
        active(&store, "alice", 105).await;
        sqlx::query("UPDATE agents SET registered_at=clock_timestamp()-INTERVAL '30 days' WHERE agent_id='alice'").execute(&pool).await.unwrap();
        let original = store.review("alice").await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (_, bytes) = export(&store, dir.path()).await;
        sqlx::query("UPDATE agents SET state='PENDING',joined_at=NULL,conformance_passed_at=NULL,conformance_suite_version=NULL WHERE agent_id='alice'").execute(&pool).await.unwrap();
        let prepared = store.prepare_restore_controls(&bytes, "1").await.unwrap();
        assert_eq!(
            prepared.plan.required_approvals,
            vec![original.application_id]
        );
        assert!(prepared.commit(&[]).await.is_err());
        assert_eq!(store.review("alice").await.unwrap().state, "PENDING");
        store
            .prepare_restore_controls(&bytes, "1")
            .await
            .unwrap()
            .commit(&[original.application_id])
            .await
            .unwrap();
        let restored = store.review("alice").await.unwrap();
        assert_eq!(restored.state, "ACTIVE");
        assert_eq!(restored.application_id, original.application_id);
        assert_eq!(restored.registered_at, original.registered_at);
        assert!(restored.application_revision > original.application_revision);
        store.cleanup_expired(Duration::days(14)).await.unwrap();
        assert!(store.agent("alice").await.unwrap().is_some());
    }
    #[sqlx::test(migrations = "./migrations")]
    async fn missing_final_left_identity_is_reserved_with_revoked_keys(pool: PgPool) {
        let store = Store::from_pool(pool.clone());
        active(&store, "alice", 106).await;
        store.kick("alice", "spam").await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (snapshot, bytes) = export(&store, dir.path()).await;
        sqlx::query("DELETE FROM events")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE room_state SET last_seq=0")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM agents")
            .execute(&pool)
            .await
            .unwrap();
        let prepared = store.prepare_restore_controls(&bytes, "1").await.unwrap();
        assert!(prepared.plan.required_approvals.is_empty());
        prepared.commit(&[]).await.unwrap();
        let agent = store.review("alice").await.unwrap();
        assert_eq!(agent.application_id, snapshot.agents[0].application_id);
        assert_eq!(agent.state, "LEFT");
        assert!(agent.public_key.is_none());
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM agent_keys WHERE revoked_at IS NULL"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            0
        );
    }
    #[sqlx::test(migrations = "./migrations")]
    async fn lost_withdrawal_replaces_only_old_pending_identity_not_names(pool: PgPool) {
        let store = Store::from_pool(pool.clone());
        let old = active(&store, "alice", 107).await;
        // Construct a T-time pending identity and a later source pending identity with the same name.
        sqlx::query("DELETE FROM events")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE room_state SET last_seq=0")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE agents SET state='PENDING',joined_at=NULL WHERE agent_id='alice'")
            .execute(&pool)
            .await
            .unwrap();
        let before = store.review("alice").await.unwrap();
        store.owner_leave(&old, "withdraw").await.unwrap();
        store
            .apply(
                ApplicationInput {
                    agent_id: "alice".into(),
                    display_name: "alice".into(),
                    display_name_key: "alice".into(),
                    bio: "新的待审身份".into(),
                    owner_contact: "new@example.invalid".into(),
                    framework_label: None,
                    public_key: old.public_key.clone(),
                },
                ApplyLimits::default(),
            )
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (source, bytes) = export(&store, dir.path()).await;
        // A T-time backup contains the old row; the immutable identity trigger
        // correctly prevents faking that state with UPDATE application_id.
        sqlx::query("DELETE FROM agents WHERE agent_id='alice'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO agents SELECT * FROM jsonb_populate_record(NULL::agents,$1)")
            .bind(serde_json::to_value(&before).unwrap())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO agent_keys(public_key,agent_id,created_at) VALUES($1,'alice',clock_timestamp())")
            .bind(&old.public_key).execute(&pool).await.unwrap();
        store
            .prepare_restore_controls(&bytes, "1")
            .await
            .unwrap()
            .commit(&[])
            .await
            .unwrap();
        assert_eq!(
            store.review("alice").await.unwrap().application_id,
            source.agents[0].application_id
        );
        let old_audit: Uuid = sqlx::query_scalar(
            "SELECT target_application_id FROM operator_actions WHERE kind='owner_withdraw'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(old_audit, before.application_id);
    }
    #[sqlx::test(migrations = "./migrations")]
    async fn malformed_private_fields_keys_and_stale_plan_do_not_write(pool: PgPool) {
        let store = Store::from_pool(pool.clone());
        active(&store, "alice", 108).await;
        let dir = tempfile::tempdir().unwrap();
        let (_, bytes) = export(&store, dir.path()).await;
        let mut malformed: Value = serde_json::from_slice(&bytes).unwrap();
        malformed["agents"][0]["owner_contact"] = json!("bad\ncontact");
        assert!(
            store
                .prepare_restore_controls(&serde_json::to_vec(&malformed).unwrap(), "1")
                .await
                .is_err()
        );
        let mut malformed: Value = serde_json::from_slice(&bytes).unwrap();
        malformed["agent_keys"][0]["public_key"] = json!("weak");
        assert!(
            store
                .prepare_restore_controls(&serde_json::to_vec(&malformed).unwrap(), "1")
                .await
                .is_err()
        );
        let prepared = store.prepare_restore_controls(&bytes, "1").await.unwrap();
        store.mute("alice", None, "spam").await.unwrap();
        assert!(prepared.commit(&[]).await.is_err());
        assert!(store.review("alice").await.unwrap().muted_indefinitely);
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM operator_actions WHERE kind='restore_controls_completed'"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            0
        );
    }

    async fn recovery_database_state(pool: &PgPool) -> Value {
        sqlx::query_scalar(
            "SELECT jsonb_build_object(
              'agents', (SELECT jsonb_agg(to_jsonb(a) ORDER BY agent_id) FROM agents a),
              'keys', (SELECT jsonb_agg(to_jsonb(k) ORDER BY public_key) FROM agent_keys k),
              'events', (SELECT jsonb_agg(to_jsonb(e) ORDER BY seq) FROM events e),
              'mentions', (SELECT jsonb_agg(to_jsonb(m) ORDER BY seq,agent_id) FROM event_mentions m),
              'attempts', (SELECT jsonb_agg(to_jsonb(t) ORDER BY agent_id,client_message_id) FROM message_attempts t),
              'runs', (SELECT jsonb_agg(to_jsonb(c) ORDER BY run_id) FROM conformance_runs c),
              'actions', (SELECT jsonb_agg(to_jsonb(o) ORDER BY action_id) FROM operator_actions o),
              'room', (SELECT to_jsonb(r) FROM room_state r))",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn malformed_lifecycle_and_mute_controls_cannot_be_restored(pool: PgPool) {
        let store = Store::from_pool(pool.clone());
        active(&store, "controls-active", 180).await;
        store
            .apply(
                ApplicationInput {
                    agent_id: "controls-pending".into(),
                    display_name: "controls-pending".into(),
                    display_name_key: "controls-pending".into(),
                    bio: "本地恢复控制待审状态".into(),
                    owner_contact: "owner@example.invalid".into(),
                    framework_label: None,
                    public_key: public_key_from_seed(&[181; 32]),
                },
                ApplyLimits::default(),
            )
            .await
            .unwrap();
        active(&store, "controls-left", 182).await;
        store.kick("controls-left", "owner_request").await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (_, bytes) = export(&store, dir.path()).await;
        let original: Value = serde_json::from_slice(&bytes).unwrap();
        let before = recovery_database_state(&pool).await;
        let until = json!(Utc::now() + Duration::hours(1));
        let cases = [
            ("controls-active", json!({"state_reason":"undocumented"})),
            ("controls-active", json!({"state_reason":"spam"})),
            ("controls-pending", json!({"state_reason":"other"})),
            (
                "controls-pending",
                json!({"state":"REJECTED","state_reason":null}),
            ),
            (
                "controls-pending",
                json!({"state":"REJECTED","state_reason":"undocumented"}),
            ),
            ("controls-left", json!({"state_reason":null})),
            ("controls-left", json!({"state_reason":"undocumented"})),
            ("controls-active", json!({"mute_reason":"spam"})),
            ("controls-active", json!({"muted_indefinitely":true})),
            ("controls-active", json!({"muted_until":until})),
            (
                "controls-active",
                json!({"muted_indefinitely":true,"mute_reason":"undocumented"}),
            ),
            (
                "controls-active",
                json!({"muted_until":until,"mute_reason":"undocumented"}),
            ),
            (
                "controls-active",
                json!({"muted_indefinitely":true,"muted_until":until,"mute_reason":"spam"}),
            ),
            (
                "controls-pending",
                json!({"muted_indefinitely":true,"mute_reason":"spam"}),
            ),
            (
                "controls-pending",
                json!({"state":"REJECTED","state_reason":"other","muted_until":until,"mute_reason":"spam"}),
            ),
            (
                "controls-left",
                json!({"muted_indefinitely":true,"mute_reason":"spam"}),
            ),
        ];
        for (id, fields) in cases {
            let mut malformed = original.clone();
            let agent = malformed["agents"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|agent| agent["agent_id"] == id)
                .unwrap()
                .as_object_mut()
                .unwrap();
            agent.extend(fields.as_object().unwrap().clone());
            let error = match store
                .prepare_restore_controls(&serde_json::to_vec(&malformed).unwrap(), "1")
                .await
            {
                Ok(prepared) => prepared.commit(&[]).await.unwrap_err(),
                Err(error) => error,
            };
            assert_eq!(error.code, "bad_request", "{id}: {fields}");
            assert_eq!(error.fields["field"], "control_snapshot");
            assert_eq!(
                recovery_database_state(&pool).await,
                before,
                "{id}: {fields}"
            );
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn valid_lifecycle_reasons_and_absolute_mute_controls_still_restore(pool: PgPool) {
        let store = Store::from_pool(pool);
        for (index, id) in [
            "mute-expired",
            "mute-future",
            "mute-indefinite",
            "mute-cleared",
            "controls-left",
        ]
        .into_iter()
        .enumerate()
        {
            active(&store, id, 190 + index as u8).await;
        }
        store
            .mute(
                "mute-expired",
                Some(Utc::now() - Duration::hours(2)),
                "other",
            )
            .await
            .unwrap();
        store
            .mute("mute-future", Some(Utc::now() + Duration::hours(2)), "spam")
            .await
            .unwrap();
        store
            .mute("mute-indefinite", None, "owner_request")
            .await
            .unwrap();
        store.mute("mute-cleared", None, "runaway").await.unwrap();
        store.unmute("mute-cleared").await.unwrap();
        store.kick("controls-left", "owner_request").await.unwrap();
        for (id, seed) in [("controls-pending", 195), ("controls-rejected", 196)] {
            store
                .apply(
                    ApplicationInput {
                        agent_id: id.into(),
                        display_name: id.into(),
                        display_name_key: id.into(),
                        bio: "本地恢复控制合法状态".into(),
                        owner_contact: "owner@example.invalid".into(),
                        framework_label: None,
                        public_key: public_key_from_seed(&[seed; 32]),
                    },
                    ApplyLimits::default(),
                )
                .await
                .unwrap();
        }
        store
            .reject("controls-rejected", "bio_inadequate")
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (snapshot, bytes) = export(&store, dir.path()).await;
        for id in ["mute-expired", "mute-future", "mute-indefinite"] {
            store.unmute(id).await.unwrap();
        }
        store.mute("mute-cleared", None, "content").await.unwrap();
        store
            .prepare_restore_controls(&bytes, "1")
            .await
            .unwrap()
            .commit(&[])
            .await
            .unwrap();
        for source in snapshot.agents {
            let restored = store.review(&source.agent_id).await.unwrap();
            assert_eq!(restored.state, source.state);
            assert_eq!(restored.state_reason, source.state_reason);
            assert_eq!(restored.muted_until, source.muted_until);
            assert_eq!(restored.muted_indefinitely, source.muted_indefinitely);
            assert_eq!(restored.mute_reason, source.mute_reason);
        }
    }
    #[sqlx::test(migrations = "./migrations")]
    async fn killing_the_same_export_or_restore_transaction_has_zero_commit(pool: PgPool) {
        let store = Store::from_pool(pool.clone());
        active(&store, "alice", 109).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("killed.json");
        let killer = pool.clone();
        let result = store
            .export_controls_observed(
                &path,
                &dir.path().join("public"),
                Utc::now(),
                move |pid| async move {
                    sqlx::query_scalar::<_, bool>("SELECT pg_terminate_backend($1)")
                        .bind(pid)
                        .fetch_one(&killer)
                        .await
                        .map_err(db)?;
                    Ok(())
                },
            )
            .await;
        assert!(result.is_err());
        assert!(!path.exists());
        store.mute("alice", None, "spam").await.unwrap();
        let (_, bytes) = export(&store, dir.path()).await;
        store.unmute("alice").await.unwrap();
        let prepared = store.prepare_restore_controls(&bytes, "1").await.unwrap();
        let killer = pool.clone();
        let result = prepared
            .commit_observed(&[], move |pid| async move {
                sqlx::query_scalar::<_, bool>("SELECT pg_terminate_backend($1)")
                    .bind(pid)
                    .fetch_one(&killer)
                    .await
                    .map_err(db)?;
                Ok(())
            })
            .await;
        assert!(result.is_err());
        assert!(!store.review("alice").await.unwrap().muted_indefinitely);
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM operator_actions WHERE kind='restore_controls_completed'"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            0
        );
        store
            .prepare_restore_controls(&bytes, "1")
            .await
            .unwrap()
            .commit(&[])
            .await
            .unwrap();
        assert!(store.review("alice").await.unwrap().muted_indefinitely);
    }
}
