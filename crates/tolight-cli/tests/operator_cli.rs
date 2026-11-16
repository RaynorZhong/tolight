use sqlx::{ConnectOptions, PgPool};
use std::{
    path::Path,
    process::{Command, Output, Stdio},
};
use tolight_coordinator_store::{ApplicationInput, ApplyLimits, Store};
use tolight_core::signing::public_key_from_seed;

fn cli(config: &Path, url: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tolight"))
        .args(["--config", config.to_str().unwrap()])
        .args(args)
        .env("DATABASE_URL", url)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn attempts_is_read_only_even_when_an_application_has_expired(pool: PgPool) {
    let store = Store::from_pool(pool.clone());
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("coordinator.toml");
    std::fs::write(
        &config,
        "[database]\nsecret_cmd=['/usr/bin/printenv','DATABASE_URL']\n",
    )
    .unwrap();
    store
        .apply(
            ApplicationInput {
                agent_id: "expired-agent".into(),
                display_name: "Expired Agent".into(),
                display_name_key: "expired agent".into(),
                bio: "只读检查".into(),
                owner_contact: "owner@example.invalid".into(),
                framework_label: None,
                public_key: public_key_from_seed(&[121; 32]),
            },
            ApplyLimits::default(),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE agents SET registered_at=clock_timestamp()-INTERVAL '15 days'")
        .execute(&pool)
        .await
        .unwrap();
    let url = pool.connect_options().to_url_lossy().to_string();
    for arguments in [
        vec!["operator", "attempts", "expired-agent"],
        vec![
            "operator",
            "attempts",
            "expired-agent",
            "--since",
            "2001-01-01T00:00:00Z",
        ],
    ] {
        let output = cli(&config, &url, &arguments);
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert!(store.agent("expired-agent").await.unwrap().is_some());
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_keys")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM operator_actions")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn operator_confirmation_reason_codes_and_repeated_migrate(pool: PgPool) {
    let store = Store::from_pool(pool.clone());
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("coordinator.toml");
    std::fs::write(
        &config,
        "[database]\nsecret_cmd=['/usr/bin/printenv','DATABASE_URL']\n",
    )
    .unwrap();
    let url = pool.connect_options().to_url_lossy().to_string();
    for _ in 0..2 {
        let output = cli(&config, &url, &["migrate"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let key = public_key_from_seed(&[19; 32]);
    store
        .apply(
            ApplicationInput {
                agent_id: "alice".into(),
                display_name: "Alice".into(),
                display_name_key: "alice".into(),
                bio: "本地脚本".into(),
                owner_contact: "alice@example.invalid".into(),
                framework_label: None,
                public_key: key.clone(),
            },
            ApplyLimits::default(),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE agent_keys SET created_at='2026-11-01T00:00:00Z' WHERE agent_id='alice'")
        .execute(&pool)
        .await
        .unwrap();
    let refused = cli(&config, &url, &["operator", "agent", "approve", "alice"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--yes"));
    assert!(
        String::from_utf8_lossy(&refused.stdout)
            .contains("public_key_registered_at=2026-11-01T00:00:00+00:00")
    );
    assert_eq!(
        store.agent("alice").await.unwrap().unwrap().state,
        "PENDING"
    );
    // Legacy CLI confirmation fixture. New conformance e2e tests earn this basis over HTTP.
    sqlx::query("UPDATE agents SET conformance_passed_at=clock_timestamp(),conformance_suite_version='1',application_revision=application_revision+1 WHERE agent_id='alice'").execute(&pool).await.unwrap();
    let approved = cli(
        &config,
        &url,
        &["operator", "agent", "approve", "alice", "--yes"],
    );
    assert!(
        approved.status.success(),
        "{}",
        String::from_utf8_lossy(&approved.stderr)
    );
    assert_eq!(store.agent("alice").await.unwrap().unwrap().state, "ACTIVE");
    let new = public_key_from_seed(&[20; 32]);
    let refused = cli(
        &config,
        &url,
        &[
            "operator",
            "agent",
            "set-key",
            "alice",
            "--public-key",
            &new,
            "--reason",
            "owner_request",
        ],
    );
    assert!(!refused.status.success());
    assert_eq!(
        store.agent("alice").await.unwrap().unwrap().public_key,
        Some(key)
    );
    let invalid = cli(
        &config,
        &url,
        &[
            "operator",
            "agent",
            "set-key",
            "alice",
            "--public-key",
            &new,
            "--reason",
            "unknown",
            "--yes",
        ],
    );
    assert!(!invalid.status.success());
    let changed = cli(
        &config,
        &url,
        &[
            "operator",
            "agent",
            "set-key",
            "alice",
            "--public-key",
            &new,
            "--reason",
            "owner_request",
            "--yes",
        ],
    );
    assert!(
        changed.status.success(),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    let reason: String =
        sqlx::query_scalar("SELECT reason_code FROM operator_actions WHERE kind='set_key'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reason, "owner_request");
    let snapshot = store.review("alice").await.unwrap();
    assert!(String::from_utf8_lossy(&changed.stdout).contains(&format!(
        "public_key_registered_at={}",
        snapshot.public_key_registered_at.unwrap().to_rfc3339()
    )));
    assert_ne!(
        snapshot.public_key_registered_at.unwrap(),
        chrono::DateTime::parse_from_rfc3339("2026-11-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    );
    assert_eq!(
        store.agent("alice").await.unwrap().unwrap().public_key,
        Some(new)
    );
    sqlx::query("UPDATE schema_version SET version=99")
        .execute(&pool)
        .await
        .unwrap();
    let refused = cli(&config, &url, &["operator", "agent", "list"]);
    assert!(!refused.status.success());
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn pause_mute_attempts_kick_and_retract_are_confirmed_and_audited(pool: PgPool) {
    use chrono::Utc;
    use tolight_coordinator_store::{SendInput, SendLimits};
    use tolight_core::models::AgentContext;
    let store = Store::from_pool(pool.clone());
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[database]\nsecret_cmd=['/usr/bin/printenv','DATABASE_URL']\n[public]\ndir={:?}\n",
            directory.path().join("public").to_str().unwrap()
        ),
    )
    .unwrap();
    let url = pool.connect_options().to_url_lossy().to_string();
    let key = public_key_from_seed(&[120; 32]);
    store
        .apply(
            ApplicationInput {
                agent_id: "alice".into(),
                display_name: "Alice".into(),
                display_name_key: "alice".into(),
                bio: "测试".into(),
                owner_contact: "owner@example.invalid".into(),
                framework_label: None,
                public_key: key.clone(),
            },
            ApplyLimits::default(),
        )
        .await
        .unwrap();
    let ctx = AgentContext {
        agent_id: "alice".into(),
        public_key: key.clone(),
    };
    store
        .conformance_transaction(&ctx, |d| {
            d.agent.conformance_passed_at = Some(Utc::now());
            d.agent.conformance_suite_version = Some("1".into());
        })
        .await
        .unwrap();
    store
        .approve(&store.review("alice").await.unwrap(), true)
        .await
        .unwrap();
    for args in [
        vec!["operator", "pause", "--reason", "等待本地恢复检查"],
        vec!["operator", "pause"],
        vec!["operator", "resume"],
    ] {
        let out = cli(&config, &url, &args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(!store.room_state().await.unwrap().paused);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM operator_actions WHERE kind='pause' AND reason_code IN ('other','等待本地恢复检查')"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        2
    );
    let head = store.room_state().await.unwrap().last_seq;
    let snapshot = store
        .export_controls(
            &directory.path().join("private-controls.json"),
            &directory.path().join("public"),
            Utc::now(),
        )
        .await
        .unwrap();
    assert!(
        snapshot
            .operator_actions
            .iter()
            .any(|action| action.reason_code.as_deref() == Some("等待本地恢复检查"))
    );
    assert!(
        !serde_json::to_string(&store.public_snapshot().await.unwrap().events)
            .unwrap()
            .contains("等待本地恢复检查")
    );
    for args in [
        vec!["operator", "agent", "mute", "alice", "--for", "30m"],
        vec!["operator", "agent", "mute", "alice"],
        vec!["operator", "agent", "unmute", "alice"],
        vec![
            "operator",
            "agent",
            "mute",
            "alice",
            "--until",
            "2026-01-01T00:00:00Z",
        ],
    ] {
        let out = cli(&config, &url, &args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        if args.len() == 4 && args[2] == "mute" {
            assert!(String::from_utf8_lossy(&out.stdout).contains("无限期"));
        }
    }
    assert_eq!(store.room_state().await.unwrap().last_seq, head);
    let before = store.review("alice").await.unwrap().muted_until;
    for args in [
        vec!["operator", "agent", "mute", "alice", "--for", "0m"],
        vec![
            "operator",
            "agent",
            "mute",
            "alice",
            "--until",
            "2026-01-01T00:00:00+00:00",
        ],
        vec![
            "operator",
            "agent",
            "mute",
            "alice",
            "--for",
            "1m",
            "--until",
            "2026-01-01T00:00:00Z",
        ],
        vec!["operator", "agent", "mute", "alice", "--reason", "unknown"],
    ] {
        assert!(!cli(&config, &url, &args).status.success());
        assert_eq!(store.review("alice").await.unwrap().muted_until, before);
    }
    let limits = SendLimits {
        messages_per_minute: 100,
        messages_per_day: 1000,
        max_mentions_per_message: 3,
    };
    let make = |id: &str, text: std::result::Result<String, String>| SendInput {
        agent_id: "alice".into(),
        public_key: key.clone(),
        client_message_id: id.into(),
        text,
        reply_to_event_id: None,
        mentions: vec![],
    };
    assert!(
        store
            .send(make("rejected", Err("empty".into())), limits.clone())
            .await
            .is_err()
    );
    sqlx::query("UPDATE message_attempts SET reason_code='invalid_mention',reason_detail=$1 WHERE client_message_id='rejected'")
        .bind("bad\n\u{200b}")
        .execute(&pool)
        .await
        .unwrap();
    let out = cli(&config, &url, &["operator", "attempts", "alice"]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 1);
    assert!(!text.contains('\u{200b}'));
    assert!(text.contains("\\u{200b}"));
    sqlx::query("UPDATE message_attempts SET created_at='2000-01-01T00:00:00Z' WHERE client_message_id='rejected'")
        .execute(&pool).await.unwrap();
    let filtered = cli(
        &config,
        &url,
        &[
            "operator",
            "attempts",
            "alice",
            "--since",
            "2001-01-01T00:00:00Z",
        ],
    );
    assert!(filtered.status.success());
    assert!(filtered.stdout.is_empty());
    assert!(
        !cli(
            &config,
            &url,
            &["operator", "attempts", "alice", "--since", "invalid"]
        )
        .status
        .success()
    );
    let sent = store
        .send(make("public", Ok("本地撤回正文".into())), limits)
        .await
        .unwrap();
    let filtered = cli(
        &config,
        &url,
        &[
            "operator",
            "attempts",
            "alice",
            "--since",
            "2001-01-01T00:00:00Z",
        ],
    );
    assert!(filtered.status.success());
    let filtered = String::from_utf8(filtered.stdout).unwrap();
    assert_eq!(filtered.lines().count(), 1);
    assert!(filtered.contains("client_message_id=public"));
    assert!(
        !cli(&config, &url, &["operator", "retract", &sent.event_id])
            .status
            .success()
    );
    let out = cli(
        &config,
        &url,
        &[
            "operator",
            "retract",
            &sent.event_id,
            "--reason",
            "content",
            "--yes",
        ],
    );
    let output = String::from_utf8_lossy(&out.stdout);
    assert!(output.contains("此理由码会公开"));
    assert!(output.contains("reason=content"));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        store
            .public_snapshot()
            .await
            .unwrap()
            .events
            .iter()
            .find(|e| e["event_id"] == sent.event_id)
            .unwrap()
            .get("text")
            .is_none()
    );
    assert!(
        !cli(&config, &url, &["operator", "agent", "kick", "alice"])
            .status
            .success()
    );
    assert_eq!(store.review("alice").await.unwrap().state, "ACTIVE");
    assert!(
        cli(
            &config,
            &url,
            &[
                "operator", "agent", "kick", "alice", "--reason", "spam", "--yes"
            ]
        )
        .status
        .success()
    );
    assert!(store.agent_by_key(&key).await.unwrap().is_none());
}
