mod support;
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::path::{Path, PathBuf};
use support::fake_agent::{FakeAgent, TestApp};
use tolight_coordinator::{metrics, operator};

struct Listener(tokio::task::JoinHandle<()>);
impl Drop for Listener {
    fn drop(&mut self) {
        self.0.abort();
    }
}
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}
fn private_write(path: &Path, bytes: impl AsRef<[u8]>) {
    std::fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}
async fn checker(directory: &Path, origin: &str, name: &str) -> Value {
    let notify = json!([
        "/bin/sh",
        "-c",
        "umask 077; cat >> \"$1\"",
        "tolight-test-notify",
        directory.join("notifications.jsonl")
    ]);
    let output = tokio::process::Command::new(root().join("scripts/alert-check.sh"))
        .args([
            "--health-url",
            &format!("{origin}/healthz"),
            "--metrics-url",
            &format!("{origin}/metrics"),
            "--state-file",
        ])
        .arg(directory.join("alert-state.json"))
        .arg("--backup-dir")
        .arg(directory.join("backups"))
        .args([
            "--free-bytes",
            "999999999999",
            "--message-limit-per-hour",
            "1",
            "--sanitizer-rejections-per-hour",
            "1",
            "--clock-skew-per-hour",
            "1",
            "--messages-per-hour",
            "1",
            "--messages-per-day",
            "1000",
            "--notify-command",
            &notify.to_string(),
        ])
        .output()
        .await
        .unwrap();
    assert!(
        matches!(output.status.code(), Some(0 | 1)),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    private_write(
        &directory.join(format!("{name}.stdout.json")),
        &output.stdout,
    );
    private_write(&directory.join(format!("{name}.stderr")), &output.stderr);
    serde_json::from_slice(&output.stdout).unwrap()
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn actual_metrics_notify_local_sink_and_operator_mute_preserves_reading_and_attempt_accounting(
    pool: PgPool,
) {
    let local = root().join(".local");
    std::fs::create_dir_all(&local).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("m4-alert-drill.")
        .tempdir_in(local)
        .unwrap()
        .keep();
    std::fs::create_dir(directory.join("backups")).unwrap();
    // Synthetic completion marker for this isolated notification drill. Real
    // pg_dump production/consumer compatibility is covered by restore_drill.
    let backup_name = "tolight-20000101T000000Z-00000000000000000000000000000001.dump";
    private_write(
        &directory.join("backups").join(backup_name),
        b"PGDMPfixture",
    );
    private_write(
        &directory
            .join("backups")
            .join(format!("{backup_name}.meta.json")),
        serde_json::to_vec(&json!({
            "schema":"tolight-backup/1", "file":backup_name,
            "created_at":Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "sha256":"d3f6f2a2fda39b3e845c096384e9854aac323b2ed8e847de65332ed32b3532be",
            "size":12, "format":"custom", "compression":"none", "synthetic_test_fixture":true,
        }))
        .unwrap(),
    );
    let app = TestApp::configured(pool, |config| {
        config.messages_per_minute = 3;
        config.messages_per_day = 1000;
        config.observability.log_dir = directory.join("service-logs");
    })
    .await;
    let alice = FakeAgent::new("alice", 177);
    let bob = FakeAgent::new("bobby", 178);
    alice.apply(&app).await;
    bob.apply(&app).await;
    app.approve("alice").await;
    app.approve("bobby").await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let router = metrics::router(app.room.clone());
    let _listener = Listener(tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    }));
    let before = checker(&directory, &origin, "baseline").await;
    assert_eq!(before["ok"], true);
    assert_eq!(alice.send(&app, "本地告警处置的公开样例").await.status, 201);
    assert_eq!(bob.send(&app, "其他身份仍然可用").await.status, 201);
    assert_eq!(
        alice
            .send(&app, "-----BEGIN PRIVATE KEY----- rejected-fixture-marker")
            .await
            .status,
        422
    );
    let mention = alice.request(&app, "POST", "messages", "", Some(json!({"client_message_id":"bad-mention","text":"本地拒绝样例","mentions":["absent-agent"]}))).await;
    assert_eq!(mention.status, 422);
    assert_eq!(alice.send(&app, "limited-one").await.status, 429);
    assert_eq!(alice.send(&app, "limited-two").await.status, 429);
    for _ in 0..2 {
        let response = alice
            .raw(
                &app,
                "GET",
                "self",
                "",
                vec![],
                Utc::now().timestamp() - 1000,
                &[],
            )
            .await;
        assert_eq!(response.status, 401);
        assert_eq!(response.body["error"]["code"], "clock_skew");
    }
    let observed = metrics::render(&app.room).await.unwrap();
    private_write(&directory.join("actual-metrics.txt"), &observed);
    let raised = checker(&directory, &origin, "raised").await;
    let codes = raised["conditions"].as_array().unwrap();
    for code in [
        "message_limit_hourly:alice",
        "sanitizer_rejections_hourly:alice",
        "clock_skew_hourly",
        "hourly_volume",
    ] {
        assert!(
            codes.iter().any(|item| item == code),
            "missing {code}: {raised}"
        );
    }
    let notes: Vec<Value> = std::fs::read_to_string(directory.join("notifications.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(notes.len(), 4);
    checker(&directory, &origin, "deduplicated").await;
    assert_eq!(
        std::fs::read_to_string(directory.join("notifications.jsonl"))
            .unwrap()
            .lines()
            .count(),
        notes.len()
    );
    // This is the real local Operator handler, after the local sink received the alert.
    let attempts_before = app.room.store().attempts("alice").await.unwrap().len();
    let muted = operator::mute(
        app.room.store(),
        app.room.config(),
        "alice",
        None,
        "runaway",
    )
    .await
    .unwrap();
    assert!(muted.muted_indefinitely);
    let rejected = alice.send(&app, "after-mute-unique-message").await;
    assert_eq!(rejected.status, 403);
    assert_eq!(rejected.body["error"]["code"], "agent_muted");
    let attempts_after = app.room.store().attempts("alice").await.unwrap().len();
    assert_eq!(attempts_before, attempts_after);
    assert_eq!(
        alice
            .request(&app, "GET", "events", "limit=0", None)
            .await
            .status,
        200
    );
    assert_eq!(
        alice.request(&app, "GET", "agents", "", None).await.status,
        200
    );
    assert_eq!(
        alice.request(&app, "GET", "self", "", None).await.status,
        200
    );
    assert_eq!(bob.send(&app, "处置后其他Agent仍可发言").await.status, 201);
    let logs = std::fs::read_dir(&app.room.config().observability.log_dir)
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect::<String>();
    assert!(!logs.contains("rejected-fixture-marker"));
    assert!(!logs.contains("owner@example.invalid"));
    assert!(!logs.contains(&alice.key()));
    let report = json!({"schema":"tolight-alert-drill/1","scope":"isolated real HTTP/PostgreSQL/metrics with local JSONL sink; qualified test identities","recorded_at":Utc::now().to_rfc3339(),"thresholds":{"message_limit_per_hour":1,"sanitizer_rejections_per_hour":1,"clock_skew_per_hour":1,"messages_per_hour":1},"actual_counter_and_gauge_alerts":raised["conditions"],"sink_notifications":notes.len(),"duplicate_notifications":0,"action":"local sink observed then production operator::mute handler called","human_operator_delivery":false,"external_notifications":false,"mute_state":"infinite","post_mute_status":rejected.status,"attempts_before":attempts_before,"attempts_after":attempts_after,"muted_reads_succeeded":true,"other_agent_send_status":201,"service_log_sensitive_values_absent":true,"process_cleanup":"RAII abort of fixture participant and metrics listeners at test exit","evidence_directory":directory});
    private_write(
        &directory.join("report.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    );
    eprintln!("Local alert drill evidence: {}", directory.display());
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn six_reason_rejection_gauge_is_exact_at_the_one_hour_boundary_and_retains_old_24h_metric(
    pool: PgPool,
) {
    let app = TestApp::configured(pool, |config| {
        config.messages_per_minute = 100;
        config.max_mentions_per_message = 1;
    })
    .await;
    let alice = FakeAgent::new("alice", 179);
    let bob = FakeAgent::new("bobby", 180);
    let carol = FakeAgent::new("carol", 181);
    for agent in [&alice, &bob, &carol] {
        agent.apply(&app).await;
        app.approve(&agent.id).await;
    }
    let too_long = "文".repeat(2001);
    for (reason, text, mentions) in [
        ("empty", "", vec![]),
        ("too_long", too_long.as_str(), vec![]),
        ("invalid_chars", "text\u{202e}", vec![]),
        (
            "secret_shape",
            "-----BEGIN PRIVATE KEY----- synthetic",
            vec![],
        ),
        ("invalid_mention", "text", vec!["absent-agent"]),
        ("too_many_mentions", "text", vec!["bobby", "carol"]),
    ] {
        let result = alice.request(&app, "POST", "messages", "", Some(json!({"client_message_id":format!("reason-{reason}"),"text":text,"mentions":mentions}))).await;
        assert_eq!(result.status, 422, "{reason}: {}", result.body);
    }
    let now = Utc::now();
    for (reason, time) in [
        ("empty", now - ChronoDuration::hours(1)),
        (
            "too_long",
            now - ChronoDuration::hours(1) + ChronoDuration::microseconds(1),
        ),
        ("invalid_chars", now - ChronoDuration::hours(2)),
    ] {
        sqlx::query(
            "UPDATE message_attempts SET created_at=$1 WHERE agent_id='alice' AND reason_code=$2",
        )
        .bind(time)
        .bind(reason)
        .execute(app.room.store().pool())
        .await
        .unwrap();
    }
    let snapshot = app.room.store().telemetry(now).await.unwrap();
    let counts = snapshot
        .rejections
        .iter()
        .map(|row| (row.reason.as_str(), (row.count, row.count_last_hour)))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(counts.len(), 6);
    for reason in ["empty", "invalid_chars"] {
        assert_eq!(counts[reason], (1, 0));
    }
    for reason in [
        "too_long",
        "secret_shape",
        "invalid_mention",
        "too_many_mentions",
    ] {
        assert_eq!(counts[reason], (1, 1));
    }
    let rendered = metrics::render(&app.room).await.unwrap();
    for reason in [
        "empty",
        "too_long",
        "invalid_chars",
        "secret_shape",
        "invalid_mention",
        "too_many_mentions",
    ] {
        assert!(rendered.contains(&format!("tolight_agent_rejected_attempts_last_hour{{agent_id=\"alice\",reason_code=\"{reason}\"}}")));
        assert!(rendered.contains(&format!(
            "tolight_agent_rejected_attempts{{agent_id=\"alice\",reason_code=\"{reason}\"}} 1"
        )));
    }
}
