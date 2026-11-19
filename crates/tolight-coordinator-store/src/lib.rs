//! PostgreSQL persistence. Only this crate contains SQL or SQLx types.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool, Postgres, Row, Transaction, postgres::PgPoolOptions};
use time::{Duration, OffsetDateTime};
use tolight_core::{mentions, models::ApiError};
use uuid::Uuid;

pub const SCHEMA_VERSION: i64 = 1;
const APPLICATION_LOCK: i64 = 0x746f6c6967687401;
pub const SERVE_LOCK: i64 = 0x746f6c6967687402;
pub type Result<T> = std::result::Result<T, ApiError>;

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
}

#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    pub agent_id: String,
    pub application_id: Uuid,
    pub application_revision: i64,
    pub display_name: String,
    pub display_name_key: String,
    pub bio: String,
    pub framework_label: Option<String>,
    pub owner_contact: String,
    pub state: String,
    pub state_reason: Option<String>,
    #[serde(with = "tolight_core::datetime::serde")]
    pub state_changed_at: OffsetDateTime,
    #[serde(with = "tolight_core::datetime::serde")]
    pub registered_at: OffsetDateTime,
    #[serde(default, with = "tolight_core::datetime::serde::option")]
    pub joined_at: Option<OffsetDateTime>,
    #[serde(default, with = "tolight_core::datetime::serde::option")]
    pub muted_until: Option<OffsetDateTime>,
    pub muted_indefinitely: bool,
    pub mute_reason: Option<String>,
    pub conformance_suite_version: Option<String>,
    #[serde(default, with = "tolight_core::datetime::serde::option")]
    pub conformance_passed_at: Option<OffsetDateTime>,
    #[serde(default, with = "tolight_core::datetime::serde::option")]
    pub last_seen_at: Option<OffsetDateTime>,
    pub public_key: Option<String>,
    #[serde(default, with = "tolight_core::datetime::serde::option")]
    pub public_key_registered_at: Option<OffsetDateTime>,
}
pub type ReviewSnapshot = Agent;

