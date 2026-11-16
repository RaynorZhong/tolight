//! Admission is separate from RoomService's four participant operations.

use crate::room::RoomService;
use chrono::{Duration, SecondsFormat};
use serde_json::{Value, json};
use tolight_coordinator_store::{ApplicationInput as StoredApplication, ApplyLimits};
use tolight_core::{
    models::{ApiError, ApplicationInput},
    validation,
};

pub async fn apply(
    room: &RoomService,
    input: ApplicationInput,
    public_key: String,
    source: &str,
) -> Result<(bool, Value), ApiError> {
    let validated = validation::validate_application(&input)?;
    let config = room.config();
    let now = room.now();
    // One Coordinator owns this in-memory quota. Keep the guard until the DB result
    // is known so concurrent successful applications cannot both use the last slot.
    let mut quota = room.application_quota().lock().await;
    let retry = quota.retry_after(source, config.applications_per_ip_per_day, now);
    let result = room
        .store()
        .apply(
            StoredApplication {
                agent_id: validated.agent_id,
                display_name: validated.display_name,
                display_name_key: validated.display_name_key,
                bio: validated.bio,
                owner_contact: validated.owner_contact,
                framework_label: if validated.framework_label.is_empty() {
                    None
                } else {
                    Some(validated.framework_label)
                },
                public_key,
            },
            ApplyLimits {
                max_pending_applications: config.max_pending_applications as i64,
                application_ttl: Duration::seconds(config.application_ttl as i64),
                operator_bypass: false,
                source_retry_after: retry,
            },
        )
        .await?;
    if result.counts_for_daily {
        quota.record(source, now);
    }
    let applied = result.agent.registered_at;
    Ok((
        result.created,
        json!({"agent_id":result.agent.agent_id,"state":result.agent.state,
        "applied_at":applied.to_rfc3339_opts(SecondsFormat::Secs,true),
        "expires_at":(applied+Duration::seconds(config.application_ttl as i64)).to_rfc3339_opts(SecondsFormat::Secs,true)}),
    ))
}
