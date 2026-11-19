mod support;
use serde_json::json;
use sqlx::PgPool;
use support::fake_agent::{FakeAgent, TestApp};
use time::OffsetDateTime;
use tolight_coordinator::operator;

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn signature_order_and_application_state_are_enforced(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let alice = FakeAgent::new("alice", 21);
    let unknown = alice.request(&app, "GET", "self", "", None).await;
    assert_eq!(unknown.status, 401);
    assert_eq!(unknown.body["error"]["code"], "unauthorized");
    let skew = alice
        .raw(
            &app,
            "GET",
            "self",
            "",
            vec![],
            OffsetDateTime::now_utc().unix_timestamp() - 1000,
            &[],
        )
        .await;
    assert_eq!(skew.status, 401);
    assert_eq!(skew.body["error"]["code"], "clock_skew");
    assert!(
        skew.body["error"]["server_time"]
            .as_str()
            .unwrap()
            .ends_with('Z')
    );
    let wrong_host = alice
        .raw(
            &app,
            "GET",
            "self",
            "",
            vec![],
            OffsetDateTime::now_utc().unix_timestamp() - 1000,
            &[("Host", "other.invalid")],
        )
        .await;
    assert_eq!(wrong_host.status, 421);
    assert_eq!(alice.apply(&app).await.status, 201);
    assert_eq!(alice.apply(&app).await.status, 200);
    let pending = alice.request(&app, "GET", "self", "", None).await;
    assert_eq!(pending.body["state"], "PENDING");
    assert_eq!(pending.body["conformance"]["runs_today"], 0);
    assert!(pending.body["conformance"]["last_run"].is_null());
    let read = alice.request(&app, "GET", "events", "", None).await;
    assert_eq!(read.body["timeline"], "conformance");
    assert_eq!(read.body["head_seq"], 3);
    assert_eq!(read.body["events"].as_array().unwrap().len(), 3);
    assert!(
        read.body["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event["agent"]["agent_id"] == "examiner")
    );
    assert_eq!(alice.send(&app, "待审不应公开").await.status, 403);
    assert_eq!(
        app.room.store().public_snapshot().await.unwrap().last_seq,
        0
    );
    operator::reject(
        app.room.store(),
        app.room.config(),
        "alice",
        "bio_inadequate",
    )
    .await
    .unwrap();
    let rejected = alice.request(&app, "GET", "self", "", None).await;
    assert_eq!(rejected.body["state"], "REJECTED");
    assert_eq!(rejected.body["state_reason"], "bio_inadequate");
    for endpoint in ["agents", "events"] {
        assert_eq!(
            alice.request(&app, "GET", endpoint, "", None).await.status,
            403
        );
    }
    let before = app.room.store().review("alice").await.unwrap();
    assert_eq!(alice.apply(&app).await.status, 200);
    let after = app.room.store().review("alice").await.unwrap();
    assert_eq!(before.application_id, after.application_id);
    assert!(after.application_revision > before.application_revision);
    assert!(alice.request(&app, "GET", "self", "", None).await.body["state_reason"].is_null());
    assert_eq!(
        operator::approve(app.room.store(), app.room.config(), &before)
            .await
            .unwrap_err()
            .code,
        "review_changed"
    );
    app.approve("alice").await;
    let current = alice.request(&app, "GET", "events", "", None).await;
    assert_eq!(current.body["timeline"], "public");
    assert_eq!(current.body["events"][0]["kind"], "agent_joined");
    let forwarded = alice
        .raw(
            &app,
            "GET",
            "self",
            "",
            vec![],
            OffsetDateTime::now_utc().unix_timestamp(),
            &[
                ("Forwarded", "host=other.invalid;proto=https"),
                ("X-Forwarded-Proto", "https"),
            ],
        )
        .await;
    assert_eq!(forwarded.status, 200);
    let other = FakeAgent::new("other", 22);
    let rotated = operator::set_key(
        app.room.store(),
        app.room.config(),
        "alice",
        &other.key(),
        "owner_request",
    )
    .await
    .unwrap();
    assert_eq!(rotated.state, "ACTIVE");
    assert_eq!(
        alice.request(&app, "GET", "self", "", None).await.status,
        401
    );
    assert_eq!(
        other.request(&app, "GET", "self", "", None).await.body["agent_id"],
        "alice"
    );
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn fields_duplicate_requests_and_source_quotas_have_correct_precedence(pool: PgPool) {
    let app = TestApp::configured(pool, |config| {
        config.applications_per_ip_per_day = 2;
        config.unauthenticated_requests_per_minute = 100;
    })
    .await;
    let alice = FakeAgent::new("alice", 31);
    let bob = FakeAgent::new("bobby", 32);
    let carol = FakeAgent::new("carol", 33);
    let invalid=alice.request(&app,"POST","applications","",Some(json!({"agent_id":"examiner","display_name":"Name","bio":"有效简介","owner_contact":"a@example.invalid"}))).await;
    assert_eq!(invalid.status, 400);
    assert_eq!(invalid.body["error"]["field"], "agent_id");
    assert_eq!(alice.apply(&app).await.status, 201);
    assert_eq!(bob.apply(&app).await.status, 201);
    let blocked = carol.apply(&app).await;
    assert_eq!(blocked.status, 429);
    assert!(blocked.headers.contains_key("retry-after"));
    assert_eq!(alice.apply(&app).await.status, 200);
    operator::applications(app.room.store(), false)
        .await
        .unwrap();
    assert_eq!(alice.apply(&app).await.status, 200);
    // Invalid contents and conflicts did not use the two successful-application slots.
    assert_eq!(app.room.store().list_agents(true).await.unwrap().len(), 2);
}
