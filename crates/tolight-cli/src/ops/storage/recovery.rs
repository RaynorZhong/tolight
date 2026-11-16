use super::{
    ExitStatus, PitrDrill, PitrInspect, RestoreDrill, Result,
    common::*,
    public_tree::{self, PublicTree},
    root,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};
pub fn pitr(args: PitrDrill) -> Result<()> {
    super::physical_recovery::pitr(args)
}
pub fn inspect(args: PitrInspect) -> Result<()> {
    super::physical_recovery::inspect(args)
}
pub(super) fn step(report: &mut Value, value: &str) {
    report["steps"]
        .as_array_mut()
        .expect("initialized steps")
        .push(value.into());
}
pub(super) struct RecoveryTools<'a> {
    pub binary: &'a Path,
    pub env: &'a Env,
    pub work: &'a Path,
}
pub(super) fn source_reference(
    tools: &RecoveryTools<'_>,
    url: &str,
    config: &tolight_core::config::Config,
    snapshot: &Path,
    published: &PublicTree,
    report: &mut Value,
) -> Result<PublicTree> {
    let (binary, env, work) = (tools.binary, tools.env, tools.work);
    let path = work.join("source-reference.toml");
    let public = work.join("source-reference");
    write_config(
        &path,
        "TOLIGHT_SOURCE_REFERENCE_DATABASE_URL",
        &public,
        config.public.demo_banner,
        None,
    )?;
    let mut environment = env.clone();
    environment.insert("TOLIGHT_SOURCE_REFERENCE_DATABASE_URL".into(), url.into());
    capture(
        cli(binary, &path, &environment).args(["publish", "--rebuild"]),
        &work.join("source-reference.stdout"),
    )?;
    let tree = PublicTree::load(&public)?;
    let controls = json_file(snapshot)?;
    if tree.manifest["newest_seq"] != controls["source_anchor"]["seq"]
        || tree.manifest["newest_event_id"] != controls["source_anchor"]["event_id"]
    {
        return Err("source database changed after frozen controls were exported".into());
    }
    public_tree::verify_prefix(published, &tree)?;
    let head = published.manifest["newest_seq"].as_i64().unwrap();
    let current = tree.manifest["newest_seq"].as_i64().unwrap();
    report["source_reference"] = json!({"built_from_current_database":true,"public":public,"published_head":head,"database_head":current,"published_lag_events":(current-head).max(0),"published_anchor_matches_database":head==0||tree.events.get(&head).map(|e|&e["event_id"])==Some(&published.manifest["newest_event_id"]),"database_anchor_matches_control_snapshot":true});
    Ok(tree)
}
pub(super) fn reconcile(
    tools: &RecoveryTools<'_>,
    config: &Path,
    snapshot: &Path,
    target: &str,
    approved: &[uuid::Uuid],
    source: &PublicTree,
    report: &mut Value,
) -> Result<bool> {
    let (binary, env, work) = (tools.binary, tools.env, tools.work);
    let restore = || {
        let mut c = cli(binary, config, env);
        c.args(["operator", "restore-controls", "--snapshot"])
            .arg(snapshot);
        c
    };
    capture(restore().arg("--dry-run"), &work.join("plan.json"))?;
    let plan = json_file(&work.join("plan.json"))?;
    step(report, "private_restore_plan_reviewed");
    let required = plan["required_approvals"]
        .as_array()
        .ok_or("required approvals")?
        .iter()
        .map(|s| s.as_str().map(str::to_owned).ok_or("approval ID"))
        .collect::<std::result::Result<BTreeSet<_>, _>>()?;
    let approvals = approved
        .iter()
        .map(ToString::to_string)
        .collect::<BTreeSet<_>>();
    let missing = required.difference(&approvals).cloned().collect::<Vec<_>>();
    report["required_approvals"] = json!(required);
    report["missing_approvals"] = json!(missing);
    if !missing.is_empty() {
        report["status"] = "needs_approval".into();
        return Ok(false);
    }
    if !approvals.is_subset(&required) {
        return Err("approve-active includes an identity not requested by this plan".into());
    }
    let mut apply = restore();
    for id in approvals {
        apply.args(["--approve-active", &id]);
    }
    capture(apply.arg("--yes"), &work.join("apply.stdout"))?;
    let stdout = fs::read_to_string(work.join("apply.stdout"))?;
    report["applied"] = serde_json::from_str(stdout.lines().last().ok_or("restore result")?)?;
    let verified = work.join("verified-controls.json");
    capture(
        cli(binary, config, env)
            .args(["operator", "export-controls", "--output"])
            .arg(&verified)
            .args(["--restore-target", target]),
        &work.join("verify-controls.stdout"),
    )?;
    let original = json_file(snapshot)?;
    let verified = json_file(&verified)?;
    let agents = |value: &Value| -> Result<BTreeMap<String, Value>> {
        value["agents"]
            .as_array()
            .ok_or("control agents")?
            .iter()
            .map(|a| {
                Ok((
                    a["agent_id"]
                        .as_str()
                        .ok_or("control agent ID")?
                        .to_string(),
                    a.clone(),
                ))
            })
            .collect::<Result<_>>()
    };
    let expected = agents(&original)?;
    let actual = agents(&verified)?;
    if expected.keys().ne(actual.keys()) || original["room"] != verified["room"] {
        return Err("restored identities or room flags differ".into());
    }
    for (id, value) in &expected {
        let actual = &actual[id];
        for (key, value) in value.as_object().ok_or("control identity")? {
            if !["application_revision", "state_changed_at", "joined_at"].contains(&key.as_str())
                && actual[key] != *value
            {
                return Err("restored private identity controls differ".into());
            }
        }
        if actual["application_revision"].as_i64() < value["application_revision"].as_i64() {
            return Err("application revision moved backwards".into());
        }
    }
    let keys = |value: &Value| -> Result<BTreeMap<String, Value>> {
        value["agent_keys"]
            .as_array()
            .ok_or("control keys")?
            .iter()
            .map(|k| {
                Ok((
                    k["public_key"].as_str().ok_or("public key")?.to_string(),
                    k.clone(),
                ))
            })
            .collect()
    };
    if keys(&original)? != keys(&verified)? {
        return Err("restored key history differs".into());
    }
    report["current_controls_verified"] = true.into();
    step(
        report,
        "current_controls_applied_with_explicit_active_approvals",
    );
    capture(
        cli(binary, config, env).args(["publish", "--rebuild"]),
        &work.join("publish.stdout"),
    )?;
    let restored = PublicTree::load(&work.join("public"))?;
    report["comparison"] = public_tree::compare(source, &restored, &plan)?;
    step(report, "rebuilt_and_compared_public_artifacts");
    capture(restore().arg("--dry-run"), &work.join("idempotence.json"))?;
    if json_file(&work.join("idempotence.json"))?["already_applied"] != true {
        return Err("restore completion marker is missing".into());
    }
    report["idempotence_verified"] = true.into();
    Ok(true)
}
pub(super) fn finish(
    work: &Path,
    report: &mut Value,
    result: Result<()>,
    inspection: bool,
) -> Result<()> {
    if let Err(error) = &result
        && error.downcast_ref::<ExitStatus>().is_none()
    {
        report["error_type"] = "Error".into();
        report["error"] = error.to_string().into();
    }
    private_json(&work.join("report.json"), report)?;
    sync_directory(work)?;
    let mut summary = json!({"status":report["status"],"report":work.join("report.json")});
    if inspection {
        summary["unknown_controls"] = true.into();
        summary["restore_eligible"] = false.into();
    } else {
        summary["missing_approvals"] = report
            .get("missing_approvals")
            .cloned()
            .unwrap_or(json!([]));
    }
    println!("{summary}");
    if report["status"] == "cleanup_failed" {
        return Err(ExitStatus(1).into());
    }
    result
}
pub fn logical(args: RestoreDrill) -> Result<()> {
    if !(1..=120).contains(&args.startup_timeout_seconds) {
        return Err("startup timeout must be 1..120".into());
    }
    utc(&args.restore_target)?;
    let config = config(Some(&args.source_config))?;
    let source_public = canonical(&args.source_public_dir)?;
    if source_public != canonical(&config.public.dir)? {
        return Err("source-public-dir must match source config public.dir".into());
    }
    let base = private_path(
        &private_path(&args.work_dir, &root().join("public-dist"))?,
        &source_public,
    )?;
    let work = temp_dir(&base, "tolight-restore-")?;
    let temporary = format!("tolight_restore_{}", uuid::Uuid::new_v4().simple());
    let mut report = json!({"schema":"tolight-restore-drill/1","scope":"logical_no_wal","status":"failed","work_dir":work,"temporary_database":temporary});
    let mut created = false;
    let mut target_child = None;
    let mut admin = None;
    let result = (|| -> Result<()> {
        let backup = private_path(&args.backup, &source_public)?;
        regular(&backup)?;
        let hash = sha256(&backup)?;
        report["backup_sha256"] = hash.clone().into();
        let metadata = PathBuf::from(format!("{}.meta.json", backup.display()));
        if metadata.exists() {
            let m = json_file(&metadata)?;
            if m["schema"] != "tolight-backup/1"
                || m["file"].as_str() != backup.file_name().and_then(|n| n.to_str())
                || m["sha256"] != hash
            {
                return Err("backup metadata checksum mismatch".into());
            }
        }
        let published = PublicTree::load(&source_public)?;
        let files = tree_files(&source_public)?;
        let url = database_url(&config)?;
        let mut env = base_env();
        let mut parsed = url::Url::parse(&url)?;
        parsed.set_path(&temporary);
        env.insert(
            "TOLIGHT_RESTORE_DATABASE_URL".into(),
            parsed.as_str().into(),
        );
        let binary = canonical(&args.tolight)?;
        let snapshot = work.join("controls.json");
        capture(
            cli(&binary, &args.source_config, &env)
                .args(["operator", "export-controls", "--output"])
                .arg(&snapshot)
                .args(["--restore-target", &args.restore_target]),
            &work.join("export.stdout"),
        )?;
        report["steps"] = json!(["frozen_source_controls_exported"]);
        let administrator = pg_environment(&url, Some("postgres"))?;
        capture_cleanup(
            command(program("createdb"), &administrator).arg(&temporary),
            &work.join("createdb.stdout"),
        )?;
        admin = Some(administrator);
        created = true;
        capture(
            command(
                program("pg_restore"),
                &pg_environment(&url, Some(&temporary))?,
            )
            .args([
                "--exit-on-error",
                "--no-owner",
                "--no-privileges",
                "--dbname",
                &temporary,
            ])
            .arg(&backup),
            &work.join("pg_restore.stdout"),
        )?;
        step(&mut report, "backup_restored_into_disposable_database");
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
        let target = work.join("target.toml");
        let (api, ops) = (port()?, port()?);
        write_config(
            &target,
            "TOLIGHT_RESTORE_DATABASE_URL",
            &work.join("public"),
            config.public.demo_banner,
            Some((api, ops)),
        )?;
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
        target_child = Some(OwnedChild::start(
            cli(&binary, &target, &env).arg("serve"),
            &work.join("serve.stdout"),
        )?);
        report["target_serve"] = json!({"pid":target_child.as_ref().unwrap().child.id(),"participant_port":api,"operations_port":ops});
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
        if !target_child.as_mut().unwrap().stop()? {
            return Err("restored serve did not exit normally after SIGTERM".into());
        }
        if tree_files(&source_public)? != files {
            return Err("source public files changed during frozen-source exercise".into());
        }
        report["source_public_unchanged"] = true.into();
        step(&mut report, "restored_serve_healthy_and_normally_stopped");
        report["status"] = "passed".into();
        Ok(())
    })();
    if let Some(child) = &mut target_child {
        let normal = child.stop().unwrap_or(false);
        report["target_serve"]["normal_shutdown"] = normal.into();
        report["target_serve"]["exit_code"] =
            json!(child.child.try_wait().ok().flatten().and_then(|s| s.code()));
        report["target_serve"]["process_removed"] =
            child.child.try_wait().is_ok_and(|s| s.is_some()).into();
    }
    let safe = target_child
        .as_mut()
        .is_none_or(|c| c.child.try_wait().is_ok_and(|s| s.is_some()));
    if created {
        if safe {
            let cleanup = capture_cleanup(
                command(
                    program("dropdb"),
                    admin.as_ref().expect("created database admin"),
                )
                .args(["--force", &temporary]),
                &work.join("dropdb.stdout"),
            );
            report["temporary_database_removed"] = cleanup.is_ok().into();
            if cleanup.is_err() {
                report["status"] = "cleanup_failed".into();
            }
        } else {
            report["temporary_database_removed"] = false.into();
            report["status"] = "cleanup_failed".into();
        }
    }
    finish(&work, &mut report, result, false)
}
