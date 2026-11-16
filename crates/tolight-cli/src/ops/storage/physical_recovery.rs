use super::{
    ExitStatus, PitrDrill, PitrInspect, Result,
    common::*,
    public_tree::PublicTree,
    recovery::{RecoveryTools, finish, reconcile, source_reference, step},
    root, wal,
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

struct TargetPg {
    data: PathBuf,
    env: Env,
    started: bool,
}
impl TargetPg {
    fn stop(&mut self) -> bool {
        if !self.started {
            return true;
        }
        let _ = status_cleanup(
            command(program("pg_ctl"), &self.env)
                .arg("-D")
                .arg(&self.data)
                .args(["-m", "fast", "-t", "15", "-w", "stop"])
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
            Duration::from_secs(20),
        );
        let status = status_cleanup(
            command(program("pg_ctl"), &self.env)
                .arg("-D")
                .arg(&self.data)
                .arg("status")
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
            Duration::from_secs(5),
        );
        let safe = status.is_ok_and(|s| s.code() == Some(3));
        if safe {
            self.started = false
        }
        safe
    }
}
impl Drop for TargetPg {
    fn drop(&mut self) {
        self.stop();
    }
}
fn recovered_url(database: &str, user: &str, number: u16, socket: &Path) -> Result<String> {
    if database.is_empty()
        || user.is_empty()
        || database
            .chars()
            .chain(user.chars())
            .any(|c| ['\0', '\r', '\n'].contains(&c))
    {
        return Err("base backup database/login metadata required".into());
    }
    let mut url = url::Url::parse("postgresql://localhost")?;
    url.set_username(user)
        .map_err(|_| "invalid restored database user")?;
    url.set_port(Some(number)).map_err(|_| "invalid port")?;
    url.set_path(&format!("/{database}"));
    url.query_pairs_mut()
        .append_pair("host", &socket.to_string_lossy());
    Ok(url.into())
}
fn validate_bundle(bundle: &Path, work: &Path, env: &Env) -> Result<Value> {
    regular(&bundle.join("backup.json"))?;
    let metadata = json_file(&bundle.join("backup.json"))?;
    validate_base(&bundle.join("data"))?;
    if metadata["schema"] != "tolight-base-backup/1"
        || metadata["status"] != "passed"
        || metadata["postgres_major"].as_u64() != Some(18)
        || metadata["data"] != "data"
        || metadata["manifest_sha256"].as_str()
            != Some(&sha256(&bundle.join("data/backup_manifest"))?)
        || fs::read_to_string(bundle.join("data/PG_VERSION"))?.trim() != "18"
    {
        return Err("invalid PG18 base backup metadata/version/manifest checksum".into());
    }
    let manifest = json_file(&bundle.join("data/backup_manifest"))?;
    let id = metadata["system_identifier"]
        .as_str()
        .ok_or("base system identity")?;
    if id.is_empty()
        || !id.bytes().all(|v| v.is_ascii_digit())
        || manifest["System-Identifier"].to_string().trim_matches('"') != id
    {
        return Err("base backup system identifier mismatch".into());
    }
    capture(
        command(program("pg_verifybackup"), env)
            .arg("--exit-on-error")
            .arg(bundle.join("data")),
        &work.join("verify-base.stdout"),
    )?;
    Ok(metadata)
}
struct RecoveryStartup<'a> {
    bundle: &'a Path,
    metadata: &'a Value,
    archive: &'a Path,
    work: &'a Path,
    public: &'a Path,
    target: Option<DateTime<Utc>>,
    timeout: u64,
    env: &'a Env,
}
fn start_recovered(
    startup: RecoveryStartup<'_>,
    pg: &mut Option<TargetPg>,
) -> Result<(String, u16, Vec<String>)> {
    let RecoveryStartup {
        bundle,
        metadata,
        archive,
        work,
        public,
        target,
        timeout,
        env,
    } = startup;
    let data = work.join("data");
    copy_tree(&bundle.join("data"), &data)?;
    let number = port()?;
    let socket = work.join("socket");
    private_dir(&socket)?;
    let restore = std::env::current_exe()?;
    let argv = [
        restore.to_string_lossy().into_owned(),
        "storage".into(),
        "wal-archive".into(),
        "restore".into(),
        "--archive-dir".into(),
        archive.to_string_lossy().into_owned(),
        "--name".into(),
        "%f".into(),
        "--destination".into(),
        "%p".into(),
        "--public-dir".into(),
        public.to_string_lossy().into_owned(),
    ];
    let restore_command = argv.iter().map(shell).collect::<Vec<_>>().join(" ");
    let mut settings = format!(
        "listen_addresses='127.0.0.1'\nport={number}\nunix_socket_directories={}\narchive_mode=off\nhot_standby=off\nmax_connections=30\nshared_buffers='16MB'\nfsync=on\ntimezone='UTC'\nlc_messages='C'\nrestore_command={}\nrecovery_target_action='promote'\nrecovery_target_timeline='current'\n",
        quote(socket.to_string_lossy()),
        quote(restore_command)
    );
    if let Some(target) = target {
        settings.push_str(&format!(
            "recovery_target_time={}\nrecovery_target_inclusive=on\n",
            quote(target.to_rfc3339())
        ))
    }
    replace_text(&data.join("postgresql.conf"), &settings)?;
    replace_text(&data.join("postgresql.auto.conf"), "")?;
    replace_text(
        &data.join("pg_hba.conf"),
        "local all all trust\nhost all all 127.0.0.1/32 reject\n",
    )?;
    replace_text(&data.join("pg_ident.conf"), "")?;
    for name in [
        "postmaster.pid",
        "postmaster.opts",
        "standby.signal",
        "recovery.signal",
    ] {
        let path = data.join(name);
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    new_file(&data.join("recovery.signal"))?;
    *pg = Some(TargetPg {
        data: data.clone(),
        env: env.clone(),
        started: true,
    });
    capture_startup(
        command(program("pg_ctl"), env)
            .arg("-D")
            .arg(&data)
            .arg("-l")
            .arg(work.join("postgres.log"))
            .args(["-t", &timeout.to_string(), "-w", "start"]),
        &work.join("pg-start.stdout"),
    )?;
    let database = metadata["database"].as_str().ok_or("backup database")?;
    let user = metadata["database_user"].as_str().unwrap_or("tolight");
    let url = recovered_url(database, user, number, &socket)?;
    let admin = pg_environment(&url, None)?;
    let deadline = Instant::now() + Duration::from_secs(timeout);
    loop {
        if CANCELLED.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("recovery interrupted".into());
        }
        let value=output(command(program("psql"),&admin).args(["-w","-XAt","-F","\t","-c","SELECT pg_is_in_recovery(),current_setting('archive_mode'),current_setting('recovery_target_time')"]));
        if let Ok(value) = value {
            let values = value
                .trim_end_matches(['\r', '\n'])
                .split('\t')
                .map(str::to_owned)
                .collect::<Vec<_>>();
            let values = if values.len() == 2 {
                vec![values[0].clone(), values[1].clone(), String::new()]
            } else {
                values
            };
            if values.len() == 3 && values[0] == "f" && !data.join("recovery.signal").exists() {
                text(&work.join("recovery-state.stdout"), &(value + "\n"))?;
                return Ok((url, number, values));
            }
        }
        if !data.join("postmaster.pid").exists() {
            return Err("PostgreSQL stopped before reaching requested recovery target".into());
        }
        if Instant::now() >= deadline {
            return Err("recovery target was not reached before startup deadline".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
fn cleanup(
    work: &Path,
    report: &mut Value,
    child: &mut Option<OwnedChild>,
    pg: &mut Option<TargetPg>,
) {
    let mut safe = true;
    if let Some(child) = child {
        let normal = child.stop().unwrap_or(false);
        report["target_serve"]["normal_shutdown"] = normal.into();
        let status = child.child.try_wait().ok().flatten();
        report["target_serve"]["exit_code"] = json!(status.and_then(|s| s.code()));
        report["target_serve"]["process_removed"] = status.is_some().into();
        safe = status.is_some();
    }
    if safe && let Some(pg) = pg {
        safe = pg.stop();
        report["target_postgres_stopped"] = safe.into();
    }
    if safe {
        for (name, field) in [
            ("data", "target_data_removed"),
            ("archive", "temporary_archive_removed"),
        ] {
            let path = work.join(name);
            if path.exists() && fs::remove_dir_all(&path).is_err() {
                safe = false
            }
            report[field] = (!path.exists()).into();
        }
    }
    if !safe {
        report["status"] = "cleanup_failed".into();
    }
}
pub fn pitr(args: PitrDrill) -> Result<()> {
    if !(1..=120).contains(&args.startup_timeout_seconds)
        || args.rto_budget_seconds < 1
        || args.rpo_budget_seconds < 1
    {
        return Err("positive recovery budgets/startup timeout 1..120 required".into());
    }
    let target_time = utc(&args.restore_target)?;
    let config = config(Some(&args.source_config))?;
    let source_public = canonical(&args.source_public_dir)?;
    if source_public != canonical(&config.public.dir)? {
        return Err("source-public-dir must match source config public.dir".into());
    }
    let base = private_path(
        &private_path(&args.work_dir, &root().join("public-dist"))?,
        &source_public,
    )?;
    let work = temp_dir(&base, "tolight-pitr-")?;
    let started = Instant::now();
    let mut report = json!({"schema":"tolight-pitr-drill/1","scope":"local_wal_pitr","status":"failed","phase":"source_snapshot","work_dir":work,"restore_target":args.restore_target,"postgres_major":18,"known_current_controls":false,"unknown_controls":true,"unknown_control_interval":"current source controls have not been verified","local_targets":{"rto_seconds":args.rto_budget_seconds,"rpo_window_seconds":args.rpo_budget_seconds,"production_targets_confirmed":false}});
    let mut target_child = None;
    let mut pg = None;
    let result = (|| -> Result<()> {
        let published = PublicTree::load(&source_public)?;
        let source_files = tree_files(&source_public)?;
        let url = database_url(&config)?;
        let mut env = base_env();
        let binary = canonical(&args.tolight)?;
        capture(
            command(program("psql"), &pg_environment(&url, None)?).args([
                "-w",
                "-XAt",
                "-c",
                "SELECT system_identifier FROM pg_control_system()",
            ]),
            &work.join("source-system.stdout"),
        )?;
        let identity = fs::read_to_string(work.join("source-system.stdout"))?
            .trim()
            .to_string();
        if identity.is_empty() || !identity.bytes().all(|c| c.is_ascii_digit()) {
            return Err("cannot verify source PostgreSQL system identity".into());
        }
        report["source_system_identifier"] = identity.clone().into();
        let snapshot = work.join("controls.json");
        if capture(
            cli(&binary, &args.source_config, &env)
                .args(["operator", "export-controls", "--output"])
                .arg(&snapshot)
                .args(["--restore-target", &args.restore_target]),
            &work.join("export.stdout"),
        )
        .is_err()
        {
            report["unknown_control_interval"]="source is unreadable or not frozen; current revocations/identities cannot be verified".into();
            return Err("cannot obtain a verified frozen-source control snapshot".into());
        }
        report["known_current_controls"] = true.into();
        report["unknown_controls"] = false.into();
        report
            .as_object_mut()
            .unwrap()
            .remove("unknown_control_interval");
        let original = json_file(&snapshot)?;
        let window = (utc(original["exported_at"]
            .as_str()
            .ok_or("source export time")?)?
            - target_time)
            .num_microseconds()
            .ok_or("source time overflow")? as f64
            / 1e6;
        if window < 0.0 {
            return Err("restore target is after frozen source".into());
        }
        report["frozen_source_window_seconds"] = window.into();
        report["steps"] = json!(["frozen_source_controls_exported"]);
        let source = source_reference(
            &RecoveryTools {
                binary: &binary,
                env: &env,
                work: &work,
            },
            &url,
            &config,
            &snapshot,
            &published,
            &mut report,
        )?;
        report["phase"] = "base_backup_validation".into();
        let bundle = private_path(
            &private_path(&args.base_backup, &root().join("public-dist"))?,
            &source_public,
        )?;
        if symlink(&args.base_backup) {
            return Err("base backup bundle must not be a symlink".into());
        }
        let metadata = validate_bundle(&bundle, &work, &env)?;
        if metadata["system_identifier"] != identity {
            return Err("base backup does not belong to current source cluster".into());
        }
        if target_time
            <= utc(metadata["completed_at"]
                .as_str()
                .ok_or("backup completion time")?)?
        {
            return Err("restore target must follow completion of base backup".into());
        }
        report["base_manifest_sha256"] = metadata["manifest_sha256"].clone();
        report["base_backup"] = json!(bundle);
        report["phase"] = "wal_validation".into();
        let archive = wal::checked_archive(&args.archive_dir, Some(&source_public))?;
        if archive
            != canonical(Path::new(
                metadata["archive_dir"].as_str().ok_or("archive metadata")?,
            ))?
        {
            return Err("archive directory differs from base backup metadata".into());
        }
        report["archived_files_verified"] =
            json!(wal::copy_archive(&archive, &work.join("archive"))?);
        report["phase"] = "postgresql_recovery".into();
        let (restored, number, values) = start_recovered(
            RecoveryStartup {
                bundle: &bundle,
                metadata: &metadata,
                archive: &work.join("archive"),
                work: &work,
                public: &work.join("public"),
                target: Some(target_time),
                timeout: args.startup_timeout_seconds,
                env: &env,
            },
            &mut pg,
        )?;
        if values[0] != "f" || values[1] != "off" {
            return Err("recovery did not promote with archiving disabled".into());
        }
        let actual = DateTime::parse_from_str(&values[2], "%Y-%m-%d %H:%M:%S%.f%#z")
            .or_else(|_| DateTime::parse_from_rfc3339(&values[2]))?
            .with_timezone(&Utc);
        if actual != target_time {
            return Err("PostgreSQL used a different restore target".into());
        }
        let admin = pg_environment(&restored, None)?;
        let mut tcp = admin.clone();
        tcp.insert("PGHOST".into(), "127.0.0.1".into());
        let denied = output_status(
            command(program("psql"), &tcp).args(["-w", "-XAt", "-c", "SELECT 1"]),
            Duration::from_secs(15),
        )?;
        if denied.status.success()
            || !String::from_utf8_lossy(&denied.stderr).contains("pg_hba.conf rejects")
        {
            return Err("restored database TCP connections were not explicitly rejected".into());
        }
        text(
            &work.join("tcp-denial.stderr"),
            &String::from_utf8_lossy(&denied.stderr),
        )?;
        report["recovery_target_reached"] = true.into();
        report["target_postgres"] = json!({"port":number,"loopback":true,"archive_mode":"off","in_recovery":false,"data":work.join("data"),"socket_private":true,"tcp_auth":"reject","tcp_auth_verified":true});
        step(&mut report, "base_and_archived_wal_recovered_to_target");
        env.insert("TOLIGHT_RESTORE_DATABASE_URL".into(), restored.into());
        let target = work.join("target.toml");
        let (api, ops) = (port()?, port()?);
        write_config(
            &target,
            "TOLIGHT_RESTORE_DATABASE_URL",
            &work.join("public"),
            config.public.demo_banner,
            Some((api, ops)),
        )?;
        capture(
            cli(&binary, &target, &env)
                .args(["operator", "export-controls", "--output"])
                .arg(work.join("at-target-controls.json"))
                .args(["--restore-target", &args.restore_target]),
            &work.join("at-target-export.stdout"),
        )?;
        report["phase"] = "current_control_review".into();
        if !reconcile(
            &RecoveryTools {
                binary: &binary,
                env: &env,
                work: &work,
            },
            &target,
            &snapshot,
            &args.restore_target,
            &args.approve_active,
            &source,
            &mut report,
        )? {
            return Err(ExitStatus(2).into());
        }
        report["phase"] = "restored_service".into();
        let tree = PublicTree::load(&work.join("public"))?;
        let restored_ids = tree
            .events
            .values()
            .map(|e| e["event_id"].as_str().unwrap())
            .collect::<BTreeSet<_>>();
        let source_ids = source
            .events
            .values()
            .map(|e| e["event_id"].as_str().unwrap())
            .collect::<BTreeSet<_>>();
        report["known_message_event_ids_not_restored"] = json!(
            source
                .events
                .values()
                .filter(|e| e["kind"] == "agent_message"
                    && !restored_ids.contains(e["event_id"].as_str().unwrap()))
                .map(|e| e["event_id"].clone())
                .collect::<Vec<_>>()
        );
        report["source_event_ids_not_restored"] = json!(
            source
                .events
                .values()
                .filter(|e| !restored_ids.contains(e["event_id"].as_str().unwrap()))
                .map(|e| e["event_id"].clone())
                .collect::<Vec<_>>()
        );
        report["reconstructed_event_ids"] = json!(
            tree.events
                .values()
                .filter(|e| !source_ids.contains(e["event_id"].as_str().unwrap()))
                .map(|e| e["event_id"].clone())
                .collect::<Vec<_>>()
        );
        target_child = Some(OwnedChild::start(
            cli(&binary, &target, &env).arg("serve"),
            &work.join("serve.stdout"),
        )?);
        report["target_serve"] = json!({"participant_port":api,"operations_port":ops});
        let health = healthy(
            target_child.as_mut().unwrap(),
            api,
            ops,
            args.startup_timeout_seconds,
        )?;
        report["target_serve"]
            .as_object_mut()
            .unwrap()
            .extend(health.as_object().unwrap().clone());
        let elapsed = started.elapsed().as_secs_f64();
        report["restore_elapsed_seconds"] = elapsed.into();
        let met =
            elapsed <= args.rto_budget_seconds as f64 && window <= args.rpo_budget_seconds as f64;
        report["local_targets"]["met"] = met.into();
        if !met {
            return Err("local recovery measurement exceeds configured local budget".into());
        }
        if !target_child.as_mut().unwrap().stop()? {
            return Err("restored serve failed normal shutdown".into());
        }
        if tree_files(&source_public)? != source_files {
            return Err("source public files changed during frozen-source recovery".into());
        }
        report["source_public_unchanged"] = true.into();
        step(
            &mut report,
            "restored_serve_verified_before_normal_shutdown",
        );
        report["phase"] = "complete".into();
        report["status"] = "passed".into();
        Ok(())
    })();
    cleanup(&work, &mut report, &mut target_child, &mut pg);
    report["total_elapsed_seconds"] = started.elapsed().as_secs_f64().into();
    finish(&work, &mut report, result, false)
}
pub fn inspect(args: PitrInspect) -> Result<()> {
    if !(1..=120).contains(&args.startup_timeout_seconds) {
        return Err("startup timeout must be 1..120".into());
    }
    let mut roots = vec![root().join("public-dist"), canonical(&args.public_dir)?];
    if symlink(&args.base_backup) {
        return Err("inspection inputs must not be symlinks".into());
    }
    let mut bundle = canonical(&args.base_backup)?;
    for public in &roots {
        bundle = private_path(&bundle, public)?
    }
    regular(&bundle.join("backup.json"))?;
    let mut metadata = json_file(&bundle.join("backup.json"))?;
    if let Some(public) = metadata["public_dir"].as_str() {
        roots.push(canonical(Path::new(public))?);
    }
    for public in &roots {
        bundle = private_path(&bundle, public)?;
    }
    if symlink(&args.work_dir) {
        return Err("inspection outputs must not be symlinks".into());
    }
    let mut base = canonical(&args.work_dir)?;
    for public in &roots {
        base = private_path(&base, public)?;
    }
    let work = temp_dir(&base, "tolight-inspection-")?;
    let raw = work.join(".inspection-raw-controls.json");
    let mut report = json!({"schema":"tolight-pitr-inspection/1","scope":"unverified_latest_wal_inspection","status":"failed","work_dir":work,"source_connected":false,"unknown_controls":true,"known_current_controls":false,"restore_eligible":false,"application_started":false,"public_tree_created":false,"unknown_control_interval":"Unarchived source activity and external dispositions are not observed; the upper bound is unknown.","production_targets_confirmed":false});
    let mut env = base_env();
    for key in [
        "DATABASE_URL",
        "TOLIGHT_RESTORE_DATABASE_URL",
        "INSPECT_SOURCE_DB",
    ] {
        env.remove(std::ffi::OsStr::new(key));
    }
    let mut pg = None;
    let result = (|| -> Result<()> {
        report["phase"] = "base_backup_validation".into();
        metadata = validate_bundle(&bundle, &work, &env)?;
        if let Some(user) = &args.database_user {
            metadata["database_user"] = user.clone().into();
        }
        report["phase"] = "wal_validation".into();
        let archive = wal::checked_archive(&args.archive_dir, Some(&args.public_dir))?;
        for public in &roots {
            private_path(&archive, public)?;
        }
        if archive
            != canonical(Path::new(
                metadata["archive_dir"].as_str().ok_or("archive metadata")?,
            ))?
        {
            return Err("archive directory differs from base backup metadata".into());
        }
        report["archived_files_verified"] =
            json!(wal::copy_archive(&archive, &work.join("archive"))?);
        report["phase"] = "latest_available_recovery".into();
        let (url, number, values) = start_recovered(
            RecoveryStartup {
                bundle: &bundle,
                metadata: &metadata,
                archive: &work.join("archive"),
                work: &work,
                public: &args.public_dir,
                target: None,
                timeout: args.startup_timeout_seconds,
                env: &env,
            },
            &mut pg,
        )?;
        if values[0] != "f" || values[1] != "off" || !values[2].is_empty() {
            return Err(
                "latest recovery did not finish without a time target and with archiving disabled"
                    .into(),
            );
        }
        let admin = pg_environment(&url, None)?;
        capture(command(program("psql"),&admin).args(["-w","-XAt","-c","SELECT json_build_object('system_identifier',(SELECT system_identifier::text FROM pg_control_system()),'replay_lsn',pg_last_wal_replay_lsn()::text,'last_replay_commit_time',pg_last_xact_replay_timestamp(),'in_recovery',pg_is_in_recovery(),'archive_mode',current_setting('archive_mode'))"]),&work.join("coverage.json"))?;
        let mut coverage = json_file(&work.join("coverage.json"))?;
        if coverage["system_identifier"] != metadata["system_identifier"]
            || coverage["in_recovery"] != false
            || coverage["archive_mode"] != "off"
            || coverage["replay_lsn"].as_str().is_none_or(str::is_empty)
        {
            return Err("recovered cluster identity/coverage verification failed".into());
        }
        coverage["base_completed_at"] = metadata["completed_at"].clone();
        coverage["latest_source_commit_time"] = Value::Null;
        coverage["source_upper_bound_known"] = false.into();
        report["coverage"] = coverage.clone();
        report["target_postgres"] = json!({"port":number,"socket":work.join("socket"),"archive_mode":"off","tcp_authentication":"reject","in_recovery":false});
        report["phase"] = "unverified_observation".into();
        let config = work.join("target.toml");
        write_config(
            &config,
            "TOLIGHT_INSPECT_DATABASE_URL",
            &work.join("public"),
            false,
            None,
        )?;
        env.insert("TOLIGHT_INSPECT_DATABASE_URL".into(), url.into());
        capture(
            cli(&canonical(&args.tolight)?, &config, &env)
                .args(["operator", "export-controls", "--output"])
                .arg(&raw)
                .args(["--restore-target", "1970-01-01T00:00:00Z"]),
            &work.join("export.stdout"),
        )?;
        let mut state = json_file(&raw)?;
        for key in ["schema", "export_id", "exported_at", "restore_target"] {
            state
                .as_object_mut()
                .ok_or("control observation")?
                .remove(key);
        }
        private_json(
            &work.join("unverified-observation.json"),
            &json!({"schema":"tolight-unverified-recovery-observation/1","scope":"latest_available_wal_inspection","unknown_controls":true,"restore_eligible":false,"known_current_controls":false,"inspected_at":now(),"coverage":coverage,"observed_state":state,"warning":"Archived state is not verified current controls. No approval, publication, or application startup is permitted."}),
        )?;
        fs::remove_file(&raw)?;
        sync_directory(&work)?;
        report["observation"] = json!(work.join("unverified-observation.json"));
        report["importable_control_snapshot_retained"] = false.into();
        report["public_tree_created"] = work.join("public").exists().into();
        if work.join("public").exists() {
            return Err("inspection must never materialize a public tree".into());
        }
        report["phase"] = "observation_complete".into();
        report["status"] = "needs_verified_controls".into();
        Err(ExitStatus(2).into())
    })();
    let raw_removed = !raw.exists() || fs::remove_file(&raw).is_ok();
    let mut child = None;
    cleanup(&work, &mut report, &mut child, &mut pg);
    if !raw_removed {
        report["status"] = "cleanup_failed".into();
    }
    report["importable_control_snapshot_retained"] = raw.exists().into();
    finish(&work, &mut report, result, true)
}
