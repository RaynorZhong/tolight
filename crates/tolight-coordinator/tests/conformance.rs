mod support;
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};
use support::fake_agent::{FakeAgent, Reply, TestApp};
use tolight_coordinator::operator;
use tolight_core::participant::ReadRequest;

async fn app(pool: PgPool, daily: usize, window: u64) -> (TestApp, Arc<AtomicI64>) {
    let now = Arc::new(AtomicI64::new(Utc::now().timestamp()));
    let clock = now.clone();
    let app = TestApp::configured_clock(
        pool,
        |cfg| {
            cfg.conformance_runs_per_day = daily;
            cfg.conformance_reply_window = window;
            cfg.conformance_retry_after = 1;
        },
        Arc::new(move || {
            chrono::DateTime::from_timestamp(clock.load(Ordering::SeqCst), 0).unwrap()
        }),
    )
    .await;
    (app, now)
}
async fn request(
    agent: &FakeAgent,
    app: &TestApp,
    clock: &AtomicI64,
    method: &str,
    path: &str,
    query: &str,
    body: Option<Value>,
) -> Reply {
    agent
        .raw(
            app,
            method,
            path,
            query,
            body.map(|v| serde_json::to_vec(&v).unwrap())
                .unwrap_or_default(),
            clock.load(Ordering::SeqCst),
            &[],
        )
        .await
}
async fn apply(agent: &FakeAgent, app: &TestApp, clock: &AtomicI64) {
    let response=request(agent,app,clock,"POST","applications","",Some(json!({"agent_id":agent.id,"display_name":format!("{} Agent",agent.id),"bio":"本地沙盒测试","owner_contact":"owner@example.invalid","framework_label":"test"}))).await;
    assert_eq!(response.status, 201, "{}", response.body);
}
fn payload(target: &Value) -> Value {
    json!({"client_message_id":"sandbox-test","text":"简短回复","reply_to_event_id":target["event_id"],"mentions":[]})
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn three_checks_gate_approval_and_do_not_publish_sandbox_data(pool: PgPool) {
    let (app, clock) = app(pool, 5, 20).await;
    let alice = FakeAgent::new("alice", 91);
    apply(&alice, &app, &clock).await;
    let before = operator::review(app.room.store(), app.room.config(), "alice")
        .await
        .unwrap();
    assert!(
        operator::approve(app.room.store(), app.room.config(), &before)
            .await
            .is_err()
    );
    let first = request(&alice, &app, &clock, "GET", "events", "limit=0", None).await;
    assert_eq!(first.status, 200);
    assert_eq!(first.body["head_seq"], 3);
    assert_eq!(first.body["events"], json!([]));
    let next = request(
        &alice,
        &app,
        &clock,
        "GET",
        "events",
        &format!("after={}", first.body["next_cursor"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(next.body["events"].as_array().unwrap().len(), 1);
    assert_eq!(next.body["events"][0]["seq"], 4);
    let body = payload(&next.body["events"][0]);
    let limited = request(
        &alice,
        &app,
        &clock,
        "POST",
        "messages",
        "",
        Some(body.clone()),
    )
    .await;
    assert_eq!(limited.status, 429);
    assert_eq!(limited.headers["retry-after"], "1");
    assert_eq!(limited.body["error"]["scope"], "minute");
    assert_eq!(
        app.room
            .store()
            .review("alice")
            .await
            .unwrap()
            .application_revision,
        before.application_revision
    );
    clock.fetch_add(1, Ordering::SeqCst);
    let accepted = request(
        &alice,
        &app,
        &clock,
        "POST",
        "messages",
        "",
        Some(body.clone()),
    )
    .await;
    assert_eq!(accepted.status, 201, "{}", accepted.body);
    assert_eq!(accepted.body["seq"], 5);
    assert_eq!(accepted.body["mentions"], json!(["examiner"]));
    assert_eq!(
        request(&alice, &app, &clock, "POST", "messages", "", Some(body))
            .await
            .status,
        403
    );
    let status = request(&alice, &app, &clock, "GET", "self", "", None).await;
    assert_eq!(status.body["state"], "PENDING");
    assert_eq!(status.body["conformance"]["last_run"]["passed"], true);
    assert_eq!(status.body["conformance"]["suite_version"], "1");
    assert_eq!(status.body["usage"]["day_used"], 0);
    assert_eq!(
        app.room.store().public_snapshot().await.unwrap().last_seq,
        0
    );
    assert_eq!(
        app.room
            .store()
            .review("alice")
            .await
            .unwrap()
            .application_revision,
        before.application_revision + 1
    );
    assert_eq!(
        operator::approve(app.room.store(), app.room.config(), &before)
            .await
            .unwrap_err()
            .code,
        "review_changed"
    );
    let idle = request(
        &alice,
        &app,
        &clock,
        "GET",
        "events",
        &format!(
            "after={}&wait=5",
            next.body["next_cursor"].as_str().unwrap()
        ),
        None,
    )
    .await;
    assert_eq!(idle.body["cursor_reset"], true);
    assert_eq!(idle.body["head_seq"], 0);
    assert_eq!(idle.body["events"], json!([]));
    let query = format!(
        "after={}&wait=5",
        idle.body["next_cursor"].as_str().unwrap()
    );
    let pending = request(&alice, &app, &clock, "GET", "events", &query, None);
    tokio::pin!(pending);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(80), &mut pending)
            .await
            .is_err()
    );
    let snapshot = operator::review(app.room.store(), app.room.config(), "alice")
        .await
        .unwrap();
    operator::approve(app.room.store(), app.room.config(), &snapshot)
        .await
        .unwrap();
    app.room.notify_change();
    let public = tokio::time::timeout(std::time::Duration::from_secs(1), pending)
        .await
        .unwrap();
    assert_eq!(public.body["timeline"], "public");
    assert_eq!(public.body["cursor_reset"], true);
    assert_eq!(public.body["events"][0]["kind"], "agent_joined");
    let public = app.room.store().public_snapshot().await.unwrap();
    assert_eq!(public.events.len(), 1);
    assert!(
        !serde_json::to_string(&public.events)
            .unwrap()
            .contains("examiner")
    );
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn invalid_reads_do_not_start_sessions_and_daily_cap_recovers_on_a_new_utc_day(pool: PgPool) {
    let (app, clock) = app(pool, 2, 20).await;
    let alice = FakeAgent::new("alice", 92);
    apply(&alice, &app, &clock).await;
    for path in ["self", "agents"] {
        assert_eq!(
            request(&alice, &app, &clock, "GET", path, "", None)
                .await
                .status,
            200
        );
    }
    assert!(
        app.room
            .store()
            .conformance_runs("alice")
            .await
            .unwrap()
            .is_empty()
    );
    let agents = request(&alice, &app, &clock, "GET", "agents", "", None).await;
    assert_eq!(agents.body["agents"].as_array().unwrap().len(), 2);
    assert_eq!(agents.body["agents"][0]["agent_id"], "alice");
    assert_eq!(agents.body["agents"][1]["agent_id"], "examiner");
    for query in ["after=old&limit=0", "limit=51", "wait=31"] {
        assert_eq!(
            request(&alice, &app, &clock, "GET", "events", query, None)
                .await
                .status,
            400
        );
    }
    assert!(
        app.room
            .store()
            .conformance_runs("alice")
            .await
            .unwrap()
            .is_empty()
    );
    for n in 0..2 {
        let first = request(
            &alice,
            &app,
            &clock,
            "GET",
            "events",
            if n == 0 { "" } else { "after=old" },
            None,
        )
        .await;
        assert_eq!(first.body["head_seq"], 3);
        assert_eq!(first.body["cursor_reset"], n > 0);
        let bad = json!({"client_message_id":format!("bad-{n}"),"text":"回复历史","reply_to_event_id":first.body["events"][1]["event_id"]});
        assert_eq!(
            request(&alice, &app, &clock, "POST", "messages", "", Some(bad))
                .await
                .status,
            403
        );
        let status = request(&alice, &app, &clock, "GET", "self", "", None).await;
        assert_eq!(status.body["state"], "PENDING");
        assert_eq!(
            status.body["conformance"]["last_run"]["failure_reason"],
            "replied_to_history"
        );
    }
    let empty = request(&alice, &app, &clock, "GET", "events", "", None).await;
    assert_eq!(empty.body["events"], json!([]));
    assert_eq!(empty.body["head_seq"], 0);
    assert_eq!(
        app.room
            .store()
            .conformance_runs("alice")
            .await
            .unwrap()
            .len(),
        2
    );
    let wait = format!(
        "after={}&wait=1",
        empty.body["next_cursor"].as_str().unwrap()
    );
    let pending = request(&alice, &app, &clock, "GET", "events", &wait, None);
    tokio::pin!(pending);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(80), &mut pending)
            .await
            .is_err()
    );
    assert_eq!(pending.await.body["events"], json!([]));
    clock.fetch_add(86400, Ordering::SeqCst);
    let next = request(&alice, &app, &clock, "GET", "events", "", None).await;
    assert_eq!(next.body["head_seq"], 3);
    let status = request(&alice, &app, &clock, "GET", "self", "", None).await;
    assert_eq!(status.body["conformance"]["runs_today"], 1);
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn sanitized_failures_timeouts_early_retries_and_restart_are_private_and_persistent(
    pool: PgPool,
) {
    let (app, clock) = app(pool.clone(), 5, 4).await;
    let alice = FakeAgent::new("alice", 93);
    apply(&alice, &app, &clock).await;
    request(&alice, &app, &clock, "GET", "events", "", None).await;
    let run = app
        .room
        .store()
        .conformance_runs("alice")
        .await
        .unwrap()
        .pop()
        .unwrap();
    let malformed =
        json!({"client_message_id":"bad","text":"","reply_to_event_id":run.event_ids[3]});
    assert_eq!(
        request(
            &alice,
            &app,
            &clock,
            "POST",
            "messages",
            "",
            Some(malformed)
        )
        .await
        .status,
        422
    );
    assert_eq!(
        app.room.store().conformance_runs("alice").await.unwrap()[0].limited_at,
        None
    );
    clock.fetch_add(4, Ordering::SeqCst);
    let status = request(&alice, &app, &clock, "GET", "self", "", None).await;
    assert_eq!(
        status.body["conformance"]["last_run"]["failure_reason"],
        "no_reply"
    );
    request(&alice, &app, &clock, "GET", "events", "", None).await;
    let run = app
        .room
        .store()
        .conformance_runs("alice")
        .await
        .unwrap()
        .pop()
        .unwrap();
    let reply = json!({"client_message_id":"limited","text":"简短回复","reply_to_event_id":run.event_ids[3]});
    assert_eq!(
        request(
            &alice,
            &app,
            &clock,
            "POST",
            "messages",
            "",
            Some(reply.clone())
        )
        .await
        .status,
        429
    );
    // A reconstructed service/Store sees the same row and limited_at.
    let restored = tolight_coordinator_store::Store::from_pool(pool.clone());
    assert_eq!(
        restored
            .conformance_runs("alice")
            .await
            .unwrap()
            .last()
            .unwrap()
            .limited_at,
        Some(chrono::DateTime::from_timestamp(clock.load(Ordering::SeqCst), 0).unwrap())
    );
    assert_eq!(
        request(&alice, &app, &clock, "POST", "messages", "", Some(reply))
            .await
            .status,
        403
    );
    assert_eq!(
        request(&alice, &app, &clock, "GET", "self", "", None)
            .await
            .body["conformance"]["last_run"]["failure_reason"],
        "retried_too_early"
    );
    request(&alice, &app, &clock, "GET", "events", "", None).await;
    let run = app
        .room
        .store()
        .conformance_runs("alice")
        .await
        .unwrap()
        .pop()
        .unwrap();
    let reply = json!({"client_message_id":"timeout","text":"简短回复","reply_to_event_id":run.event_ids[3]});
    assert_eq!(
        request(&alice, &app, &clock, "POST", "messages", "", Some(reply))
            .await
            .status,
        429
    );
    clock.fetch_add(4, Ordering::SeqCst);
    assert_eq!(
        request(&alice, &app, &clock, "GET", "agents", "", None)
            .await
            .status,
        200
    );
    let status = request(&alice, &app, &clock, "GET", "self", "", None).await;
    assert_eq!(
        status.body["conformance"]["last_run"]["failure_reason"],
        "no_retry"
    );
    assert_eq!(status.body["usage"]["day_used"], 0);
    assert_eq!(
        app.room.store().public_snapshot().await.unwrap().last_seq,
        0
    );
    let runs = app.room.store().conformance_runs("alice").await.unwrap();
    assert_eq!(runs.len(), 3);
    assert_eq!(
        runs.iter()
            .flat_map(|r| r.event_ids.iter())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        12
    );
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn concurrent_initial_reads_create_one_session_and_expiration_at_wait_end_does_not_restart_it(
    pool: PgPool,
) {
    let (app, clock) = app(pool, 5, 2).await;
    let alice = FakeAgent::new("alice", 94);
    apply(&alice, &app, &clock).await;
    let mut jobs = tokio::task::JoinSet::new();
    for _ in 0..12 {
        let room = app.room.clone();
        let context = alice.context();
        jobs.spawn(async move { room.read(&context, ReadRequest::default()).await.unwrap() });
    }
    let mut first = 0;
    while let Some(value) = jobs.join_next().await {
        if value.unwrap().head_seq == 3 {
            first += 1;
        }
    }
    assert_eq!(first, 1);
    assert_eq!(
        app.room
            .store()
            .conformance_runs("alice")
            .await
            .unwrap()
            .len(),
        1
    );
    let run = app
        .room
        .store()
        .conformance_runs("alice")
        .await
        .unwrap()
        .pop()
        .unwrap();
    let after = tolight_core::participant::encode_cursor(
        "conformance",
        4,
        Some(&run.event_ids[3]),
        Some(&run.run_id),
    );
    let room = app.room.clone();
    let context = alice.context();
    let pending = tokio::spawn(async move {
        room.read(
            &context,
            ReadRequest {
                after: Some(after),
                wait: 1,
                ..ReadRequest::default()
            },
        )
        .await
        .unwrap()
    });
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    clock.fetch_add(2, Ordering::SeqCst);
    let response = pending.await.unwrap();
    assert!(response.events.is_empty());
    assert!(response.cursor_reset);
    assert_eq!(response.head_seq, 0);
    let runs = app.room.store().conformance_runs("alice").await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].failure_reason.as_deref(), Some("no_reply"));
    let usage = app
        .room
        .store()
        .usage("alice", &app.room.send_limits())
        .await
        .unwrap();
    assert_eq!(usage.day, 0);
}

async fn waiting_locks(pool: &PgPool, wanted: i64) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock'").fetch_one(pool).await.unwrap();
        if count >= wanted {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected {wanted} locks, got {count}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn sandbox_deadlines_use_time_after_the_agent_row_lock_is_acquired(pool: PgPool) {
    let (app, clock) = app(pool.clone(), 1, 2).await;
    let today = Utc::now().date_naive();
    clock.store(
        today.and_hms_opt(12, 0, 0).unwrap().and_utc().timestamp(),
        Ordering::SeqCst,
    );
    for (index, path) in ["messages", "events", "self", "agents"]
        .into_iter()
        .enumerate()
    {
        let id = format!("deadline-{index}");
        let agent = FakeAgent::new(&id, 181 + index as u8);
        apply(&agent, &app, &clock).await;
        let first = request(&agent, &app, &clock, "GET", "events", "limit=0", None).await;
        assert_eq!(first.status, 200);
        let run = app
            .room
            .store()
            .conformance_runs(&id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let reply = tolight_core::models::SendRequest {
            client_message_id: "waited-on-row-lock".into(),
            text: "简短回复".into(),
            reply_to_event_id: Some(run.event_ids[3].clone()),
            mentions: vec![],
        };
        if path == "messages" {
            let limited = request(
                &agent,
                &app,
                &clock,
                "POST",
                "messages",
                "",
                Some(
                    json!({"client_message_id":reply.client_message_id,"text":reply.text,
                    "reply_to_event_id":reply.reply_to_event_id,"mentions":[]}),
                ),
            )
            .await;
            assert_eq!(limited.status, 429);
            clock.fetch_add(1, Ordering::SeqCst);
        }
        let before = app.room.store().review(&id).await.unwrap();
        let mut blocker = pool.begin().await.unwrap();
        sqlx::query("SELECT agent_id FROM agents WHERE agent_id=$1 FOR NO KEY UPDATE")
            .bind(&id)
            .execute(&mut *blocker)
            .await
            .unwrap();
        let room = app.room.clone();
        let context = agent.context();
        let after = tolight_core::participant::encode_cursor(
            "conformance",
            4,
            Some(&run.event_ids[3]),
            Some(&run.run_id),
        );
        let waiting = tokio::spawn(async move {
            match path {
                "messages" => room.send(&context, reply).await.map(|sent| json!({"seq":sent.seq})),
                "events" => room.read(&context, ReadRequest {
                    after: Some(after), ..ReadRequest::default()
                }).await.map(|response| json!({"head_seq":response.head_seq,"cursor_reset":response.cursor_reset})),
                "self" => room.self_info(&context).await,
                _ => room.list_participants(&context).await,
            }
        });
        waiting_locks(&pool, 1).await;
        // The request arrived before expiry and is genuinely blocked on the
        // owned Agent row. Its decision must use time after that wait, including
        // when the reply was already eligible to pass before it blocked.
        clock.fetch_add(3, Ordering::SeqCst);
        let decision_time =
            chrono::DateTime::from_timestamp(clock.load(Ordering::SeqCst), 0).unwrap();
        blocker.commit().await.unwrap();
        let response = waiting.await.unwrap();
        if path == "messages" {
            assert_eq!(
                response.unwrap_err().code,
                "agent_not_active",
                "an expired sandbox reply must not become a passing run"
            );
        } else {
            let response = response.unwrap();
            if path == "events" {
                assert_eq!(response["head_seq"], 0);
                assert_eq!(response["cursor_reset"], true);
            }
            if path == "self" {
                assert_eq!(response["conformance"]["last_run"]["passed"], false);
                assert!(!response["conformance"]["last_run"]["finished_at"].is_null());
            }
        }
        let runs = app.room.store().conformance_runs(&id).await.unwrap();
        assert_eq!(
            runs.len(),
            1,
            "a lock wait must not create an extra daily session"
        );
        assert_eq!(runs[0].finished_at, Some(decision_time), "{path}");
        assert_eq!(runs[0].passed, Some(false), "{path}");
        assert_eq!(
            runs[0].failure_reason.as_deref(),
            Some(if path == "messages" {
                "no_retry"
            } else {
                "no_reply"
            }),
            "{path}"
        );
        let current = app.room.store().review(&id).await.unwrap();
        assert!(current.conformance_passed_at.is_none(), "{path}");
        assert_eq!(
            current.application_revision, before.application_revision,
            "{path}"
        );
    }
    // Daily counts and a new session must use the same post-lock decision time.
    // Reuse applicants without consuming another per-source application slot.
    clock.store(
        today.and_hms_opt(23, 59, 58).unwrap().and_utc().timestamp(),
        Ordering::SeqCst,
    );
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("SELECT agent_id FROM agents WHERE agent_id IN ('deadline-1','deadline-2') ORDER BY agent_id FOR NO KEY UPDATE")
        .execute(&mut *blocker).await.unwrap();
    let room = app.room.clone();
    let context = FakeAgent::new("deadline-2", 183).context();
    let waiting_status = tokio::spawn(async move { room.self_info(&context).await.unwrap() });
    let room = app.room.clone();
    let context = FakeAgent::new("deadline-1", 182).context();
    let waiting_read =
        tokio::spawn(async move { room.read(&context, ReadRequest::default()).await.unwrap() });
    waiting_locks(&pool, 2).await;
    clock.fetch_add(3, Ordering::SeqCst);
    let decision_time = chrono::DateTime::from_timestamp(clock.load(Ordering::SeqCst), 0).unwrap();
    blocker.commit().await.unwrap();
    assert_eq!(
        waiting_status.await.unwrap()["conformance"]["runs_today"],
        0
    );
    let first = waiting_read.await.unwrap();
    assert_eq!(
        first.head_seq, 3,
        "a fresh UTC allowance must expose only initial context"
    );
    let runs = app
        .room
        .store()
        .conformance_runs("deadline-1")
        .await
        .unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1].started_at, decision_time);
    let context = FakeAgent::new("deadline-1", 182).context();
    assert_eq!(
        app.room.self_info(&context).await.unwrap()["conformance"]["runs_today"],
        1
    );
    assert_eq!(
        app.room.store().public_snapshot().await.unwrap().last_seq,
        0
    );
    let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM message_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempts, 0);
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn key_rotation_revalidates_all_three_pending_read_paths_before_any_session_progress(
    pool: PgPool,
) {
    let (app, clock) = app(pool.clone(), 5, 2).await;
    for (index, path) in ["events", "self", "agents"].into_iter().enumerate() {
        let id = format!("agent-{index}");
        let agent = FakeAgent::new(&id, 161 + index as u8);
        apply(&agent, &app, &clock).await;
        request(&agent, &app, &clock, "GET", "events", "", None).await;
        let before = app.room.store().conformance_runs(&id).await.unwrap();
        clock.fetch_add(3, Ordering::SeqCst); // An old-key callback would now expire the row.
        let mut blocker = pool.begin().await.unwrap();
        sqlx::query("SELECT agent_id FROM agents WHERE agent_id=$1 FOR NO KEY UPDATE")
            .bind(&id)
            .execute(&mut *blocker)
            .await
            .unwrap();
        let store = app.room.store().clone();
        let target = id.clone();
        let key = tolight_core::signing::public_key_from_seed(&[171 + index as u8; 32]);
        let rotation = tokio::spawn(async move { store.set_key(&target, &key).await });
        waiting_locks(&pool, 1).await;
        let room = app.room.clone();
        let context = agent.context();
        let old = tokio::spawn(async move {
            match path {
                "events" => room
                    .read(&context, ReadRequest::default())
                    .await
                    .map(|_| ()),
                "self" => room.self_info(&context).await.map(|_| ()),
                _ => room.list_participants(&context).await.map(|_| ()),
            }
        });
        waiting_locks(&pool, 2).await;
        blocker.commit().await.unwrap();
        rotation.await.unwrap().unwrap();
        assert_eq!(
            old.await.unwrap().unwrap_err().code,
            "unauthorized",
            "{path}"
        );
        assert_eq!(
            app.room.store().conformance_runs(&id).await.unwrap(),
            before,
            "old {path} altered session"
        );
    }
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn an_agents_cursor_cannot_select_another_agents_sandbox(pool: PgPool) {
    let (app, clock) = app(pool, 5, 20).await;
    let alice = FakeAgent::new("alice", 181);
    let bob = FakeAgent::new("bobby", 182);
    apply(&alice, &app, &clock).await;
    apply(&bob, &app, &clock).await;
    let first = request(&alice, &app, &clock, "GET", "events", "", None).await;
    let other = request(&bob, &app, &clock, "GET", "events", "", None).await;
    let foreign = other.body["next_cursor"].as_str().unwrap();
    let response = request(
        &alice,
        &app,
        &clock,
        "GET",
        "events",
        &format!("after={foreign}&mentions_only=true"),
        None,
    )
    .await;
    assert_eq!(response.body["cursor_reset"], true);
    assert!(
        response.body["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event["mentions"]
                .as_array()
                .unwrap()
                .iter()
                .all(|target| target["agent_id"] == "alice"))
    );
    assert_eq!(
        response.body["events"][0]["event_id"],
        first.body["events"][0]["event_id"]
    );
    assert_ne!(
        response.body["events"][0]["event_id"],
        other.body["events"][0]["event_id"]
    );
}
