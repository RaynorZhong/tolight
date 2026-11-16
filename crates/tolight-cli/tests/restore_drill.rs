use chrono::{Duration, SecondsFormat, Utc};
use serde_json::Value;
use sqlx::{ConnectOptions, PgPool};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
use tolight_coordinator_store::{ApplicationInput, ApplyLimits, SendInput, SendLimits, Store};
use tolight_core::models::AgentContext;
use tolight_core::signing::public_key_from_seed;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn script(name: &str, url: &str, args: &[&str]) -> Output {
    Command::new(root().join("scripts").join(name))
        .args(args)
        .env("DATABASE_URL", url)
        .current_dir(root())
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn cli(config: &Path, url: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tolight"))
        .args(["--config", config.to_str().unwrap()])
        .args(args)
        .env("DATABASE_URL", url)
        .current_dir(root())
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn successful(output: Output) -> Value {
    assert!(
        output.status.success(),
        "stdout={} stderr={} drill={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        drill_diagnostics(&output)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn drill_diagnostics(output: &Output) -> String {
    let Ok(value) = serde_json::from_slice::<Value>(&output.stdout) else {
        return String::new();
    };
    let Some(report) = value["report"].as_str() else {
        return String::new();
    };
    let path = Path::new(report);
    let mut text = fs::read_to_string(path).unwrap();
    for entry in fs::read_dir(path.parent().unwrap()).unwrap() {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|extension| extension == "stderr")
        {
            text.push_str(&fs::read_to_string(path).unwrap());
        }
    }
    text.replace("PRIVATE-OWNER-CONTACT@example.invalid", "<private-contact>")
}

fn read_json(path: impl AsRef<Path>) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn config(directory: &Path) -> (PathBuf, PathBuf) {
    let public = directory.join("source-public");
    let config = directory.join("source.toml");
    fs::write(
        &config,
        format!(
            "[database]\nsecret_cmd=['/usr/bin/printenv','DATABASE_URL']\n[public]\ndir={}\n",
            serde_json::to_string(public.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();
    (config, public)
}

async fn apply(store: &Store, id: &str, seed: u8) -> AgentContext {
    let public_key = public_key_from_seed(&[seed; 32]);
    store
        .apply(
            ApplicationInput {
                agent_id: id.into(),
                display_name: id.into(),
                display_name_key: id.into(),
                bio: "恢复演练脚本".into(),
                owner_contact: "PRIVATE-OWNER-CONTACT@example.invalid".into(),
                framework_label: None,
                public_key: public_key.clone(),
            },
            ApplyLimits::default(),
        )
        .await
        .unwrap();
    AgentContext {
        agent_id: id.into(),
        public_key,
    }
}

async fn approve(store: &Store, context: &AgentContext) {
    // Seed a passed conformance basis only for this isolated recovery fixture;
    // reference-client HTTP tests exercise the real four-event examiner.
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

fn drill_args<'a>(
    backup: &'a Path,
    config: &'a Path,
    public: &'a Path,
    target: &'a str,
    work: &'a Path,
) -> Vec<&'a str> {
    vec![
        "--backup",
        backup.to_str().unwrap(),
        "--source-config",
        config.to_str().unwrap(),
        "--source-public-dir",
        public.to_str().unwrap(),
        "--restore-target",
        target,
        "--work-dir",
        work.to_str().unwrap(),
        "--tolight",
        env!("CARGO_BIN_EXE_tolight"),
    ]
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn real_backup_and_six_step_restore_require_new_active_identity_approval(pool: PgPool) {
    let store = Store::from_pool(pool.clone());
    let directory = tempfile::tempdir().unwrap();
    let (config, public) = config(directory.path());
    let url = pool.connect_options().to_url_lossy().to_string();
    let alice_old = apply(&store, "alice", 131).await;
    let old_application = store.review("alice").await.unwrap().application_id;
    let bob = apply(&store, "bob", 132).await;
    approve(&store, &bob).await;
    let cara = apply(&store, "cara", 133).await;
    approve(&store, &cara).await;
    let mut retract_id = String::new();
    for index in 0..203 {
        let sent = store
            .send(
                SendInput {
                    agent_id: bob.agent_id.clone(),
                    public_key: bob.public_key.clone(),
                    client_message_id: format!("backup-{index}"),
                    text: Ok(if index == 0 {
                        "NEVER-RESTORE-THIS-TEXT".into()
                    } else {
                        format!("保留消息 {index}")
                    }),
                    reply_to_event_id: None,
                    mentions: vec![],
                },
                SendLimits {
                    messages_per_minute: 1000,
                    messages_per_day: 1000,
                    ..SendLimits::default()
                },
            )
            .await
            .unwrap();
        if index == 0 {
            retract_id = sent.event_id;
        }
    }
    let backups = directory.path().join("backups");
    let backup_result = successful(script(
        "backup.sh",
        &url,
        &[
            "--config",
            config.to_str().unwrap(),
            "--output-dir",
            backups.to_str().unwrap(),
        ],
    ));
    let backup = PathBuf::from(backup_result["backup"].as_str().unwrap());
    assert_eq!(&fs::read(&backup).unwrap()[..5], b"PGDMP");
    let target = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    store.owner_leave(&alice_old, "withdraw").await.unwrap();
    let alice = apply(&store, "alice", 134).await;
    approve(&store, &alice).await;
    let alice_application = store.review("alice").await.unwrap().application_id;
    assert_ne!(old_application, alice_application);
    let dave = apply(&store, "dave", 135).await;
    approve(&store, &dave).await;
    let dave_application = store.review("dave").await.unwrap().application_id;
    let new_key = public_key_from_seed(&[136; 32]);
    store.set_key("bob", &new_key).await.unwrap();
    let muted_until = Utc::now() + Duration::hours(2);
    store.mute("bob", Some(muted_until), "spam").await.unwrap();
    let muted_until = store.review("bob").await.unwrap().muted_until.unwrap();
    store.owner_leave(&cara, "leave").await.unwrap();
    store.retract(&retract_id, "owner_request").await.unwrap();
    store.pause(true, "other").await.unwrap();
    store.applications_open(false).await.unwrap();
    let publish = cli(&config, &url, &["publish", "--rebuild"]);
    assert!(publish.status.success(), "{:?}", publish);
    let source_count: i64 = sqlx::query_scalar("SELECT count(*) FROM operator_actions")
        .fetch_one(&pool)
        .await
        .unwrap();
    let work = directory.path().join("drills");
    let mut args = drill_args(&backup, &config, &public, &target, &work);
    let refused = script("restore-drill.sh", &url, &args);
    assert_eq!(
        refused.status.code(),
        Some(2),
        "{}",
        drill_diagnostics(&refused)
    );
    assert!(!String::from_utf8_lossy(&refused.stdout).contains("PRIVATE-OWNER"));
    assert!(!String::from_utf8_lossy(&refused.stderr).contains("PRIVATE-OWNER"));
    let refusal: Value = serde_json::from_slice(&refused.stdout).unwrap();
    let refusal_report = read_json(refusal["report"].as_str().unwrap());
    let missing = refusal_report["missing_approvals"].as_array().unwrap();
    assert_eq!(missing.len(), 2);
    assert!(missing.contains(&Value::String(alice_application.to_string())));
    assert!(missing.contains(&Value::String(dave_application.to_string())));
    assert!(
        refusal_report["temporary_database_removed"]
            .as_bool()
            .unwrap()
    );
    assert!(
        !Path::new(refusal_report["work_dir"].as_str().unwrap())
            .join("public")
            .exists()
    );

    let alice_approval = alice_application.to_string();
    let dave_approval = dave_application.to_string();
    args.extend([
        "--approve-active",
        &alice_approval,
        "--approve-active",
        &dave_approval,
    ]);
    let completed = successful(script("restore-drill.sh", &url, &args));
    assert_eq!(completed["status"], "passed");
    let report_path = Path::new(completed["report"].as_str().unwrap());
    let report = read_json(report_path);
    assert_eq!(report["steps"].as_array().unwrap().len(), 6);
    assert_eq!(report["scope"], "logical_no_wal");
    assert_eq!(report["temporary_database_removed"], true);
    assert_eq!(report["current_controls_verified"], true);
    assert_eq!(report["source_public_unchanged"], true);
    assert_eq!(report["idempotence_verified"], true);
    assert_eq!(report["target_serve"]["health_status"], 200);
    assert_eq!(report["target_serve"]["unsigned_participant_status"], 401);
    assert_eq!(report["target_serve"]["normal_shutdown"], true);
    assert_eq!(report["target_serve"]["exit_code"], 0);
    assert_eq!(report["target_serve"]["process_removed"], true);
    assert_eq!(
        report["comparison"]["full_chunks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        report["comparison"]["full_chunks"][0]["retraction_exception"],
        serde_json::json!([retract_id])
    );
    let verified = read_json(report_path.parent().unwrap().join("verified-controls.json"));
    let agents = verified["agents"].as_array().unwrap();
    let bob = agents.iter().find(|a| a["agent_id"] == "bob").unwrap();
    assert_eq!(bob["public_key"], new_key);
    assert_eq!(
        bob["muted_until"],
        serde_json::to_value(muted_until).unwrap()
    );
    let cara = agents.iter().find(|a| a["agent_id"] == "cara").unwrap();
    assert_eq!(cara["state"], "LEFT");
    assert!(cara["public_key"].is_null());
    let alice = agents.iter().find(|a| a["agent_id"] == "alice").unwrap();
    assert_eq!(alice["application_id"], alice_approval);
    let rebuilt = report_path.parent().unwrap().join("public");
    let published_agents = read_json(rebuilt.join("data/agents.json"));
    assert!(
        !published_agents["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["agent_id"] == "cara")
    );
    for entry in fs::read_dir(rebuilt.join("data/chunks")).unwrap() {
        assert!(
            !fs::read_to_string(entry.unwrap().path())
                .unwrap()
                .contains("NEVER-RESTORE-THIS-TEXT")
        );
    }
    assert_eq!(store.review("bob").await.unwrap().public_key, Some(new_key));
    assert_eq!(store.review("cara").await.unwrap().state, "LEFT");
    assert_eq!(
        source_count,
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM operator_actions")
            .fetch_one(&pool)
            .await
            .unwrap()
    );
    let left_over: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_database WHERE datname=$1")
        .bind(report["temporary_database"].as_str().unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left_over, 0);
    // Even a chunk with a permitted retraction must reject an unrelated edit.
    let source_chunk = public.join("data/chunks/000000000001-000000000200.json");
    let mut tampered = read_json(&source_chunk);
    tampered["events"][3]["text"] = Value::String("unrelated source corruption".into());
    fs::write(&source_chunk, serde_json::to_vec(&tampered).unwrap()).unwrap();
    let mismatch = script("restore-drill.sh", &url, &args);
    assert_eq!(mismatch.status.code(), Some(1));
    let mismatch: Value = serde_json::from_slice(&mismatch.stdout).unwrap();
    let mismatch_report = read_json(mismatch["report"].as_str().unwrap());
    assert_eq!(mismatch_report["temporary_database_removed"], true);
    assert_eq!(mismatch_report["status"], "failed");
    assert!(
        mismatch_report["error"]
            .as_str()
            .unwrap()
            .contains("unrelated event")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [
            backup,
            report_path.to_path_buf(),
            report_path.parent().unwrap().join("controls.json"),
            report_path.parent().unwrap().join("plan.json"),
            report_path.parent().unwrap().join("target.toml"),
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            fs::metadata(report_path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn backup_rejects_public_aliases_and_retention_only_prunes_owned_complete_pairs(
    pool: PgPool,
) {
    let directory = tempfile::tempdir().unwrap();
    let (config, public) = config(directory.path());
    fs::create_dir(&public).unwrap();
    let url = pool.connect_options().to_url_lossy().to_string();
    let refused = script(
        "backup.sh",
        &url,
        &[
            "--config",
            config.to_str().unwrap(),
            "--output-dir",
            public.join("private").to_str().unwrap(),
        ],
    );
    assert!(!refused.status.success());
    assert!(!public.join("private").exists());
    #[cfg(unix)]
    {
        let alias = directory.path().join("public-alias");
        std::os::unix::fs::symlink(&public, &alias).unwrap();
        let refused = script(
            "backup.sh",
            &url,
            &[
                "--config",
                config.to_str().unwrap(),
                "--output-dir",
                alias.to_str().unwrap(),
            ],
        );
        assert!(!refused.status.success());
    }
    let backups = directory.path().join("backups");
    let first = successful(script(
        "backup.sh",
        &url,
        &["--output-dir", backups.to_str().unwrap()],
    ));
    let first_path = Path::new(first["backup"].as_str().unwrap());
    let metadata_path = PathBuf::from(format!("{}.meta.json", first_path.display()));
    let mut metadata = read_json(&metadata_path);
    assert_eq!(
        metadata["size"].as_u64(),
        Some(fs::metadata(first_path).unwrap().len())
    );
    let created =
        chrono::DateTime::parse_from_rfc3339(metadata["created_at"].as_str().unwrap()).unwrap();
    let stamp = created.timestamp() as f64 + created.timestamp_subsec_nanos() as f64 / 1e9;
    assert!(
        tolight_cli::ops::storage::backup::successful_backup_times(
            first_path.parent().unwrap(),
            Utc::now().timestamp() as f64 + 1.0
        )
        .contains(&stamp)
    );
    metadata["created_at"] = Value::String("2000-01-01T00:00:00Z".into());
    fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    let unrelated = backups.join("somebody-elses.dump");
    fs::write(&unrelated, "unrelated").unwrap();
    let orphan = backups.join("tolight-20000101T000000Z-00000000000000000000000000000000.dump");
    fs::write(&orphan, "not ours without a matching metadata/hash").unwrap();
    let second = successful(script(
        "backup.sh",
        &url,
        &[
            "--output-dir",
            backups.to_str().unwrap(),
            "--retention-days",
            "1",
        ],
    ));
    assert_eq!(second["removed"].as_array().unwrap().len(), 1);
    assert!(!first_path.exists());
    assert!(!metadata_path.exists());
    assert!(unrelated.exists());
    assert!(orphan.exists());
    assert!(Path::new(second["backup"].as_str().unwrap()).exists());
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn retention_bounds_metadata_and_preserves_incomplete_pairs_and_unexpired_backups(
    pool: PgPool,
) {
    let directory = tempfile::tempdir().unwrap();
    let url = pool.connect_options().to_url_lossy().to_string();
    let backups = directory.path().join("backups");
    let first = successful(script(
        "backup.sh",
        &url,
        &["--output-dir", backups.to_str().unwrap()],
    ));
    let original = Path::new(first["backup"].as_str().unwrap());
    let marker = read_json(format!("{}.meta.json", original.display()));
    let mut preserved = Vec::new();
    let mut expired = Vec::new();
    for case in [
        "expired",
        "legacy",
        "boundary",
        "oversized",
        "wrong-size",
        "wrong-hash",
        "unexpired",
    ] {
        let name = format!(
            "tolight-20000101T000000Z-{}.dump",
            uuid::Uuid::new_v4().simple()
        );
        let dump = backups.join(&name);
        fs::copy(original, &dump).unwrap();
        let mut metadata = marker.clone();
        metadata["file"] = name.clone().into();
        if case != "unexpired" {
            metadata["created_at"] = "2000-01-01T00:00:00Z".into();
        }
        if case == "legacy" {
            metadata.as_object_mut().unwrap().remove("size");
        }
        if case == "wrong-size" {
            metadata["size"] = (fs::metadata(&dump).unwrap().len() + 1).into();
        }
        if case == "wrong-hash" {
            metadata["sha256"] = "0".repeat(64).into();
        }
        let mut bytes = serde_json::to_vec(&metadata).unwrap();
        if case == "boundary" || case == "oversized" {
            bytes.resize(if case == "boundary" { 65536 } else { 65537 }, b' ');
        }
        fs::write(backups.join(format!("{name}.meta.json")), bytes).unwrap();
        if matches!(case, "expired" | "legacy" | "boundary") {
            expired.push(name);
        } else {
            preserved.push(name);
        }
    }
    let second = successful(script(
        "backup.sh",
        &url,
        &[
            "--output-dir",
            backups.to_str().unwrap(),
            "--retention-days",
            "1",
        ],
    ));
    expired.sort();
    assert_eq!(second["removed"], serde_json::json!(expired));
    for name in expired {
        assert!(!backups.join(&name).exists());
        assert!(!backups.join(format!("{name}.meta.json")).exists());
    }
    for name in preserved {
        assert!(backups.join(&name).exists(), "lost {name}");
        assert!(backups.join(format!("{name}.meta.json")).exists());
    }
    assert!(original.exists());
    assert!(Path::new(second["backup"].as_str().unwrap()).exists());
}

#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn drill_refuses_running_source_before_creating_database_and_cleans_failed_restore(
    pool: PgPool,
) {
    let store = Store::from_pool(pool.clone());
    let directory = tempfile::tempdir().unwrap();
    let (config, public) = config(directory.path());
    let url = pool.connect_options().to_url_lossy().to_string();
    assert!(
        cli(&config, &url, &["publish", "--rebuild"])
            .status
            .success()
    );
    let broken = directory.path().join("broken.dump");
    fs::write(&broken, "not a pg_dump").unwrap();
    let target = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    let work = directory.path().join("drills");
    let args = drill_args(&broken, &config, &public, &target, &work);
    let lease = store.acquire_serve_lock().await.unwrap();
    let refused = script("restore-drill.sh", &url, &args);
    assert_eq!(refused.status.code(), Some(1));
    let output: Value = serde_json::from_slice(&refused.stdout).unwrap();
    let report = read_json(output["report"].as_str().unwrap());
    assert!(report["steps"].is_null());
    assert!(report["temporary_database_removed"].is_null());
    drop(lease);
    let failed = script("restore-drill.sh", &url, &args);
    assert_eq!(failed.status.code(), Some(1));
    let output: Value = serde_json::from_slice(&failed.stdout).unwrap();
    let report = read_json(output["report"].as_str().unwrap());
    assert_eq!(report["temporary_database_removed"], true);
    assert!(
        !Path::new(report["work_dir"].as_str().unwrap())
            .join("public")
            .exists()
    );
}

#[cfg(unix)]
#[sqlx::test(migrations = "../tolight-coordinator-store/migrations")]
async fn failed_target_startup_stops_only_its_child_before_dropping_its_database(pool: PgPool) {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let (config, public) = config(directory.path());
    let url = pool.connect_options().to_url_lossy().to_string();
    assert!(
        cli(&config, &url, &["publish", "--rebuild"])
            .status
            .success()
    );
    let backups = directory.path().join("backups");
    let result = successful(script(
        "backup.sh",
        &url,
        &["--output-dir", backups.to_str().unwrap()],
    ));
    let backup = Path::new(result["backup"].as_str().unwrap());
    let target = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    // All data operations use the real binary. Only the final server is replaced
    // by an actual process that never listens, proving the finally cleanup path.
    let wrapper = directory.path().join("unhealthy-serve.sh");
    let late_write = directory.path().join("late-serve-writer.txt");
    let late_write_shell = late_write.to_string_lossy().replace('\'', "'\\''");
    let binary = env!("CARGO_BIN_EXE_tolight").replace('\'', "'\\''");
    fs::write(&wrapper, format!("#!/bin/sh\nlast=\nfor argument do last=$argument; done\nif [ \"$last\" = serve ]; then\n  (trap '' TERM; sleep 2; printf late > '{late_write_shell}') &\n  exec /bin/sleep 60\nfi\nexec '{binary}' \"$@\"\n")).unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    struct Unrelated(std::process::Child);
    impl Drop for Unrelated {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut unrelated = Unrelated(Command::new("/bin/sleep").arg("60").spawn().unwrap());
    let work = directory.path().join("drills");
    let mut args = drill_args(backup, &config, &public, &target, &work);
    let binary_index = args.iter().position(|arg| *arg == "--tolight").unwrap() + 1;
    args[binary_index] = wrapper.to_str().unwrap();
    args.extend(["--startup-timeout-seconds", "1"]);
    let output = script("restore-drill.sh", &url, &args);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        drill_diagnostics(&output)
    );
    let output: Value = serde_json::from_slice(&output.stdout).unwrap();
    let report = read_json(output["report"].as_str().unwrap());
    assert_eq!(report["status"], "failed");
    assert_eq!(report["target_serve"]["process_removed"], true);
    assert_eq!(report["temporary_database_removed"], true);
    assert!(
        report["error"]
            .as_str()
            .unwrap()
            .contains("startup deadline")
    );
    let remains: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_database WHERE datname=$1")
        .bind(report["temporary_database"].as_str().unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(remains, 0);
    std::thread::sleep(std::time::Duration::from_millis(2200));
    assert!(
        !late_write.exists(),
        "a failed restored server left a background writer"
    );
    assert!(
        unrelated.0.try_wait().unwrap().is_none(),
        "cleanup touched an unrelated process"
    );
}
