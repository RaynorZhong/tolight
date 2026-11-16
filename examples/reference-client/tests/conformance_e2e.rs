use serde_json::Value;
use sqlx::PgPool;
use std::time::Duration;
use tolight_core::{
    config::{Config, PublicConfig},
    participant::decode_cursor,
};
use tolight_reference_client::{
    LocalState, ReferenceClient, ReferenceConfig, RunOptions, keygen, model::ModelConfig,
};

#[sqlx::test(migrations = "../../crates/tolight-coordinator-store/migrations")]
async fn command_model_passes_three_checks_then_switches_in_same_process(pool: PgPool) {
    let temp = tempfile::tempdir().unwrap();
    let config = Config {
        conformance_retry_after: 1,
        conformance_reply_window: 30,
        requests_per_minute: 10000,
        public: PublicConfig {
            dir: temp.path().join("public"),
            demo_banner: true,
        },
        ..Config::default()
    };
    let server = tolight_coordinator::testing::start(pool, config)
        .await
        .unwrap();
    let capture = temp.path().join("model-inputs.jsonl");
    let mandate = temp.path().join("mandate.txt");
    std::fs::write(&mandate, "公开职责：参与测试聊天室。").unwrap();
    let mut cfg = ReferenceConfig {
        base_url: format!("{}/participant/v1/", server.origin),
        key_path: temp.path().join("candidate.key"),
        state_path: temp.path().join("candidate-state.json"),
        mode: "model".into(),
        model: Some(ModelConfig {
            command: vec![
                env!("CARGO_BIN_EXE_reference-fixture").into(),
                "conformance".into(),
                capture.to_string_lossy().into_owned(),
            ],
            mandate_path: mandate,
            max_output_tokens: 91,
            timeout_seconds: 5,
            ..ModelConfig::default()
        }),
        min_speaking_interval_seconds: 0,
        long_poll_seconds: 1,
        ..ReferenceConfig::default()
    };
    cfg.application.agent_id = "candidate".into();
    cfg.application.display_name = "Candidate".into();
    keygen(&cfg).unwrap();
    let mut client = ReferenceClient::open(cfg.clone()).unwrap();
    client.apply().await.unwrap();
    let original = server.room.store().review("candidate").await.unwrap();
    assert!(server.room.store().approve(&original, true).await.is_err());
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        client
            .run(
                rx,
                RunOptions {
                    run_for_seconds: Some(20),
                    max_messages: Some(1),
                },
            )
            .await
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    let review = loop {
        let review = server.room.store().review("candidate").await.unwrap();
        if review.conformance_passed_at.is_some() {
            break review;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "model did not pass sandbox"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(review.application_revision > original.application_revision);
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
    let inputs = std::fs::read_to_string(&capture).unwrap();
    assert_eq!(
        inputs.lines().count(),
        1,
        "429 must reuse the generated output"
    );
    assert!(inputs.contains("seq\\\":4"));
    server.room.store().approve(&review, true).await.unwrap();
    server.room.notify_change();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let state = LocalState::load(&cfg.state_path).unwrap();
        // White-box assertion only; the running client never parses its opaque cursor.
        if state
            .cursor
            .as_deref()
            .and_then(decode_cursor)
            .is_some_and(|cursor| cursor.timeline == "public")
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "same process did not switch to public"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut other = ReferenceConfig {
        base_url: cfg.base_url.clone(),
        key_path: temp.path().join("other.key"),
        state_path: temp.path().join("other.json"),
        lines_path: temp.path().join("other.txt"),
        min_speaking_interval_seconds: 0,
        ..ReferenceConfig::default()
    };
    other.application.agent_id = "other-agent".into();
    other.application.display_name = "Other".into();
    std::fs::write(&other.lines_path, "@candidate 这是一条新的公共提及。\n").unwrap();
    keygen(&other).unwrap();
    let mut other = ReferenceClient::open(other).unwrap();
    other.apply().await.unwrap();
    // A separate low-level fixture authorizes only the sender used by this test.
    let other_review = server.room.store().review("other-agent").await.unwrap();
    server
        .room
        .store()
        .approve(&other_review, false)
        .await
        .unwrap();
    assert!(other.say(None).await.unwrap());
    let report = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(report.sandbox_replies, 1);
    assert_eq!(report.messages, 1);
    drop(stop);
    let state = LocalState::load(&cfg.state_path).unwrap();
    assert_eq!(state.calls, 2);
    let inputs = std::fs::read_to_string(capture).unwrap();
    assert_eq!(inputs.lines().count(), 2);
    for input in inputs.lines() {
        let request: Value = serde_json::from_str(input).unwrap();
        assert_eq!(request["max_output_tokens"], 91);
        assert!(request.get("tools").is_none());
        assert!(!input.contains("owner_contact"));
    }
    let public = server.room.store().public_snapshot().await.unwrap();
    assert_eq!(
        public
            .events
            .iter()
            .filter(|event| event["kind"] == "agent_message")
            .count(),
        2
    );
    assert!(
        public
            .events
            .iter()
            .all(|event| event["agent"]["agent_id"] != "examiner")
    );
    server.shutdown().await.unwrap();
}
