use serde_json::{Value, json};
use sqlx::PgPool;
use std::{
    future::Future,
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, watch};
use tolight_core::config::{Config, PublicConfig};
use tolight_reference_client::{
    Generator, ReferenceClient, ReferenceConfig, RunOptions, SystemClock, keygen,
};

#[derive(Default)]
struct Conversation {
    prompts: Mutex<Vec<String>>,
    became_silent: Notify,
}
impl Generator for Conversation {
    fn generate<'a>(
        &'a self,
        context: &'a str,
        index: u64,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        self.prompts.lock().unwrap().push(context.into());
        Box::pin(async move {
            if index == 0 {
                Ok("建议先做故障注入，再根据重试结果核对幂等记录。".into())
            } else {
                self.became_silent.notify_one();
                Ok("[SILENT]".into())
            }
        })
    }
}

fn config(root: &Path, origin: &str, id: &str) -> ReferenceConfig {
    let mut config = ReferenceConfig {
        base_url: format!("{origin}/participant/v1/"),
        key_path: root.join(format!("{id}.key")),
        state_path: root.join(format!("{id}.json")),
        lines_path: root.join(format!("{id}.txt")),
        min_speaking_interval_seconds: 0,
        long_poll_seconds: 1,
        ..ReferenceConfig::default()
    };
    config.application.agent_id = id.into();
    config.application.display_name = format!("Reply {id}");
    std::fs::write(&config.lines_path, "[SILENT]\n").unwrap();
    keygen(&config).unwrap();
    config
}

fn trigger_ids(prompt: &str) -> Vec<String> {
    let encoded = prompt
        .split_once("触发这次调用的消息\n")
        .unwrap()
        .1
        .split_once("\n主动提示\n")
        .unwrap()
        .0;
    serde_json::from_str::<Vec<Value>>(encoded)
        .unwrap()
        .into_iter()
        .map(|event| event["event_id"].as_str().unwrap().into())
        .collect()
}

fn reply_target(prompt: &str) -> String {
    let encoded = prompt
        .split_once("本次回复目标\n")
        .unwrap()
        .1
        .split_once("\n对话约定\n")
        .unwrap()
        .0;
    serde_json::from_str::<Value>(encoded).unwrap()["event_id"]
        .as_str()
        .unwrap()
        .into()
}

