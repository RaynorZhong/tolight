mod support;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{path::Path, time::Duration};
use support::fake_agent::{FakeAgent, TestApp};
use tolight_coordinator::{metrics, room::RoomService, service};
use tolight_coordinator_store::Store;
use tolight_core::{
    config::Config,
    models::{AgentContext, SendRequest},
    participant::ReadRequest,
};
async fn read_manifest(path: &Path, head: i64) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(bytes) = tokio::fs::read(path).await
                && serde_json::from_slice::<Value>(&bytes).unwrap()["newest_seq"] == head
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn loopback_metrics_are_complete_and_service_logs_never_contain_request_payloads(
    pool: PgPool,
) {
    let app = TestApp::new(pool).await;
    let alice = FakeAgent::new("alice", 108);
    let contact = "runtime-private-owner@example.invalid";
    let applied=alice.request(&app,"POST","applications","",Some(json!({"agent_id":"alice","display_name":"Alice","bio":"runtime metric test","owner_contact":contact}))).await;
    assert_eq!(applied.status, 201);
    let review = app.room.store().review("alice").await.unwrap();
    app.room.store().approve(&review, false).await.unwrap();
    let bob = FakeAgent::new("bob", 109);
    bob.apply(&app).await;
    assert_eq!(alice.send(&app, "public metric sample").await.status, 201);
    let secret = "-----BEGIN PRIVATE KEY----- runtime-secret-marker";
    assert_eq!(alice.send(&app, secret).await.status, 422);
    assert_eq!(alice.send(&app, "rate blocked").await.status, 429);
    app.room.store().mute("alice", None, "other").await.unwrap();
    let rendered = metrics::render(&app.room).await.unwrap();
    for name in [
        "tolight_room_messages_last_hour 1",
        "tolight_pending_applications 1",
        "tolight_muted_agents 1",
        "tolight_agent_attempts_last_day{agent_id=\"alice\"} 2",
        "tolight_agent_rejected_attempts{agent_id=\"alice\",reason_code=\"secret_shape\"} 1",
        "tolight_message_limit_rejections_total{agent_id=\"alice\"} 1",
        "tolight_requests_total{endpoint=\"messages\"} 3",
        "tolight_auth_failures_total",
        "tolight_clock_skew_total",
        "tolight_publish_lag_events",
        "tolight_publish_lag_seconds",
        "tolight_oldest_pending_seconds",
        "tolight_latest_pending_timestamp_seconds",
    ] {
        assert!(rendered.contains(name), "missing {name}: {rendered}");
    }
    assert!(!rendered.contains(contact));
    assert!(!rendered.contains(secret));
    let read = app
        .room
        .read(
            &alice.context(),
            ReadRequest {
                limit: Some(0),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let room = app.room.clone();
    let context = alice.context();
    let pending = tokio::spawn(async move {
        room.read(
            &context,
            ReadRequest {
                after: Some(read.next_cursor),
                wait: 2,
                ..Default::default()
            },
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        metrics::render(&app.room)
            .await
            .unwrap()
            .contains("tolight_pending_long_polls 1")
    );
    pending.abort();
    // Real metric listener remains separate from the participant router.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = metrics::router(app.room.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    assert_eq!(
        client
            .get(format!("http://{address}/healthz"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    app.room.store().pool().close().await;
    assert_eq!(
        client
            .get(format!("http://{address}/healthz"))
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    let unavailable = alice.request(&app, "GET", "self", "", None).await;
    assert_eq!(unavailable.status, 503);
    assert_eq!(unavailable.body["error"]["code"], "unavailable");
    assert_eq!(unavailable.headers["retry-after"], "1");
    task.abort();
    let logs = std::fs::read_dir(&app.room.config().observability.log_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
        .collect::<String>();
    assert!(!logs.contains(contact));
    assert!(!logs.contains(secret));
    assert!(!logs.contains(&alice.key()));
    assert!(!logs.contains("Tolight-Signature"));
}
#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn abort_after_commit_before_publish_recovers_and_same_message_id_stays_idempotent(
    pool: PgPool,
) {
    let directory = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = Config {
        listen: address,
        participant_hosts: vec![address.to_string()],
        publish_coalesce: 60,
        ..Default::default()
    };
    config.public.dir = directory.path().join("public");
    config.observability.listen = "127.0.0.1:0".parse().unwrap();
    config.observability.log_dir = directory.path().join("logs");
    let store = Store::from_pool(pool);
    let alice = FakeAgent::new("alice", 110);
    let input = tolight_coordinator_store::ApplicationInput {
        agent_id: "alice".into(),
        display_name: "Alice".into(),
        display_name_key: "alice".into(),
        bio: "restart test".into(),
        owner_contact: "owner@example.invalid".into(),
        framework_label: None,
        public_key: alice.key(),
    };
    store.apply(input, Default::default()).await.unwrap();
    let review = store.review("alice").await.unwrap();
    store.approve(&review, false).await.unwrap();
    let room = RoomService::new(store.clone(), config.clone());
    let running = room.clone();
    let task =
        tokio::spawn(
            async move { service::serve_on(listener, running, std::future::pending()).await },
        );
    let path = config.public.dir.join("data/manifest.json");
    read_manifest(&path, 1).await;
    let context = AgentContext {
        agent_id: "alice".into(),
        public_key: alice.key(),
    };
    let request = SendRequest {
        client_message_id: "restart-stable-id".into(),
        text: "committed before publication".into(),
        reply_to_event_id: None,
        mentions: vec![],
    };
    let original = room.send(&context, request.clone()).await.unwrap();
    assert_eq!(original.seq, 2);
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap()["newest_seq"],
        1
    );
    task.abort();
    let _ = task.await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let room = RoomService::new(store, config);
    let running = room.clone();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        service::serve_on(listener, running, async {
            let _ = stopped.await;
        })
        .await
    });
    read_manifest(&path, 2).await;
    let duplicate = room.send(&context, request).await.unwrap();
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.event_id, original.event_id);
    assert_eq!(
        room.store()
            .usage("alice", &room.send_limits())
            .await
            .unwrap()
            .minute,
        1
    );
    let _ = stop.send(());
    task.await.unwrap().unwrap();
}
