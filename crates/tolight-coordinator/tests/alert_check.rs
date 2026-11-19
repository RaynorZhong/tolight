use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Output,
    time::{SystemTime, UNIX_EPOCH},
};
const LOGICAL_BACKUP: &str = "tolight-20000101T000000Z-00000000000000000000000000000001.dump";
fn write_logical_backup(directory: &Path, created: u64) {
    std::fs::write(directory.join(LOGICAL_BACKUP), b"PGDMPfixture").unwrap();
    let created = tolight_core::datetime::utc_seconds(
        time::OffsetDateTime::from_unix_timestamp(created as i64).unwrap(),
    );
    let metadata = serde_json::json!({"schema":"tolight-backup/1","file":LOGICAL_BACKUP,"created_at":created,"sha256":"d3f6f2a2fda39b3e845c096384e9854aac323b2ed8e847de65332ed32b3532be","size":12,"format":"custom","compression":"none"});
    std::fs::write(
        directory.join(format!("{LOGICAL_BACKUP}.meta.json")),
        serde_json::to_vec(&metadata).unwrap(),
    )
    .unwrap();
}
struct Fixture {
    _dir: tempfile::TempDir,
    metrics: PathBuf,
    state: PathBuf,
    sink: PathBuf,
    notify: String,
    backups: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let metrics = dir.path().join("metrics.txt");
        let state = dir.path().join("state.json");
        let sink = dir.path().join("sink.txt");
        let notify = dir.path().join("notify.sh");
        let backups = dir.path().join("backups");
        std::fs::create_dir(&backups).unwrap();
        write_logical_backup(
            &backups,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        );
        std::fs::write(&notify, format!("#!/bin/sh\ncat >> '{}'\n", sink.display())).unwrap();
        let command = serde_json::json!(["/bin/sh", notify]);
        Self {
            _dir: dir,
            metrics,
            state,
            sink,
            notify: command.to_string(),
            backups,
        }
    }
    async fn run(&self, now: u64, extra: &[&str]) -> Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/alert-check.sh");
        tokio::process::Command::new(script)
            .args([
                "--metrics-file",
                self.metrics.to_str().unwrap(),
                "--health-status",
                "200",
                "--state-file",
                self.state.to_str().unwrap(),
                "--backup-dir",
                self.backups.to_str().unwrap(),
                "--notify-command",
                &self.notify,
                "--now",
                &now.to_string(),
                "--free-bytes",
                "999999999999",
            ])
            .args(extra)
            .output()
            .await
            .unwrap()
    }
    fn notifications(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.sink)
            .unwrap_or_default()
            .lines()
            .map(|v| serde_json::from_str(v).unwrap())
            .collect()
    }
}
fn sample(latest: u64, oldest: u64, day: u64, counter: u64) -> String {
    format!(
        "tolight_database_up 1\ntolight_process_start_timestamp_seconds 1\ntolight_clock_skew_total 0\ntolight_pending_applications 1\ntolight_oldest_pending_seconds {oldest}\ntolight_latest_pending_timestamp_seconds {latest}\ntolight_agent_attempts_last_day{{agent_id=\"alice\"}} {day}\ntolight_message_limit_rejections_total{{agent_id=\"alice\"}} {counter}\ntolight_publish_lag_events 0\ntolight_publish_lag_seconds 0\ntolight_room_messages_last_hour 0\ntolight_agent_last_seen_seconds{{agent_id=\"alice\"}} 0\n"
    ) + &["empty", "too_long", "invalid_chars", "secret_shape", "invalid_mention", "too_many_mentions"].iter().map(|reason| format!("tolight_agent_rejected_attempts_last_hour{{agent_id=\"alice\",reason_code=\"{reason}\"}} 0\n")).collect::<String>()
}
#[tokio::test]
async fn failed_notification_is_retried_and_overlapping_checks_cannot_send_or_overwrite_state() {
    let fixture = Fixture::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(&fixture.metrics, sample(100, 0, 0, 0)).unwrap();
    let failed = fixture
        .run(
            now,
            &["--notify-command", "[\"/bin/sh\",\"-c\",\"exit 1\"]"],
        )
        .await;
    assert!(!failed.status.success());
    let undelivered: Value =
        serde_json::from_slice(&std::fs::read(&fixture.state).unwrap()).unwrap();
    assert!(
        undelivered.get("latest_pending").is_none(),
        "failed delivery must not acknowledge the pending application"
    );
    let ready = fixture._dir.path().join("notifier-ready");
    let release = fixture._dir.path().join("notifier-release");
    let blocked = fixture._dir.path().join("blocked-notifier.sh");
    std::fs::write(
        &blocked,
        format!(
            "#!/bin/sh\ncat >> '{}'\ntouch '{}'\nwhile [ ! -f '{}' ]; do sleep 0.01; done\n",
            fixture.sink.display(),
            ready.display(),
            release.display()
        ),
    )
    .unwrap();
    let command = serde_json::json!(["/bin/sh", blocked]).to_string();
    let first_flags = ["--notify-command", command.as_str()];
    let first = fixture.run(now + 1, &first_flags);
    let concurrent = async {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ready.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "notifier did not start"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let output = fixture.run(now + 2, &[]).await;
        std::fs::write(&release, "go").unwrap();
        assert!(
            !output.status.success(),
            "overlapping checker must fail before it can notify"
        );
        assert_eq!(fixture.notifications().len(), 1);
    };
    let (completed, ()) = tokio::join!(first, concurrent);
    assert!(completed.status.success());
    let state: Value = serde_json::from_slice(&std::fs::read(&fixture.state).unwrap()).unwrap();
    assert_eq!(state["counter_sampled_at"].as_f64(), Some((now + 1) as f64));
    assert!(fixture.run(now + 3, &[]).await.status.success());
    assert_eq!(
        fixture.notifications().len(),
        1,
        "successful delivery stays deduplicated"
    );
}

