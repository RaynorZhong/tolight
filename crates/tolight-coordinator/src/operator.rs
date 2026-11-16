//! Operator actions are local. Confirmation happens outside these DB transactions.

use chrono::Duration;
use tolight_coordinator_store::{
    Agent, ApplicationInput as StoredApplication, ApplyLimits, ReviewSnapshot, ReviewedControl,
    Store,
};
use tolight_core::{
    config::Config,
    models::{ApiError, ApplicationInput},
    validation,
};

async fn ready(store: &Store, config: &Config) -> Result<(), ApiError> {
    store.check_schema().await?;
    store
        .cleanup_expired(Duration::seconds(config.application_ttl as i64))
        .await?;
    Ok(())
}
fn reason(reason: &str) -> Result<(), ApiError> {
    if validation::is_reason_code(reason) {
        Ok(())
    } else {
        Err(ApiError::new("bad_request").with("field", "reason"))
    }
}
pub async fn list(store: &Store, config: &Config, pending: bool) -> Result<Vec<Agent>, ApiError> {
    ready(store, config).await?;
    store.list_agents(pending).await
}
pub async fn active(store: &Store) -> Result<Vec<Agent>, ApiError> {
    store.check_schema().await?;
    store.list_active_agents().await
}
pub async fn review(store: &Store, config: &Config, id: &str) -> Result<ReviewSnapshot, ApiError> {
    ready(store, config).await?;
    store.review(id).await
}
pub async fn approve(
    store: &Store,
    config: &Config,
    snapshot: &ReviewSnapshot,
) -> Result<Agent, ApiError> {
    store.check_schema().await?;
    // From M2 every approval requires the current conformance basis as well as CAS.
    store
        .approve_with_ttl(
            snapshot,
            true,
            Duration::seconds(config.application_ttl as i64),
        )
        .await
}
pub async fn reject(
    store: &Store,
    config: &Config,
    id: &str,
    reason_code: &str,
) -> Result<Agent, ApiError> {
    ready(store, config).await?;
    reason(reason_code)?;
    store
        .reject_with_ttl(
            id,
            reason_code,
            Duration::seconds(config.application_ttl as i64),
        )
        .await
}
pub async fn reject_reviewed(
    store: &Store,
    config: &Config,
    snapshot: &ReviewSnapshot,
    reason_code: &str,
) -> Result<Agent, ApiError> {
    ready(store, config).await?;
    reason(reason_code)?;
    store
        .reject_reviewed_with_ttl(
            snapshot,
            reason_code,
            Duration::seconds(config.application_ttl as i64),
        )
        .await
}
pub async fn add(
    store: &Store,
    config: &Config,
    input: ApplicationInput,
    public_key: &str,
) -> Result<Agent, ApiError> {
    ready(store, config).await?;
    tolight_core::signing::decode_public_key(public_key)
        .map_err(|_| ApiError::new("bad_request").with("field", "public_key"))?;
    let input = validation::validate_application(&input)?;
    Ok(store
        .apply(
            StoredApplication {
                agent_id: input.agent_id,
                display_name: input.display_name,
                display_name_key: input.display_name_key,
                bio: input.bio,
                owner_contact: input.owner_contact,
                framework_label: if input.framework_label.is_empty() {
                    None
                } else {
                    Some(input.framework_label)
                },
                public_key: public_key.into(),
            },
            ApplyLimits {
                max_pending_applications: config.max_pending_applications as i64,
                application_ttl: Duration::seconds(config.application_ttl as i64),
                operator_bypass: true,
                source_retry_after: None,
            },
        )
        .await?
        .agent)
}
pub async fn set_key(
    store: &Store,
    config: &Config,
    id: &str,
    public_key: &str,
    reason_code: &str,
) -> Result<Agent, ApiError> {
    ready(store, config).await?;
    reason(reason_code)?;
    store
        .set_key_with_options(
            id,
            public_key,
            reason_code,
            Duration::seconds(config.application_ttl as i64),
        )
        .await
}
pub async fn set_key_reviewed(
    store: &Store,
    config: &Config,
    snapshot: &ReviewSnapshot,
    public_key: &str,
    reason_code: &str,
) -> Result<Agent, ApiError> {
    ready(store, config).await?;
    reason(reason_code)?;
    store
        .set_key_reviewed(
            snapshot,
            public_key,
            reason_code,
            Duration::seconds(config.application_ttl as i64),
        )
        .await
}
pub async fn applications(store: &Store, open: bool) -> Result<(), ApiError> {
    store.check_schema().await?;
    store.applications_open(open).await
}

