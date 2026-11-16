use sqlx::PgPool;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
use tolight_core::config::{Config, PublicConfig};
use tolight_reference_client::{ReferenceClient, ReferenceConfig, keygen};

fn configuration(root: &Path, origin: &str, id: &str) -> ReferenceConfig {
    let mut cfg = ReferenceConfig {
        base_url: format!("{origin}/participant/v1/"),
        key_path: root.join(format!("{id}.key")),
        state_path: root.join(format!("{id}.json")),
        lines_path: root.join(format!("{id}.txt")),
        min_speaking_interval_seconds: 0,
        ..ReferenceConfig::default()
    };
    cfg.application.agent_id = id.into();
    cfg.application.display_name = format!("Test {id}");
    std::fs::write(&cfg.lines_path, "本地脚本生成的公开发言。\n").unwrap();
    cfg
}
fn save(root: &Path, name: &str, cfg: &ReferenceConfig) -> PathBuf {
    let path = root.join(format!("{name}.toml"));
    std::fs::write(&path, toml::to_string(cfg).unwrap()).unwrap();
    path
}
async fn command(path: &Path, yes: bool) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_reference-client"));
    command.args(["--config", path.to_str().unwrap(), "leave"]);
    if yes {
        command.arg("--yes");
    }
    command.stdin(Stdio::null()).output().await.unwrap()
}

#[sqlx::test(migrations = "../../crates/tolight-coordinator-store/migrations")]
async fn pending_and_rejected_withdrawal_free_identity_and_cli_requires_confirmation(pool: PgPool) {
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
    let cfg = configuration(temp.path(), &server.origin, "withdraw-agent");
    keygen(&cfg).unwrap();
    let client = ReferenceClient::open(cfg.clone()).unwrap();
    client.apply().await.unwrap();
    let original = server.room.store().review("withdraw-agent").await.unwrap();
    let path = save(temp.path(), "withdraw-agent", &cfg);
    let refused = command(&path, false).await;
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--yes"));
    assert_eq!(client.status().await.unwrap()["state"], "PENDING");
    let approved = command(&path, true).await;
    assert!(
        approved.status.success(),
        "{}",
        String::from_utf8_lossy(&approved.stderr)
    );
    assert!(String::from_utf8_lossy(&approved.stdout).contains("撤回申请"));
    assert_eq!(client.status().await.unwrap_err().exit_code(), 5);
    assert!(
        server
            .room
            .store()
            .agent("withdraw-agent")
            .await
            .unwrap()
            .is_none()
    );
    client.apply().await.unwrap();
    let current = server.room.store().review("withdraw-agent").await.unwrap();
    assert_ne!(original.application_id, current.application_id);
    assert_eq!(current.application_revision, 1);
    server
        .room
        .store()
        .reject("withdraw-agent", "other")
        .await
        .unwrap();
    assert_eq!(client.status().await.unwrap()["state"], "REJECTED");
    client.leave_with_mode("withdraw").await.unwrap();
    assert_eq!(client.status().await.unwrap_err().exit_code(), 5);
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
    server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../../crates/tolight-coordinator-store/migrations")]
async fn stale_withdrawal_never_becomes_leave_and_active_leave_keeps_public_history(pool: PgPool) {
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
    let cfg = configuration(temp.path(), &server.origin, "leaving-agent");
    keygen(&cfg).unwrap();
    let mut client = ReferenceClient::open(cfg.clone()).unwrap();
    client.apply().await.unwrap();
    // An Owner decided on withdraw while pending, but approval won the race.
    let review = server.room.store().review("leaving-agent").await.unwrap();
    server.room.store().approve(&review, false).await.unwrap();
    let error = client.leave_with_mode("withdraw").await.unwrap_err();
    assert_eq!(error.code, "invalid_state");
    assert!(error.detail.contains("未自动改变"));
    assert_eq!(client.status().await.unwrap()["state"], "ACTIVE");
    assert!(client.say(None).await.unwrap());
    server
        .room
        .store()
        .mute("leaving-agent", None, "other")
        .await
        .unwrap();
    let path = save(temp.path(), "leaving-agent", &cfg);
    let refused = command(&path, false).await;
    assert!(!refused.status.success());
    let done = command(&path, true).await;
    assert!(
        done.status.success(),
        "{}",
        String::from_utf8_lossy(&done.stderr)
    );
    assert!(String::from_utf8_lossy(&done.stdout).contains("退出不可撤销"));
    assert_eq!(client.status().await.unwrap_err().exit_code(), 5);
    assert_eq!(
        client
            .leave_with_mode("leave")
            .await
            .unwrap_err()
            .exit_code(),
        5
    );
    let agent = server
        .room
        .store()
        .agent("leaving-agent")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(agent.state, "LEFT");
    assert!(!agent.muted_indefinitely);
    assert!(agent.public_key.is_none());
    let public = server.room.store().public_snapshot().await.unwrap();
    assert_eq!(
        public
            .events
            .iter()
            .filter(|event| event["kind"] == "agent_message")
            .count(),
        1
    );
    assert_eq!(public.events.last().unwrap()["kind"], "agent_left");
    server.shutdown().await.unwrap();
}
