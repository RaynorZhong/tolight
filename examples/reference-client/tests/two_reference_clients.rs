use sqlx::PgPool;
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::sync::Notify;
use tolight_core::config::{Config, PublicConfig};
use tolight_reference_client::{
    Generator, ReferenceClient, ReferenceConfig, RunOptions, ScriptGenerator, SystemClock, keygen,
};

struct PublicStartGate {
    script: ScriptGenerator,
    ready: Arc<Notify>,
}
impl Generator for PublicStartGate {
    fn generate<'a>(
        &'a self,
        context: &'a str,
        index: u64,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            // The sandbox response (index 0) proceeds normally. Delay the first
            // public topic until both clients have consumed their public reset.
            if index == 1 {
                self.ready.notified().await;
            }
            self.script.generate(context, index).await
        })
    }
}

#[sqlx::test(migrations = "../../crates/tolight-coordinator-store/migrations")]
async fn two_script_instances_apply_wait_approve_and_chat_twenty_messages(pool: PgPool) {
    let temp = tempfile::tempdir().unwrap();
    let cfg = Config {
        messages_per_minute: 100,
        messages_per_day: 1000,
        requests_per_minute: 10000,
        conformance_retry_after: 1,
        conformance_reply_window: 30,
        public: PublicConfig {
            dir: temp.path().join("public"),
            demo_banner: true,
        },
        ..Config::default()
    };
    let server = tolight_coordinator::testing::start(pool, cfg)
        .await
        .unwrap();
    let make = |id: &str, other: &str, proactive| {
        let mut cfg = ReferenceConfig {
            base_url: format!("{}/participant/v1/", server.origin),
            key_path: temp.path().join(format!("{id}.key")),
            state_path: temp.path().join(format!("{id}.json")),
            lines_path: temp.path().join(format!("{id}.txt")),
            min_speaking_interval_seconds: 0,
            long_poll_seconds: 1,
            proactive_interval_seconds: proactive,
            ..ReferenceConfig::default()
        };
        cfg.application.agent_id = id.into();
        cfg.application.display_name = format!("Script {id}");
        std::fs::write(
            &cfg.lines_path,
            format!("@{other} 我们可以自由接话，没有轮次。\n@{other} 上下文让回应更具体。\n"),
        )
        .unwrap();
        keygen(&cfg).unwrap();
        cfg
    };
    let a_cfg = make("script-alice", "script-bob", Some(1));
    let b_cfg = make("script-bob", "script-alice", None);
    let ready = Arc::new(Notify::new());
    let mut a = ReferenceClient::with_dependencies(
        a_cfg.clone(),
        Arc::new(SystemClock),
        Arc::new(PublicStartGate {
            script: ScriptGenerator::load(&a_cfg.lines_path).unwrap(),
            ready: ready.clone(),
        }),
    )
    .unwrap();
    let mut b = ReferenceClient::open(b_cfg.clone()).unwrap();
    a.apply().await.unwrap();
    b.apply().await.unwrap();
    let (a_stop, a_rx) = tokio::sync::watch::channel(false);
    let (b_stop, b_rx) = tokio::sync::watch::channel(false);
    let a_task = tokio::spawn(async move {
        a.run(
            a_rx,
            RunOptions {
                max_messages: Some(10),
                run_for_seconds: Some(30),
            },
        )
        .await
    });
    let b_task = tokio::spawn(async move {
        b.run(
            b_rx,
            RunOptions {
                max_messages: Some(10),
                run_for_seconds: Some(30),
            },
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        server
            .room
            .store()
            .public_snapshot()
            .await
            .unwrap()
            .events
            .is_empty()
    );
    for id in ["script-alice", "script-bob"] {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        let review = loop {
            let review = server.room.store().review(id).await.unwrap();
            if review.conformance_passed_at.is_some() {
                break review;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "client did not pass all conformance checks: {id}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        server.room.store().approve(&review, true).await.unwrap();
    }
    server.room.notify_change();
    // Never reply to messages inside cursor_reset. Check the opaque cursor via
    // the actual API instead of relying on approval order or a timing sleep.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    loop {
        let mut both_public = true;
        for config in [&a_cfg, &b_cfg] {
            let state = tolight_reference_client::LocalState::load(&config.state_path).unwrap();
            let Some(cursor) = state.cursor else {
                both_public = false;
                continue;
            };
            let probe = ReferenceClient::open(config.clone()).unwrap();
            let page = probe
                .request(
                    "GET",
                    "events",
                    &[
                        ("after", cursor),
                        ("limit", "1".into()),
                        ("wait", "0".into()),
                    ],
                    None,
                )
                .await
                .unwrap();
            both_public &= page["timeline"] == "public" && page["cursor_reset"] == false;
        }
        if both_public {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "public cursors were not established"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    ready.notify_one();
    let a_report = tokio::time::timeout(Duration::from_secs(20), a_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let b_report = tokio::time::timeout(Duration::from_secs(20), b_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(a_report.messages, 10);
    assert_eq!(a_report.sandbox_replies, 1);
    assert_eq!(b_report.sandbox_replies, 1);
    assert_eq!(b_report.messages, 10);
    drop((a_stop, b_stop));
    let snapshot = server.room.store().public_snapshot().await.unwrap();
    assert_eq!(snapshot.events.len(), 22);
    for (index, event) in snapshot.events.iter().enumerate() {
        assert_eq!(event["seq"], (index + 1) as u64);
    }
    let messages: Vec<_> = snapshot
        .events
        .iter()
        .filter(|event| event["kind"] == "agent_message")
        .collect();
    assert_eq!(messages.len(), 20);
    assert!(messages.iter().any(|event| !event["reply_to"].is_null()));
    assert!(
        messages
            .iter()
            .all(|event| !event["mentions"].as_array().unwrap().is_empty())
    );
    for cfg in [&a_cfg, &b_cfg] {
        let state = tolight_reference_client::LocalState::load(&cfg.state_path).unwrap();
        assert!(state.cursor.is_some());
        assert_eq!(state.calls, 11);
        assert!(state.pending.is_none());
    }
    server.shutdown().await.unwrap();
}
