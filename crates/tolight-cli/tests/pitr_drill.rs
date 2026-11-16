use chrono::{Duration, SecondsFormat, Utc};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration as Wait, Instant},
};
use tempfile::TempDir;
use tolight_coordinator_store::{ApplicationInput, ApplyLimits, SendInput, SendLimits, Store};
use tolight_core::{models::AgentContext, signing::public_key_from_seed};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}
#[test]
fn wal_archive_retry_repairs_interrupted_metadata_but_never_replaces_conflicting_wal() {
    let directory = tempfile::tempdir().unwrap();
    let archive = directory.path().join("archive");
    let source = directory.path().join("source.wal");
    let destination = directory.path().join("restored.wal");
    let name = "000000010000000000000001";
    fs::create_dir(&archive).unwrap();
    fs::write(&source, b"immutable WAL fixture").unwrap();
    fs::copy(&source, archive.join(name)).unwrap();
    let metadata = archive.join(format!("{name}.meta.json"));
    fs::write(&metadata, b"{\"schema\":").unwrap();
    let run = |mode: &str| {
        Command::new(root().join("scripts/wal-archive.sh"))
            .args([mode, "--archive-dir"])
            .arg(&archive)
            .args(["--name", name])
            .arg(if mode == "archive" {
                "--source"
            } else {
                "--destination"
            })
            .arg(if mode == "archive" {
                &source
            } else {
                &destination
            })
            .output()
            .unwrap()
    };
    assert!(run("archive").status.success());
    let complete = fs::read(&metadata).unwrap();
    assert!(run("archive").status.success());
    assert_eq!(fs::read(&metadata).unwrap(), complete);
    assert!(run("restore").status.success());
    assert_eq!(fs::read(&destination).unwrap(), fs::read(&source).unwrap());
    fs::write(&source, b"a different cluster's WAL").unwrap();
    assert!(!run("archive").status.success());
    assert_eq!(
        fs::read(archive.join(name)).unwrap(),
        b"immutable WAL fixture"
    );
    assert_eq!(fs::read(&metadata).unwrap(), complete);
    assert!(fs::read_dir(&archive).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")
    }));
}
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
}
fn shell(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn pg(name: &str) -> PathBuf {
    let local = root().join(".local/pg18/bin").join(name);
    if local.exists() {
        local
    } else {
        PathBuf::from(name)
    }
}
fn json(path: impl AsRef<Path>) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "stdout={} stderr={} private={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        diagnostics(&output)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
fn diagnostics(output: &Output) -> String {
    let Ok(value) = serde_json::from_slice::<Value>(&output.stdout) else {
        return String::new();
    };
    let Some(path) = value["report"].as_str() else {
        return String::new();
    };
    let path = Path::new(path);
    let mut result = fs::read_to_string(path).unwrap();
    for name in [
        "pg-start.stdout.stderr",
        "postgres.log",
        "apply.stdout.stderr",
        "verify-base.stdout.stderr",
    ] {
        if let Ok(text) = fs::read_to_string(path.parent().unwrap().join(name)) {
            result.push_str(&text);
        }
    }
    result.replace("private-pitr-owner@example.invalid", "<private-contact>")
}

struct Primary {
    directory: TempDir,
    url: String,
    config: PathBuf,
    public: PathBuf,
    archive: PathBuf,
}
impl Primary {
    fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("m4-pitr-source.")
            .tempdir_in(root().join(".local"))
            .unwrap();
        let data = directory.path().join("data");
        let socket = directory.path().join("socket");
        fs::create_dir(&socket).unwrap();
        let public = directory.path().join("public");
        let archive = directory.path().join("archive");
        fs::create_dir(&archive).unwrap();
        let output = Command::new(pg("initdb"))
            .args([
                "-U",
                "tolight",
                "--encoding=UTF8",
                "--locale=C",
                "--auth-local=trust",
                "--auth-host=reject",
                "--wal-segsize=1",
                "-D",
            ])
            .arg(&data)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let archive_command = format!(
            "{} archive --archive-dir {} --source '%p' --name '%f' --public-dir {}",
            shell(root().join("scripts/wal-archive.sh").to_str().unwrap()),
            shell(archive.to_str().unwrap()),
            shell(public.to_str().unwrap()),
        );
        let config_path = data.join("postgresql.conf");
        let mut settings = fs::read_to_string(&config_path).unwrap();
        settings.push_str(&format!(
            "\narchive_mode=on\narchive_timeout='1s'\nwal_level=replica\nmax_wal_senders=4\narchive_command={}\n",
            quote(&archive_command)
        ));
        fs::write(&config_path, settings).unwrap();
        let output = Command::new(pg("pg_ctl"))
            .arg("-D")
            .arg(&data)
            .arg("-l")
            .arg(directory.path().join("postgres.log"))
            .args(["-t", "15", "-w", "-o"])
            .arg(format!(
                "-h '' -k '{}' -p 5432 -c timezone=UTC",
                socket.display()
            ))
            .arg("start")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let env: BTreeMap<String, String> = BTreeMap::from([
            ("PGHOST".into(), socket.to_str().unwrap().into()),
            ("PGPORT".into(), "5432".into()),
            ("PGUSER".into(), "tolight".into()),
            ("PGDATABASE".into(), "postgres".into()),
        ]);
        assert!(
            Command::new(pg("createdb"))
                .arg("tolight_pitr")
                .envs(&env)
                .status()
                .unwrap()
                .success()
        );
        let url = format!(
            "postgresql://tolight@localhost:5432/tolight_pitr?host={}",
            percent_encoding::utf8_percent_encode(
                socket.to_str().unwrap(),
                percent_encoding::NON_ALPHANUMERIC
            )
        );
        let config = directory.path().join("source.toml");
        fs::write(
            &config,
            format!(
                "[database]\nsecret_cmd=['/usr/bin/printenv','DATABASE_URL']\n[public]\ndir={}\n",
                serde_json::to_string(public.to_str().unwrap()).unwrap()
            ),
        )
        .unwrap();
        Self {
            directory,
            url,
            config,
            public,
            archive,
        }
    }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_tolight"))
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .env("DATABASE_URL", &self.url)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
    fn script(&self, name: &str, args: &[&str]) -> Output {
        Command::new(root().join("scripts").join(name))
            .args(args)
            .env("DATABASE_URL", &self.url)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
    fn backup(&self) -> PathBuf {
        let value = success(self.script(
            "physical-backup.sh",
            &[
                "--config",
                self.config.to_str().unwrap(),
                "--archive-dir",
                self.archive.to_str().unwrap(),
                "--output-dir",
                self.directory.path().join("base").to_str().unwrap(),
            ],
        ));
        PathBuf::from(value["base_backup"].as_str().unwrap())
    }
    fn persistent_backup(&self) -> PathBuf {
        let value = success(self.script(
            "physical-backup.sh",
            &[
                "--config",
                self.config.to_str().unwrap(),
                "--archive-dir",
                self.archive.to_str().unwrap(),
            ],
        ));
        PathBuf::from(value["base_backup"].as_str().unwrap())
    }
    fn drill(&self, backup: &Path, target: &str, approvals: &[String]) -> Output {
        let mut arguments = vec![
            "--mode",
            "pitr",
            "--base-backup",
            backup.to_str().unwrap(),
            "--archive-dir",
            self.archive.to_str().unwrap(),
            "--source-config",
            self.config.to_str().unwrap(),
            "--source-public-dir",
            self.public.to_str().unwrap(),
            "--restore-target",
            target,
            "--tolight",
            env!("CARGO_BIN_EXE_tolight"),
        ];
        for id in approvals {
            arguments.extend(["--approve-active", id]);
        }
        self.script("restore-drill.sh", &arguments)
    }
    fn wal_names(&self) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(&self.archive)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.len() == 24 && name.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .filter(|name| self.archive.join(format!("{name}.meta.json")).exists())
            .collect();
        names.sort();
        names
    }
    fn wait_for_more_wal(&self, _previous: &[String]) {
        // Wait for the segment containing the final fixture commit, not merely
        // any new archive file (a base-backup boundary can arrive asynchronously).
        let segment = Command::new(pg("psql"))
            .args([
                "-w",
                "-XAt",
                "--dbname",
                &self.url,
                "-c",
                "SELECT pg_walfile_name(pg_current_wal_lsn()-1)",
            ])
            .output()
            .unwrap();
        assert!(segment.status.success());
        let segment = String::from_utf8(segment.stdout)
            .unwrap()
            .trim()
            .to_string();
        assert!(
            Command::new(pg("psql"))
                .args([
                    "-w",
                    "-XAt",
                    "--dbname",
                    &self.url,
                    "-c",
                    "SELECT pg_switch_wal()"
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Wait::from_secs(12);
        while Instant::now() < deadline {
            if self.wal_names().contains(&segment) {
                return;
            }
            std::thread::sleep(Wait::from_millis(50));
        }
        panic!(
            "archive_timeout did not produce a complete WAL; source log={}",
            fs::read_to_string(self.directory.path().join("postgres.log")).unwrap()
        );
    }
}
impl Drop for Primary {
    fn drop(&mut self) {
        let _ = Command::new(pg("pg_ctl"))
            .arg("-D")
            .arg(self.directory.path().join("data"))
            .args(["-m", "fast", "-t", "15", "-w", "stop"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

async fn apply(store: &Store, id: &str, seed: u8, activate: bool) -> AgentContext {
    let public_key = public_key_from_seed(&[seed; 32]);
    store
        .apply(
            ApplicationInput {
                agent_id: id.into(),
                display_name: id.into(),
                display_name_key: id.into(),
                bio: "本地WAL恢复演练".into(),
                owner_contact: "private-pitr-owner@example.invalid".into(),
                framework_label: None,
                public_key: public_key.clone(),
            },
            ApplyLimits::default(),
        )
        .await
        .unwrap();
    let context = AgentContext {
        agent_id: id.into(),
        public_key,
    };
    if activate {
        approve(store, &context).await;
    }
    context
}
async fn approve(store: &Store, context: &AgentContext) {
    // This recovery fixture seeds its admission basis; real HTTP conformance is
    // separately exercised by reference-client/conformance_e2e.rs.
    store
        .conformance_transaction(context, |data| {
            data.agent.conformance_passed_at = Some(Utc::now());
            data.agent.conformance_suite_version =
                Some(tolight_core::CONFORMANCE_SUITE_VERSION.into());
        })
        .await
        .unwrap();
    store
        .approve(&store.review(&context.agent_id).await.unwrap(), true)
        .await
        .unwrap();
}
async fn message(store: &Store, context: &AgentContext, id: &str, text: &str) -> String {
    store
        .send(
            SendInput {
                agent_id: context.agent_id.clone(),
                public_key: context.public_key.clone(),
                client_message_id: id.into(),
                text: Ok(text.into()),
                reply_to_event_id: None,
                mentions: vec![],
            },
            SendLimits {
                messages_per_minute: 1000,
                messages_per_day: 10000,
                max_mentions_per_message: 3,
            },
        )
        .await
        .unwrap()
        .event_id
}
fn publish(primary: &Primary) {
    let output = primary.cli(&["publish", "--rebuild"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn report(output: &Output) -> Value {
    let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
    json(summary["report"].as_str().unwrap())
}

#[tokio::test]
async fn real_archived_wal_pitr_requires_fresh_approval_and_preserves_current_controls() {
    let primary = Primary::new();
    let store = Store::connect(&primary.url, 10).await.unwrap();
    store.migrate().await.unwrap();
    let alice = apply(&store, "alice", 71, true).await;
    apply(&store, "bobby", 72, true).await;
    let carol = apply(&store, "carol", 73, false).await;
    let unique = message(&store, &alice, "unique", "仅此一份等待撤回的WAL测试正文").await;
    for index in 0..200 {
        message(
            &store,
            &alice,
            &format!("before-{index}"),
            "恢复点之前的公开测试消息",
        )
        .await;
    }
    let backup = primary.persistent_backup();
    let marker = json(backup.join("backup.json"));
    let created =
        chrono::DateTime::parse_from_rfc3339(marker["completed_at"].as_str().unwrap()).unwrap();
    let stamp = created.timestamp() as f64 + created.timestamp_subsec_nanos() as f64 / 1e9;
    assert!(
        tolight_cli::ops::storage::backup::successful_backup_times(
            backup.parent().unwrap(),
            Utc::now().timestamp() as f64 + 1.0
        )
        .contains(&stamp)
    );
    let before = primary.wal_names();
    message(&store, &alice, "at-target", "必须保留的恢复点前消息").await;
    let target = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);
    tokio::time::sleep(Wait::from_millis(25)).await;
    let lost = message(
        &store,
        &alice,
        "after-target",
        "恢复点后的已知消息，应列入损失",
    )
    .await;
    let new_key = public_key_from_seed(&[74; 32]);
    store.set_key("alice", &new_key).await.unwrap();
    let until = Utc::now() + Duration::hours(1);
    store.mute("alice", Some(until), "spam").await.unwrap();
    let until = store.review("alice").await.unwrap().muted_until.unwrap();
    store.kick("bobby", "spam").await.unwrap();
    approve(&store, &carol).await;
    let dead = apply(&store, "final-left", 75, true).await;
    store.kick(&dead.agent_id, "other").await.unwrap();
    store.retract(&unique, "owner_request").await.unwrap();
    store.pause(true, "PITR fixture frozen").await.unwrap();
    store.applications_open(false).await.unwrap();
    primary.wait_for_more_wal(&before);
    publish(&primary);
    let source_audits: i64 = sqlx::query_scalar("SELECT count(*) FROM operator_actions")
        .fetch_one(store.pool())
        .await
        .unwrap();
    let missing = primary.drill(&backup, &target, &[]);
    assert_eq!(missing.status.code(), Some(2), "{}", diagnostics(&missing));
    let missing_report = report(&missing);
    assert_eq!(missing_report["status"], "needs_approval");
    assert!(missing_report["target_data_removed"].as_bool().unwrap());
    assert!(
        !Path::new(missing_report["work_dir"].as_str().unwrap())
            .join("public")
            .exists()
    );
    let id = store
        .review("carol")
        .await
        .unwrap()
        .application_id
        .to_string();
    let successful = primary.drill(&backup, &target, &[id]);
    let summary = success(successful);
    let proof = json(summary["report"].as_str().unwrap());
    assert_eq!(proof["status"], "passed");
    assert_eq!(proof["recovery_target_reached"], true);
    assert_eq!(proof["target_postgres"]["archive_mode"], "off");
    assert_eq!(proof["target_postgres_stopped"], true);
    assert_eq!(proof["target_data_removed"], true);
    assert_eq!(proof["source_public_unchanged"], true);
    assert_eq!(proof["current_controls_verified"], true);
    assert_eq!(proof["target_serve"]["health_status"], 200);
    assert_eq!(proof["target_serve"]["normal_shutdown"], true);
    assert!(
        proof["known_message_event_ids_not_restored"]
            .as_array()
            .unwrap()
            .contains(&Value::String(lost))
    );
    assert!(proof["local_targets"]["met"].as_bool().unwrap());
    let work = Path::new(proof["work_dir"].as_str().unwrap());
    let current = json(work.join("verified-controls.json"));
    let agents = current["agents"].as_array().unwrap();
    let restored_alice = agents.iter().find(|a| a["agent_id"] == "alice").unwrap();
    assert_eq!(restored_alice["public_key"], new_key);
    assert_eq!(
        restored_alice["muted_until"],
        serde_json::to_value(until).unwrap()
    );
    assert_eq!(
        agents.iter().find(|a| a["agent_id"] == "bobby").unwrap()["state"],
        "LEFT"
    );
    assert_eq!(
        agents
            .iter()
            .find(|a| a["agent_id"] == "final-left")
            .unwrap()["state"],
        "LEFT"
    );
    let public =
        fs::read_to_string(work.join("public/data/chunks/000000000001-000000000200.json")).unwrap();
    assert!(!public.contains("仅此一份等待撤回的WAL测试正文"));
    let after_audits: i64 = sqlx::query_scalar("SELECT count(*) FROM operator_actions")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(source_audits, after_audits);
    fs::write(
        root().join(".local/logs/m4-pitr-latest.json"),
        serde_json::to_vec_pretty(&proof).unwrap(),
    )
    .unwrap();
    store.pool().close().await;
}

#[tokio::test]
async fn corrupt_missing_wal_and_unattainable_target_never_publish_or_start_the_app() {
    let primary = Primary::new();
    let store = Store::connect(&primary.url, 5).await.unwrap();
    store.migrate().await.unwrap();
    let alice = apply(&store, "alice", 81, true).await;
    message(&store, &alice, "before", "基础备份之前").await;
    let backup = primary.backup();
    let previous = primary.wal_names();
    let target = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);
    tokio::time::sleep(Wait::from_millis(25)).await;
    message(&store, &alice, "after", "确保目标之后有提交").await;
    primary.wait_for_more_wal(&previous);
    publish(&primary);
    let newest = primary.wal_names().pop().unwrap();
    let file = primary.archive.join(&newest);
    let bytes = fs::read(&file).unwrap();
    fs::write(&file, "corrupted WAL").unwrap();
    let output = primary.drill(&backup, &target, &[]);
    assert_eq!(output.status.code(), Some(1));
    let proof = report(&output);
    assert_eq!(proof["phase"], "wal_validation");
    assert!(proof.get("target_serve").is_none());
    assert!(
        !Path::new(proof["work_dir"].as_str().unwrap())
            .join("public")
            .exists()
    );
    fs::write(&file, &bytes).unwrap();
    fs::remove_file(&file).unwrap();
    fs::remove_file(primary.archive.join(format!("{newest}.meta.json"))).unwrap();
    let output = primary.drill(&backup, &target, &[]);
    assert_eq!(output.status.code(), Some(1), "{}", diagnostics(&output));
    let proof = report(&output);
    assert!(proof.get("target_serve").is_none());
    assert_eq!(proof["target_data_removed"], true);
    fs::write(&file, &bytes).unwrap();
    let output = primary.script(
        "wal-archive.sh",
        &[
            "archive",
            "--archive-dir",
            primary.archive.to_str().unwrap(),
            "--source",
            file.to_str().unwrap(),
            "--name",
            &newest,
        ],
    );
    assert!(output.status.success());
    let future = (Utc::now() + Duration::seconds(1)).to_rfc3339_opts(SecondsFormat::Micros, true);
    tokio::time::sleep(Wait::from_millis(1250)).await;
    let output = primary.drill(&backup, &future, &[]);
    assert_eq!(output.status.code(), Some(1));
    let proof = report(&output);
    assert_eq!(proof["phase"], "postgresql_recovery");
    assert!(proof.get("target_serve").is_none());
    store.pool().close().await;
}

#[test]
fn archive_is_private_idempotent_and_rejects_conflicts_symlinks_and_public_paths() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    let archive = directory.path().join("archive");
    fs::write(&source, "WAL fixture bytes").unwrap();
    let name = "000000010000000000000001";
    let run = |destination: &Path, filename: &str| {
        Command::new(root().join("scripts/wal-archive.sh"))
            .args(["archive", "--archive-dir"])
            .arg(destination)
            .arg("--source")
            .arg(&source)
            .args(["--name", filename])
            .arg("--public-dir")
            .arg(directory.path().join("public"))
            .output()
            .unwrap()
    };
    assert!(run(&archive, name).status.success());
    assert!(run(&archive, name).status.success());
    fs::write(&source, "different WAL fixture bytes").unwrap();
    assert!(!run(&archive, name).status.success());
    assert!(!run(&archive, "../../escape").status.success());
    assert!(
        !run(&directory.path().join("public/archive"), name)
            .status
            .success()
    );
    #[cfg(unix)]
    {
        use std::os::unix::{fs::PermissionsExt, fs::symlink};
        assert_eq!(
            fs::metadata(archive.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let alias = directory.path().join("alias");
        symlink(&archive, &alias).unwrap();
        assert!(!run(&alias, name).status.success());
    }
}

#[tokio::test]
async fn lagging_published_tree_before_target_does_not_hide_unpublished_message_losses() {
    let primary = Primary::new();
    let store = Store::connect(&primary.url, 5).await.unwrap();
    store.migrate().await.unwrap();
    let alice = apply(&store, "alice", 91, true).await;
    message(&store, &alice, "visible", "网页中已发布的旧前缀").await;
    publish(&primary);
    let published_head = json(primary.public.join("data/manifest.json"))["newest_seq"]
        .as_i64()
        .unwrap();
    let backup = primary.backup();
    let before = primary.wal_names();
    let retained = message(
        &store,
        &alice,
        "unpublished-before-target",
        "恢复点前尚未发布的消息仍须恢复",
    )
    .await;
    let target = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);
    tokio::time::sleep(Wait::from_millis(25)).await;
    let lost_a = message(
        &store,
        &alice,
        "unpublished-after-a",
        "未发布但必须统计的损失甲",
    )
    .await;
    let lost_b = message(
        &store,
        &alice,
        "unpublished-after-b",
        "未发布但必须统计的损失乙",
    )
    .await;
    let new_key = public_key_from_seed(&[92; 32]);
    store.set_key("alice", &new_key).await.unwrap();
    store.mute("alice", None, "other").await.unwrap();
    primary.wait_for_more_wal(&before);
    // Deliberately do not run publish again: public is older than target T.
    let output = primary.drill(&backup, &target, &[]);
    let result = success(output);
    let proof = json(result["report"].as_str().unwrap());
    assert_eq!(proof["status"], "passed");
    assert_eq!(proof["source_reference"]["published_head"], published_head);
    assert_eq!(proof["source_reference"]["published_lag_events"], 3);
    assert_eq!(
        proof["source_reference"]["database_anchor_matches_control_snapshot"],
        true
    );
    assert_eq!(proof["source_public_unchanged"], true);
    let lost = proof["known_message_event_ids_not_restored"]
        .as_array()
        .unwrap();
    assert_eq!(lost.len(), 2);
    assert!(lost.contains(&Value::String(lost_a)));
    assert!(lost.contains(&Value::String(lost_b)));
    assert!(!lost.contains(&Value::String(retained.clone())));
    let work = Path::new(proof["work_dir"].as_str().unwrap());
    let restored = json(work.join("public/data/chunks/000000000001-000000000200.json"));
    assert!(
        restored["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["event_id"] == retained)
    );
    let controls = json(work.join("verified-controls.json"));
    let restored_alice = controls["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["agent_id"] == "alice")
        .unwrap();
    assert_eq!(restored_alice["public_key"], new_key);
    assert_eq!(restored_alice["muted_indefinitely"], true);
    assert_eq!(
        json(primary.public.join("data/manifest.json"))["newest_seq"],
        published_head
    );
    fs::write(
        root().join(".local/logs/m4-pitr-lag-latest.json"),
        serde_json::to_vec_pretty(&proof).unwrap(),
    )
    .unwrap();
    store.pool().close().await;
}

#[tokio::test]
async fn wrong_cluster_and_unreadable_source_cannot_bypass_current_control_verification() {
    let first = Primary::new();
    let first_store = Store::connect(&first.url, 3).await.unwrap();
    first_store.migrate().await.unwrap();
    let backup = first.backup();
    let second = Primary::new();
    let second_store = Store::connect(&second.url, 3).await.unwrap();
    second_store.migrate().await.unwrap();
    publish(&second);
    let target = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);
    let output = second.script(
        "pitr-drill.sh",
        &[
            "--base-backup",
            backup.to_str().unwrap(),
            "--archive-dir",
            first.archive.to_str().unwrap(),
            "--source-config",
            second.config.to_str().unwrap(),
            "--source-public-dir",
            second.public.to_str().unwrap(),
            "--restore-target",
            &target,
            "--tolight",
            env!("CARGO_BIN_EXE_tolight"),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let proof = report(&output);
    assert_eq!(proof["phase"], "base_backup_validation");
    assert!(
        proof["error"]
            .as_str()
            .unwrap()
            .contains("current source cluster")
    );
    assert!(proof.get("target_serve").is_none());
    let broken = second.directory.path().join("unreadable-source.toml");
    fs::write(
        &broken,
        format!(
            "[database]\nsecret_cmd=['/usr/bin/false']\n[public]\ndir={}\n",
            serde_json::to_string(second.public.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();
    let output = second.script(
        "pitr-drill.sh",
        &[
            "--base-backup",
            backup.to_str().unwrap(),
            "--archive-dir",
            first.archive.to_str().unwrap(),
            "--source-config",
            broken.to_str().unwrap(),
            "--source-public-dir",
            second.public.to_str().unwrap(),
            "--restore-target",
            &target,
            "--tolight",
            env!("CARGO_BIN_EXE_tolight"),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let proof = report(&output);
    assert_eq!(proof["known_current_controls"], false);
    assert_eq!(proof["unknown_controls"], true);
    assert!(proof.get("unknown_control_interval").is_some());
    assert!(proof.get("target_serve").is_none());
    assert!(
        !Path::new(proof["work_dir"].as_str().unwrap())
            .join("public")
            .exists()
    );
    let output = second.script(
        "physical-backup.sh",
        &[
            "--config",
            second.config.to_str().unwrap(),
            "--output-dir",
            second.public.join("private-backup").to_str().unwrap(),
            "--archive-dir",
            second.archive.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(!second.public.join("private-backup").exists());
    first_store.pool().close().await;
    second_store.pool().close().await;
}