#[derive(Clone, Debug)]
pub enum ReviewedControl {
    Mute {
        seconds: Option<u64>,
        reason: String,
    },
    Unmute,
    Kick {
        reason: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApplicationInput {
    pub agent_id: String,
    pub display_name: String,
    pub display_name_key: String,
    pub bio: String,
    pub owner_contact: String,
    pub framework_label: Option<String>,
    pub public_key: String,
}
#[derive(Clone, Debug)]
pub struct ApplyLimits {
    pub max_pending_applications: i64,
    pub application_ttl: Duration,
    pub operator_bypass: bool,
    pub source_retry_after: Option<u64>,
}
impl Default for ApplyLimits {
    fn default() -> Self {
        Self {
            max_pending_applications: 50,
            application_ttl: Duration::days(14),
            operator_bypass: false,
            source_retry_after: None,
        }
    }
}
#[derive(Clone, Debug)]
pub struct ApplyResult {
    pub agent: Agent,
    pub created: bool,
    pub counts_for_daily: bool,
}
#[derive(Clone, Debug)]
pub struct SendInput {
    pub agent_id: String,
    pub public_key: String,
    pub client_message_id: String,
    /// Sanitized public text, or a sanitizer reason code. Rejected text is never passed here.
    pub text: std::result::Result<String, String>,
    pub reply_to_event_id: Option<String>,
    pub mentions: Vec<String>,
}
#[derive(Clone, Debug)]
pub struct SendLimits {
    pub messages_per_minute: usize,
    pub messages_per_day: usize,
    pub max_mentions_per_message: usize,
}
impl Default for SendLimits {
    fn default() -> Self {
        Self {
            messages_per_minute: 2,
            messages_per_day: 100,
            max_mentions_per_message: 3,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SendResult {
    pub seq: i64,
    pub event_id: String,
    pub duplicate: bool,
    pub mentions: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct RoomState {
    pub paused: bool,
    pub applications_open: bool,
    pub last_seq: i64,
    #[serde(with = "tolight_core::datetime::serde")]
    pub updated_at: OffsetDateTime,
}
#[derive(Clone, Debug)]
pub struct PublicSnapshot {
    pub events: Vec<Value>,
    pub agents: Vec<Value>,
    pub last_seq: i64,
    pub paused: bool,
}
#[derive(Clone, Debug)]
pub struct ReadSnapshot {
    pub agent: Agent,
    pub events: Vec<Value>,
    pub last_seq: i64,
    pub paused: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct Usage {
    pub minute: usize,
    pub day: usize,
    #[serde(default, with = "tolight_core::datetime::serde::option")]
    pub next_allowed_at: Option<OffsetDateTime>,
}

macro_rules! agent_sql {
    ($tail:literal) => { concat!("SELECT a.*, (SELECT k.public_key FROM agent_keys k WHERE k.agent_id=a.agent_id AND k.revoked_at IS NULL) AS public_key, (SELECT k.created_at FROM agent_keys k WHERE k.agent_id=a.agent_id AND k.revoked_at IS NULL) AS public_key_registered_at ", $tail) };
}
fn db(_: sqlx::Error) -> ApiError {
    ApiError::new("unavailable").retry(1)
}
fn missing() -> ApiError {
    ApiError::new("not_found")
}
fn unauthorized() -> ApiError {
    ApiError::new("unauthorized")
}
fn invalid_state() -> ApiError {
    ApiError::new("invalid_state")
}
fn unique(error: sqlx::Error) -> ApiError {
    if let Some(error) = error.as_database_error() {
        return match error.constraint() {
            Some("agents_pkey") => ApiError::new("agent_id_taken"),
            Some("agents_display_name_key_key") => ApiError::new("display_name_taken"),
            Some("agent_keys_pkey") | Some("agent_keys_one_active") => {
                ApiError::new("key_already_registered")
            }
            _ => ApiError::new("unavailable"),
        };
    }
    db(error)
}

impl Store {
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(url)
            .await
            .map_err(db)?;
        Ok(Self { pool })
    }
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
    pub async fn migrate(&self) -> Result<()> {
        let encoding: String = sqlx::query_scalar("SHOW server_encoding")
            .fetch_one(&self.pool)
            .await
            .map_err(db)?;
        if encoding != "UTF8" {
            return Err(ApiError::new("schema_mismatch"));
        }
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .map_err(|_| ApiError::new("unavailable"))?;
        Ok(())
    }
    pub async fn check_schema(&self) -> Result<()> {
        let version: Option<i64> =
            sqlx::query_scalar!("SELECT version FROM schema_version WHERE singleton=TRUE")
                .fetch_optional(&self.pool)
                .await
                .map_err(|_| ApiError::new("schema_mismatch"))?;
        if version != Some(SCHEMA_VERSION) {
            return Err(ApiError::new("schema_mismatch"));
        }
        let latest: Option<i64> =
            sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success")
                .fetch_one(&self.pool)
                .await
                .map_err(|_| ApiError::new("schema_mismatch"))?;
        if latest != Some(SCHEMA_VERSION) {
            return Err(ApiError::new("schema_mismatch"));
        }
        let unknown: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM _sqlx_migrations WHERE NOT success OR version<>1",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|_| ApiError::new("schema_mismatch"))?;
        if unknown != 0 {
            return Err(ApiError::new("schema_mismatch"));
        }
        Ok(())
    }
    pub async fn room_state(&self) -> Result<RoomState> {
        sqlx::query_as(
            "SELECT paused,applications_open,last_seq,updated_at FROM room_state WHERE singleton",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(db)
    }
    pub async fn agent(&self, id: &str) -> Result<Option<Agent>> {
        sqlx::query_as(agent_sql!("FROM agents a WHERE a.agent_id=$1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)
    }
    pub async fn agent_by_key(&self, key: &str) -> Result<Option<Agent>> {
        sqlx::query_as(agent_sql!("FROM agents a JOIN agent_keys lookup ON lookup.agent_id=a.agent_id WHERE lookup.public_key=$1 AND lookup.revoked_at IS NULL")).bind(key).fetch_optional(&self.pool).await.map_err(db)
    }
    pub async fn list_agents(&self, pending: bool) -> Result<Vec<Agent>> {
        sqlx::query_as(agent_sql!(
            "FROM agents a WHERE NOT $1 OR a.state='PENDING' ORDER BY a.registered_at,a.agent_id"
        ))
        .bind(pending)
        .fetch_all(&self.pool)
        .await
        .map_err(db)
    }
    pub async fn list_active_agents(&self) -> Result<Vec<Agent>> {
        sqlx::query_as(agent_sql!(
            "FROM agents a WHERE a.state='ACTIVE' ORDER BY a.joined_at,a.agent_id"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(db)
    }
    pub async fn review(&self, id: &str) -> Result<ReviewSnapshot> {
        self.agent(id).await?.ok_or_else(missing)
    }
    pub async fn cleanup_expired(&self, ttl: Duration) -> Result<u64> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        application_lock(&mut tx).await?;
        let count = cleanup(&mut tx, ttl).await?;
        tx.commit().await.map_err(db)?;
        Ok(count)
    }
    pub async fn touch_last_seen(&self, id: &str, key: &str) -> Result<()> {
        // The common read path needs no transaction or row lock. Only an actual
        // throttled activity update enters the guarded write transaction.
        let needs_update:Option<bool>=sqlx::query_scalar("SELECT a.last_seen_at IS NULL OR a.last_seen_at<=clock_timestamp()-INTERVAL '30 seconds' FROM agents a JOIN agent_keys k ON k.agent_id=a.agent_id WHERE a.agent_id=$1 AND k.public_key=$2 AND k.revoked_at IS NULL")
            .bind(id).bind(key).fetch_optional(&self.pool).await.map_err(db)?;
        if !needs_update.ok_or_else(unauthorized)? {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.map_err(db)?;
        let agent = lock_agent(&mut tx, id).await?;
        check_key(&agent, key)?;
        sqlx::query("UPDATE agents SET last_seen_at=clock_timestamp() WHERE agent_id=$1 AND (last_seen_at IS NULL OR last_seen_at <= clock_timestamp()-INTERVAL '30 seconds')").bind(id).execute(&mut *tx).await.map_err(db)?;
        tx.commit().await.map_err(db)
    }
    pub async fn public_snapshot(&self) -> Result<PublicSnapshot> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        let room = room_in(&mut tx).await?;
        let events = public_events(&mut tx).await?;
        let agents = public_agents(&mut tx).await?;
        tx.commit().await.map_err(db)?;
        Ok(PublicSnapshot {
            events,
            agents,
            last_seq: room.last_seq,
            paused: room.paused,
        })
    }
    pub async fn public_participants(&self) -> Result<Vec<Value>> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let agents = public_agents(&mut tx).await?;
        tx.commit().await.map_err(db)?;
        Ok(agents)
    }
    pub async fn public_event_by_id(&self, id: &str) -> Result<Option<Value>> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        let sequence: Option<i64> = sqlx::query_scalar("SELECT seq FROM events WHERE event_id=$1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?;
        let event = if let Some(sequence) = sequence {
            selected_public_events(&mut tx, Some(&[sequence]))
                .await?
                .pop()
        } else {
            None
        };
        tx.commit().await.map_err(db)?;
        Ok(event)
    }
    /// Complete changed chunks and the previous manifest anchor, all read from
    /// one snapshot. A caught-up Publisher only needs the anchor and metadata.
    pub async fn public_snapshot_since(
        &self,
        start: i64,
        chunk_size: i64,
    ) -> Result<PublicSnapshot> {
        if start < 0 || chunk_size < 1 {
            return Err(ApiError::new("bad_request"));
        }
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        let room = room_in(&mut tx).await?;
        let sequences: Vec<i64> = sqlx::query_scalar(
            "SELECT seq FROM events WHERE seq=$1 AND seq<=$3 UNION SELECT seq FROM events WHERE $1<$3 AND seq>($1/$2)*$2 AND seq<=$3 UNION SELECT e.seq FROM (SELECT DISTINCT ((target_seq-1)/$2)*$2 AS base FROM events WHERE seq>$1 AND seq<=$3 AND kind='event_retracted') changed JOIN events e ON e.seq>changed.base AND e.seq<=LEAST(changed.base::numeric+$2,$3)::bigint ORDER BY seq",
        )
        .bind(start).bind(chunk_size).bind(room.last_seq)
        .fetch_all(&mut *tx).await.map_err(db)?;
        let events = selected_public_events(&mut tx, Some(&sequences)).await?;
        let agents = public_agents(&mut tx).await?;
        tx.commit().await.map_err(db)?;
        Ok(PublicSnapshot {
            events,
            agents,
            last_seq: room.last_seq,
            paused: room.paused,
        })
    }
    pub async fn read_snapshot(&self, id: &str, key: &str) -> Result<ReadSnapshot> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        let agent = agent_in(&mut tx, id).await?.ok_or_else(unauthorized)?;
        check_key(&agent, key)?;
        let room = room_in(&mut tx).await?;
        let events = if agent.state == "ACTIVE" {
            public_events(&mut tx).await?
        } else {
            vec![]
        };
        tx.commit().await.map_err(db)?;
        Ok(ReadSnapshot {
            agent,
            events,
            last_seq: room.last_seq,
            paused: room.paused,
        })
    }
    /// Authentication and room metadata without loading any timeline content.
    pub async fn metadata_snapshot(&self, id: &str, key: &str) -> Result<ReadSnapshot> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        let agent = agent_in(&mut tx, id).await?.ok_or_else(unauthorized)?;
        check_key(&agent, key)?;
        let room = room_in(&mut tx).await?;
        tx.commit().await.map_err(db)?;
        Ok(ReadSnapshot {
            agent,
            events: vec![],
            last_seq: room.last_seq,
            paused: room.paused,
        })
    }

    /// Fetch only the requested candidates, plus cursor/head anchors needed by
    /// the pure protocol selector. Long-lived rooms must not be materialized on
    /// every poll; even a mentions-only scan returns at most 202 event values.
    pub async fn participant_read_snapshot(
        &self,
        id: &str,
        key: &str,
        request: &tolight_core::participant::ReadRequest,
        config: &tolight_core::config::Config,
    ) -> Result<ReadSnapshot> {
        use tolight_core::participant::decode_cursor;
        request.validate(config)?;
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        let agent = agent_in(&mut tx, id).await?.ok_or_else(unauthorized)?;
        check_key(&agent, key)?;
        let room = room_in(&mut tx).await?;
        let mut events = vec![];
        if agent.state == "ACTIVE" {
            let cursor = request.after.as_deref().and_then(decode_cursor);
            let mut valid = false;
            if let Some(cursor) = &cursor
                && cursor.timeline == "public"
                && cursor.session.is_none()
                && cursor.seq <= room.last_seq as u64
            {
                valid = if cursor.seq == 0 {
                    cursor.event_id.is_none()
                } else {
                    let anchor: Option<String> =
                        sqlx::query_scalar("SELECT event_id FROM events WHERE seq=$1")
                            .bind(cursor.seq as i64)
                            .fetch_optional(&mut *tx)
                            .await
                            .map_err(db)?;
                    anchor.is_some() && cursor.event_id == anchor
                };
            }
            let mut selected: Vec<i64> = if valid {
                let after = cursor.as_ref().expect("valid cursor").seq as i64;
                let limit = request.limit.unwrap_or(config.page_default_limit).min(200) as i64;
                if request.mentions_only {
                    sqlx::query_scalar("SELECT e.seq FROM event_mentions m JOIN events e ON e.seq=m.seq WHERE m.agent_id=$1 AND e.seq>$2 AND e.seq<=$3 AND e.kind='agent_message' AND e.retracted_at IS NULL AND e.agent_id<>$1 ORDER BY e.seq LIMIT $4")
                        .bind(id).bind(after).bind(room.last_seq).bind(limit)
                        .fetch_all(&mut *tx).await.map_err(db)?
                } else {
                    sqlx::query_scalar(
                        "SELECT seq FROM events WHERE seq>$1 AND seq<=$2 ORDER BY seq LIMIT $3",
                    )
                    .bind(after)
                    .bind(room.last_seq)
                    .bind(limit)
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(db)?
                }
            } else {
                let limit = request
                    .limit
                    .unwrap_or(config.read_default_limit)
                    .clamp(1, 50) as i64;
                sqlx::query_scalar(
                    "SELECT seq FROM events WHERE seq<=$1 ORDER BY seq DESC LIMIT $2",
                )
                .bind(room.last_seq)
                .bind(limit)
                .fetch_all(&mut *tx)
                .await
                .map_err(db)?
            };
            if let Some(cursor) = &cursor
                && cursor.seq <= room.last_seq as u64
            {
                // Include even a mismatched anchor: otherwise an invalid
                // cursor with a missing ID could compare equal to a missing
                // candidate and accidentally become valid in the selector.
                selected.push(cursor.seq as i64);
            }
            // The selector needs the head ID for context/empty reads and needs
            // to advance past nonmatching events when a mention page is short.
            selected.push(room.last_seq);
            selected.sort_unstable();
            selected.dedup();
            events = selected_public_events(&mut tx, Some(&selected)).await?;
        }
        tx.commit().await.map_err(db)?;
        Ok(ReadSnapshot {
            agent,
            events,
            last_seq: room.last_seq,
            paused: room.paused,
        })
    }
}

async fn application_lock(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(APPLICATION_LOCK)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    Ok(())
}
async fn cleanup(tx: &mut Transaction<'_, Postgres>, ttl: Duration) -> Result<u64> {
    Ok(sqlx::query("DELETE FROM agents WHERE state IN ('PENDING','REJECTED') AND registered_at < clock_timestamp()-make_interval(secs=>$1)").bind(ttl.whole_seconds() as f64).execute(&mut **tx).await.map_err(db)?.rows_affected())
}
async fn agent_in(tx: &mut Transaction<'_, Postgres>, id: &str) -> Result<Option<Agent>> {
    sqlx::query_as(agent_sql!("FROM agents a WHERE a.agent_id=$1"))
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)
}
async fn lock_agent(tx: &mut Transaction<'_, Postgres>, id: &str) -> Result<Agent> {
    // A JOIN/subquery in the locking statement can retain a pre-wait MVCC
    // snapshot even after EvalPlanQual refreshes the locked agent row. Obtain
    // the row lock first, then read the current key in a new READ COMMITTED
    // statement. This is the revocation barrier for every in-flight writer.
    let locked: Option<String> =
        sqlx::query_scalar("SELECT agent_id FROM agents WHERE agent_id=$1 FOR NO KEY UPDATE")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(db)?;
    if locked.is_none() {
        return Err(unauthorized());
    }
    agent_in(tx, id).await?.ok_or_else(unauthorized)
}
fn check_key(agent: &Agent, key: &str) -> Result<()> {
    if agent.public_key.as_deref() != Some(key) {
        Err(unauthorized())
    } else {
        Ok(())
    }
}
async fn room_in(tx: &mut Transaction<'_, Postgres>) -> Result<RoomState> {
    sqlx::query_as(
        "SELECT paused,applications_open,last_seq,updated_at FROM room_state WHERE singleton",
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(db)
}
async fn lock_room(tx: &mut Transaction<'_, Postgres>) -> Result<RoomState> {
    sqlx::query_as("SELECT paused,applications_open,last_seq,updated_at FROM room_state WHERE singleton FOR UPDATE").fetch_one(&mut **tx).await.map_err(db)
}
async fn public_agents(tx: &mut Transaction<'_, Postgres>) -> Result<Vec<Value>> {
    sqlx::query_scalar::<_,Value>(r#"SELECT jsonb_build_object('agent_id',agent_id,'display_name',display_name,'bio',bio,'joined_at',to_char(joined_at AT TIME ZONE 'UTC','YYYY-MM-DD"T"HH24:MI:SS"Z"')) FROM agents WHERE state='ACTIVE' ORDER BY date_trunc('second',joined_at),agent_id"#).fetch_all(&mut **tx).await.map_err(db)
}
async fn public_events(tx: &mut Transaction<'_, Postgres>) -> Result<Vec<Value>> {
    selected_public_events(tx, None).await
}
macro_rules! public_events_sql {
    ($tail:literal) => { concat!(r#"
SELECT jsonb_build_object('schema','tolight-event/1','seq',e.seq,'event_id',e.event_id,'kind',e.kind,'created_at',to_char(e.created_at AT TIME ZONE 'UTC','YYYY-MM-DD"T"HH24:MI:SS"Z"'),'retracted',e.retracted_at IS NOT NULL)
 || CASE WHEN e.agent_id IS NOT NULL THEN jsonb_build_object('agent',jsonb_build_object('agent_id',a.agent_id,'display_name',a.display_name)) ELSE '{}'::jsonb END
 || CASE WHEN e.kind='agent_message' THEN jsonb_build_object('reply_to',CASE WHEN reply.seq IS NOT NULL THEN jsonb_build_object('seq',reply.seq,'event_id',reply.event_id) ELSE NULL END,'mentions',COALESCE((SELECT jsonb_agg(jsonb_build_object('agent_id',ma.agent_id,'display_name',ma.display_name) ORDER BY ma.agent_id) FROM event_mentions em JOIN agents ma ON ma.agent_id=em.agent_id WHERE em.seq=e.seq),'[]'::jsonb)) ELSE '{}'::jsonb END
 || CASE WHEN e.public_text IS NOT NULL THEN jsonb_build_object('text',e.public_text) ELSE '{}'::jsonb END
 || CASE WHEN e.kind='event_retracted' THEN jsonb_build_object('target',jsonb_build_object('seq',target.seq,'event_id',target.event_id),'reason_code',e.reason_code) ELSE '{}'::jsonb END
FROM events e LEFT JOIN agents a ON a.agent_id=e.agent_id LEFT JOIN events reply ON reply.seq=e.reply_to_seq LEFT JOIN events target ON target.seq=e.target_seq
"#, $tail) };
}
async fn selected_public_events(
    tx: &mut Transaction<'_, Postgres>,
    sequences: Option<&[i64]>,
) -> Result<Vec<Value>> {
    if let Some(sequences) = sequences {
        sqlx::query_scalar::<_, Value>(public_events_sql!(" WHERE e.seq=ANY($1) ORDER BY e.seq"))
            .bind(sequences)
            .fetch_all(&mut **tx)
            .await
            .map_err(db)
    } else {
        sqlx::query_scalar::<_, Value>(public_events_sql!(" ORDER BY e.seq"))
            .fetch_all(&mut **tx)
            .await
            .map_err(db)
    }
}

pub struct ServeLease {
    connection: PgConnection,
}
impl ServeLease {
    pub async fn check(&mut self) -> Result<()> {
        sqlx::query("SELECT 1")
            .execute(&mut self.connection)
            .await
            .map_err(db)?;
        Ok(())
    }
    pub async fn backend_pid(&mut self) -> Result<i32> {
        sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut self.connection)
            .await
            .map_err(db)
    }
}

impl Store {
    pub async fn acquire_serve_lock(&self) -> Result<ServeLease> {
        use sqlx::Connection;
        let mut connection = PgConnection::connect_with(&self.pool.connect_options())
            .await
            .map_err(db)?;
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(SERVE_LOCK)
            .fetch_one(&mut connection)
            .await
            .map_err(db)?;
        if !acquired {
            return Err(ApiError::new("already_running"));
        }
        Ok(ServeLease { connection })
    }
    pub async fn apply(&self, input: ApplicationInput, limits: ApplyLimits) -> Result<ApplyResult> {
        tolight_core::signing::decode_public_key(&input.public_key)
            .map_err(|_| ApiError::new("bad_request").with("field", "public_key"))?;
        let mut tx = self.pool.begin().await.map_err(db)?;
        application_lock(&mut tx).await?;
        cleanup(&mut tx, limits.application_ttl).await?;
        let key_row: Option<(String, bool)> = sqlx::query_as(
            "SELECT agent_id,revoked_at IS NOT NULL FROM agent_keys WHERE public_key=$1",
        )
        .bind(&input.public_key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?;
        if let Some((id, revoked)) = &key_row {
            if *revoked || id != &input.agent_id {
                return Err(ApiError::new("key_already_registered"));
            }
            let agent = lock_agent(&mut tx, id).await?;
            if agent.public_key.as_deref() != Some(input.public_key.as_str()) {
                return Err(ApiError::new("key_already_registered"));
            }
            if agent.state != "REJECTED" {
                tx.commit().await.map_err(db)?;
                return Ok(ApplyResult {
                    agent,
                    created: false,
                    counts_for_daily: false,
                });
            }
        } else if agent_in(&mut tx, &input.agent_id).await?.is_some() {
            return Err(ApiError::new("agent_id_taken"));
        }
        if !limits.operator_bypass {
            let open: bool =
                sqlx::query_scalar("SELECT applications_open FROM room_state WHERE singleton")
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(db)?;
            if !open {
                return Err(ApiError::new("applications_closed"));
            }
            if let Some(retry) = limits.source_retry_after {
                return Err(ApiError::new("rate_limited").retry(retry));
            }
            let count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM agents WHERE state='PENDING'")
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(db)?;
            if count >= limits.max_pending_applications {
                return Err(ApiError::new("applications_full").retry(60));
            }
        }
        let created = key_row.is_none();
        if created {
            sqlx::query("INSERT INTO agents(agent_id,application_id,display_name,display_name_key,bio,framework_label,owner_contact,state,state_changed_at,registered_at) VALUES($1,$2,$3,$4,$5,$6,$7,'PENDING',clock_timestamp(),clock_timestamp())")
                .bind(&input.agent_id).bind(Uuid::new_v4()).bind(&input.display_name).bind(&input.display_name_key).bind(&input.bio).bind(&input.framework_label).bind(&input.owner_contact).execute(&mut *tx).await.map_err(unique)?;
            sqlx::query("INSERT INTO agent_keys(public_key,agent_id,created_at) VALUES($1,$2,clock_timestamp())").bind(&input.public_key).bind(&input.agent_id).execute(&mut *tx).await.map_err(unique)?;
        } else {
            sqlx::query("UPDATE agents SET display_name=$2,display_name_key=$3,bio=$4,framework_label=$5,owner_contact=$6,state='PENDING',state_reason=NULL,state_changed_at=clock_timestamp(),application_revision=application_revision+1 WHERE agent_id=$1 AND state='REJECTED'")
                .bind(&input.agent_id).bind(&input.display_name).bind(&input.display_name_key).bind(&input.bio).bind(&input.framework_label).bind(&input.owner_contact).execute(&mut *tx).await.map_err(unique)?;
        }
        let agent = agent_in(&mut tx, &input.agent_id)
            .await?
            .ok_or_else(missing)?;
        if limits.operator_bypass {
            audit(&mut tx, "add", Some(&agent), None, None, None, None).await?;
        }
        tx.commit().await.map_err(db)?;
        Ok(ApplyResult {
            agent,
            created,
            counts_for_daily: !limits.operator_bypass,
        })
    }
    pub async fn approve(
        &self,
        snapshot: &ReviewSnapshot,
        require_conformance: bool,
    ) -> Result<Agent> {
        self.approve_with_ttl(snapshot, require_conformance, Duration::days(14))
            .await
    }
    pub async fn approve_with_ttl(
        &self,
        snapshot: &ReviewSnapshot,
        require_conformance: bool,
        ttl: Duration,
    ) -> Result<Agent> {
        self.cleanup_expired(ttl).await?;
        let mut tx = self.pool.begin().await.map_err(db)?;
        let agent = lock_agent(&mut tx, &snapshot.agent_id)
            .await
            .map_err(|_| missing())?;
        if agent.application_id != snapshot.application_id
            || agent.application_revision != snapshot.application_revision
            || agent.public_key != snapshot.public_key
        {
            return Err(ApiError::new("review_changed"));
        }
        if agent.state != "PENDING"
            || (require_conformance
                && (agent.conformance_passed_at.is_none()
                    || agent.conformance_suite_version.as_deref()
                        != Some(tolight_core::CONFORMANCE_SUITE_VERSION)))
        {
            return Err(invalid_state());
        }
        lock_room(&mut tx).await?;
        sqlx::query("UPDATE agents SET state='ACTIVE',state_reason=NULL,state_changed_at=clock_timestamp(),joined_at=clock_timestamp(),application_revision=application_revision+1 WHERE agent_id=$1").bind(&agent.agent_id).execute(&mut *tx).await.map_err(db)?;
        append_event(
            &mut tx,
            "agent_joined",
            Some(&agent.agent_id),
            None,
            None,
            None,
            None,
            None,
        )
        .await?;
        audit(&mut tx, "approve", Some(&agent), None, None, None, None).await?;
        let result = agent_in(&mut tx, &agent.agent_id)
            .await?
            .ok_or_else(missing)?;
        tx.commit().await.map_err(db)?;
        Ok(result)
    }
    pub async fn reject(&self, id: &str, reason: &str) -> Result<Agent> {
        self.reject_with_ttl(id, reason, Duration::days(14)).await
    }
    pub async fn reject_with_ttl(&self, id: &str, reason: &str, ttl: Duration) -> Result<Agent> {
        self.reject_reviewed_inner(id, None, reason, ttl).await
    }
    /// Reject only the application snapshot actually reviewed by the Operator.
    pub async fn reject_reviewed_with_ttl(
        &self,
        snapshot: &ReviewSnapshot,
        reason: &str,
        ttl: Duration,
    ) -> Result<Agent> {
        self.reject_reviewed_inner(&snapshot.agent_id, Some(snapshot), reason, ttl)
            .await
    }
    async fn reject_reviewed_inner(
        &self,
        id: &str,
        snapshot: Option<&ReviewSnapshot>,
        reason: &str,
        ttl: Duration,
    ) -> Result<Agent> {
        self.cleanup_expired(ttl).await?;
        let mut tx = self.pool.begin().await.map_err(db)?;
        let agent = lock_agent(&mut tx, id).await.map_err(|_| missing())?;
        if snapshot.is_some_and(|snapshot| {
            agent.application_id != snapshot.application_id
                || agent.application_revision != snapshot.application_revision
                || agent.public_key != snapshot.public_key
        }) {
            return Err(ApiError::new("review_changed"));
        }
        if agent.state != "PENDING" {
            return Err(invalid_state());
        }
        sqlx::query("UPDATE agents SET state='REJECTED',state_reason=$2,state_changed_at=clock_timestamp(),application_revision=application_revision+1 WHERE agent_id=$1").bind(id).bind(reason).execute(&mut *tx).await.map_err(db)?;
        audit(
            &mut tx,
            "reject",
            Some(&agent),
            None,
            None,
            Some(reason),
            None,
        )
        .await?;
        let result = agent_in(&mut tx, id).await?.ok_or_else(missing)?;
        tx.commit().await.map_err(db)?;
        Ok(result)
    }
    pub async fn set_key(&self, id: &str, key: &str) -> Result<Agent> {
        self.set_key_with_options(id, key, "other", Duration::days(14))
            .await
    }
    pub async fn set_key_with_options(
        &self,
        id: &str,
        key: &str,
        reason: &str,
        ttl: Duration,
    ) -> Result<Agent> {
        self.set_key_inner(id, key, reason, ttl, None).await
    }
    pub async fn set_key_reviewed(
        &self,
        snapshot: &ReviewSnapshot,
        key: &str,
        reason: &str,
        ttl: Duration,
    ) -> Result<Agent> {
        self.set_key_inner(&snapshot.agent_id, key, reason, ttl, Some(snapshot))
            .await
    }
    async fn set_key_inner(
        &self,
        id: &str,
        key: &str,
        reason: &str,
        ttl: Duration,
        reviewed: Option<&ReviewSnapshot>,
    ) -> Result<Agent> {
        tolight_core::signing::decode_public_key(key)
            .map_err(|_| ApiError::new("bad_request").with("field", "public_key"))?;
        self.cleanup_expired(ttl).await?;
        let mut tx = self.pool.begin().await.map_err(db)?;
        let agent = lock_agent(&mut tx, id).await.map_err(|_| missing())?;
        if reviewed.is_some_and(|snapshot| {
            snapshot.application_id != agent.application_id
                || snapshot.application_revision != agent.application_revision
                || snapshot.public_key != agent.public_key
        }) {
            return Err(ApiError::new("review_changed"));
        }
        if agent.state == "LEFT" {
            return Err(invalid_state());
        }
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM agent_keys WHERE public_key=$1)")
                .bind(key)
                .fetch_one(&mut *tx)
                .await
                .map_err(db)?;
        if exists {
            return Err(ApiError::new("key_already_registered"));
        }
        sqlx::query("UPDATE agent_keys SET revoked_at=clock_timestamp() WHERE agent_id=$1 AND revoked_at IS NULL").bind(id).execute(&mut *tx).await.map_err(db)?;
        sqlx::query("INSERT INTO agent_keys(public_key,agent_id,created_at) VALUES($1,$2,clock_timestamp())").bind(key).bind(id).execute(&mut *tx).await.map_err(unique)?;
        sqlx::query(
            "UPDATE agents SET application_revision=application_revision+1 WHERE agent_id=$1",
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        audit(
            &mut tx,
            "set_key",
            Some(&agent),
            None,
            Some(key),
            Some(reason),
            Some(json!({"old_public_key":agent.public_key})),
        )
        .await?;
        let result = agent_in(&mut tx, id).await?.ok_or_else(missing)?;
        tx.commit().await.map_err(db)?;
        Ok(result)
    }
    pub async fn applications_open(&self, open: bool) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query("UPDATE room_state SET applications_open=$1 WHERE singleton")
            .bind(open)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        audit(
            &mut tx,
            if open {
                "applications_open"
            } else {
                "applications_close"
            },
            None,
            None,
            None,
            None,
            None,
        )
        .await?;
        tx.commit().await.map_err(db)
    }
    pub async fn pause(&self, paused: bool, reason: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let room = lock_room(&mut tx).await?;
        if room.paused == paused {
            audit(
                &mut tx,
                if paused { "pause" } else { "resume" },
                None,
                None,
                None,
                Some(reason),
                None,
            )
            .await?;
            tx.commit().await.map_err(db)?;
            return Ok(false);
        }
        sqlx::query("UPDATE room_state SET paused=$1 WHERE singleton")
            .bind(paused)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        append_event(
            &mut tx,
            if paused {
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
        audit(
            &mut tx,
            if paused { "pause" } else { "resume" },
            None,
            None,
            None,
            Some(reason),
            None,
        )
        .await?;
        tx.commit().await.map_err(db)?;
        Ok(true)
    }
    pub async fn mute(&self, id: &str, until: Option<OffsetDateTime>, reason: &str) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let agent = lock_agent(&mut tx, id).await?;
        if agent.state != "ACTIVE" {
            return Err(invalid_state());
        }
        mute_in(&mut tx, &agent, until, reason).await?;
        tx.commit().await.map_err(db)
    }
    pub async fn unmute(&self, id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let agent = lock_agent(&mut tx, id).await?;
        if agent.state != "ACTIVE" {
            return Err(invalid_state());
        }
        unmute_in(&mut tx, &agent).await?;
        tx.commit().await.map_err(db)
    }
    pub async fn control_reviewed(
        &self,
        snapshot: &ReviewSnapshot,
        control: ReviewedControl,
    ) -> Result<Agent> {
        match &control {
            ReviewedControl::Mute { seconds, reason } => {
                if seconds.is_some_and(|seconds| !(1..=31_536_000).contains(&seconds)) {
                    return Err(ApiError::new("bad_request").with("field", "mute_seconds"));
                }
                if !tolight_core::validation::is_reason_code(reason) {
                    return Err(ApiError::new("bad_request").with("field", "reason"));
                }
            }
            ReviewedControl::Kick { reason } => {
                if !tolight_core::validation::is_reason_code(reason) {
                    return Err(ApiError::new("bad_request").with("field", "reason"));
                }
            }
            ReviewedControl::Unmute => {}
        }
        let mut tx = self.pool.begin().await.map_err(db)?;
        let agent = lock_agent(&mut tx, &snapshot.agent_id).await?;
        // CLI controls do not change application_revision. Compare their raw
        // fields too, while holding the same row lock used by those controls.
        if agent.application_id != snapshot.application_id
            || agent.application_revision != snapshot.application_revision
            || agent.public_key != snapshot.public_key
            || agent.muted_until != snapshot.muted_until
            || agent.muted_indefinitely != snapshot.muted_indefinitely
            || agent.mute_reason != snapshot.mute_reason
        {
            return Err(ApiError::new("review_changed"));
        }
        if agent.state != "ACTIVE" || snapshot.state != "ACTIVE" {
            return Err(invalid_state());
        }
        match control {
            ReviewedControl::Mute { seconds, reason } => {
                let now: OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(db)?;
                let until = seconds
                    .map(|seconds| {
                        now.checked_add(Duration::seconds(seconds as i64))
                            .ok_or_else(|| {
                                ApiError::new("bad_request").with("field", "mute_seconds")
                            })
                    })
                    .transpose()?;
                mute_in(&mut tx, &agent, until, &reason).await?;
            }
            ReviewedControl::Unmute => unmute_in(&mut tx, &agent).await?,
            ReviewedControl::Kick { reason } => {
                leave_active_in(&mut tx, &agent, &reason, false).await?;
            }
        }
        let result = agent_in(&mut tx, &agent.agent_id)
            .await?
            .ok_or_else(missing)?;
        tx.commit().await.map_err(db)?;
        Ok(result)
    }
    pub async fn kick(&self, id: &str, reason: &str) -> Result<()> {
        self.leave_inner(id, None, reason, false, None).await
    }
    pub async fn withdraw(&self, id: &str, key: &str) -> Result<()> {
        self.leave_inner(id, Some(key), "other", true, None).await
    }
    async fn leave_inner(
        &self,
        id: &str,
        key: Option<&str>,
        reason: &str,
        owner: bool,
        mode: Option<&str>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let agent = lock_agent(&mut tx, id).await?;
        if let Some(key) = key {
            check_key(&agent, key)?;
        }
        if let Some(mode) = mode {
            let valid = match mode {
                "withdraw" => matches!(agent.state.as_str(), "PENDING" | "REJECTED"),
                "leave" => agent.state == "ACTIVE",
                _ => false,
            };
            if !valid {
                return Err(invalid_state());
            }
        }
        if matches!(agent.state.as_str(), "PENDING" | "REJECTED") && owner {
            audit(
                &mut tx,
                "owner_withdraw",
                Some(&agent),
                None,
                None,
                None,
                None,
            )
            .await?;
            sqlx::query("DELETE FROM agents WHERE agent_id=$1")
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(db)?;
        } else if agent.state == "ACTIVE" {
            leave_active_in(&mut tx, &agent, reason, owner).await?;
        } else {
            return Err(invalid_state());
        }
        tx.commit().await.map_err(db)
    }
}

async fn mute_in(
    tx: &mut Transaction<'_, Postgres>,
    agent: &Agent,
    until: Option<OffsetDateTime>,
    reason: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE agents SET muted_until=$2,muted_indefinitely=$3,mute_reason=$4 WHERE agent_id=$1",
    )
    .bind(&agent.agent_id)
    .bind(until)
    .bind(until.is_none())
    .bind(reason)
    .execute(&mut **tx)
    .await
    .map_err(db)?;
    audit(
        tx,
        "mute",
        Some(agent),
        None,
        None,
        Some(reason),
        Some(json!({"until":until.map(tolight_core::datetime::json_timestamp)})),
    )
    .await
}

async fn unmute_in(tx: &mut Transaction<'_, Postgres>, agent: &Agent) -> Result<()> {
    sqlx::query("UPDATE agents SET muted_until=NULL,muted_indefinitely=FALSE,mute_reason=NULL WHERE agent_id=$1").bind(&agent.agent_id).execute(&mut **tx).await.map_err(db)?;
    audit(tx, "unmute", Some(agent), None, None, None, None).await
}

async fn leave_active_in(
    tx: &mut Transaction<'_, Postgres>,
    agent: &Agent,
    reason: &str,
    owner: bool,
) -> Result<()> {
    lock_room(tx).await?;
    sqlx::query("UPDATE agents SET state='LEFT',state_reason=$2,state_changed_at=clock_timestamp(),muted_until=NULL,muted_indefinitely=FALSE,mute_reason=NULL,application_revision=application_revision+1 WHERE agent_id=$1").bind(&agent.agent_id).bind(reason).execute(&mut **tx).await.map_err(db)?;
    sqlx::query("UPDATE agent_keys SET revoked_at=clock_timestamp() WHERE agent_id=$1 AND revoked_at IS NULL").bind(&agent.agent_id).execute(&mut **tx).await.map_err(db)?;
    append_event(
        tx,
        "agent_left",
        Some(&agent.agent_id),
        None,
        None,
        None,
        None,
        None,
    )
    .await?;
    audit(
        tx,
        if owner { "owner_leave" } else { "kick" },
        Some(agent),
        None,
        None,
        Some(reason),
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    kind: &str,
    agent: Option<&Agent>,
    event: Option<&str>,
    key: Option<&str>,
    reason: Option<&str>,
    detail: Option<Value>,
) -> Result<()> {
    sqlx::query("INSERT INTO operator_actions(action_id,kind,target_agent_id,target_application_id,target_event_id,target_public_key,reason_code,detail,created_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,clock_timestamp())")
        .bind(Uuid::new_v4().to_string()).bind(kind).bind(agent.map(|a|a.agent_id.as_str())).bind(agent.map(|a|a.application_id)).bind(event).bind(key).bind(reason).bind(detail).execute(&mut **tx).await.map_err(db)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn append_event(
    tx: &mut Transaction<'_, Postgres>,
    kind: &str,
    agent: Option<&str>,
    text: Option<&str>,
    reply: Option<i64>,
    target: Option<i64>,
    reason: Option<&str>,
    clock_override: Option<OffsetDateTime>,
) -> Result<(i64, String)> {
    // room_state is the only sequence allocator. Rollback returns the number,
    // and the previous event clamps both transaction-order and wall-clock skew.
    let (seq,created):(i64,OffsetDateTime)=sqlx::query_as("UPDATE room_state SET last_seq=last_seq+1,updated_at=GREATEST(COALESCE($1,clock_timestamp()),COALESCE((SELECT created_at FROM events ORDER BY seq DESC LIMIT 1),'-infinity'::timestamptz)) WHERE singleton RETURNING last_seq,updated_at")
        .bind(clock_override).fetch_one(&mut **tx).await.map_err(db)?;
    let event_id = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO events(seq,event_id,kind,agent_id,public_text,reply_to_seq,target_seq,reason_code,created_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
        .bind(seq).bind(&event_id).bind(kind).bind(agent).bind(text).bind(reply).bind(target).bind(reason).bind(created).execute(&mut **tx).await.map_err(db)?;
    Ok((seq, event_id))
}

impl Store {
    pub async fn usage(&self, id: &str, limits: &SendLimits) -> Result<Usage> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let now: OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?;
        let usage = usage_in(&mut tx, id, limits, now).await?;
        tx.commit().await.map_err(db)?;
        Ok(usage)
    }
    pub async fn send(&self, input: SendInput, limits: SendLimits) -> Result<SendResult> {
        mentions::validate_explicit(&input.mentions)?;
        if limits.messages_per_minute == 0 || limits.messages_per_day == 0 {
            return Err(ApiError::new("bad_request"));
        }
        let mut tx = self.pool.begin().await.map_err(db)?;
        let agent = lock_agent(&mut tx, &input.agent_id).await?;
        check_key(&agent, &input.public_key)?;
        let attempt=sqlx::query("SELECT outcome,reason_code,reason_detail,event_seq FROM message_attempts WHERE agent_id=$1 AND client_message_id=$2")
            .bind(&input.agent_id).bind(&input.client_message_id).fetch_optional(&mut *tx).await.map_err(db)?;
        if let Some(attempt) = attempt {
            let outcome: String = attempt.try_get("outcome").map_err(db)?;
            if outcome == "published" {
                let seq: i64 = attempt.try_get("event_seq").map_err(db)?;
                let event_id: String =
                    sqlx::query_scalar("SELECT event_id FROM events WHERE seq=$1")
                        .bind(seq)
                        .fetch_one(&mut *tx)
                        .await
                        .map_err(db)?;
                let mentions = sqlx::query_scalar(
                    "SELECT agent_id FROM event_mentions WHERE seq=$1 ORDER BY agent_id",
                )
                .bind(seq)
                .fetch_all(&mut *tx)
                .await
                .map_err(db)?;
                tx.commit().await.map_err(db)?;
                return Ok(SendResult {
                    seq,
                    event_id,
                    duplicate: true,
                    mentions,
                });
            }
            let reason: String = attempt.try_get("reason_code").map_err(db)?;
            let detail: Option<String> = attempt.try_get("reason_detail").map_err(db)?;
            return Err(rejected_error(
                &reason,
                detail.as_deref(),
                limits.max_mentions_per_message,
            ));
        }
        if agent.state != "ACTIVE" {
            return Err(ApiError::new("agent_not_active"));
        }
        let now: OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?;
        if agent.muted_indefinitely || agent.muted_until.is_some_and(|until| until > now) {
            let until = if agent.muted_indefinitely {
                None
            } else {
                agent.muted_until
            };
            let mut error = ApiError::new("agent_muted")
                .with("muted_until", until.map(format_time))
                .with("reason", agent.mute_reason);
            if let Some(until) = until {
                error = error.retry(seconds_until(now, until));
            }
            return Err(error);
        }
        if room_in(&mut tx).await?.paused {
            return Err(ApiError::new("room_paused"));
        }
        let usage = usage_in(&mut tx, &input.agent_id, &limits, now).await?;
        if let Some(next) = usage.next_allowed_at {
            let scope = if usage.day >= limits.messages_per_day {
                "day"
            } else {
                "minute"
            };
            return Err(ApiError::new("message_limit")
                .with("scope", scope)
                .retry(seconds_until(now, next)));
        }
        let text = match input.text {
            Ok(text) => text,
            Err(reason) => {
                record_rejected(
                    &mut tx,
                    &input.agent_id,
                    &input.client_message_id,
                    &reason,
                    None,
                )
                .await?;
                tx.commit().await.map_err(db)?;
                return Err(rejected_error(
                    &reason,
                    None,
                    limits.max_mentions_per_message,
                ));
            }
        };
        let room = lock_room(&mut tx).await?;
        if room.paused {
            return Err(ApiError::new("room_paused"));
        }
        let reply: Option<(i64, String)> = if let Some(event_id) = input.reply_to_event_id {
            sqlx::query_as("SELECT seq,agent_id FROM events WHERE event_id=$1 AND kind='agent_message' AND retracted_at IS NULL").bind(event_id).fetch_optional(&mut *tx).await.map_err(db)?
        } else {
            None
        };
        let active: Vec<String> = sqlx::query_scalar(
            "SELECT agent_id FROM agents WHERE state='ACTIVE' ORDER BY agent_id",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;
        let mentions = match mentions::resolve_with_reply(
            &text,
            &input.mentions,
            &input.agent_id,
            &active,
            reply.as_ref().map(|(_, author)| author.as_str()),
            limits.max_mentions_per_message,
        ) {
            Ok(mentions) => mentions,
            Err(error) => {
                let detail = error.fields.get("agent_id").and_then(Value::as_str);
                record_rejected(
                    &mut tx,
                    &input.agent_id,
                    &input.client_message_id,
                    &error.code,
                    detail,
                )
                .await?;
                tx.commit().await.map_err(db)?;
                return Err(error);
            }
        };
        let (seq, event_id) = append_event(
            &mut tx,
            "agent_message",
            Some(&input.agent_id),
            Some(&text),
            reply.map(|(seq, _)| seq),
            None,
            None,
            None,
        )
        .await?;
        for mention in &mentions {
            sqlx::query("INSERT INTO event_mentions(seq,agent_id) VALUES($1,$2)")
                .bind(seq)
                .bind(mention)
                .execute(&mut *tx)
                .await
                .map_err(db)?;
        }
        sqlx::query("INSERT INTO message_attempts(agent_id,client_message_id,created_at,outcome,event_seq) VALUES($1,$2,clock_timestamp(),'published',$3)")
            .bind(&input.agent_id).bind(&input.client_message_id).bind(seq).execute(&mut *tx).await.map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(SendResult {
            seq,
            event_id,
            duplicate: false,
            mentions,
        })
    }
    pub async fn retract(&self, event_id: &str, reason: &str) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        lock_room(&mut tx).await?;
        let row:Option<(i64,bool)>=sqlx::query_as("SELECT seq,retracted_at IS NOT NULL FROM events WHERE event_id=$1 AND kind='agent_message'").bind(event_id).fetch_optional(&mut *tx).await.map_err(db)?;
        let (seq, retracted) = row.ok_or_else(missing)?;
        if retracted {
            return Err(invalid_state());
        }
        sqlx::query(
            "UPDATE events SET public_text=NULL,retracted_at=clock_timestamp() WHERE seq=$1",
        )
        .bind(seq)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query("DELETE FROM event_mentions WHERE seq=$1")
            .bind(seq)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
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
            "retract",
            None,
            Some(event_id),
            None,
            Some(reason),
            None,
        )
        .await?;
        tx.commit().await.map_err(db)
    }
}

pub fn format_time(time: OffsetDateTime) -> String {
    tolight_core::datetime::utc_seconds(time)
}
fn seconds_until(now: OffsetDateTime, next: OffsetDateTime) -> u64 {
    std::time::Duration::try_from(next - now)
        .map(|duration| duration.as_secs() + u64::from(duration.subsec_nanos() > 0))
        .unwrap_or(1)
        .max(1)
}
async fn usage_in(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    limits: &SendLimits,
    now: OffsetDateTime,
) -> Result<Usage> {
    let times:Vec<OffsetDateTime>=sqlx::query_scalar("SELECT created_at FROM message_attempts WHERE agent_id=$1 AND created_at>$2 ORDER BY created_at").bind(id).bind(now-Duration::days(1)).fetch_all(&mut **tx).await.map_err(db)?;
    let minute: Vec<_> = times
        .iter()
        .copied()
        .filter(|time| *time > now - Duration::seconds(60))
        .collect();
    let recovery = |times: &[OffsetDateTime], limit: usize, window: Duration| {
        if limit > 0 && times.len() >= limit {
            Some(times[times.len() - limit] + window)
        } else {
            None
        }
    };
    Ok(Usage {
        minute: minute.len(),
        day: times.len(),
        next_allowed_at: recovery(&minute, limits.messages_per_minute, Duration::seconds(60))
            .max(recovery(&times, limits.messages_per_day, Duration::days(1))),
    })
}
async fn record_rejected(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    client_id: &str,
    reason: &str,
    detail: Option<&str>,
) -> Result<()> {
    sqlx::query("INSERT INTO message_attempts(agent_id,client_message_id,created_at,outcome,reason_code,reason_detail) VALUES($1,$2,clock_timestamp(),'rejected',$3,$4)").bind(id).bind(client_id).bind(reason).bind(detail).execute(&mut **tx).await.map_err(db)?;
    Ok(())
}
fn rejected_error(reason: &str, detail: Option<&str>, max: usize) -> ApiError {
    match reason {
        "invalid_mention" => ApiError::new("invalid_mention").with("agent_id", detail),
        "too_many_mentions" => ApiError::new("too_many_mentions").with("max", max),
        _ => ApiError::new("content_rejected").with("reason_code", reason),
    }
}

#[cfg(test)]
mod transaction_tests {
    use super::*;
    #[test]
    fn retry_after_rounds_up_even_submillisecond_remainders() {
        let now = OffsetDateTime::now_utc();
        assert_eq!(seconds_until(now, now + Duration::seconds(1)), 1);
        assert_eq!(
            seconds_until(now, now + Duration::seconds(1) + Duration::nanoseconds(1)),
            2
        );
        assert_eq!(seconds_until(now, now + Duration::nanoseconds(1)), 1);
        assert_eq!(seconds_until(now, now), 1);
        assert_eq!(seconds_until(now, now - Duration::nanoseconds(1)), 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn rollback_returns_sequence_and_all_event_append_paths_clamp_wall_clock(pool: PgPool) {
        let store = Store::from_pool(pool);
        let input = ApplicationInput {
            agent_id: "alice".into(),
            display_name: "Alice".into(),
            display_name_key: "alice".into(),
            bio: "公开简介".into(),
            owner_contact: "owner@example.invalid".into(),
            framework_label: None,
            public_key: tolight_core::signing::public_key_from_seed(&[1; 32]),
        };
        let agent = store
            .apply(input, ApplyLimits::default())
            .await
            .unwrap()
            .agent;
        store.approve(&agent, false).await.unwrap();
        let mut rolled_back = store.pool.begin().await.unwrap();
        lock_room(&mut rolled_back).await.unwrap();
        let (reserved, _) = append_event(
            &mut rolled_back,
            "agent_message",
            Some("alice"),
            Some("will roll back"),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(reserved, 2);
        rolled_back.rollback().await.unwrap();
        assert_eq!(store.room_state().await.unwrap().last_seq, 1);
        let captured_before = OffsetDateTime::now_utc();
        let mut tx = store.pool.begin().await.unwrap();
        lock_room(&mut tx).await.unwrap();
        let future = OffsetDateTime::now_utc() + Duration::hours(1);
        let (seq, future_event_id) = append_event(
            &mut tx,
            "agent_message",
            Some("alice"),
            Some("future clock"),
            None,
            None,
            None,
            Some(future),
        )
        .await
        .unwrap();
        assert_eq!(seq, 2);
        tx.commit().await.unwrap();
        let mut earlier_transaction = store.pool.begin().await.unwrap();
        sqlx::query("SELECT 1")
            .execute(&mut *earlier_transaction)
            .await
            .unwrap();
        store.pause(true, "other").await.unwrap();
        store.pause(false, "other").await.unwrap();
        lock_room(&mut earlier_transaction).await.unwrap();
        append_event(
            &mut earlier_transaction,
            "agent_message",
            Some("alice"),
            Some("earlier tx later lock"),
            None,
            None,
            None,
            Some(captured_before - Duration::days(1)),
        )
        .await
        .unwrap();
        earlier_transaction.commit().await.unwrap();
        store
            .send(
                SendInput {
                    agent_id: "alice".into(),
                    public_key: agent.public_key.unwrap(),
                    client_message_id: "actual-attempt-time".into(),
                    text: Ok("actual wall clock attempt".into()),
                    reply_to_event_id: None,
                    mentions: vec![],
                },
                SendLimits::default(),
            )
            .await
            .unwrap();
        store.retract(&future_event_id, "other").await.unwrap();
        store.kick("alice", "other").await.unwrap();
        let attempts: OffsetDateTime = sqlx::query_scalar(
            "SELECT created_at FROM message_attempts WHERE client_message_id='actual-attempt-time'",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert!(attempts < future);
        let rows: Vec<(i64, OffsetDateTime)> =
            sqlx::query_as("SELECT seq,created_at FROM events ORDER BY seq")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert_eq!(
            store
                .public_snapshot()
                .await
                .unwrap()
                .events
                .iter()
                .map(|event| event["kind"].as_str().unwrap().to_string())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            6
        );
        for pair in rows.windows(2) {
            assert_eq!(pair[1].0, pair[0].0 + 1);
            assert!(pair[1].1 >= pair[0].1);
        }
    }
}

pub mod audit;
pub mod conformance;
pub mod controls;
pub mod telemetry;
