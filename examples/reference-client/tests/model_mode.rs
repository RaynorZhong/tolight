use serde_json::Value;
use sqlx::PgPool;
use std::{
    path::Path,
    time::{Duration, Instant},
};
use tolight_core::config::{Config, PublicConfig};
use tolight_reference_client::{
    Generator, ReferenceClient, ReferenceConfig, keygen,
    model::{CommandGenerator, ModelConfig},
};

fn model(directory: &Path, mode: &str) -> ModelConfig {
    let mandate = directory.join("mandate.txt");
    std::fs::write(&mandate, "公开职责：只讨论测试上下文，不带私人记忆。\n").unwrap();
    ModelConfig {
        command: vec![env!("CARGO_BIN_EXE_reference-fixture").into(), mode.into()],
        mandate_path: mandate,
        ..ModelConfig::default()
    }
}

#[tokio::test]
async fn one_shot_command_gets_public_context_tokens_timeout_and_only_explicit_credential() {
    let temp = tempfile::tempdir().unwrap();
    let capture = temp.path().join("request.json");
    let mut cfg = model(temp.path(), "capture");
    cfg.command.push(capture.to_string_lossy().into_owned());
    cfg.max_output_tokens = 73;
    cfg.timeout_seconds = 4;
    cfg.secret_cmd = Some(vec![
        "/bin/sh".into(),
        "-c".into(),
        "printf 'local-test-credential\\n'".into(),
    ]);
    let generator = CommandGenerator::new(cfg).unwrap();
    for _ in 0..2 {
        let output = generator
            .generate(
                "固定提示\n自己的身份\n{\"agent_id\":\"test\"}\n公开上下文仅本次",
                0,
            )
            .await
            .unwrap();
        assert_eq!(output, "模型生成的公开文本\n");
    }
    let input: Value = serde_json::from_slice(&std::fs::read(capture).unwrap()).unwrap();
    assert_eq!(input["request"]["max_output_tokens"], 73);
    assert_eq!(input["request"]["timeout_seconds"], 4);
    assert_eq!(
        input["request"]["conversation_rules"],
        tolight_reference_client::conversation_rules()
    );
    assert!(
        input["request"]["notice"]
            .as_str()
            .unwrap()
            .contains("不可信")
    );
    assert!(
        input["request"]["mandate"]
            .as_str()
            .unwrap()
            .contains("公开职责")
    );
    assert!(
        input["request"]["input"]
            .as_str()
            .unwrap()
            .contains("agent_id")
    );
    assert!(input["request"].get("tools").is_none());
    assert!(input["request"].get("memory").is_none());
    assert!(input["request"].get("session").is_none());
    assert!(
        !input["request"]
            .to_string()
            .contains("local-test-credential")
    );
    assert_eq!(input["credential_length"], 21);
    // env_clear must leave only the explicitly requested credential.
    assert!(
        input["env_names"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "TOLIGHT_MODEL_CREDENTIAL")
    );
}

#[tokio::test]
async fn timeout_failure_and_oversize_output_are_bounded_and_do_not_echo_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let mut cfg = model(temp.path(), "timeout");
    cfg.timeout_seconds = 1;
    let started = Instant::now();
    let error = CommandGenerator::new(cfg)
        .unwrap()
        .generate("test", 0)
        .await
        .unwrap_err();
    assert!(error.contains("超时"));
    assert!(started.elapsed() < Duration::from_secs(3));
    let cfg = model(temp.path(), "failure");
    let error = CommandGenerator::new(cfg)
        .unwrap()
        .generate("test", 0)
        .await
        .unwrap_err();
    assert!(!error.contains("local-test-credential"));
    let cfg = model(temp.path(), "oversize");
    assert!(
        CommandGenerator::new(cfg)
            .unwrap()
            .generate("test", 0)
            .await
            .unwrap_err()
            .contains("上限")
    );
}

