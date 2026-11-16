mod support;
use chrono::Utc;
use serde_json::json;
use sqlx::PgPool;
use support::fake_agent::{FakeAgent, TestApp};

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn malformed_requests_use_the_uniform_error_mapping(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let alice = FakeAgent::new("alice", 41);
    alice.apply(&app).await;
    app.approve("alice").await;
    for (method, path, query, body, status) in [
        ("POST", "messages", "", b"{bad".to_vec(), 400),
        (
            "POST",
            "messages",
            "",
            br#"{"text":"missing id"}"#.to_vec(),
            400,
        ),
        (
            "POST",
            "messages",
            "",
            br#"{"client_message_id":"a","text":"hi","unknown":true}"#.to_vec(),
            400,
        ),
        ("POST", "messages", "", vec![b'a'; 32769], 413),
        ("POST", "events", "", vec![], 404),
        ("GET", "unknown", "", vec![], 404),
        ("GET", "events", "limit=51", vec![], 400),
        ("GET", "events", "after=bad&limit=0", vec![], 400),
        ("GET", "events", "wait=31", vec![], 400),
        ("GET", "events", "unknown=true", vec![], 400),
        ("GET", "events", "limit=1&limit=2", vec![], 400),
    ] {
        let response = alice
            .raw(&app, method, path, query, body, Utc::now().timestamp(), &[])
            .await;
        assert_eq!(
            response.status, status,
            "{method} {path}?{query}: {}",
            response.body
        );
        assert!(response.body["error"]["code"].is_string());
    }
    let rejected = alice
        .request(
            &app,
            "POST",
            "messages",
            "",
            Some(json!({"client_message_id":"reject","text":"   "})),
        )
        .await;
    assert_eq!(rejected.status, 422);
    assert_eq!(rejected.body["error"]["reason_code"], "empty");
    let invalid = alice
        .request(
            &app,
            "POST",
            "messages",
            "",
            Some(json!({"client_message_id":"invalid","text":"正常正文","mentions":["bad\n"]})),
        )
        .await;
    assert_eq!(invalid.status, 400);
    let usage = alice.request(&app, "GET", "self", "", None).await;
    assert_eq!(usage.body["usage"]["minute_used"], 1);
    assert_eq!(
        alice.request(&app, "HEAD", "events", "", None).await.status,
        404
    );
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn new_long_poll_replaces_old_and_approval_wakes_without_restarting(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let alice = FakeAgent::new("alice", 51);
    alice.apply(&app).await;
    let start = alice.request(&app, "GET", "events", "limit=0", None).await;
    let cursor = start.body["next_cursor"].as_str().unwrap();
    let current = alice
        .request(&app, "GET", "events", &format!("after={cursor}"), None)
        .await;
    assert_eq!(current.body["events"][0]["seq"], 4);
    let cursor = current.body["next_cursor"].as_str().unwrap();
    let query = format!("after={cursor}&wait=5");
    let old = alice.request(&app, "GET", "events", &query, None);
    tokio::pin!(old);
    // Poll the first future until it is suspended on the real HTTP request.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(80), &mut old)
            .await
            .is_err()
    );
    let newer = alice.request(&app, "GET", "events", &query, None);
    tokio::pin!(newer);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(80), &mut newer)
            .await
            .is_err()
    );
    let replaced = tokio::time::timeout(std::time::Duration::from_secs(1), old)
        .await
        .unwrap();
    assert_eq!(replaced.body["events"], json!([]));
    app.approve("alice").await;
    let changed = tokio::time::timeout(std::time::Duration::from_secs(1), newer)
        .await
        .unwrap();
    assert_eq!(changed.body["timeline"], "public");
    assert_eq!(changed.body["cursor_reset"], true);
    assert_eq!(changed.body["events"][0]["kind"], "agent_joined");
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn a_parked_request_does_not_recheck_its_original_signature_timestamp(pool: PgPool) {
    use std::sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    };
    let began = Utc::now().timestamp();
    let seconds = Arc::new(AtomicI64::new(began));
    let injected = seconds.clone();
    let app = TestApp::configured_clock(
        pool,
        |config| config.signature_max_skew = 30,
        Arc::new(move || {
            chrono::DateTime::from_timestamp(injected.load(Ordering::SeqCst), 0).unwrap()
        }),
    )
    .await;
    let alice = FakeAgent::new("alice", 53);
    let bob = FakeAgent::new("bobby", 54);
    alice.apply(&app).await;
    bob.apply(&app).await;
    app.approve("alice").await;
    app.approve("bobby").await;
    let cursor = alice
        .request(&app, "GET", "events", "limit=0", None)
        .await
        .body["next_cursor"]
        .as_str()
        .unwrap()
        .to_string();
    let query = format!("after={cursor}&wait=5");
    let waiting = alice.raw(&app, "GET", "events", &query, vec![], began, &[]);
    tokio::pin!(waiting);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(60), &mut waiting)
            .await
            .is_err()
    );
    seconds.store(began + 600, Ordering::SeqCst);
    let body =
        serde_json::to_vec(&json!({"client_message_id":"wake","text":"@alice 唤醒挂起请求"}))
            .unwrap();
    assert_eq!(
        bob.raw(&app, "POST", "messages", "", body, began + 600, &[])
            .await
            .status,
        201
    );
    let answer = waiting.await;
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body["events"].as_array().unwrap().len(), 1);
    assert_eq!(
        alice
            .raw(&app, "GET", "self", "", vec![], began, &[])
            .await
            .body["error"]["code"],
        "clock_skew"
    );
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn more_parked_reads_than_pool_connections_leave_message_writes_available(pool: PgPool) {
    use std::{sync::Arc, time::Duration};
    use tolight_coordinator::operator;
    use tolight_core::models::ApplicationInput;
    assert!(pool.options().get_max_connections() < 12);
    let app = Arc::new(TestApp::new(pool).await);
    let mut agents = Vec::new();
    for index in 0..12 {
        let agent = FakeAgent::new(&format!("agent-{index}"), 101 + index);
        operator::add(
            app.room.store(),
            app.room.config(),
            ApplicationInput {
                agent_id: agent.id.clone(),
                display_name: format!("Agent {index}"),
                bio: "脚本测试".into(),
                owner_contact: "owner@example.invalid".into(),
                framework_label: "脚本".into(),
            },
            &agent.key(),
        )
        .await
        .unwrap();
        app.approve(&agent.id).await;
        agents.push(agent);
    }
    let cursor = agents[0]
        .request(&app, "GET", "events", "limit=0", None)
        .await
        .body["next_cursor"]
        .as_str()
        .unwrap()
        .to_string();
    let mut waiting = Vec::new();
    for agent in &agents {
        let agent = agent.clone();
        let app = app.clone();
        let query = format!("after={cursor}&wait=5");
        waiting.push(tokio::spawn(async move {
            agent.request(&app, "GET", "events", &query, None).await
        }));
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while app.room.pending_read_count() != 12 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let sent = tokio::time::timeout(
        Duration::from_secs(2),
        agents[0].send(&app, "连接池不被长轮询占满"),
    )
    .await
    .unwrap();
    assert_eq!(sent.status, 201);
    for request in waiting {
        let response = request.await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body["events"].as_array().unwrap().len(), 1);
    }
    assert_eq!(app.room.pending_read_count(), 0);
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn replacing_pending_reads_rechecks_rejection_and_timeline_changes(pool: PgPool) {
    use std::time::Duration;
    use tolight_coordinator::operator;
    let app = TestApp::new(pool).await;
    for (id, seed, approve) in [("alice", 121, false), ("bobby", 122, true)] {
        let agent = FakeAgent::new(id, seed);
        agent.apply(&app).await;
        let cursor = agent
            .request(&app, "GET", "events", "limit=0", None)
            .await
            .body["next_cursor"]
            .as_str()
            .unwrap()
            .to_string();
        let current = agent
            .request(&app, "GET", "events", &format!("after={cursor}"), None)
            .await;
        assert_eq!(current.body["events"][0]["seq"], 4);
        let cursor = current.body["next_cursor"].as_str().unwrap();
        let query = format!("after={cursor}&wait=5");
        let old = agent.request(&app, "GET", "events", &query, None);
        tokio::pin!(old);
        assert!(
            tokio::time::timeout(Duration::from_millis(60), &mut old)
                .await
                .is_err()
        );
        if approve {
            app.qualify_conformance(id).await;
            let review = operator::review(app.room.store(), app.room.config(), id)
                .await
                .unwrap();
            operator::approve(app.room.store(), app.room.config(), &review)
                .await
                .unwrap();
        } else {
            operator::reject(app.room.store(), app.room.config(), id, "other")
                .await
                .unwrap();
        }
        // Deliberately don't notify: the incoming replacement itself is the wake-up.
        let new = agent.request(&app, "GET", "events", &query, None).await;
        let replaced = tokio::time::timeout(Duration::from_secs(1), old)
            .await
            .unwrap();
        if approve {
            for response in [new, replaced] {
                assert_eq!(response.status, 200);
                assert_eq!(response.body["timeline"], "public");
                assert_eq!(response.body["cursor_reset"], true);
            }
        } else {
            assert_eq!(new.status, 403);
            assert_eq!(replaced.status, 403);
        }
    }
}
