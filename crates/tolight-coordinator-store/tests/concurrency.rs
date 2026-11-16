mod support;
use chrono::{Duration, Utc};
use sqlx::{PgPool, Row};
use support::*;
use tolight_coordinator_store::{ApplyLimits, SendLimits, Store};

#[sqlx::test(migrations = "./migrations")]
async fn reviewed_key_replacement_rejects_recreated_or_changed_identity_without_writes(
    pool: PgPool,
) {
    let store = Store::from_pool(pool);
    let original = pending(&store, "reviewed-agent", 160).await;
    let original_time = original.public_key_registered_at.unwrap();
    assert_eq!(
        original.public_key.as_deref(),
        Some(input("reviewed-agent", 160).public_key.as_str())
    );
    let key = input("replacement", 161).public_key;
    store
        .withdraw(&original.agent_id, original.public_key.as_deref().unwrap())
        .await
        .unwrap();
    let replacement = pending(&store, "reviewed-agent", 162).await;
    let count_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operator_actions")
        .fetch_one(store.pool())
        .await
        .unwrap();
    let error = store
        .set_key_reviewed(&original, &key, "other", Duration::days(14))
        .await
        .unwrap_err();
    assert_eq!(error.code, "review_changed");
    assert_eq!(
        store.review("reviewed-agent").await.unwrap().public_key,
        replacement.public_key
    );
    assert!(store.agent_by_key(&key).await.unwrap().is_none());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM operator_actions")
            .fetch_one(store.pool())
            .await
            .unwrap(),
        count_before
    );
    store.reject("reviewed-agent", "other").await.unwrap();
    let changed = store.review("reviewed-agent").await.unwrap();
    assert_eq!(
        store
            .set_key_reviewed(&replacement, &key, "other", Duration::days(14))
            .await
            .unwrap_err()
            .code,
        "review_changed"
    );
    let bound = store
        .set_key_reviewed(&changed, &key, "other", Duration::days(14))
        .await
        .unwrap();
    assert_eq!(bound.public_key.as_deref(), Some(key.as_str()));
    assert!(bound.public_key_registered_at.unwrap() > original_time);
    assert_eq!(bound.registered_at, replacement.registered_at);
    assert_eq!(
        store
            .review("reviewed-agent")
            .await
            .unwrap()
            .public_key_registered_at,
        bound.public_key_registered_at
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn public_agents_sort_by_the_published_timestamp_precision(pool: PgPool) {
    let store = Store::from_pool(pool);
    active(&store, "bobby", 2).await;
    active(&store, "alice", 1).await;
    sqlx::query("UPDATE agents SET joined_at='2026-11-14T00:00:00Z'::timestamptz + CASE agent_id WHEN 'bobby' THEN interval '1 microsecond' ELSE interval '2 microseconds' END")
        .execute(store.pool()).await.unwrap();
    let snapshot = store.public_snapshot().await.unwrap();
    assert_eq!(snapshot.agents[0]["agent_id"], "alice");
    assert_eq!(snapshot.agents[1]["agent_id"], "bobby");
    assert_eq!(
        snapshot.agents[0]["joined_at"],
        snapshot.agents[1]["joined_at"]
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn fifty_concurrent_messages_have_no_sequence_gaps_or_time_reversals(pool: PgPool) {
    let store = Store::from_pool(pool);
    let alice = active(&store, "alice", 1).await;
    let mut jobs = tokio::task::JoinSet::new();
    for n in 0..50 {
        let store = store.clone();
        let alice = alice.clone();
        jobs.spawn(async move {
            store
                .send(
                    message(&alice, format!("concurrent-{n}"), "并发发言", &[]),
                    limits(),
                )
                .await
        });
    }
    while let Some(job) = jobs.join_next().await {
        job.unwrap().unwrap();
    }
    let snapshot = store.public_snapshot().await.unwrap();
    assert_eq!(snapshot.last_seq, 51);
    assert_eq!(
        snapshot
            .events
            .iter()
            .map(|event| event["seq"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        (1..=51).collect::<Vec<_>>()
    );
    let backwards:i64=sqlx::query_scalar("SELECT COUNT(*) FROM (SELECT created_at,LAG(created_at) OVER(ORDER BY seq) previous FROM events) t WHERE created_at<previous").fetch_one(store.pool()).await.unwrap();
    assert_eq!(backwards, 0);
    assert_eq!(count(&store, "message_attempts").await, 50);
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_idempotency_and_rate_limits_are_serialized_per_agent(pool: PgPool) {
    let store = Store::from_pool(pool);
    let alice = active(&store, "alice", 1).await;
    let mut jobs = tokio::task::JoinSet::new();
    for _ in 0..10 {
        let store = store.clone();
        let alice = alice.clone();
        jobs.spawn(async move {
            store
                .send(message(&alice, "same-id", "同一条", &[]), limits())
                .await
        });
    }
    let mut identities = vec![];
    let mut first = 0;
    while let Some(job) = jobs.join_next().await {
        let result = job.unwrap().unwrap();
        identities.push((result.seq, result.event_id));
        first += usize::from(!result.duplicate);
    }
    assert_eq!(first, 1);
    assert!(identities.iter().all(|value| value == &identities[0]));
    assert_eq!(count(&store, "message_attempts").await, 1);
    let bobby = active(&store, "bobby", 2).await;
    for n in 0..10 {
        let store = store.clone();
        let bobby = bobby.clone();
        jobs.spawn(async move {
            store
                .send(
                    message(&bobby, format!("limited-{n}"), "有额度的发言", &[]),
                    SendLimits::default(),
                )
                .await
        });
    }
    let (mut passed, mut limited) = (0, 0);
    while let Some(job) = jobs.join_next().await {
        match job.unwrap() {
            Ok(_) => passed += 1,
            Err(error) => {
                assert_eq!(error.code, "message_limit");
                assert_eq!(error.fields["scope"], "minute");
                assert!(error.retry_after.unwrap() >= 1);
                limited += 1;
            }
        }
    }
    assert_eq!((passed, limited), (2, 8));
}

#[sqlx::test(migrations = "./migrations")]
async fn mutual_mentions_and_concurrent_exit_do_not_deadlock_or_retain_departed_targets(
    pool: PgPool,
) {
    let store = Store::from_pool(pool);
    let alice = active(&store, "alice", 1).await;
    let bobby = active(&store, "bobby", 2).await;
    let started = std::time::Instant::now();
    let mut jobs = tokio::task::JoinSet::new();
    for n in 0..30 {
        for (sender, target) in [(&alice, "bobby"), (&bobby, "alice")] {
            let store = store.clone();
            let sender = sender.clone();
            jobs.spawn(async move {
                store
                    .send(
                        message(
                            &sender,
                            format!("mutual-{n}"),
                            &format!("你好 @{target}"),
                            &[target],
                        ),
                        limits(),
                    )
                    .await
            });
        }
    }
    while let Some(job) = jobs.join_next().await {
        job.unwrap().unwrap();
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "unexpectedly slow mutual-mention lock waits"
    );
    let store1 = store.clone();
    let store2 = store.clone();
    let sender = alice.clone();
    let (sent, kicked) = tokio::join!(
        store1.send(
            message(&sender, "race-kick", "你好 @bobby", &["bobby"]),
            limits()
        ),
        store2.kick("bobby", "other")
    );
    kicked.unwrap();
    let left: i64 =
        sqlx::query_scalar("SELECT seq FROM events WHERE kind='agent_left' AND agent_id='bobby'")
            .fetch_one(store.pool())
            .await
            .unwrap();
    match sent {
        Ok(result) => assert!(result.seq < left),
        Err(error) => assert_eq!(error.code, "invalid_mention"),
    }
    let explicit = store
        .send(
            message(&alice, "after-kick", "你好 @bobby", &["bobby"]),
            limits(),
        )
        .await
        .unwrap_err();
    assert_eq!(explicit.code, "invalid_mention");
    let plain = store
        .send(
            message(&alice, "text-after-kick", "你好 @bobby", &[]),
            limits(),
        )
        .await
        .unwrap();
    assert!(plain.mentions.is_empty());
    assert_eq!(
        store
            .send(
                message(&bobby, "kicked-old-request", "旧签名请求", &[]),
                limits()
            )
            .await
            .unwrap_err()
            .code,
        "unauthorized"
    );
    let rejected:i64=sqlx::query_scalar("SELECT COUNT(*) FROM message_attempts WHERE agent_id='alice' AND client_message_id='after-kick' AND outcome='rejected' AND event_seq IS NULL").fetch_one(store.pool()).await.unwrap();
    assert_eq!(rejected, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn committed_mute_pause_and_key_changes_block_later_messages(pool: PgPool) {
    let store = Store::from_pool(pool);
    let alice = active(&store, "alice", 1).await;
    let first = store
        .send(message(&alice, "first", "首次成功", &[]), limits())
        .await
        .unwrap();
    store.mute("alice", None, "spam").await.unwrap();
    assert_eq!(
        store
            .send(message(&alice, "muted", "被禁言的新消息", &[]), limits())
            .await
            .unwrap_err()
            .code,
        "agent_muted"
    );
    let replay = store
        .send(
            message(&alice, "first", "不同正文也重放首次结果", &[]),
            limits(),
        )
        .await
        .unwrap();
    assert!(replay.duplicate);
    assert_eq!(replay.event_id, first.event_id);
    store.unmute("alice").await.unwrap();
    store
        .mute("alice", Some(Utc::now() - Duration::seconds(1)), "other")
        .await
        .unwrap();
    store
        .send(message(&alice, "expired-mute", "禁言已到期", &[]), limits())
        .await
        .unwrap();
    store
        .mute("alice", Some(Utc::now() + Duration::minutes(5)), "other")
        .await
        .unwrap();
    let error = store
        .send(
            message(&alice, "future-mute", "没有额度影响", &[]),
            limits(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "agent_muted");
    assert!(error.retry_after.unwrap() > 1);
    store.unmute("alice").await.unwrap();
    store.pause(true, "other").await.unwrap();
    assert_eq!(
        store
            .send(message(&alice, "paused", "暂停后新消息", &[]), limits())
            .await
            .unwrap_err()
            .code,
        "room_paused"
    );
    assert_eq!(count(&store, "message_attempts").await, 2);
    store.pause(false, "other").await.unwrap();
    let newkey = tolight_core::signing::public_key_from_seed(&[2; 32]);
    store.set_key("alice", &newkey).await.unwrap();
    assert_eq!(
        store
            .send(
                message(&alice, "first", "旧钥重放也不能通过", &[]),
                limits()
            )
            .await
            .unwrap_err()
            .code,
        "unauthorized"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_applications_cannot_overfill_and_replays_bypass_closed_or_full_gates(
    pool: PgPool,
) {
    let store = Store::from_pool(pool);
    let mut jobs = tokio::task::JoinSet::new();
    for n in 1..=20 {
        let store = store.clone();
        jobs.spawn(async move {
            store
                .apply(
                    input(&format!("agent-{n}"), n),
                    ApplyLimits {
                        max_pending_applications: 5,
                        ..Default::default()
                    },
                )
                .await
        });
    }
    let (mut created, mut full) = (0, 0);
    while let Some(job) = jobs.join_next().await {
        match job.unwrap() {
            Ok(_) => created += 1,
            Err(error) => {
                assert_eq!(error.code, "applications_full");
                full += 1;
            }
        }
    }
    assert_eq!((created, full), (5, 15));
    assert_eq!(count(&store, "agents").await, 5);
    let existing = store.list_agents(true).await.unwrap().remove(0);
    store.applications_open(false).await.unwrap();
    let mut replay_input = input(&existing.agent_id, 99);
    replay_input.public_key = existing.public_key.clone().unwrap();
    let replay = store
        .apply(
            replay_input,
            ApplyLimits {
                max_pending_applications: 0,
                source_retry_after: Some(60),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(!replay.counts_for_daily);
    assert_eq!(
        store
            .apply(input("new-agent", 90), ApplyLimits::default())
            .await
            .unwrap_err()
            .code,
        "applications_closed"
    );
    store.applications_open(true).await.unwrap();
    assert_eq!(
        store
            .apply(
                input("new-agent", 90),
                ApplyLimits {
                    source_retry_after: Some(30),
                    ..Default::default()
                }
            )
            .await
            .unwrap_err()
            .code,
        "rate_limited"
    );
    let direct = store
        .apply(
            input("operator-added", 90),
            ApplyLimits {
                operator_bypass: true,
                max_pending_applications: 0,
                source_retry_after: Some(30),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(direct.created);
    assert!(!direct.counts_for_daily);
}

#[sqlx::test(migrations = "./migrations")]
async fn rejected_attempt_replays_and_retracted_successes_preserve_identity_without_content(
    pool: PgPool,
) {
    let store = Store::from_pool(pool);
    let alice = active(&store, "alice", 1).await;
    let _bobby = active(&store, "bobby", 2).await;
    let mut rejected = message(&alice, "rejected", "ignored", &[]);
    rejected.text = Err("secret_shape".into());
    let first = store.send(rejected, limits()).await.unwrap_err();
    assert_eq!(first.code, "content_rejected");
    assert_eq!(first.fields["reason_code"], "secret_shape");
    let replay = store
        .send(
            message(&alice, "rejected", "合法正文不会改变原结果", &[]),
            limits(),
        )
        .await
        .unwrap_err();
    assert_eq!(replay, first);
    let published = store
        .send(
            message(&alice, "accepted", "你好 @bobby", &["bobby"]),
            limits(),
        )
        .await
        .unwrap();
    assert_eq!(published.mentions, vec!["bobby"]);
    store.retract(&published.event_id, "other").await.unwrap();
    let replay = store
        .send(message(&alice, "accepted", "不恢复正文", &[]), limits())
        .await
        .unwrap();
    assert!(replay.duplicate);
    assert_eq!(replay.event_id, published.event_id);
    assert!(replay.mentions.is_empty());
    let attempts = sqlx::query(
        "SELECT outcome,event_seq,reason_code FROM message_attempts ORDER BY client_message_id",
    )
    .fetch_all(store.pool())
    .await
    .unwrap();
    assert_eq!(attempts.len(), 2);
    for row in attempts {
        let published = row.get::<String, _>("outcome") == "published";
        assert_eq!(row.get::<Option<i64>, _>("event_seq").is_some(), published);
        assert_eq!(
            row.get::<Option<String>, _>("reason_code").is_none(),
            published
        );
    }
    let snapshot = store.public_snapshot().await.unwrap();
    let event = snapshot
        .events
        .iter()
        .find(|event| event["event_id"] == published.event_id)
        .unwrap();
    assert!(event.get("text").is_none());
    assert_eq!(event["mentions"], serde_json::json!([]));
    let columns:Vec<String>=sqlx::query_scalar("SELECT column_name FROM information_schema.columns WHERE table_schema='public' AND table_name='message_attempts'").fetch_all(store.pool()).await.unwrap();
    assert!(
        !columns
            .iter()
            .any(|column| column.contains("text") || column.contains("hash"))
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn rate_recovery_uses_both_windows_and_configured_threshold_not_oldest_attempt(pool: PgPool) {
    let store = Store::from_pool(pool);
    let alice = active(&store, "alice", 1).await;
    for n in 0..4 {
        store
            .send(
                message(&alice, format!("used-{n}"), "占用额度", &[]),
                limits(),
            )
            .await
            .unwrap();
    }
    sqlx::query("UPDATE message_attempts SET created_at=clock_timestamp()-INTERVAL '59 seconds' WHERE client_message_id IN ('used-0','used-1')").execute(store.pool()).await.unwrap();
    sqlx::query("UPDATE message_attempts SET created_at=clock_timestamp()-INTERVAL '55 seconds' WHERE client_message_id IN ('used-2','used-3')").execute(store.pool()).await.unwrap();
    let lowered = SendLimits {
        messages_per_minute: 2,
        messages_per_day: 100,
        max_mentions_per_message: 3,
    };
    let usage = store.usage("alice", &lowered).await.unwrap();
    assert_eq!(usage.minute, 4);
    assert!((usage.next_allowed_at.unwrap() - Utc::now()).num_seconds() >= 3);
    let error = store
        .send(message(&alice, "over", "新尝试", &[]), lowered)
        .await
        .unwrap_err();
    assert_eq!(error.code, "message_limit");
    assert!(error.retry_after.unwrap() >= 4);
    let both = SendLimits {
        messages_per_minute: 2,
        messages_per_day: 4,
        max_mentions_per_message: 3,
    };
    sqlx::query("UPDATE message_attempts SET created_at=clock_timestamp()-INTERVAL '86399 seconds' WHERE client_message_id='used-0'").execute(store.pool()).await.unwrap();
    let error = store
        .send(message(&alice, "both", "新尝试", &[]), both)
        .await
        .unwrap_err();
    assert_eq!(error.fields["scope"], "day");
    assert!(error.retry_after.unwrap() >= 4);
}

#[sqlx::test(migrations = "./migrations")]
async fn serve_lease_is_pool_independent_exclusive_and_detects_connection_loss(pool: PgPool) {
    let store = Store::from_pool(pool);
    let mut first = store.acquire_serve_lock().await.unwrap();
    assert!(store.acquire_serve_lock().await.is_err());
    let pid = first.backend_pid().await.unwrap();
    let killed: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert!(killed);
    assert!(first.check().await.is_err());
    let mut next = store.acquire_serve_lock().await.unwrap();
    next.check().await.unwrap();
}

async fn room_waiters(store: &Store, minimum: i64) {
    let start = std::time::Instant::now();
    loop {
        let waiting:i64=sqlx::query_scalar("SELECT COUNT(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE '%FROM room_state WHERE singleton FOR UPDATE%'").fetch_one(store.pool()).await.unwrap();
        if waiting >= minimum {
            return;
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "room lock waiter did not arrive"
        );
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn final_mentions_follow_both_committed_exit_orders_without_locking_the_target(pool: PgPool) {
    let store = Store::from_pool(pool);
    // Run both Operator kick and Owner leave; force each possible room-lock order.
    for (n, owner, exit_first) in [
        (1, false, true),
        (2, true, true),
        (3, false, false),
        (4, true, false),
    ] {
        let sender = active(&store, &format!("sender-{n}"), (n * 2) as u8).await;
        let target = active(&store, &format!("target-{n}"), (n * 2 + 1) as u8).await;
        let mut blocker = store.pool().begin().await.unwrap();
        sqlx::query("SELECT singleton FROM room_state WHERE singleton FOR UPDATE")
            .execute(&mut *blocker)
            .await
            .unwrap();
        let make_exit = || {
            let store = store.clone();
            let target = target.clone();
            tokio::spawn(async move {
                if owner {
                    store
                        .withdraw(&target.agent_id, target.public_key.as_ref().unwrap())
                        .await
                } else {
                    store.kick(&target.agent_id, "other").await
                }
            })
        };
        let make_send = || {
            let store = store.clone();
            let sender = sender.clone();
            let target = target.clone();
            tokio::spawn(async move {
                store
                    .send(
                        message(
                            &sender,
                            "ordered-message",
                            &format!("你好 @{}", target.agent_id),
                            &[&target.agent_id],
                        ),
                        limits(),
                    )
                    .await
            })
        };
        let (exit, send) = if exit_first {
            let exit = make_exit();
            room_waiters(&store, 1).await;
            let send = make_send();
            room_waiters(&store, 2).await;
            (exit, send)
        } else {
            let send = make_send();
            room_waiters(&store, 1).await;
            let exit = make_exit();
            room_waiters(&store, 2).await;
            (exit, send)
        };
        blocker.commit().await.unwrap();
        exit.await.unwrap().unwrap();
        let result = send.await.unwrap();
        if exit_first {
            assert_eq!(result.unwrap_err().code, "invalid_mention");
            let refused:i64=sqlx::query_scalar("SELECT COUNT(*) FROM message_attempts WHERE agent_id=$1 AND client_message_id='ordered-message' AND outcome='rejected' AND event_seq IS NULL").bind(&sender.agent_id).fetch_one(store.pool()).await.unwrap();
            assert_eq!(refused, 1);
            let accepted = store
                .send(
                    message(
                        &sender,
                        "text-only",
                        &format!("你好 @{}", target.agent_id),
                        &[],
                    ),
                    limits(),
                )
                .await
                .unwrap();
            assert!(accepted.mentions.is_empty());
        } else {
            let result = result.unwrap();
            assert_eq!(result.mentions, vec![target.agent_id.clone()]);
            let left: i64 = sqlx::query_scalar(
                "SELECT seq FROM events WHERE kind='agent_left' AND agent_id=$1",
            )
            .bind(&target.agent_id)
            .fetch_one(store.pool())
            .await
            .unwrap();
            assert!(result.seq < left);
        }
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn racing_sender_controls_linearize_before_or_after_the_message_transaction(pool: PgPool) {
    let store = Store::from_pool(pool);
    for n in 1..=4 {
        let agent = active(&store, &format!("racer-{n}"), n).await;
        let (message_result, muted) = tokio::join!(
            store.send(message(&agent, "during-mute", "并发禁言", &[]), limits()),
            store.mute(&agent.agent_id, None, "other")
        );
        muted.unwrap();
        match message_result {
            Ok(_) => {}
            Err(error) => assert_eq!(error.code, "agent_muted"),
        }
        let out_of_order:i64=sqlx::query_scalar("SELECT COUNT(*) FROM events e JOIN operator_actions a ON a.target_agent_id=e.agent_id AND a.kind='mute' WHERE e.agent_id=$1 AND e.kind='agent_message' AND e.created_at>a.created_at").bind(&agent.agent_id).fetch_one(store.pool()).await.unwrap();
        assert_eq!(out_of_order, 0);
        assert_eq!(
            store
                .send(message(&agent, "after-mute", "禁止的新消息", &[]), limits())
                .await
                .unwrap_err()
                .code,
            "agent_muted"
        );
        store.unmute(&agent.agent_id).await.unwrap();
        let (message_result, paused) = tokio::join!(
            store.send(message(&agent, "during-pause", "并发暂停", &[]), limits()),
            store.pause(true, "other")
        );
        paused.unwrap();
        let pause_seq: i64 =
            sqlx::query_scalar("SELECT MAX(seq) FROM events WHERE kind='room_paused'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        match message_result {
            Ok(result) => assert!(result.seq < pause_seq),
            Err(error) => assert_eq!(error.code, "room_paused"),
        }
        assert_eq!(
            store
                .send(
                    message(&agent, "after-pause", "禁止的新消息", &[]),
                    limits()
                )
                .await
                .unwrap_err()
                .code,
            "room_paused"
        );
        store.pause(false, "other").await.unwrap();
        let (message_result, kicked) = tokio::join!(
            store.send(message(&agent, "during-kick", "并发踢出", &[]), limits()),
            store.kick(&agent.agent_id, "other")
        );
        kicked.unwrap();
        let left: i64 =
            sqlx::query_scalar("SELECT seq FROM events WHERE kind='agent_left' AND agent_id=$1")
                .bind(&agent.agent_id)
                .fetch_one(store.pool())
                .await
                .unwrap();
        match message_result {
            Ok(result) => assert!(result.seq < left),
            Err(error) => assert_eq!(error.code, "unauthorized"),
        }
        assert_eq!(
            store
                .send(message(&agent, "after-kick", "禁止的新消息", &[]), limits())
                .await
                .unwrap_err()
                .code,
            "unauthorized"
        );
    }
}

async fn agent_waiters(store: &Store, minimum: i64) {
    let start = std::time::Instant::now();
    loop {
        let waiting:i64=sqlx::query_scalar("SELECT COUNT(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE '%FROM agents WHERE agent_id=$1 FOR NO KEY UPDATE%'").fetch_one(store.pool()).await.unwrap();
        if waiting >= minimum {
            return;
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "agent lock waiter did not arrive"
        );
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn revoked_keys_waiting_for_agent_lock_cannot_write_activity_messages_or_reapplications(
    pool: PgPool,
) {
    let store = Store::from_pool(pool);
    for operation in 0..3 {
        let agent = if operation == 2 {
            let agent = pending(&store, "pending-racer", 7).await;
            store.reject(&agent.agent_id, "other").await.unwrap()
        } else {
            active(&store, &format!("active-racer-{operation}"), operation + 1).await
        };
        let mut blocker = store.pool().begin().await.unwrap();
        sqlx::query("SELECT agent_id FROM agents WHERE agent_id=$1 FOR NO KEY UPDATE")
            .bind(&agent.agent_id)
            .execute(&mut *blocker)
            .await
            .unwrap();
        let newkey = tolight_core::signing::public_key_from_seed(&[operation + 20; 32]);
        let rotate_store = store.clone();
        let rotate_agent = agent.clone();
        let rotate_key = newkey.clone();
        let rotate = tokio::spawn(async move {
            rotate_store
                .set_key(&rotate_agent.agent_id, &rotate_key)
                .await
        });
        agent_waiters(&store, 1).await;
        let old_store = store.clone();
        let old_agent = agent.clone();
        let old = tokio::spawn(async move {
            match operation {
                0 => old_store
                    .send(
                        message(&old_agent, "old-in-flight", "不应公开", &[]),
                        limits(),
                    )
                    .await
                    .map(|_| ()),
                1 => {
                    old_store
                        .touch_last_seen(
                            &old_agent.agent_id,
                            old_agent.public_key.as_ref().unwrap(),
                        )
                        .await
                }
                _ => {
                    let mut input = input(&old_agent.agent_id, 7);
                    input.bio = "旧钥不得修改申请".into();
                    old_store
                        .apply(input, ApplyLimits::default())
                        .await
                        .map(|_| ())
                }
            }
        });
        agent_waiters(&store, 2).await;
        blocker.commit().await.unwrap();
        rotate.await.unwrap().unwrap();
        let error = old.await.unwrap().unwrap_err();
        assert_eq!(
            error.code,
            if operation == 2 {
                "key_already_registered"
            } else {
                "unauthorized"
            }
        );
        let after = store.agent(&agent.agent_id).await.unwrap().unwrap();
        assert!(after.last_seen_at.is_none());
        assert_eq!(after.bio, agent.bio);
        assert_eq!(after.state, agent.state);
        assert_eq!(after.public_key, Some(newkey));
    }
    assert_eq!(count(&store, "message_attempts").await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn throttled_activity_check_does_not_wait_for_an_agent_write_lock(pool: PgPool) {
    let store = Store::from_pool(pool);
    let agent = active(&store, "alice", 1).await;
    store
        .touch_last_seen(&agent.agent_id, agent.public_key.as_ref().unwrap())
        .await
        .unwrap();
    let mut tx = store.pool().begin().await.unwrap();
    sqlx::query("SELECT agent_id FROM agents WHERE agent_id='alice' FOR NO KEY UPDATE")
        .execute(&mut *tx)
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_millis(200),
        store.touch_last_seen(&agent.agent_id, agent.public_key.as_ref().unwrap()),
    )
    .await
    .expect("a throttled read must not take the row lock")
    .unwrap();
    tx.rollback().await.unwrap();
}