#[test]
fn model_configuration_requires_public_mandate_and_rejects_plain_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let valid = model(temp.path(), "noop");
    for field in ["api_key", "token", "credential", "password"] {
        let value = format!("mode='model'\n[model]\n{field}='forbidden'\n");
        assert!(toml::from_str::<ReferenceConfig>(&value).is_err());
    }
    let mut bad = ReferenceConfig {
        mode: "model".into(),
        ..ReferenceConfig::default()
    };
    assert!(bad.validate().is_err());
    bad.model = Some(valid.clone());
    assert!(bad.validate().is_ok());
    for tokens in [0, 8193] {
        let mut cfg = valid.clone();
        cfg.max_output_tokens = tokens;
        assert!(cfg.validate().is_err());
    }
    let mut cfg = valid.clone();
    cfg.credential_env = Some("TOKEN".into());
    cfg.secret_cmd = Some(vec!["/bin/true".into()]);
    assert!(cfg.validate().is_err());
    let mut cfg = valid.clone();
    cfg.command[0] = "relative".into();
    assert!(cfg.validate().is_err());
    let mut cfg = valid;
    cfg.mandate_path = temp.path().join("absent");
    assert!(CommandGenerator::new(cfg).is_err());
}

#[sqlx::test(migrations = "../../crates/tolight-coordinator-store/migrations")]
async fn model_silent_failure_and_timeout_consume_budget_without_public_text(pool: PgPool) {
    let temp = tempfile::tempdir().unwrap();
    let server = tolight_coordinator::testing::start(
        pool,
        Config {
            public: PublicConfig {
                dir: temp.path().join("public"),
                demo_banner: true,
            },
            ..Config::default()
        },
    )
    .await
    .unwrap();
    let mut cfg = ReferenceConfig {
        base_url: format!("{}/participant/v1/", server.origin),
        key_path: temp.path().join("client.key"),
        state_path: temp.path().join("state.json"),
        mode: "model".into(),
        model: Some(model(temp.path(), "control")),
        min_speaking_interval_seconds: 0,
        daily_call_budget: 3,
        ..ReferenceConfig::default()
    };
    let control = temp.path().join("model-mode.txt");
    std::fs::write(&control, "silent").unwrap();
    cfg.model
        .as_mut()
        .unwrap()
        .command
        .push(control.to_string_lossy().into_owned());
    cfg.model.as_mut().unwrap().timeout_seconds = 1;
    cfg.application.agent_id = "model-agent".into();
    cfg.application.display_name = "Model Agent".into();
    keygen(&cfg).unwrap();
    let mut client = ReferenceClient::open(cfg.clone()).unwrap();
    client.apply().await.unwrap();
    let review = server.room.store().review("model-agent").await.unwrap();
    server.room.store().approve(&review, false).await.unwrap();
    assert!(!client.say(Some("不能直接公开的提示".into())).await.unwrap());
    assert_eq!(client.state.calls, 1);
    std::fs::write(&control, "failure").unwrap();
    assert!(!client.say(None).await.unwrap());
    assert_eq!(client.state.calls, 2);
    std::fs::write(&control, "timeout").unwrap();
    assert!(!client.say(None).await.unwrap());
    assert_eq!(client.state.calls, 3);
    assert!(!client.say(None).await.unwrap());
    assert_eq!(client.state.calls, 3);
    let snapshot = server.room.store().public_snapshot().await.unwrap();
    assert_eq!(snapshot.events.len(), 1);
    assert_eq!(snapshot.events[0]["kind"], "agent_joined");
    assert_eq!(
        tolight_reference_client::LocalState::load(&cfg.state_path)
            .unwrap()
            .calls,
        3
    );
    server.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn timeout_and_completed_parent_never_leave_model_descendants_running() {
    for behavior in ["wait", "exit-open", "exit-closed"] {
        let temp = tempfile::tempdir().unwrap();
        let mut cfg = model(temp.path(), "descendant");
        cfg.command
            .extend([temp.path().to_string_lossy().into_owned(), behavior.into()]);
        cfg.timeout_seconds = 1;
        cfg.secret_cmd = Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf local-test-credential".into(),
        ]);
        let started = Instant::now();
        let error = CommandGenerator::new(cfg)
            .unwrap()
            .generate("public input", 0)
            .await
            .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{behavior}: {error}"
        );
        assert!(
            temp.path().join("ready").is_file(),
            "the owned descendant must actually start"
        );
        assert!(!error.contains("local-test-credential"));
        assert!(!error.contains("parent output"));
        std::fs::write(
            temp.path().join("release"),
            "release after invocation returned",
        )
        .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !temp.path().join("late-effect").exists(),
            "{behavior} left a descendant able to write after cleanup"
        );
    }
}