#[sqlx::test(migrations = "../../crates/tolight-coordinator-store/migrations")]
async fn reply_without_at_triggers_once_and_silent_closing_does_not_start_a_loop(pool: PgPool) {
    let temp = tempfile::tempdir().unwrap();
    let server = tolight_coordinator::testing::start(
        pool,
        Config {
            messages_per_minute: 100,
            requests_per_minute: 10000,
            public: PublicConfig {
                dir: temp.path().join("public"),
                demo_banner: true,
            },
            ..Config::default()
        },
    )
    .await
    .unwrap();
    let alice_config = config(temp.path(), &server.origin, "reply-alice");
    let bob_config = config(temp.path(), &server.origin, "reply-bobby");
    let conversation = Arc::new(Conversation::default());
    let alice = ReferenceClient::open(alice_config).unwrap();
    let mut bob = ReferenceClient::with_dependencies(
        bob_config.clone(),
        Arc::new(SystemClock),
        conversation.clone(),
    )
    .unwrap();
    for client in [&alice, &bob] {
        client.apply().await.unwrap();
        let review = server
            .room
            .store()
            .review(&client.config.application.agent_id)
            .await
            .unwrap();
        server.room.store().approve(&review, false).await.unwrap();
    }
    let root = bob
        .request(
            "POST",
            "messages",
            &[],
            Some(&json!({"client_message_id":"topic","text":"我正在比较故障注入和人工验证。"})),
        )
        .await
        .unwrap();
    // Establish a real public cursor. The root and join events are context,
    // never fresh triggers, including when run starts below.
    let page = bob.request("GET", "events", &[], None).await.unwrap();
    let baseline_cursor = page["next_cursor"].as_str().unwrap().to_owned();
    bob.state.cursor = Some(baseline_cursor.clone());
    bob.state.save(&bob_config.state_path).unwrap();
    let question = json!({"client_message_id":"question","text":"哪种方法更适合先验证重试的幂等性？","reply_to_event_id":root["event_id"]});
    let directed = alice
        .request("POST", "messages", &[], Some(&question))
        .await
        .unwrap();
    assert_eq!(directed["mentions"], json!(["reply-bobby"]));
    let replay = alice
        .request("POST", "messages", &[], Some(&question))
        .await
        .unwrap();
    assert_eq!(replay["duplicate"], true);
    assert_eq!(replay["event_id"], directed["event_id"]);
    let (_stop, receiver) = watch::channel(false);
    let first = bob
        .run(
            receiver,
            RunOptions {
                max_messages: Some(1),
                run_for_seconds: Some(5),
            },
        )
        .await
        .unwrap();
    assert_eq!(first.messages, 1);
    assert_eq!(conversation.prompts.lock().unwrap().len(), 1);
    let snapshot = server.room.store().public_snapshot().await.unwrap();
    let answer = snapshot.events.last().unwrap();
    assert_eq!(answer["reply_to"]["event_id"], directed["event_id"]);
    assert_eq!(answer["mentions"][0]["agent_id"], "reply-alice");
    assert!(!answer["text"].as_str().unwrap().contains('@'));
    let closing = alice
        .request(
            "POST",
            "messages",
            &[],
            Some(&json!({"client_message_id":"closing","text":"明白了，谢谢，这个问题先到这里。","reply_to_event_id":answer["event_id"]})),
        )
        .await
        .unwrap();
    assert_eq!(closing["mentions"], json!(["reply-bobby"]));
    // A replayed page must not call the generator for the old question again.
    // A fresh directed closing is still considered, but [SILENT] is final and
    // durable: there is no automatic acknowledgement sent back to Alice.
    bob.state.cursor = Some(baseline_cursor);
    bob.state.save(&bob_config.state_path).unwrap();
    let (stop, receiver) = watch::channel(false);
    let task = tokio::spawn(async move {
        let report = bob.run(receiver, RunOptions::default()).await.unwrap();
        (bob, report)
    });
    tokio::time::timeout(
        Duration::from_secs(5),
        conversation.became_silent.notified(),
    )
    .await
    .expect("no-@ closing reply was not routed to the client");
    stop.send(true).unwrap();
    let (bob, second) = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.messages, 0);
    assert_eq!(bob.state.calls, 2);
    assert!(bob.state.pending.is_none());
    let persisted = tolight_reference_client::LocalState::load(&bob_config.state_path).unwrap();
    assert_eq!(persisted.calls, 2);
    assert!(persisted.pending.is_none());
    for event in [&directed, &closing] {
        assert!(
            bob.state
                .processed
                .contains(event["event_id"].as_str().unwrap())
        );
        assert!(
            persisted
                .processed
                .contains(event["event_id"].as_str().unwrap())
        );
    }
    {
        let prompts = conversation.prompts.lock().unwrap();
        assert_eq!(prompts.len(), 2);
        assert_eq!(
            trigger_ids(&prompts[0]),
            [directed["event_id"].as_str().unwrap()]
        );
        assert_eq!(reply_target(&prompts[0]), directed["event_id"]);
        assert_eq!(
            trigger_ids(&prompts[1]),
            [closing["event_id"].as_str().unwrap()]
        );
        assert_eq!(reply_target(&prompts[1]), closing["event_id"]);
    }
    let final_snapshot = server.room.store().public_snapshot().await.unwrap();
    assert_eq!(
        final_snapshot
            .events
            .iter()
            .filter(|event| event["kind"] == "agent_message")
            .count(),
        4
    );
    server.shutdown().await.unwrap();
}
