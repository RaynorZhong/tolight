mod support;
use serde_json::json;
use sqlx::PgPool;
use support::fake_agent::{FakeAgent, TestApp};

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn withdrawal_is_mode_checked_atomic_and_cascades_private_sandbox(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    let alice = FakeAgent::new("alice", 71);
    alice.apply(&app).await;
    let identity = app
        .room
        .store()
        .review("alice")
        .await
        .unwrap()
        .application_id;
    alice.request(&app, "GET", "events", "", None).await;
    let invalid = alice
        .request(
            &app,
            "POST",
            "self/leave",
            "",
            Some(json!({"mode":"leave"})),
        )
        .await;
    assert_eq!(invalid.status, 409);
    let withdrawn = alice
        .request(
            &app,
            "POST",
            "self/leave",
            "",
            Some(json!({"mode":"withdraw"})),
        )
        .await;
    assert_eq!(withdrawn.status, 200);
    assert_eq!(withdrawn.body, json!({"agent_id":"alice","state":null}));
    assert!(app.room.store().agent("alice").await.unwrap().is_none());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM conformance_runs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let audit: String = sqlx::query_scalar(
        "SELECT target_application_id::text FROM operator_actions WHERE kind='owner_withdraw'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audit, identity.to_string());
    assert_eq!(
        alice
            .request(
                &app,
                "POST",
                "self/leave",
                "",
                Some(json!({"mode":"withdraw"}))
            )
            .await
            .status,
        401
    );
    alice.apply(&app).await;
    let next = app.room.store().review("alice").await.unwrap();
    assert_ne!(next.application_id, identity);
    app.approve("alice").await;
    assert_eq!(
        alice
            .request(
                &app,
                "POST",
                "self/leave",
                "",
                Some(json!({"mode":"withdraw"}))
            )
            .await
            .status,
        409
    );
    assert_eq!(
        app.room.store().review("alice").await.unwrap().state,
        "ACTIVE"
    );
    let left = alice
        .request(
            &app,
            "POST",
            "self/leave",
            "",
            Some(json!({"mode":"leave"})),
        )
        .await;
    assert_eq!(left.status, 200);
    assert_eq!(left.body["state"], "LEFT");
    assert_eq!(
        alice.request(&app, "GET", "self", "", None).await.status,
        401
    );
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn self_control_cannot_target_another_agent_or_survive_key_rotation(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let alice = FakeAgent::new("alice", 72);
    let bob = FakeAgent::new("bob", 73);
    alice.apply(&app).await;
    bob.apply(&app).await;
    app.approve("alice").await;
    app.approve("bob").await;
    let bad = alice
        .request(
            &app,
            "POST",
            "self/leave",
            "",
            Some(json!({"mode":"leave","agent_id":"bob"})),
        )
        .await;
    assert_eq!(bad.status, 400);
    let old = tolight_core::models::AgentContext {
        agent_id: "alice".into(),
        public_key: tolight_core::signing::public_key_from_seed(&[72; 32]),
    };
    app.room
        .store()
        .set_key(
            "alice",
            &tolight_core::signing::public_key_from_seed(&[74; 32]),
        )
        .await
        .unwrap();
    assert_eq!(
        app.room
            .store()
            .owner_leave(&old, "leave")
            .await
            .unwrap_err()
            .code,
        "unauthorized"
    );
    assert_eq!(
        app.room.store().review("bob").await.unwrap().state,
        "ACTIVE"
    );
}
