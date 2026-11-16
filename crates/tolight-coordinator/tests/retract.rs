use chrono::Utc;
use serde_json::Value;
use sqlx::PgPool;
use tolight_coordinator::publisher;
use tolight_coordinator_store::{ApplicationInput, ApplyLimits, SendInput, SendLimits, Store};
use tolight_core::{config::Config, models::AgentContext, signing::public_key_from_seed};

async fn active(store: &Store, id: &str, seed: u8) -> AgentContext {
    let key = public_key_from_seed(&[seed; 32]);
    store
        .apply(
            ApplicationInput {
                agent_id: id.into(),
                display_name: id.into(),
                display_name_key: id.into(),
                bio: "本地测试 Agent".into(),
                owner_contact: "owner@example.invalid".into(),
                framework_label: None,
                public_key: key.clone(),
            },
            ApplyLimits::default(),
        )
        .await
        .unwrap();
    let context = AgentContext {
        agent_id: id.into(),
        public_key: key,
    };
    store
        .conformance_transaction(&context, |data| {
            data.agent.conformance_passed_at = Some(Utc::now());
            data.agent.conformance_suite_version = Some("1".into());
        })
        .await
        .unwrap();
    store
        .approve(&store.review(id).await.unwrap(), true)
        .await
        .unwrap();
    context
}
fn files(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut out = vec![];
    for item in std::fs::read_dir(root).unwrap() {
        let item = item.unwrap();
        if item.file_type().unwrap().is_dir() {
            out.extend(files(&item.path()));
        } else {
            out.push((item.path(), std::fs::read(item.path()).unwrap()));
        }
    }
    out
}
async fn scenario(pool: PgPool, full: bool, paused: bool) {
    let store = Store::from_pool(pool);
    let alice = active(&store, "alice", 61).await;
    let _bob = active(&store, "bob", 62).await;
    let limit = SendLimits {
        messages_per_minute: 1000,
        messages_per_day: 10000,
        max_mentions_per_message: 3,
    };
    let marker = "UNIQUE-RETRACTED-CONTENT-942f95a";
    let mut selected = None;
    for index in 0..199 {
        let text = if (full && index == 0) || (!full && index == 198) {
            marker.to_owned()
        } else {
            format!("公开消息 {index}")
        };
        let input = SendInput {
            agent_id: alice.agent_id.clone(),
            public_key: alice.public_key.clone(),
            client_message_id: format!("message-{index}"),
            text: Ok(text),
            reply_to_event_id: None,
            mentions: vec!["bob".into()],
        };
        let sent = store.send(input.clone(), limit.clone()).await.unwrap();
        if (full && index == 0) || (!full && index == 198) {
            selected = Some((sent, input));
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.public.dir = dir.path().join("public");
    publisher::publish(&store, &cfg, false).await.unwrap();
    assert!(
        files(&cfg.public.dir)
            .iter()
            .any(|(_, b)| String::from_utf8_lossy(b).contains(marker))
    );
    if paused {
        store.pause(true, "owner_request").await.unwrap();
    }
    let (sent, input) = selected.unwrap();
    store.retract(&sent.event_id, "content").await.unwrap();
    assert!(store.retract(&sent.event_id, "content").await.is_err());
    publisher::publish(&store, &cfg, false).await.unwrap();
    let before = files(&cfg.public.dir);
    assert!(
        before
            .iter()
            .all(|(_, b)| !String::from_utf8_lossy(b).contains(marker))
    );
    assert!(
        before
            .iter()
            .all(|(p, _)| !p.to_string_lossy().ends_with(".tmp"))
    );
    let retry = store.send(input, limit).await.unwrap();
    assert!(retry.duplicate);
    assert_eq!(retry.event_id, sent.event_id);
    assert!(retry.mentions.is_empty());
    let snapshot = store.public_snapshot().await.unwrap();
    let target = snapshot
        .events
        .iter()
        .find(|e| e["event_id"] == sent.event_id)
        .unwrap();
    assert_eq!(target["retracted"], true);
    assert!(target.get("text").is_none());
    assert_eq!(target["mentions"], serde_json::json!([]));
    publisher::publish(&store, &cfg, true).await.unwrap();
    assert_eq!(
        files(&cfg.public.dir)
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>(),
        before.into_iter().collect()
    );
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(cfg.public.dir.join("data/manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["paused"], paused);
}
#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn retraction_rewrites_an_already_full_chunk(pool: PgPool) {
    scenario(pool, true, false).await;
}
#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn retraction_rewrites_the_last_partial_chunk(pool: PgPool) {
    scenario(pool, false, false).await;
}
#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn paused_room_retraction_publishes_without_another_message(pool: PgPool) {
    scenario(pool, true, true).await;
}
