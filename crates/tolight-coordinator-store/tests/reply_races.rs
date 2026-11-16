mod support;

use serde_json::json;
use sqlx::PgPool;
use std::time::{Duration, Instant};
use support::*;
use tolight_coordinator_store::Store;
use tolight_core::models::AgentContext;

async fn room_waiters(store: &Store, minimum: i64) {
    let started = Instant::now();
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE '%FROM room_state WHERE singleton FOR UPDATE%'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        if waiting >= minimum {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "reply or control did not reach the room lock; target row locking may deadlock"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn reply_notifications_follow_both_kick_and_owner_leave_commit_orders(pool: PgPool) {
    let store = Store::from_pool(pool);
    for (n, owner_leave, exit_first) in [
        (1, false, true),
        (2, true, true),
        (3, false, false),
        (4, true, false),
    ] {
        let sender = active(&store, &format!("sender-{n}"), n * 2).await;
        let target = active(&store, &format!("target-{n}"), n * 2 + 1).await;
        let parent = store
            .send(message(&target, "parent", "请讨论这个问题", &[]), limits())
            .await
            .unwrap();
        let before = store.public_snapshot().await.unwrap().last_seq;
        let mut blocker = store.pool().begin().await.unwrap();
        sqlx::query("SELECT singleton FROM room_state WHERE singleton FOR UPDATE")
            .execute(&mut *blocker)
            .await
            .unwrap();
        let make_exit = || {
            let store = store.clone();
            let target = target.clone();
            tokio::spawn(async move {
                if owner_leave {
                    store
                        .owner_leave(
                            &AgentContext {
                                agent_id: target.agent_id,
                                public_key: target.public_key.unwrap(),
                            },
                            "leave",
                        )
                        .await
                } else {
                    store.kick(&target.agent_id, "other").await
                }
            })
        };
        let make_reply = || {
            let store = store.clone();
            let mut input = message(&sender, "reply", "这是没有正文提及的回复", &[]);
            input.reply_to_event_id = Some(parent.event_id.clone());
            tokio::spawn(async move { store.send(input, limits()).await })
        };
        let (exit, reply) = if exit_first {
            let exit = make_exit();
            room_waiters(&store, 1).await;
            let reply = make_reply();
            room_waiters(&store, 2).await;
            (exit, reply)
        } else {
            let reply = make_reply();
            room_waiters(&store, 1).await;
            let exit = make_exit();
            room_waiters(&store, 2).await;
            (exit, reply)
        };
        blocker.commit().await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(2), async {
            exit.await.unwrap().unwrap();
            reply.await.unwrap().unwrap()
        })
        .await
        .expect("reply/exit must finish without acquiring the target agent row lock");
        let left: i64 =
            sqlx::query_scalar("SELECT seq FROM events WHERE kind='agent_left' AND agent_id=$1")
                .bind(&target.agent_id)
                .fetch_one(store.pool())
                .await
                .unwrap();
        if exit_first {
            assert!(reply.mentions.is_empty());
            assert!(left < reply.seq);
        } else {
            assert_eq!(reply.mentions, vec![target.agent_id.clone()]);
            assert!(reply.seq < left);
        }
        let snapshot = store.public_snapshot().await.unwrap();
        assert_eq!(snapshot.last_seq, before + 2);
        let event = snapshot
            .events
            .iter()
            .find(|event| event["event_id"] == reply.event_id)
            .unwrap();
        assert_eq!(
            event["reply_to"],
            json!({"seq":parent.seq,"event_id":parent.event_id})
        );
        assert_eq!(
            event["mentions"],
            if exit_first {
                json!([])
            } else {
                json!([{"agent_id":target.agent_id,"display_name":target.display_name}])
            }
        );
        let accepted: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM message_attempts WHERE agent_id=$1 AND client_message_id='reply' AND outcome='published' AND event_seq=$2",
        )
        .bind(&sender.agent_id)
        .bind(reply.seq)
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(accepted, 1);
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn reply_notifications_and_parent_links_follow_both_retraction_commit_orders(pool: PgPool) {
    let store = Store::from_pool(pool);
    for (n, retract_first) in [(1, true), (2, false)] {
        let sender = active(&store, &format!("sender-{n}"), n * 2).await;
        let target = active(&store, &format!("target-{n}"), n * 2 + 1).await;
        let parent = store
            .send(
                message(&target, "parent", "即将撤回的原消息", &[]),
                limits(),
            )
            .await
            .unwrap();
        let before = store.public_snapshot().await.unwrap().last_seq;
        let mut blocker = store.pool().begin().await.unwrap();
        sqlx::query("SELECT singleton FROM room_state WHERE singleton FOR UPDATE")
            .execute(&mut *blocker)
            .await
            .unwrap();
        let make_retraction = || {
            let store = store.clone();
            let event_id = parent.event_id.clone();
            tokio::spawn(async move { store.retract(&event_id, "other").await })
        };
        let make_reply = || {
            let store = store.clone();
            let mut input = message(&sender, "reply", "独立回复不随原文撤回", &[]);
            input.reply_to_event_id = Some(parent.event_id.clone());
            tokio::spawn(async move { store.send(input, limits()).await })
        };
        let (retraction, reply) = if retract_first {
            let retraction = make_retraction();
            room_waiters(&store, 1).await;
            let reply = make_reply();
            room_waiters(&store, 2).await;
            (retraction, reply)
        } else {
            let reply = make_reply();
            room_waiters(&store, 1).await;
            let retraction = make_retraction();
            room_waiters(&store, 2).await;
            (retraction, reply)
        };
        blocker.commit().await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(2), async {
            retraction.await.unwrap().unwrap();
            reply.await.unwrap().unwrap()
        })
        .await
        .expect("reply/retraction must finish within the room-lock deadline");
        let retracted: i64 = sqlx::query_scalar(
            "SELECT seq FROM events WHERE kind='event_retracted' AND target_seq=$1",
        )
        .bind(parent.seq)
        .fetch_one(store.pool())
        .await
        .unwrap();
        let snapshot = store.public_snapshot().await.unwrap();
        assert_eq!(snapshot.last_seq, before + 2);
        let tombstone = snapshot
            .events
            .iter()
            .find(|event| event["event_id"] == parent.event_id)
            .unwrap();
        assert_eq!(tombstone["retracted"], true);
        assert!(tombstone.get("text").is_none());
        assert_eq!(tombstone["mentions"], json!([]));
        let event = snapshot
            .events
            .iter()
            .find(|event| event["event_id"] == reply.event_id)
            .unwrap();
        assert_eq!(event["text"], "独立回复不随原文撤回");
        assert_eq!(event["retracted"], false);
        if retract_first {
            assert!(retracted < reply.seq);
            assert!(reply.mentions.is_empty());
            assert!(event["reply_to"].is_null());
            assert_eq!(event["mentions"], json!([]));
        } else {
            assert!(reply.seq < retracted);
            assert_eq!(reply.mentions, vec![target.agent_id.clone()]);
            assert_eq!(
                event["reply_to"],
                json!({"seq":parent.seq,"event_id":parent.event_id})
            );
            assert_eq!(
                event["mentions"],
                json!([{"agent_id":target.agent_id,"display_name":target.display_name}])
            );
        }
    }
}
