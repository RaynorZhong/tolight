mod support;
use chrono::{Duration, Utc};
use sqlx::PgPool;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use support::*;
use tolight_coordinator_store::{Store, conformance::ConformanceRun};
use tolight_core::{
    models::{AgentContext, ApiError},
    signing::public_key_from_seed,
};

#[sqlx::test(migrations = "./migrations")]
async fn sessions_are_serialized_per_identity_and_only_passage_changes_review_revision(
    pool: PgPool,
) {
    let store = Store::from_pool(pool);
    let agent = pending(&store, "alice", 111).await;
    let context = AgentContext {
        agent_id: agent.agent_id.clone(),
        public_key: agent.public_key.clone().unwrap(),
    };
    let mut jobs = tokio::task::JoinSet::new();
    for _ in 0..16 {
        let store = store.clone();
        let context = context.clone();
        jobs.spawn(async move {
            store
                .conformance_transaction(&context, |data| {
                    if data.current().is_none() {
                        data.runs.push(ConformanceRun::candidate(
                            &context.agent_id,
                            Utc::now(),
                            data.runs.len() + 1,
                        ));
                    }
                    data.current().unwrap().run_id.clone()
                })
                .await
                .unwrap()
        });
    }
    let mut ids = std::collections::BTreeSet::new();
    while let Some(result) = jobs.join_next().await {
        ids.insert(result.unwrap());
    }
    assert_eq!(ids.len(), 1);
    assert_eq!(count(&store, "conformance_runs").await, 1);
    assert_eq!(store.review("alice").await.unwrap().application_revision, 1);
    let limited = chrono::DateTime::from_timestamp_micros(Utc::now().timestamp_micros()).unwrap();
    let rejected: Result<(), ApiError> = store
        .conformance_transaction(&context, |data| {
            data.current_mut().unwrap().limited_at = Some(limited);
            Err(ApiError::new("message_limit").retry(20))
        })
        .await
        .unwrap();
    assert_eq!(rejected.unwrap_err().code, "message_limit");
    assert_eq!(
        store.conformance_runs("alice").await.unwrap()[0].limited_at,
        Some(limited)
    );
    assert_eq!(store.review("alice").await.unwrap().application_revision, 1);
    let passed = limited + Duration::seconds(20);
    store
        .conformance_transaction(&context, |data| {
            let run = data.current_mut().unwrap();
            run.finished_at = Some(passed);
            run.passed = Some(true);
            data.agent.conformance_passed_at = Some(passed);
            data.agent.conformance_suite_version = Some("1".into());
        })
        .await
        .unwrap();
    assert_eq!(store.review("alice").await.unwrap().application_revision, 2);
    assert_eq!(count(&store, "events").await, 0);
    assert_eq!(count(&store, "message_attempts").await, 0);
    store
        .conformance_transaction(&context, |_| ())
        .await
        .unwrap();
    assert_eq!(store.review("alice").await.unwrap().application_revision, 2);
    assert_eq!(
        store.approve(&agent, true).await.unwrap_err().code,
        "review_changed"
    );
    let latest = store.review("alice").await.unwrap();
    assert_eq!(store.approve(&latest, true).await.unwrap().state, "ACTIVE");
}

async fn wait_for_lock_waiters(pool: &PgPool, count: i64) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let waiting:i64=sqlx::query_scalar("SELECT COUNT(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock'").fetch_one(pool).await.unwrap();
        if waiting >= count {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected {count} row lock waiters, got {waiting}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn old_key_cannot_advance_or_timeout_a_session_after_waiting_for_the_agent_lock(
    pool: PgPool,
) {
    let store = Store::from_pool(pool.clone());
    let agent = pending(&store, "alice", 112).await;
    let context = AgentContext {
        agent_id: "alice".into(),
        public_key: agent.public_key.unwrap(),
    };
    store
        .conformance_transaction(&context, |data| {
            data.runs.push(ConformanceRun::candidate(
                "alice",
                Utc::now() - Duration::hours(1),
                1,
            ))
        })
        .await
        .unwrap();
    let before = store.conformance_runs("alice").await.unwrap();
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("SELECT agent_id FROM agents WHERE agent_id='alice' FOR NO KEY UPDATE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let new_key = public_key_from_seed(&[113; 32]);
    let changed = store.clone();
    let key = new_key.clone();
    let change = tokio::spawn(async move { changed.set_key("alice", &key).await });
    wait_for_lock_waiters(&pool, 1).await;
    let called = Arc::new(AtomicBool::new(false));
    let observed = called.clone();
    let old = store.clone();
    let old_context = context.clone();
    let request = tokio::spawn(async move {
        old.conformance_transaction(&old_context, |data| {
            observed.store(true, Ordering::SeqCst);
            let run = data.current_mut().unwrap();
            run.finished_at = Some(Utc::now());
            run.passed = Some(false);
            run.failure_reason = Some("no_reply".into());
        })
        .await
    });
    wait_for_lock_waiters(&pool, 2).await;
    blocker.commit().await.unwrap();
    change.await.unwrap().unwrap();
    assert_eq!(request.await.unwrap().unwrap_err().code, "unauthorized");
    assert!(!called.load(Ordering::SeqCst));
    assert_eq!(store.conformance_runs("alice").await.unwrap(), before);
    let context = AgentContext {
        agent_id: "alice".into(),
        public_key: new_key,
    };
    store
        .conformance_transaction(&context, |data| assert!(data.current().is_some()))
        .await
        .unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn approval_requires_both_pass_time_and_the_current_suite_version(pool: PgPool) {
    let store = Store::from_pool(pool);
    let agent = pending(&store, "alice", 114).await;
    let context = AgentContext {
        agent_id: "alice".into(),
        public_key: agent.public_key.unwrap(),
    };
    for suite in [None, Some("old")] {
        store
            .conformance_transaction(&context, |data| {
                data.agent.conformance_passed_at = Some(Utc::now());
                data.agent.conformance_suite_version = suite.map(str::to_owned);
            })
            .await
            .unwrap();
        let snapshot = store.review("alice").await.unwrap();
        assert_eq!(
            store.approve(&snapshot, true).await.unwrap_err().code,
            "invalid_state"
        );
        assert_eq!(store.room_state().await.unwrap().last_seq, 0);
    }
    store
        .conformance_transaction(&context, |data| {
            data.agent.conformance_suite_version =
                Some(tolight_core::CONFORMANCE_SUITE_VERSION.into());
        })
        .await
        .unwrap();
    let snapshot = store.review("alice").await.unwrap();
    assert_eq!(
        store.approve(&snapshot, true).await.unwrap().state,
        "ACTIVE"
    );
}