#[tokio::test]
async fn timed_out_notification_stops_background_delivery_and_releases_lock_without_acknowledging()
{
    let fixture = Fixture::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(&fixture.metrics, sample(100, 0, 0, 0)).unwrap();
    let late = fixture._dir.path().join("late-delivery.txt");
    let notifier = fixture._dir.path().join("delayed-notifier.sh");
    std::fs::write(
        &notifier,
        "#!/bin/sh\ncat >/dev/null\n(sleep 17; printf late > \"$1\") &\nwait\n",
    )
    .unwrap();
    let command = serde_json::json!(["/bin/sh", notifier, late]).to_string();
    let output = fixture.run(now, &["--notify-command", &command]).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("timed out"));
    let state: Value = serde_json::from_slice(&std::fs::read(&fixture.state).unwrap()).unwrap();
    assert!(
        state.get("latest_pending").is_none(),
        "timeout must not acknowledge a pending application"
    );
    assert!(
        fixture.run(now + 1, &[]).await.status.success(),
        "timeout must release the checker lock for retry"
    );
    assert_eq!(fixture.notifications().len(), 1);
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(
        !late.exists(),
        "notification worker must not deliver after a failed timeout"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_notification_stops_its_group_and_does_not_acknowledge_delivery() {
    let fixture = Fixture::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(&fixture.metrics, sample(100, 0, 0, 0)).unwrap();
    let ready = fixture._dir.path().join("cancel-ready");
    let late = fixture._dir.path().join("late-delivery.txt");
    let notifier = fixture._dir.path().join("cancel-notifier.sh");
    std::fs::write(&notifier, "#!/bin/sh\ncat >/dev/null\n(sleep 3; printf late > \"$1\") &\nprintf ready > \"$2\"\nwait\n").unwrap();
    let command = serde_json::json!(["/bin/sh", notifier, late, ready]).to_string();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/alert-check.sh");
    let mut child = tokio::process::Command::new(script)
        .args([
            "--metrics-file",
            fixture.metrics.to_str().unwrap(),
            "--health-status",
            "200",
            "--state-file",
            fixture.state.to_str().unwrap(),
            "--backup-dir",
            fixture.backups.to_str().unwrap(),
            "--free-bytes",
            "999999999999",
            "--notify-command",
            &command,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .arg("--now")
        .arg(now.to_string())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !ready.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "notifier did not start"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        tokio::process::Command::new("/bin/kill")
            .args(["-TERM", &child.id().unwrap().to_string()])
            .status()
            .await
            .unwrap()
            .success()
    );
    let status = tokio::time::timeout(std::time::Duration::from_secs(3), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(!status.success());
    let state: Value = serde_json::from_slice(&std::fs::read(&fixture.state).unwrap()).unwrap();
    assert!(state.get("latest_pending").is_none());
    let retried = fixture.run(now + 1, &[]).await;
    assert!(
        retried.status.success(),
        "{}",
        String::from_utf8_lossy(&retried.stderr)
    );
    assert_eq!(fixture.notifications().len(), 1);
    tokio::time::sleep(std::time::Duration::from_millis(3200)).await;
    assert!(
        !late.exists(),
        "cancelled worker must not deliver after the checker exits"
    );
}
#[tokio::test]
async fn later_delivery_failure_retains_successful_application_and_condition_prefix() {
    let fixture = Fixture::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(&fixture.metrics, sample(100, 0, 80, 0)).unwrap();
    let fail = fixture._dir.path().join("fail-disk");
    let notifier = fixture._dir.path().join("partial-notifier.sh");
    std::fs::write(&fail, "fail").unwrap();
    std::fs::write(&notifier, format!(
        "#!/bin/sh\nevent=$(cat)\ncase \"$event\" in *'\"code\": \"disk_low\"'*) [ ! -f '{}' ] || exit 1;; esac\nprintf '%s\\n' \"$event\" >> '{}'\n", fail.display(), fixture.sink.display()
    )).unwrap();
    let command = serde_json::json!(["/bin/sh", notifier]).to_string();
    let flags = ["--notify-command", command.as_str(), "--free-bytes", "0"];
    assert!(!fixture.run(now, &flags).await.status.success());
    assert_eq!(
        fixture
            .notifications()
            .iter()
            .map(|value| value["code"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["new_application", "daily_attempts:alice"]
    );
    let state: Value = serde_json::from_slice(&std::fs::read(&fixture.state).unwrap()).unwrap();
    assert_eq!(state["latest_pending"], 100.0);
    assert_eq!(
        state["alerts"]["daily_attempts:alice"].as_f64(),
        Some(now as f64)
    );
    assert!(state["alerts"].get("disk_low").is_none());
    std::fs::remove_file(fail).unwrap();
    let retry = fixture.run(now + 1, &flags).await;
    assert_eq!(conditions(&retry), ["daily_attempts:alice", "disk_low"]);
    assert_eq!(
        fixture
            .notifications()
            .iter()
            .map(|value| value["code"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["new_application", "daily_attempts:alice", "disk_low"]
    );
    fixture.run(now + 2, &flags).await;
    assert_eq!(fixture.notifications().len(), 3);
}
#[tokio::test]
async fn undelivered_counter_increment_survives_coordinator_restart_before_the_retry() {
    let fixture = Fixture::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let flags = ["--clock-skew-per-hour", "2"];
    std::fs::write(&fixture.metrics, sample(0, 0, 0, 0)).unwrap();
    assert!(fixture.run(now, &flags).await.status.success());
    std::fs::write(
        &fixture.metrics,
        sample(0, 0, 0, 0).replace("tolight_clock_skew_total 0", "tolight_clock_skew_total 3"),
    )
    .unwrap();
    let failed_flags = [
        "--clock-skew-per-hour",
        "2",
        "--notify-command",
        "[\"/bin/sh\",\"-c\",\"exit 1\"]",
    ];
    assert!(!fixture.run(now + 1, &failed_flags).await.status.success());
    assert!(fixture.notifications().is_empty());
    let state: Value = serde_json::from_slice(&std::fs::read(&fixture.state).unwrap()).unwrap();
    assert_eq!(state["counters"]["tolight_clock_skew_total{}"], 3.0);
    assert!(
        state
            .get("alerts")
            .and_then(|value| value.get("clock_skew_hourly"))
            .is_none()
    );
    std::fs::write(
        &fixture.metrics,
        sample(0, 0, 0, 0).replace(
            "tolight_process_start_timestamp_seconds 1",
            "tolight_process_start_timestamp_seconds 2",
        ),
    )
    .unwrap();
    let retried = fixture.run(now + 2, &flags).await;
    assert_eq!(conditions(&retried), ["clock_skew_hourly"]);
    let report: Value = serde_json::from_slice(&retried.stdout).unwrap();
    assert_eq!(
        report["hourly_counter_estimates"]["tolight_clock_skew_total{}"],
        3.0
    );
    assert_eq!(fixture.notifications().len(), 1);
    fixture.run(now + 3, &flags).await;
    assert_eq!(fixture.notifications().len(), 1);
}
#[tokio::test]
async fn new_application_uses_timestamp_and_counter_reset_never_creates_negative_delta() {
    let fixture = Fixture::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(&fixture.metrics, sample(100, 10, 79, 10)).unwrap();
    assert!(fixture.run(now, &[]).await.status.success());
    assert!(fixture.run(now + 1, &[]).await.status.success());
    assert_eq!(fixture.notifications().len(), 1);
    // Same pending count: one was approved while another arrived.
    std::fs::write(&fixture.metrics, sample(101, 10, 79, 3)).unwrap();
    assert!(fixture.run(now + 2, &[]).await.status.success());
    assert_eq!(fixture.notifications().len(), 2);
    let state: Value = serde_json::from_slice(&std::fs::read(&fixture.state).unwrap()).unwrap();
    assert_eq!(
        state["counter_deltas"]["tolight_message_limit_rejections_total{\"agent_id\": \"alice\"}"],
        3.0
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&fixture.state)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
#[tokio::test]
async fn thresholds_notify_once_repeat_then_reset_when_conditions_clear() {
    let fixture = Fixture::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(
        &fixture.metrics,
        sample(100, 259201, 80, 0)
            .replace(
                "tolight_room_messages_last_hour 0",
                "tolight_room_messages_last_hour 61",
            )
            .replace(
                "tolight_publish_lag_events 0",
                "tolight_publish_lag_events 1",
            )
            .replace(
                "tolight_publish_lag_seconds 0",
                "tolight_publish_lag_seconds 301",
            ),
    )
    .unwrap();
    assert_eq!(fixture.run(now, &[]).await.status.code(), Some(1));
    let first = fixture.notifications();
    assert!(first.iter().any(|v| v["code"] == "daily_attempts:alice"));
    assert!(first.iter().any(|v| v["code"] == "hourly_volume"));
    assert!(first.iter().any(|v| v["code"] == "publish_lag"));
    assert!(first.iter().any(|v| v["code"] == "review_waiting"));
    fixture.run(now + 1, &[]).await;
    assert_eq!(fixture.notifications().len(), first.len());
    fixture.run(now + 21601, &[]).await;
    assert_eq!(fixture.notifications().len(), first.len() + 5);
    assert!(
        fixture
            .notifications()
            .iter()
            .any(|v| v["code"] == "counter_sampling_gap")
    );
    std::fs::write(&fixture.metrics, sample(100, 0, 0, 0)).unwrap();
    assert!(fixture.run(now + 21602, &[]).await.status.success());
    std::fs::write(&fixture.metrics, sample(100, 0, 80, 0)).unwrap();
    fixture.run(now + 21603, &[]).await;
    assert_eq!(
        fixture.notifications().last().unwrap()["code"],
        "daily_attempts:alice"
    );
}
#[tokio::test]
async fn database_disk_backup_and_reference_offline_each_alert_without_payloads() {
    let fixture = Fixture::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(
        &fixture.metrics,
        sample(100, 0, 0, 0)
            .replace("tolight_database_up 1", "tolight_database_up 0")
            .replace(
                "tolight_agent_last_seen_seconds{agent_id=\"alice\"} 0",
                "tolight_agent_last_seen_seconds{agent_id=\"alice\"} 601",
            ),
    )
    .unwrap();
    std::fs::remove_file(fixture.backups.join(LOGICAL_BACKUP)).unwrap();
    let output = fixture
        .run(now, &["--free-bytes", "0", "--reference-agent", "alice"])
        .await;
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let notes = fixture.notifications();
    for code in [
        "database_unavailable",
        "disk_low",
        "backup_stale",
        "reference_offline:alice",
    ] {
        assert!(notes.iter().any(|v| v["code"] == code), "{code}");
    }
    let text = std::fs::read_to_string(&fixture.sink).unwrap();
    assert!(!text.contains("owner_contact"));
    assert!(!text.contains("Tolight-Signature"));
    write_logical_backup(&fixture.backups, now);
    let missing_reference = sample(100, 0, 0, 0)
        .lines()
        .filter(|line| !line.starts_with("tolight_agent_last_seen_seconds"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&fixture.metrics, missing_reference).unwrap();
    assert_eq!(
        conditions(&fixture.run(now + 1, &["--reference-agent", "alice"]).await),
        ["reference_offline:alice"]
    );
}

fn conditions(output: &Output) -> Vec<String> {
    serde_json::from_slice::<Value>(&output.stdout).unwrap()["conditions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn hourly_alerts_use_strict_thresholds_and_exact_six_reason_sanitation_sum() {
    let fixture = Fixture::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let flags = [
        "--message-limit-per-hour",
        "2",
        "--clock-skew-per-hour",
        "1",
        "--sanitizer-rejections-per-hour",
        "1",
    ];
    // Startup lifetime counts are not falsely attributed to the most recent hour.
    std::fs::write(
        &fixture.metrics,
        sample(100, 0, 0, 100)
            .replace("tolight_clock_skew_total 0", "tolight_clock_skew_total 100"),
    )
    .unwrap();
    assert!(fixture.run(now, &flags).await.status.success());
    std::fs::write(
        &fixture.metrics,
        sample(100, 0, 0, 102)
            .replace("tolight_clock_skew_total 0", "tolight_clock_skew_total 101"),
    )
    .unwrap();
    assert!(fixture.run(now + 1, &flags).await.status.success());
    let exceeded = sample(100, 0, 0, 103)
        .replace("tolight_clock_skew_total 0", "tolight_clock_skew_total 102")
        .replace("reason_code=\"empty\"} 0", "reason_code=\"empty\"} 1")
        .replace(
            "reason_code=\"invalid_mention\"} 0",
            "reason_code=\"invalid_mention\"} 1",
        );
    std::fs::write(&fixture.metrics, &exceeded).unwrap();
    let output = fixture.run(now + 2, &flags).await;
    assert_eq!(output.status.code(), Some(1));
    for code in [
        "message_limit_hourly:alice",
        "clock_skew_hourly",
        "sanitizer_rejections_hourly:alice",
    ] {
        assert!(
            conditions(&output).iter().any(|value| value == code),
            "{code}"
        );
    }
    let notes = fixture.notifications().len();
    fixture.run(now + 3, &flags).await;
    assert_eq!(fixture.notifications().len(), notes);
}

#[tokio::test]
async fn hourly_counter_windows_expire_and_restart_or_sparse_sampling_never_fabricates_history() {
    let fixture = Fixture::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(&fixture.metrics, sample(100, 0, 0, 0)).unwrap();
    fixture.run(now, &[]).await;
    std::fs::write(&fixture.metrics, sample(100, 0, 0, 3)).unwrap();
    assert!(
        conditions(
            &fixture
                .run(now + 1, &["--message-limit-per-hour", "2"])
                .await
        )
        .contains(&"message_limit_hourly:alice".into())
    );
    fixture.run(now + 3599, &[]).await;
    assert!(fixture.run(now + 3662, &[]).await.status.success());
    // A new process may already have accumulated MORE than the old reading.
    std::fs::write(
        &fixture.metrics,
        sample(100, 0, 0, 7).replace(
            "tolight_process_start_timestamp_seconds 1",
            "tolight_process_start_timestamp_seconds 2",
        ),
    )
    .unwrap();
    let output = fixture
        .run(now + 3663, &["--message-limit-per-hour", "5"])
        .await;
    assert!(conditions(&output).contains(&"message_limit_hourly:alice".into()));
    let state: Value = serde_json::from_slice(&std::fs::read(&fixture.state).unwrap()).unwrap();
    assert_eq!(
        state["counter_deltas"]["tolight_message_limit_rejections_total{\"agent_id\": \"alice\"}"],
        7.0
    );
    std::fs::write(
        &fixture.metrics,
        sample(100, 0, 0, 1).replace(
            "tolight_process_start_timestamp_seconds 1",
            "tolight_process_start_timestamp_seconds 2",
        ),
    )
    .unwrap();
    fixture.run(now + 3664, &[]).await;
    let state: Value = serde_json::from_slice(&std::fs::read(&fixture.state).unwrap()).unwrap();
    assert_eq!(
        state["counter_deltas"]["tolight_message_limit_rejections_total{\"agent_id\": \"alice\"}"],
        1.0
    );
    std::fs::write(&fixture.metrics, sample(100, 0, 0, 10000)).unwrap();
    let output = fixture.run(now + 8000, &[]).await;
    assert_eq!(conditions(&output), ["counter_sampling_gap"]);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        report["hourly_counter_estimates"]
            .as_object()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        conditions(&fixture.run(now + 7999, &[]).await),
        ["counter_clock_regression"]
    );
}

#[tokio::test]
async fn used_metric_corruption_fails_closed_but_unknown_extensions_are_ignored() {
    let fixture = Fixture::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let good = sample(100, 0, 0, 0);
    for broken in [
        good.replace("tolight_clock_skew_total 0", "tolight_clock_skew_total NaN"),
        good.replace("tolight_clock_skew_total 0", "tolight_clock_skew_total -1"),
        good.replace("tolight_clock_skew_total 0", "tolight_clock_skew_total 0.5"),
        good.replace("tolight_clock_skew_total 0\n", ""),
        good.clone() + "tolight_clock_skew_total 0\n",
        good.replace(
            "tolight_message_limit_rejections_total{agent_id=\"alice\"}",
            "tolight_message_limit_rejections_total{agent_id=\"alice\",agent_id=\"bob\"}",
        ),
        good.replace("reason_code=\"empty\"", "reason_code=\"unknown\""),
        good.lines()
            .filter(|line| !line.contains("reason_code=\"empty\""))
            .collect::<Vec<_>>()
            .join("\n"),
    ] {
        std::fs::write(&fixture.metrics, broken).unwrap();
        let output = fixture.run(now, &[]).await;
        assert_eq!(conditions(&output), ["metrics_unavailable"]);
        assert_eq!(output.status.code(), Some(1));
    }
    std::fs::write(
        &fixture.metrics,
        good + "future_extension{arbitrary=this-is-not-our-schema} NaN\n",
    )
    .unwrap();
    assert!(fixture.run(now + 1, &[]).await.status.success());
}

#[tokio::test]
async fn hourly_series_are_per_agent_and_minute_bucket_storage_is_bounded() {
    let fixture = Fixture::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let dual = |alice: u64, bob: u64| {
        let first = sample(100, 0, 0, alice);
        let second = sample(100, 0, 0, bob)
            .lines()
            .filter(|line| line.contains("agent_id="))
            .map(|line| line.replace("alice", "bobby") + "\n")
            .collect::<String>();
        first + &second
    };
    std::fs::write(&fixture.metrics, dual(0, 0)).unwrap();
    fixture.run(now, &[]).await;
    std::fs::write(&fixture.metrics, dual(1, 2)).unwrap();
    assert!(
        fixture
            .run(now + 1, &["--message-limit-per-hour", "2"])
            .await
            .status
            .success()
    );
    std::fs::write(&fixture.metrics, dual(3, 2)).unwrap();
    assert_eq!(
        conditions(
            &fixture
                .run(now + 2, &["--message-limit-per-hour", "2"])
                .await
        ),
        ["message_limit_hourly:alice"]
    );
    for index in 1..=120 {
        std::fs::write(&fixture.metrics, dual(3 + index, 2)).unwrap();
        fixture
            .run(now + index * 30, &["--message-limit-per-hour", "1000"])
            .await;
    }
    let bytes = std::fs::read(&fixture.state).unwrap();
    let state: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        state["counter_windows"]
            .as_object()
            .unwrap()
            .values()
            .all(|buckets| buckets.as_array().unwrap().len() <= 61)
    );
    assert!(bytes.len() < 10000);
}

#[tokio::test]
async fn physical_backup_age_requires_a_complete_pg18_marker_and_matching_manifest() {
    let fixture = Fixture::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(&fixture.metrics, sample(100, 0, 0, 0)).unwrap();
    std::fs::remove_file(fixture.backups.join(LOGICAL_BACKUP)).unwrap();
    // Match the native physical-backup producer prefix.
    let base = fixture.backups.join("tolight-base-fixture");
    std::fs::create_dir_all(base.join("data")).unwrap();
    std::fs::write(base.join("data/PG_VERSION"), "18\n").unwrap();
    std::fs::write(base.join("data/backup_manifest"), "{}").unwrap();
    let completed = tolight_core::datetime::utc_seconds(
        time::OffsetDateTime::from_unix_timestamp(now as i64).unwrap(),
    );
    let valid = serde_json::json!({"schema":"tolight-base-backup/1","status":"passed","completed_at":completed,"postgres_major":18,"data":"data","manifest_sha256":"44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a","system_identifier":"1234567890"});
    let write = |value: &Value| {
        std::fs::write(base.join("backup.json"), serde_json::to_vec(value).unwrap()).unwrap()
    };
    write(&valid);
    assert!(fixture.run(now, &[]).await.status.success());
    for (field, value) in [
        ("status", Value::from("failed")),
        ("manifest_sha256", Value::from("bad")),
        ("postgres_major", Value::from(19)),
        ("postgres_major", Value::from(true)),
        ("system_identifier", Value::from("not-a-postgres-system-id")),
        ("data", Value::from("../outside")),
        ("completed_at", Value::from("2000-01-01T00:00:00Z")),
    ] {
        let mut bad = valid.clone();
        bad[field] = value;
        write(&bad);
        assert_eq!(conditions(&fixture.run(now, &[]).await), ["backup_stale"]);
    }
    write(&valid);
    std::fs::write(base.join("data/backup_manifest"), "corrupt").unwrap();
    assert_eq!(conditions(&fixture.run(now, &[]).await), ["backup_stale"]);
    std::fs::write(base.join("data/backup_manifest"), "{}").unwrap();
    assert!(fixture.run(now, &[]).await.status.success());
    #[cfg(unix)]
    {
        std::fs::rename(base.join("backup.json"), base.join("saved-marker.json")).unwrap();
        std::os::unix::fs::symlink("saved-marker.json", base.join("backup.json")).unwrap();
        assert_eq!(conditions(&fixture.run(now, &[]).await), ["backup_stale"]);
    }
}
#[tokio::test]
async fn logical_backup_freshness_requires_a_completion_marker_and_ignores_touched_old_files() {
    let fixture = Fixture::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(&fixture.metrics, sample(100, 0, 0, 0)).unwrap();
    let dump = fixture.backups.join(LOGICAL_BACKUP);
    let marker = fixture.backups.join(format!("{LOGICAL_BACKUP}.meta.json"));
    std::fs::remove_file(&marker).unwrap();
    for name in [
        "latest.dump",
        "notes.sql.txt",
        "failed.sql",
        "failed.dump.tmp",
    ] {
        std::fs::write(fixture.backups.join(name), b"not a successful backup").unwrap();
    }
    assert_eq!(
        conditions(&fixture.run(now, &[]).await),
        ["backup_stale"],
        "even a PGDMP orphan must not suppress stale-backup alerts"
    );
    write_logical_backup(&fixture.backups, now - 100_000);
    for path in [&dump, &marker] {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now())
            .unwrap();
    }
    assert_eq!(
        conditions(&fixture.run(now, &[]).await),
        ["backup_stale"],
        "touch must not renew an old completion time"
    );
    write_logical_backup(&fixture.backups, now);
    let valid: Value = serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
    for (field, value) in [
        ("schema", Value::from("failed-backup/1")),
        ("file", Value::from("latest.dump")),
        ("sha256", Value::from("not-a-sha256")),
        ("created_at", Value::from("2026-11-15T00:00:00+00:00")),
        ("created_at", Value::from("9999-01-01T00:00:00Z")),
        ("size", Value::from(11)),
        ("size", Value::from(true)),
    ] {
        let mut malformed = valid.clone();
        malformed[field] = value;
        std::fs::write(&marker, serde_json::to_vec(&malformed).unwrap()).unwrap();
        assert_eq!(
            conditions(&fixture.run(now, &[]).await),
            ["backup_stale"],
            "invalid marker field {field} was accepted"
        );
    }
    std::fs::write(&marker, b"{\"schema\":").unwrap();
    assert_eq!(conditions(&fixture.run(now, &[]).await), ["backup_stale"]);
    std::fs::write(&marker, serde_json::to_vec(&valid).unwrap()).unwrap();
    std::fs::write(&dump, b"garbage-head").unwrap();
    assert_eq!(conditions(&fixture.run(now, &[]).await), ["backup_stale"]);
    write_logical_backup(&fixture.backups, now);
    let mut legacy = valid;
    legacy.as_object_mut().unwrap().remove("size");
    std::fs::write(&marker, serde_json::to_vec(&legacy).unwrap()).unwrap();
    assert!(
        fixture.run(now, &[]).await.status.success(),
        "old schema1 markers without size remain compatible"
    );
    #[cfg(unix)]
    {
        std::fs::rename(&dump, fixture.backups.join("saved-dump")).unwrap();
        std::os::unix::fs::symlink("saved-dump", &dump).unwrap();
        assert_eq!(conditions(&fixture.run(now, &[]).await), ["backup_stale"]);
    }
}

#[tokio::test]
async fn restarting_missing_counter_series_keeps_prior_hour_and_same_epoch_loss_is_diagnosed() {
    let fixture = Fixture::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let flags = ["--message-limit-per-hour", "2"];
    std::fs::write(&fixture.metrics, sample(100, 0, 0, 0)).unwrap();
    fixture.run(now, &flags).await;
    std::fs::write(&fixture.metrics, sample(100, 0, 0, 3)).unwrap();
    fixture.run(now + 1, &flags).await;
    let count = fixture.notifications().len();
    let restarted = sample(100, 0, 0, 3).replace(
        "tolight_process_start_timestamp_seconds 1",
        "tolight_process_start_timestamp_seconds 2",
    );
    let missing = restarted
        .lines()
        .filter(|line| !line.starts_with("tolight_message_limit_rejections_total"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&fixture.metrics, &missing).unwrap();
    assert_eq!(
        conditions(&fixture.run(now + 2, &flags).await),
        ["message_limit_hourly:alice"]
    );
    assert_eq!(fixture.notifications().len(), count);
    std::fs::write(
        &fixture.metrics,
        restarted.replace(
            "rejections_total{agent_id=\"alice\"} 3",
            "rejections_total{agent_id=\"alice\"} 4",
        ),
    )
    .unwrap();
    let output = fixture.run(now + 3, &flags).await;
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["hourly_counter_estimates"]["tolight_message_limit_rejections_total{\"agent_id\": \"alice\"}"],
        7.0
    );
    std::fs::write(&fixture.metrics, &missing).unwrap();
    assert!(
        conditions(&fixture.run(now + 4, &flags).await).contains(&"metrics_missing_series".into())
    );
    let mut state: Value = serde_json::from_slice(&std::fs::read(&fixture.state).unwrap()).unwrap();
    state["counter_windows"]["tolight_message_limit_rejections_total{\"agent_id\": \"alice\"}"] =
        serde_json::json!([[now, -1]]);
    std::fs::write(&fixture.state, serde_json::to_vec(&state).unwrap()).unwrap();
    std::fs::write(&fixture.metrics, restarted).unwrap();
    assert_eq!(
        conditions(&fixture.run(now + 5, &flags).await),
        ["metrics_unavailable"]
    );
}
