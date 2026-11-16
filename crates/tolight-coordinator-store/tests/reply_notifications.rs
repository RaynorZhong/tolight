mod support;
use sqlx::PgPool;
use support::*;
use tolight_coordinator_store::Store;
use tolight_core::{config::Config, participant::ReadRequest};

#[sqlx::test(migrations = "./migrations")]
async fn replies_without_at_are_readable_notifications_and_replays_do_not_backfill(pool: PgPool) {
    let store = Store::from_pool(pool);
    let alice = active(&store, "alice", 201).await;
    let bobby = active(&store, "bobby", 202).await;
    let carol = active(&store, "carol", 203).await;
    let parent = store
        .send(message(&alice, "parent", "一个具体问题", &[]), limits())
        .await
        .unwrap();
    let cursor = store
        .participant_read_snapshot(
            "alice",
            alice.public_key.as_ref().unwrap(),
            &ReadRequest::default(),
            &Config::default(),
        )
        .await
        .unwrap();
    let after = tolight_core::participant::read_events(
        &cursor.events,
        "public",
        "alice",
        cursor.last_seq as u64,
        None,
        &ReadRequest::default(),
        &Config::default(),
    )
    .unwrap()
    .next_cursor;
    let mut reply = message(&bobby, "reply", "直接回答，不带点名", &[]);
    reply.reply_to_event_id = Some(parent.event_id.clone());
    let sent = store.send(reply.clone(), limits()).await.unwrap();
    assert_eq!(sent.mentions, ["alice"]);
    let query = ReadRequest {
        after: Some(after),
        mentions_only: true,
        ..Default::default()
    };
    let snapshot = store
        .participant_read_snapshot(
            "alice",
            alice.public_key.as_ref().unwrap(),
            &query,
            &Config::default(),
        )
        .await
        .unwrap();
    let page = tolight_core::participant::read_events(
        &snapshot.events,
        "public",
        "alice",
        snapshot.last_seq as u64,
        None,
        &query,
        &Config::default(),
    )
    .unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0]["event_id"], sent.event_id);
    assert_eq!(page.events[0]["mentions_you"], true);
    assert_eq!(page.events[0]["reply_to"]["event_id"], parent.event_id);
    assert_eq!(page.events[0]["text"], "直接回答，不带点名");

    // A changed retry payload cannot add another recipient or change the reply.
    reply.reply_to_event_id = None;
    reply.mentions = vec![carol.agent_id.clone()];
    let replay = store.send(reply.clone(), limits()).await.unwrap();
    assert!(replay.duplicate);
    assert_eq!(replay.event_id, sent.event_id);
    assert_eq!(replay.mentions, ["alice"]);
    store.retract(&sent.event_id, "other").await.unwrap();
    assert!(
        store
            .send(reply, limits())
            .await
            .unwrap()
            .mentions
            .is_empty()
    );

    let mut self_reply = message(&alice, "self", "补充自己", &[]);
    self_reply.reply_to_event_id = Some(parent.event_id.clone());
    assert!(
        store
            .send(self_reply, limits())
            .await
            .unwrap()
            .mentions
            .is_empty()
    );
    store.kick("alice", "other").await.unwrap();
    let mut departed_reply = message(&bobby, "departed", "保留引用但不提醒已退出者", &[]);
    departed_reply.reply_to_event_id = Some(parent.event_id.clone());
    let departed = store.send(departed_reply, limits()).await.unwrap();
    assert!(departed.mentions.is_empty());
    let snapshot = store.public_snapshot().await.unwrap();
    let event = snapshot
        .events
        .iter()
        .find(|event| event["event_id"] == departed.event_id)
        .unwrap();
    assert_eq!(event["reply_to"]["event_id"], parent.event_id);

    store.retract(&parent.event_id, "other").await.unwrap();
    let mut invalid_reply = message(&bobby, "retracted", "不引用撤回目标", &[]);
    invalid_reply.reply_to_event_id = Some(parent.event_id);
    let invalid = store.send(invalid_reply, limits()).await.unwrap();
    let snapshot = store.public_snapshot().await.unwrap();
    let event = snapshot
        .events
        .iter()
        .find(|event| event["event_id"] == invalid.event_id)
        .unwrap();
    assert!(event["reply_to"].is_null());
    assert!(invalid.mentions.is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn automatic_reply_targets_share_the_limit_and_rejected_retries_keep_the_result(
    pool: PgPool,
) {
    let store = Store::from_pool(pool);
    let alice = active(&store, "alice", 211).await;
    let bobby = active(&store, "bobby", 212).await;
    active(&store, "carol", 213).await;
    active(&store, "david", 214).await;
    let parent = store
        .send(message(&alice, "parent", "问题", &[]), limits())
        .await
        .unwrap();
    let mut reply = message(&bobby, "overflow", "@carol @david 回应", &[]);
    reply.reply_to_event_id = Some(parent.event_id.clone());
    let mut small = limits();
    small.max_mentions_per_message = 2;
    let head = store.public_snapshot().await.unwrap().last_seq;
    let error = store.send(reply.clone(), small.clone()).await.unwrap_err();
    assert_eq!(error.code, "too_many_mentions");
    assert_eq!(store.public_snapshot().await.unwrap().last_seq, head);
    assert_eq!(count(&store, "message_attempts").await, 2);
    assert_eq!(
        store.send(reply, limits()).await.unwrap_err().code,
        "too_many_mentions"
    );
    assert_eq!(count(&store, "message_attempts").await, 2);
    let mut deduplicated = message(&bobby, "dedup", "@alice @carol 回应", &["alice"]);
    deduplicated.reply_to_event_id = Some(parent.event_id);
    assert_eq!(
        store.send(deduplicated, small).await.unwrap().mentions,
        ["alice", "carol"]
    );
}
