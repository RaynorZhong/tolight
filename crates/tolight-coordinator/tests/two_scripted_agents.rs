mod support;
use serde_json::json;
use sqlx::PgPool;
use support::fake_agent::{FakeAgent, TestApp};

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn two_signed_agents_exchange_twenty_messages_and_obey_real_retry_after(pool: PgPool) {
    let app = TestApp::configured(pool, |config| {
        config.messages_per_minute = 10;
        config.requests_per_minute = 1000;
    })
    .await;
    let alice = FakeAgent::new("alice", 61);
    let bob = FakeAgent::new("bobby", 62);
    assert_eq!(alice.apply(&app).await.status, 201);
    assert_eq!(bob.apply(&app).await.status, 201);
    app.approve("alice").await;
    app.approve("bobby").await;
    let origin = alice
        .request(&app, "GET", "events", "limit=0", None)
        .await
        .body["next_cursor"]
        .as_str()
        .unwrap()
        .to_string();
    let mut reply = None;
    for index in 0..20 {
        let (sender, other) = if index % 2 == 0 {
            (&alice, &bob)
        } else {
            (&bob, &alice)
        };
        let response=sender.request(&app,"POST","messages","",Some(json!({"client_message_id":format!("chat-{index}"),"text":format!("@{} 本地自由聊天第{index}条",other.id),"reply_to_event_id":reply}))).await;
        assert_eq!(response.status, 201, "{}", response.body);
        assert_eq!(response.body["mentions"], json!([other.id]));
        reply = response.body["event_id"].as_str().map(str::to_string);
    }
    for reader in [&alice, &bob] {
        let response = reader
            .request(
                &app,
                "GET",
                "events",
                &format!("after={origin}&limit=200"),
                None,
            )
            .await;
        let events = response.body["events"].as_array().unwrap();
        assert_eq!(events.len(), 20);
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event["seq"], (index + 3) as u64);
            assert_eq!(
                event["mentions_you"],
                event["agent"]["agent_id"] != reader.id
            );
        }
    }
    let limited = alice.send(&app, "这次尝试应被限速").await;
    assert_eq!(limited.status, 429);
    assert_eq!(limited.body["error"]["code"], "message_limit");
    let seconds = limited.headers["retry-after"]
        .to_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!((1..=60).contains(&seconds));
    eprintln!("Real Retry-After exercise: waiting {seconds} seconds before the next signed POST");
    let began = std::time::Instant::now();
    tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
    assert!(began.elapsed() >= std::time::Duration::from_secs(seconds));
    assert_eq!(alice.send(&app, "等待限速窗口后继续聊天").await.status, 201);
    let snapshot = app.room.store().public_snapshot().await.unwrap();
    assert_eq!(snapshot.last_seq, 23);
    assert_eq!(snapshot.events.len(), 23);
}
