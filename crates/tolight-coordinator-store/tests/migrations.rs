mod support;
use sqlx::PgPool;
use support::*;
use time::{Duration, OffsetDateTime};
use tolight_coordinator_store::{ApplyLimits, Store};

#[sqlx::test(migrations = "./migrations")]
async fn migration_is_idempotent_and_unknown_schema_is_rejected(pool: PgPool) {
    let store = Store::from_pool(pool);
    store.migrate().await.unwrap();
    store.migrate().await.unwrap();
    store.check_schema().await.unwrap();
    let encoding: String = sqlx::query_scalar("SHOW server_encoding")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(encoding, "UTF8");
    sqlx::query("UPDATE schema_version SET version=2")
        .execute(store.pool())
        .await
        .unwrap();
    assert_eq!(
        store.check_schema().await.unwrap_err().code,
        "schema_mismatch"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn schema_constraints_and_cascades_hold(pool: PgPool) {
    let store = Store::from_pool(pool);
    let agent = pending(&store, "alice", 1).await;
    assert!(sqlx::query("INSERT INTO agent_keys(public_key,agent_id,created_at) VALUES('other','alice',clock_timestamp())").execute(store.pool()).await.is_err());
    assert!(sqlx::query("INSERT INTO agent_keys(public_key,agent_id,created_at) VALUES('orphan','missing',clock_timestamp())").execute(store.pool()).await.is_err());
    assert!(
        sqlx::query("UPDATE agents SET state='PAUSED' WHERE agent_id='alice'")
            .execute(store.pool())
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE agents SET application_revision=0 WHERE agent_id='alice'")
            .execute(store.pool())
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE agents SET muted_until='infinity' WHERE agent_id='alice'")
            .execute(store.pool())
            .await
            .is_err()
    );
    sqlx::query("INSERT INTO conformance_runs(run_id,agent_id,suite_version,started_at,event_ids) VALUES('first','alice','0.1',clock_timestamp(),ARRAY['1','2','3','4'])").execute(store.pool()).await.unwrap();
    assert!(sqlx::query("INSERT INTO conformance_runs(run_id,agent_id,suite_version,started_at,event_ids) VALUES('second','alice','0.1',clock_timestamp(),ARRAY['1','2','3','4'])").execute(store.pool()).await.is_err());
    store
        .withdraw("alice", agent.public_key.as_ref().unwrap())
        .await
        .unwrap();
    assert_eq!(count(&store, "agent_keys").await, 0);
    assert_eq!(count(&store, "conformance_runs").await, 0);
    let replacement = pending(&store, "alice", 1).await;
    assert_ne!(agent.application_id, replacement.application_id);
    assert_eq!(replacement.application_revision, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn application_replays_revisions_conflicts_and_review_cas(pool: PgPool) {
    let store = Store::from_pool(pool);
    let first = pending(&store, "alice", 1).await;
    let mut modified = input("alice", 1);
    modified.bio = "新的简介".into();
    let replay = store
        .apply(modified.clone(), ApplyLimits::default())
        .await
        .unwrap();
    assert!(!replay.created && !replay.counts_for_daily);
    assert_eq!(replay.agent.bio, first.bio);
    assert_eq!(
        replay.agent.application_revision,
        first.application_revision
    );
    assert_eq!(
        store
            .apply(input("other", 1), ApplyLimits::default())
            .await
            .unwrap_err()
            .code,
        "key_already_registered"
    );
    assert_eq!(
        store
            .apply(input("alice", 2), ApplyLimits::default())
            .await
            .unwrap_err()
            .code,
        "agent_id_taken"
    );
    let mut duplicate = input("other", 2);
    duplicate.display_name = first.display_name.clone();
    duplicate.display_name_key = first.display_name_key.clone();
    assert_eq!(
        store
            .apply(duplicate, ApplyLimits::default())
            .await
            .unwrap_err()
            .code,
        "display_name_taken"
    );
    let rejected = store.reject("alice", "other").await.unwrap();
    assert_eq!(rejected.state, "REJECTED");
    let resumed = store.apply(modified, ApplyLimits::default()).await.unwrap();
    assert_eq!(resumed.agent.application_id, first.application_id);
    assert!(resumed.agent.application_revision > rejected.application_revision);
    assert_eq!(resumed.agent.registered_at, first.registered_at);
    assert!(resumed.agent.state_reason.is_none());
    assert_eq!(
        store.approve(&first, false).await.unwrap_err().code,
        "review_changed"
    );
    let reviewed = resumed.agent;
    store
        .touch_last_seen("alice", reviewed.public_key.as_ref().unwrap())
        .await
        .unwrap();
    assert_eq!(
        store
            .agent("alice")
            .await
            .unwrap()
            .unwrap()
            .application_revision,
        reviewed.application_revision
    );
    let active = store.approve(&reviewed, false).await.unwrap();
    assert_eq!(active.state, "ACTIVE");
    assert!(store.approve(&active, false).await.is_err());
    assert_eq!(count(&store, "events").await, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn unicode_name_uniqueness_is_independent_of_database_locale(pool: PgPool) {
    let store = Store::from_pool(pool);
    let mut one = input("alice", 1);
    one.display_name = "Élan".into();
    one.display_name_key = "élan".into();
    store.apply(one, ApplyLimits::default()).await.unwrap();
    let mut two = input("bobby", 2);
    two.display_name = "élan".into();
    two.display_name_key = "élan".into();
    assert_eq!(
        store
            .apply(two, ApplyLimits::default())
            .await
            .unwrap_err()
            .code,
        "display_name_taken"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn expired_applications_are_freed_but_public_identities_are_retained(pool: PgPool) {
    let store = Store::from_pool(pool);
    let a = pending(&store, "alice", 1).await;
    let b = active(&store, "bobby", 2).await;
    sqlx::query("UPDATE agents SET registered_at=clock_timestamp()-INTERVAL '15 days'")
        .execute(store.pool())
        .await
        .unwrap();
    assert_eq!(store.cleanup_expired(Duration::days(14)).await.unwrap(), 1);
    assert!(
        store
            .agent_by_key(a.public_key.as_ref().unwrap())
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.agent("bobby").await.unwrap().is_some());
    let again = pending(&store, "alice", 1).await;
    assert_ne!(again.application_id, a.application_id);
    store.kick("bobby", "other").await.unwrap();
    assert_eq!(store.cleanup_expired(Duration::days(14)).await.unwrap(), 0);
    assert!(store.agent("bobby").await.unwrap().is_some());
    assert_eq!(
        store
            .apply(input("other", 2), ApplyLimits::default())
            .await
            .unwrap_err()
            .code,
        "key_already_registered"
    );
    assert_eq!(b.state, "ACTIVE");
}

#[sqlx::test(migrations = "./migrations")]
async fn key_replacement_revokes_in_flight_activity_and_review(pool: PgPool) {
    let store = Store::from_pool(pool);
    let old = pending(&store, "alice", 1).await;
    let key = tolight_core::signing::public_key_from_seed(&[2; 32]);
    let updated = store.set_key("alice", &key).await.unwrap();
    assert_eq!(updated.application_id, old.application_id);
    assert!(updated.application_revision > old.application_revision);
    assert!(
        store
            .agent_by_key(old.public_key.as_ref().unwrap())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .touch_last_seen("alice", old.public_key.as_ref().unwrap())
            .await
            .unwrap_err()
            .code,
        "unauthorized"
    );
    assert!(
        store
            .agent("alice")
            .await
            .unwrap()
            .unwrap()
            .last_seen_at
            .is_none()
    );
    store.touch_last_seen("alice", &key).await.unwrap();
    assert_eq!(
        store.approve(&old, false).await.unwrap_err().code,
        "review_changed"
    );
    assert_eq!(count(&store, "agent_keys").await, 2);
    assert!(
        store
            .set_key("alice", old.public_key.as_ref().unwrap())
            .await
            .is_err()
    );
    let active = store
        .approve(&store.review("alice").await.unwrap(), false)
        .await
        .unwrap();
    store.mute("alice", None, "other").await.unwrap();
    let row = store.agent("alice").await.unwrap().unwrap();
    assert!(row.muted_indefinitely);
    assert!(row.muted_until.is_none());
    assert!(store.read_snapshot("alice", &key).await.is_ok());
    assert!(store.public_snapshot().await.is_ok());
    store.unmute("alice").await.unwrap();
    store
        .mute(
            "alice",
            Some(OffsetDateTime::now_utc() + Duration::minutes(1)),
            "other",
        )
        .await
        .unwrap();
    store.kick("alice", "other").await.unwrap();
    assert!(store.set_key("alice", &key).await.is_err());
    assert_eq!(active.state, "ACTIVE");
    let audit: serde_json::Value =
        sqlx::query_scalar("SELECT detail FROM operator_actions WHERE kind='set_key'")
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(audit["old_public_key"], old.public_key.unwrap());
}

#[sqlx::test(migrations = "./migrations")]
async fn review_identity_replacement_conformance_and_custom_ttl_cannot_reuse_confirmation(
    pool: PgPool,
) {
    let store = Store::from_pool(pool);
    let original = pending(&store, "alice", 1).await;
    // Force identical timestamps to show why a timestamp is not a review token.
    store
        .withdraw("alice", original.public_key.as_ref().unwrap())
        .await
        .unwrap();
    let fresh = pending(&store, "alice", 1).await;
    sqlx::query("UPDATE agents SET registered_at=$1 WHERE agent_id='alice'")
        .bind(original.registered_at)
        .execute(store.pool())
        .await
        .unwrap();
    assert_eq!(
        store.approve(&original, false).await.unwrap_err().code,
        "review_changed"
    );
    let snapshot = store.review("alice").await.unwrap();
    sqlx::query("UPDATE agents SET conformance_suite_version='0.1',conformance_passed_at=clock_timestamp() WHERE agent_id='alice'").execute(store.pool()).await.unwrap();
    assert_eq!(
        store.approve(&snapshot, false).await.unwrap_err().code,
        "review_changed"
    );
    assert_ne!(fresh.application_id, original.application_id);
    sqlx::query("UPDATE agents SET registered_at=clock_timestamp()-INTERVAL '15 days' WHERE agent_id='alice'").execute(store.pool()).await.unwrap();
    let reviewed = store.review("alice").await.unwrap();
    // A configured longer TTL must not be silently shortened by default wrappers.
    let key = tolight_core::signing::public_key_from_seed(&[2; 32]);
    let changed = store
        .set_key_with_options("alice", &key, "other", Duration::days(30))
        .await
        .unwrap();
    assert_eq!(changed.application_id, reviewed.application_id);
    store
        .reject_with_ttl("alice", "other", Duration::days(30))
        .await
        .unwrap();
    assert!(store.agent("alice").await.unwrap().is_some());
    assert!(store.cleanup_expired(Duration::days(14)).await.unwrap() > 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn rejected_resubmission_preserves_conformance_and_original_expiration(pool: PgPool) {
    let store = Store::from_pool(pool);
    let first = pending(&store, "alice", 1).await;
    sqlx::query("UPDATE agents SET conformance_suite_version='0.1',conformance_passed_at=clock_timestamp(),registered_at=clock_timestamp()-INTERVAL '13 days' WHERE agent_id='alice'").execute(store.pool()).await.unwrap();
    store.reject("alice", "other").await.unwrap();
    let reapplication = store
        .apply(input("alice", 1), ApplyLimits::default())
        .await
        .unwrap();
    assert!(!reapplication.created);
    assert!(reapplication.counts_for_daily);
    assert_eq!(reapplication.agent.application_id, first.application_id);
    assert!(reapplication.agent.conformance_passed_at.is_some());
    sqlx::query(
        "UPDATE agents SET registered_at=registered_at-INTERVAL '2 days' WHERE agent_id='alice'",
    )
    .execute(store.pool())
    .await
    .unwrap();
    assert_eq!(store.cleanup_expired(Duration::days(14)).await.unwrap(), 1);
    assert!(store.agent("alice").await.unwrap().is_none());
}