/// One line per record. No control/Cf characters from DB are interpreted by terminals.
pub fn render_agent(agent: &Agent) -> String {
    let escape = validation::escape_terminal;
    format!(
        "{} | {} | state={} reason={} | application_id={} revision={} | registered_at={} joined_at={} | public_key={} public_key_registered_at={} | bio={} | owner_contact={} | framework_label={} | muted_until={} indefinite={} mute_reason={} | conformance_passed_at={} suite={} | last_seen_at={}",
        escape(&agent.agent_id),
        escape(&agent.display_name),
        escape(&agent.state),
        escape(agent.state_reason.as_deref().unwrap_or("-")),
        agent.application_id,
        agent.application_revision,
        agent.registered_at,
        agent
            .joined_at
            .map(|time| time.to_rfc3339())
            .unwrap_or_else(|| "-".into()),
        escape(agent.public_key.as_deref().unwrap_or("-")),
        agent
            .public_key_registered_at
            .map(|time| time.to_rfc3339())
            .unwrap_or_else(|| "-".into()),
        escape(&agent.bio),
        escape(&agent.owner_contact),
        escape(agent.framework_label.as_deref().unwrap_or("")),
        agent
            .muted_until
            .map(|time| time.to_rfc3339())
            .unwrap_or_else(|| "-".into()),
        agent.muted_indefinitely,
        escape(agent.mute_reason.as_deref().unwrap_or("-")),
        agent
            .conformance_passed_at
            .map(|time| time.to_rfc3339())
            .unwrap_or_else(|| "-".into()),
        escape(agent.conformance_suite_version.as_deref().unwrap_or("-")),
        agent
            .last_seen_at
            .map(|time| time.to_rfc3339())
            .unwrap_or_else(|| "-".into())
    )
}

pub async fn pause(
    store: &Store,
    config: &Config,
    paused: bool,
    reason_text: &str,
) -> Result<bool, ApiError> {
    ready(store, config).await?;
    store.pause(paused, reason_text).await
}
pub async fn mute(
    store: &Store,
    config: &Config,
    id: &str,
    until: Option<chrono::DateTime<chrono::Utc>>,
    reason_code: &str,
) -> Result<Agent, ApiError> {
    ready(store, config).await?;
    reason(reason_code)?;
    store.mute(id, until, reason_code).await?;
    store.review(id).await
}
pub async fn unmute(store: &Store, config: &Config, id: &str) -> Result<Agent, ApiError> {
    ready(store, config).await?;
    store.unmute(id).await?;
    store.review(id).await
}
pub async fn kick(
    store: &Store,
    config: &Config,
    id: &str,
    reason_code: &str,
) -> Result<Agent, ApiError> {
    ready(store, config).await?;
    reason(reason_code)?;
    store.kick(id, reason_code).await?;
    store.review(id).await
}
pub async fn control_reviewed(
    store: &Store,
    config: &Config,
    snapshot: &ReviewSnapshot,
    control: ReviewedControl,
) -> Result<Agent, ApiError> {
    ready(store, config).await?;
    store.control_reviewed(snapshot, control).await
}
pub async fn retract(
    store: &Store,
    config: &Config,
    event_id: &str,
    reason_code: &str,
) -> Result<(), ApiError> {
    ready(store, config).await?;
    reason(reason_code)?;
    store.retract(event_id, reason_code).await
}
pub async fn review_event(
    store: &Store,
    config: &Config,
    id: &str,
) -> Result<serde_json::Value, ApiError> {
    ready(store, config).await?;
    let event = store
        .public_event_by_id(id)
        .await?
        .ok_or_else(|| ApiError::new("not_found"))?;
    if event["kind"] != "agent_message" || event["retracted"] == true {
        return Err(ApiError::new("invalid_state"));
    }
    Ok(event)
}
pub async fn attempts(
    store: &Store,
    _config: &Config,
    id: &str,
) -> Result<Vec<tolight_coordinator_store::controls::Attempt>, ApiError> {
    store.check_schema().await?;
    store.attempts(id).await
}

pub async fn attempts_since(
    store: &Store,
    _config: &Config,
    id: &str,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<Vec<tolight_coordinator_store::controls::Attempt>, ApiError> {
    store.check_schema().await?;
    store.attempts_since(id, since).await
}

/// Online diagnostic only: unlike list/review, never cleans expired identities.
pub async fn audit(
    store: &Store,
    since: Option<chrono::DateTime<chrono::Utc>>,
    agent_id: Option<&str>,
    limit: usize,
) -> Result<tolight_coordinator_store::audit::AuditPage, ApiError> {
    store.check_schema().await?;
    store.operator_audit(since, agent_id, limit).await
}
