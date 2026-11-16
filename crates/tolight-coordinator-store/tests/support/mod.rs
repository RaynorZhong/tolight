#![allow(dead_code)]
use tolight_coordinator_store::{
    Agent, ApplicationInput, ApplyLimits, SendInput, SendLimits, Store,
};
use tolight_core::signing::public_key_from_seed;

pub fn input(id: &str, seed: u8) -> ApplicationInput {
    ApplicationInput {
        agent_id: id.into(),
        display_name: format!("Name {id}"),
        display_name_key: format!("name {id}"),
        bio: "只用于测试的公开简介".into(),
        owner_contact: "owner@example.invalid".into(),
        framework_label: Some("script".into()),
        public_key: public_key_from_seed(&[seed; 32]),
    }
}
pub async fn pending(store: &Store, id: &str, seed: u8) -> Agent {
    store
        .apply(input(id, seed), ApplyLimits::default())
        .await
        .unwrap()
        .agent
}
pub async fn active(store: &Store, id: &str, seed: u8) -> Agent {
    let agent = pending(store, id, seed).await;
    store.approve(&agent, false).await.unwrap()
}
pub fn limits() -> SendLimits {
    SendLimits {
        messages_per_minute: 1000,
        messages_per_day: 10000,
        max_mentions_per_message: 3,
    }
}
pub fn message(agent: &Agent, id: impl Into<String>, text: &str, mentions: &[&str]) -> SendInput {
    SendInput {
        agent_id: agent.agent_id.clone(),
        public_key: agent.public_key.clone().unwrap(),
        client_message_id: id.into(),
        text: Ok(text.into()),
        reply_to_event_id: None,
        mentions: mentions.iter().map(|id| (*id).to_string()).collect(),
    }
}
pub async fn count(store: &Store, table: &str) -> i64 {
    // Tables are supplied only by the test itself, never by a request.
    sqlx::query_scalar(match table {
        "agents" => "SELECT COUNT(*) FROM agents",
        "agent_keys" => "SELECT COUNT(*) FROM agent_keys",
        "conformance_runs" => "SELECT COUNT(*) FROM conformance_runs",
        "events" => "SELECT COUNT(*) FROM events",
        "message_attempts" => "SELECT COUNT(*) FROM message_attempts",
        _ => panic!("unknown test table"),
    })
    .fetch_one(store.pool())
    .await
    .unwrap()
}
