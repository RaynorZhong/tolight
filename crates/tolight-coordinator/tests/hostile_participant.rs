mod support;
use serde_json::json;
use sqlx::PgPool;
use std::time::{Duration, Instant};
use support::fake_agent::{FakeAgent, TestApp};
#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn fifty_hostile_sends_and_rejected_secret_shapes_leave_other_agents_usable(pool: PgPool) {
    let app = TestApp::configured(pool, |config| {
        config.requests_per_minute = 1000;
        config.unauthenticated_requests_per_minute = 1000;
    })
    .await;
    let hostile = FakeAgent::new("hostile", 104);
    let alice = FakeAgent::new("alice", 105);
    let bob = FakeAgent::new("bob", 106);
    for agent in [&hostile, &alice, &bob] {
        agent.apply(&app).await;
        let review = app.room.store().review(&agent.id).await.unwrap();
        app.room.store().approve(&review, false).await.unwrap();
    }
    let attack = async {
        let mut tasks = Vec::new();
        for index in 0..50 {
            let agent = hostile.clone();
            let origin = app.origin.clone();
            let address = app.address;
            tasks.push(tokio::spawn(async move {
                let body=serde_json::to_vec(&json!({"client_message_id":format!("attack-{index}"),"text":if index%2==0 {"@alice @bob spam"}else{"-----BEGIN PRIVATE KEY----- hidden"}})).unwrap();
                let timestamp=chrono::Utc::now().timestamp().to_string();
                let signed=tolight_core::signing::sign_request(&agent.seed,"POST",&address.to_string(),"http","/participant/v1/messages","",&timestamp,&body).unwrap();
                let response=reqwest::Client::new().post(format!("{origin}/participant/v1/messages")).header("Tolight-Key",signed.key).header("Tolight-Timestamp",signed.timestamp).header("Tolight-Signature",signed.signature).header("Content-Type","application/json").body(body).send().await.unwrap();
                assert!([201,422,429].contains(&response.status().as_u16()));
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
    };
    let healthy = async {
        let started = Instant::now();
        assert_eq!(alice.send(&app, "healthy alice").await.status, 201);
        assert_eq!(bob.send(&app, "healthy bob").await.status, 201);
        assert!(started.elapsed() < Duration::from_secs(2));
    };
    tokio::join!(attack, healthy);
    let snapshot = app.room.store().public_snapshot().await.unwrap();
    let messages = snapshot
        .events
        .iter()
        .filter(|event| event["kind"] == "agent_message" && event["agent"]["agent_id"] == "hostile")
        .count();
    assert!(messages <= 2);
    let attempts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM message_attempts WHERE agent_id='hostile'")
            .fetch_one(app.room.store().pool())
            .await
            .unwrap();
    assert_eq!(attempts, 2);
    assert!(
        !serde_json::to_string(&snapshot.events)
            .unwrap()
            .contains("PRIVATE KEY")
    );
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn mutually_amplifying_mentions_stop_at_each_agents_limit_and_leave_a_third_agent_usable(
    pool: PgPool,
) {
    let app = TestApp::new(pool).await;
    let alice = FakeAgent::new("alice", 114);
    let bob = FakeAgent::new("bob", 115);
    let carol = FakeAgent::new("carol", 116);
    for agent in [&alice, &bob, &carol] {
        assert_eq!(agent.apply(&app).await.status, 201);
        app.approve(&agent.id).await;
    }

    // Each script replies to the other script's newest mention and mentions it
    // again. It deliberately ignores Retry-After after exhausting its quota.
    let mut reply_to = None;
    for turn in 0..12 {
        let (sender, target) = if turn % 2 == 0 {
            (&alice, &bob)
        } else {
            (&bob, &alice)
        };
        let reply = sender
            .request(
                &app,
                "POST",
                "messages",
                "",
                Some(json!({
                    "client_message_id":format!("echo-{turn}"),
                    "text":format!("@{} echo {turn}", target.id),
                    "reply_to_event_id":reply_to,
                })),
            )
            .await;
        if turn < 4 {
            assert_eq!(reply.status, 201);
            reply_to = reply.body["event_id"].as_str().map(str::to_owned);
        } else {
            assert_eq!(reply.status, 429);
            assert_eq!(reply.body["error"]["code"], "message_limit");
            assert!(reply.headers.get("Retry-After").is_some());
        }
    }
    let started = Instant::now();
    assert_eq!(
        carol.send(&app, "third agent remains usable").await.status,
        201
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    for agent in [&alice, &bob] {
        let usage = app
            .room
            .store()
            .usage(&agent.id, &app.room.send_limits())
            .await
            .unwrap();
        assert_eq!(usage.minute, 2);
    }
    let snapshot = app.room.store().public_snapshot().await.unwrap();
    assert_eq!(
        snapshot
            .events
            .iter()
            .filter(|event| event["kind"] == "agent_message")
            .count(),
        5
    );
}
