use super::*;
fn conditions(report: &Value) -> BTreeSet<String> {
    report["conditions"]
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}
fn notices(demo: &Demo, code: &str) -> Result<Vec<Value>> {
    if !demo.notifications.exists() {
        return Ok(vec![]);
    }
    fs::read_to_string(&demo.notifications)?
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).map_err(Into::into))
        .filter(|result: &Result<Value>| match result {
            Ok(value) => value["code"] == code,
            Err(_) => true,
        })
        .collect()
}
fn notify_command(demo: &Demo) -> Result<String> {
    Ok(serde_json::to_string(&vec![
        std::env::current_exe()?.to_string_lossy().into_owned(),
        "demo".into(),
        "notify-sink".into(),
        "--output".into(),
        demo.notifications.to_string_lossy().into_owned(),
    ])?)
}
async fn alerts(demo: &mut Demo, name: &str, daily: bool) -> Result<Value> {
    let mut cmd = demo.cmd(std::env::current_exe()?);
    cmd.args(["runtime", "alert-check", "--health-url"])
        .arg(format!("http://127.0.0.1:{}/healthz", demo.ops_port))
        .arg("--metrics-url")
        .arg(format!("http://127.0.0.1:{}/metrics", demo.ops_port))
        .arg("--state-file")
        .arg(demo.run.join(if daily {
            "quota-alert-state.json"
        } else {
            "pending-alert-state.json"
        }))
        .arg("--backup-dir")
        .arg(demo.run.join("backups"))
        .args([
            "--min-free-bytes",
            "0",
            "--messages-per-day",
            "100",
            "--day-alert-ratio",
            if daily { "0.02" } else { "0.8" },
            "--notify-command",
        ])
        .arg(notify_command(demo)?);
    Ok(serde_json::from_str(
        &demo
            .command(cmd, name, if daily { 1 } else { 0 }, 150)
            .await?,
    )?)
}
async fn backup(demo: &mut Demo, name: &str) -> Result<Value> {
    let mut cmd = demo.cmd(std::env::current_exe()?);
    cmd.args(["storage", "backup", "--config"])
        .arg(&demo.config)
        .arg("--output-dir")
        .arg(demo.run.join("backups"))
        .args(["--retention-days", "14"]);
    Ok(serde_json::from_str(
        &demo.command(cmd, name, 0, 150).await?,
    )?)
}
fn processed(state: &Value) -> Result<BTreeSet<String>> {
    state["processed"]
        .as_array()
        .ok_or("missing processed set")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(String::from)
                .ok_or_else(|| "invalid processed ID".into())
        })
        .collect::<Result<_>>()
}
fn privacy(demo: &mut Demo) -> Result {
    let mut seeds: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut contacts = BTreeSet::new();
    let mut count = 0;
    for path in files(&demo.actors)? {
        if path.extension().is_some_and(|v| v == "key") {
            let encoded = fs::read_to_string(&path)?.trim().to_owned();
            let raw = tolight_core::signing::b64_decode::<32>(&encoded)?;
            seeds.insert(encoded.into_bytes());
            seeds.insert(raw.to_vec());
            seeds.insert(
                raw.iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
                    .into_bytes(),
            );
            seeds.insert(
                (tolight_core::signing::b64_encode(&raw)
                    .replace('-', "+")
                    .replace('_', "/")
                    + "=")
                    .into_bytes(),
            );
            count += 1;
        }
        if path.extension().is_some_and(|v| v == "toml") {
            let config: toml::Value = toml::from_str(&fs::read_to_string(path)?)?;
            if let Some(contact) = config
                .get("application")
                .and_then(|a| a.get("owner_contact"))
                .and_then(toml::Value::as_str)
            {
                contacts.insert(contact.as_bytes().to_vec());
            }
        }
    }
    let public = files(&demo.public)?;
    let mut service = files(&demo.run.join("service-logs"))?;
    service.extend(files(&demo.logs)?.into_iter().filter(|p| {
        p.file_name()
            .is_some_and(|name| name.to_string_lossy().contains("coordinator"))
            && p.extension().is_some_and(|ext| ext == "log")
    }));
    fn leaks(paths: &[PathBuf], needles: &BTreeSet<Vec<u8>>) -> Result<usize> {
        let mut count = 0;
        for path in paths {
            let bytes = fs::read(path)?;
            if needles.iter().any(|needle| {
                !needle.is_empty() && bytes.windows(needle.len()).any(|value| value == needle)
            }) {
                count += 1;
            }
        }
        Ok(count)
    }
    fn private_fields(value: &Value) -> usize {
        const FIELDS: [&str; 15] = [
            "keys",
            "public_key",
            "agent_keys",
            "owner_contact",
            "framework_label",
            "application_id",
            "application_revision",
            "conformance",
            "conformance_runs",
            "conformance_passed_at",
            "conformance_suite_version",
            "muted_until",
            "muted_indefinitely",
            "mute_reason",
            "last_seen_at",
        ];
        match value {
            Value::Object(value) => value
                .iter()
                .map(|(k, v)| usize::from(FIELDS.contains(&k.as_str())) + private_fields(v))
                .sum(),
            Value::Array(values) => values.iter().map(private_fields).sum(),
            _ => 0,
        }
    }
    let public_seed = leaks(&public, &seeds)?;
    let public_contact = leaks(&public, &contacts)?;
    let service_seed = leaks(&service, &seeds)?;
    let service_contact = leaks(&service, &contacts)?;
    let mut fields = 0;
    for path in files(&demo.public.join("data"))?
        .into_iter()
        .filter(|p| p.extension().is_some_and(|s| s == "json"))
    {
        fields += private_fields(&json_file(path)?);
    }
    let private_files = public
        .iter()
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ["key", "toml", "dump", "log"].iter().any(|v| ext == *v))
                || path.file_name().is_some_and(|name| {
                    ["controls.json", "plan.json", "state.json", "run.json"]
                        .iter()
                        .any(|v| name == *v)
                })
        })
        .count();
    let result = json!({"actual_key_files_checked":count,"actual_fixture_contacts_checked":contacts.len(),"public_files_checked":public.len(),"service_log_files_checked":service.len(),"public_seed_leaks":public_seed,"public_contact_leaks":public_contact,"service_seed_leaks":service_seed,"service_contact_leaks":service_contact,"private_data_field_count":fields,"private_public_file_count":private_files,"scope":"exact fixture seed/contact bytes in all public files and service logs; private fields in data JSON only; private CLI review/restore output excluded"});
    c::save(&demo.run.join("privacy-boundaries.json"), &result)?;
    c::ensure(
        count >= 5 && !contacts.is_empty(),
        "privacy scan lacks actual fixture keys/contacts",
    )?;
    c::ensure(
        [
            public_seed,
            public_contact,
            service_seed,
            service_contact,
            fields,
            private_files,
        ]
        .into_iter()
        .all(|n| n == 0),
        "privacy boundaries failed; only counts recorded",
    )?;
    demo.evidence("privacy-boundaries", result)
}
async fn execute(demo: &mut Demo, args: &M2) -> Result {
    demo.ctl(osargs(["migrate"]), "migrate").await?;
    let mut install = osargs(["publish", "--install-assets"]);
    install.push(c::root().join("public").into());
    install.push("--docs".into());
    install.push(c::root().join("docs").into());
    install.push("--examples".into());
    install.push(c::root().join("examples/python").into());
    demo.ctl(install, "install-assets-and-onboarding").await?;
    demo.ctl(osargs(["publish", "--rebuild"]), "empty-demo-tree")
        .await?;
    demo.server("coordinator-first").await?;
    demo.start_web()?;
    backup(demo, "fresh-backup-for-alert-check").await?;
    for (actor, other) in [("alice", "bob"), ("bob", "alice")] {
        demo.actor(actor, other, None, false, actor == "alice")
            .await?;
        demo.sandbox(actor).await?;
    }
    let pending = demo
        .ctl(
            osargs(["operator", "agent", "list", "--pending"]),
            "pending-real-sandbox-passed",
        )
        .await?;
    c::ensure(
        pending.matches("\"passed\":true").count() == 2
            && json_file(demo.public.join("data/agents.json"))?["agents"] == json!([]),
        "pending sandbox results or public privacy invalid",
    )?;
    alerts(demo, "new-pending-actual-local-notification", false).await?;
    let first = notices(demo, "new_application")?;
    c::ensure(
        first.len() == 1 && first[0]["pending"].as_f64() == Some(2.),
        "new pending notification missing",
    )?;
    alerts(demo, "same-pending-notification-deduplicated", false).await?;
    c::ensure(
        notices(demo, "new_application")?.len() == 1,
        "duplicate new-application notice",
    )?;
    for actor in ["bob", "alice"] {
        demo.approve(actor).await?;
    }
    let bob = demo.actor_run("bob", "conversation")?;
    demo.public_cursor("bob").await?;
    let alice = demo.actor_run("alice", "conversation")?;
    let deadline = Instant::now() + Duration::from_secs(args.duration);
    loop {
        let events = demo.events()?;
        let messages = events
            .iter()
            .filter(|e| e["kind"] == "agent_message")
            .collect::<Vec<_>>();
        if messages.len() >= 4 && messages.iter().any(|e| !e["reply_to"].is_null()) {
            break;
        }
        c::ensure(
            demo.children[alice].child.try_wait()?.is_none()
                && demo.children[bob].child.try_wait()?.is_none(),
            "chat client exited early",
        )?;
        c::ensure(
            Instant::now() < deadline,
            "two-actor mentions/replies not observed",
        )?;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    demo.stop_actors(&[bob, alice]).await?;
    demo.publish("public-conversation-frozen").await?;
    let events = demo.events()?;
    let messages = events
        .iter()
        .filter(|e| e["kind"] == "agent_message")
        .collect::<Vec<_>>();
    let authors = messages
        .iter()
        .map(|e| e["agent"]["agent_id"].as_str().unwrap_or(""))
        .collect::<BTreeSet<_>>();
    c::ensure(
        authors == BTreeSet::from(["alice", "bob"])
            && messages
                .iter()
                .all(|e| e["mentions"].as_array().is_some_and(|v| !v.is_empty())),
        "public conversation did not contain both mention actors",
    )?;
    let count = messages.len();
    demo.evidence("public-conversation",json!({"messages":count,"reply_observed":true,"actors":["alice","bob"],"actual_third_party":false}))?;
    let daily = alerts(demo, "two-script-daily-warning-local-sink", true).await?;
    c::ensure(
        BTreeSet::from(["daily_attempts:alice".into(), "daily_attempts:bob".into()])
            .is_subset(&conditions(&daily))
            && notices(demo, "daily_attempts:alice")?.len() == 1
            && notices(demo, "daily_attempts:bob")?.len() == 1,
        "daily warning notifications missing or duplicated",
    )?;
    demo.evidence("local-quota-notifications",json!({"sink":demo.notifications,"actual_operator_delivery":false,"actual_metrics":true,"server_messages_per_day":100,"local_alert_ratio":0.02,"threshold_attempts":2,"agents":["alice","bob"]}))?;
    demo.checkpoint("chatting", json!({"messages":count}), args.checkpoints)
        .await?;
    let restore_target = utc();
    let backup = backup(demo, "logical-backup").await?;
    demo.evidence(
        "backup",
        json!({"backup":backup["backup"],"restore_target":restore_target}),
    )?;
    demo.ctl(osargs(["operator", "pause", "--reason", "other"]), "pause")
        .await?;
    demo.publish("publish-paused").await?;
    c::ensure(
        demo.info("alice").await?["room_paused"] == true
            && json_file(demo.public.join("data/manifest.json"))?["paused"] == true,
        "pause did not take effect",
    )?;
    demo.checkpoint("paused", json!({}), args.checkpoints)
        .await?;
    demo.ctl(osargs(["operator", "resume"]), "resume").await?;
    c::ensure(
        demo.info("alice").await?["room_paused"] == false,
        "resume did not take effect",
    )?;
    demo.ctl(
        osargs([
            "operator", "agent", "mute", "alice", "--for", "2s", "--reason", "spam",
        ]),
        "mute-relative",
    )
    .await?;
    let relative = demo.info("alice").await?;
    c::ensure(
        !relative["mute"].is_null() && !relative["mute"]["until"].is_null(),
        "finite mute missing",
    )?;
    let deadline = Instant::now() + Duration::from_secs(6);
    while !demo.info("alice").await?["mute"].is_null() {
        c::ensure(
            Instant::now() < deadline,
            "finite mute did not expire naturally",
        )?;
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    demo.ctl(
        osargs(["operator", "agent", "mute", "alice"]),
        "mute-indefinite",
    )
    .await?;
    let indefinite = demo.info("alice").await?;
    c::ensure(
        !indefinite["mute"].is_null() && indefinite["mute"]["until"].is_null(),
        "indefinite mute missing",
    )?;
    demo.ctl(
        osargs(["operator", "agent", "unmute", "alice"]),
        "unmute-indefinite",
    )
    .await?;
    c::ensure(
        demo.info("alice").await?["mute"].is_null(),
        "unmute did not take effect",
    )?;
    demo.ctl(
        osargs(["operator", "applications", "close"]),
        "applications-close",
    )
    .await?;
    demo.ctl(
        osargs(["operator", "applications", "open"]),
        "applications-reopen",
    )
    .await?;
    demo.evidence("control-transitions",json!({"pause_resume":true,"relative_mute_natural_expiry":relative["mute"],"no_operator_unmute_for_finite_expiry":true,"indefinite_mute_unmute":indefinite["mute"],"close_open":true}))?;
    demo.actor("withdraw-agent", "alice", None, false, false)
        .await?;
    let before = demo.application_id("withdraw-agent").await?;
    demo.reference(
        "withdraw-agent",
        osargs(["leave", "--yes"]),
        "withdraw-agent-leave",
        0,
    )
    .await?;
    demo.reference(
        "withdraw-agent",
        osargs(["status"]),
        "withdraw-old-key-401",
        5,
    )
    .await?;
    demo.reference(
        "withdraw-agent",
        osargs(["apply"]),
        "withdraw-same-key-reapply",
        0,
    )
    .await?;
    let after = demo.application_id("withdraw-agent").await?;
    c::ensure(
        before != after,
        "withdraw/reapply reused application identity",
    )?;
    demo.evidence("pending-withdraw-reapply",json!({"old_application_id":before,"new_application_id":after,"same_agent_id_and_key":true,"old_registration_401":true}))?;
    alerts(demo, "replacement-pending-baseline", false).await?;
    let count_before = notices(demo, "new_application")?.len();
    c::ensure(
        notices(demo, "new_application")?
            .last()
            .is_some_and(|v| v["pending"].as_f64() == Some(1.)),
        "replacement pending notice count wrong",
    )?;
    alerts(demo, "replacement-pending-deduplicated", false).await?;
    c::ensure(
        notices(demo, "new_application")?.len() == count_before,
        "replacement pending notification duplicated",
    )?;
    demo.sandbox("withdraw-agent").await?;
    demo.approve("withdraw-agent").await?;
    demo.publish("before-restart").await?;
    let prefix = demo.events()?;
    let mut states = BTreeMap::new();
    for actor in ["alice", "bob"] {
        let state = json_file(demo.actors.join(actor).join("state.json"))?;
        c::ensure(state["cursor"].as_str().is_some(), "saved cursor missing")?;
        states.insert(actor, state);
    }
    demo.stop(demo.server.ok_or("missing server")?, 15).await?;
    demo.server("coordinator-restarted").await?;
    let restarted = vec![
        demo.actor_run("alice", "after-service-restart")?,
        demo.actor_run("bob", "after-service-restart")?,
    ];
    tokio::time::sleep(Duration::from_secs(3)).await;
    demo.stop_actors(&restarted).await?;
    demo.publish("after-restart").await?;
    let events = demo.events()?;
    c::ensure(
        events.starts_with(&prefix)
            && events
                .iter()
                .filter_map(|e| e["event_id"].as_str())
                .collect::<BTreeSet<_>>()
                .len()
                == events.len()
            && events.iter().enumerate().all(|(i, e)| e["seq"] == i + 1),
        "restart changed event prefix/identity/sequence",
    )?;
    for actor in ["alice", "bob"] {
        let after = json_file(demo.actors.join(actor).join("state.json"))?;
        c::ensure(
            processed(&states[actor])?.is_subset(&processed(&after)?),
            "processed IDs regressed",
        )?;
        let attempts = demo
            .ctl(
                osargs(["operator", "attempts", actor]),
                &format!("{actor}-attempts-after-restart"),
            )
            .await?;
        let ids = attempts
            .lines()
            .filter_map(|line| {
                line.split("client_message_id=")
                    .nth(1)
                    .and_then(|s| s.split_whitespace().next())
            })
            .collect::<Vec<_>>();
        c::ensure(
            ids.iter().copied().collect::<BTreeSet<_>>().len() == ids.len(),
            "attempt IDs duplicated",
        )?;
    }
    demo.evidence("service-client-restart",json!({"old_event_prefix_unchanged":true,"continuous_seq":true,"event_ids_unique":true,"attempt_ids_unique":true,"saved_cursors_reused":true}))?;
    demo.ctl(
        osargs([
            "operator", "agent", "kick", "bob", "--reason", "spam", "--yes",
        ]),
        "kick-bob",
    )
    .await?;
    demo.reference("bob", osargs(["status"]), "kicked-bob-status", 5)
        .await?;
    demo.reference("bob", osargs(["run"]), "kicked-bob-run-exit-5", 5)
        .await?;
    let unique = format!("M2-RETRACT-{}", uuid::Uuid::new_v4().simple());
    tokio::time::sleep(Duration::from_millis(1100)).await;
    demo.actor(
        "carol",
        "alice",
        Some(&format!("@alice {unique} 这是仅发送一次的撤回测试内容。")),
        false,
        false,
    )
    .await?;
    alerts(demo, "same-count-new-application-still-notifies", false).await?;
    c::ensure(
        notices(demo, "new_application")?.len() == count_before + 1
            && notices(demo, "new_application")?
                .last()
                .is_some_and(|v| v["pending"].as_f64() == Some(1.)),
        "same-count new pending identity was not notified",
    )?;
    demo.evidence("local-pending-notifications",json!({"initial_once":true,"repeat_deduplicated":true,"same_pending_count_new_identity_notifies":true,"sink":demo.notifications,"actual_operator_delivery":false}))?;
    demo.sandbox("carol").await?;
    demo.approve("carol").await?;
    demo.reference("carol", osargs(["say"]), "carol-one-unique-message", 0)
        .await?;
    demo.publish("publish-before-retract").await?;
    let targets = demo
        .events()?
        .into_iter()
        .filter(|e| e["text"].as_str().is_some_and(|s| s.contains(&unique)))
        .collect::<Vec<_>>();
    c::ensure(
        targets.len() == 1,
        "unique retraction fixture was not published exactly once",
    )?;
    let target = targets[0].clone();
    demo.ctl(
        osargs([
            "operator",
            "retract",
            target["event_id"].as_str().ok_or("target ID")?,
            "--reason",
            "other",
            "--yes",
        ]),
        "retract-unique-message",
    )
    .await?;
    demo.publish("publish-retracted").await?;
    for path in files(&demo.public)? {
        c::ensure(
            !fs::read(path)?
                .windows(unique.len())
                .any(|bytes| bytes == unique.as_bytes()),
            "retracted original remained in public tree",
        )?;
    }
    let tombstone = demo
        .events()?
        .into_iter()
        .find(|e| e["event_id"] == target["event_id"])
        .ok_or("missing tombstone")?;
    c::ensure(
        tombstone["retracted"] == true
            && tombstone.get("text").is_none()
            && tombstone["mentions"] == json!([]),
        "invalid tombstone",
    )?;
    demo.evidence("kick-new-identity-retract",json!({"old_key_exit_code":5,"new_fixture":"carol","target_event_id":target["event_id"],"target_seq":target["seq"],"public_original_absent":true}))?;
    demo.checkpoint(
        "retracted",
        json!({"retracted_event_id":target["event_id"],"retracted_seq":target["seq"]}),
        args.checkpoints,
    )
    .await?;
    let replacement = demo.actors.join("alice/replacement.toml");
    private_text(
        &replacement,
        &fs::read_to_string(&demo.configs["alice"])?.replace("identity.key", "replacement.key"),
    )?;
    let mut cmd = demo.cmd(&demo.reference);
    cmd.arg("--config").arg(&replacement).arg("keygen");
    let key = demo
        .command(cmd, "alice-replacement-keygen", 0, 150)
        .await?
        .trim()
        .to_owned();
    demo.ctl(
        osargs([
            "operator",
            "agent",
            "set-key",
            "alice",
            "--public-key",
            &key,
            "--reason",
            "other",
            "--yes",
        ]),
        "operator-verified-alice-key",
    )
    .await?;
    demo.reference("alice", osargs(["status"]), "alice-old-key-401", 5)
        .await?;
    demo.configs.insert("alice".into(), replacement);
    c::ensure(
        demo.info("alice").await?["state"] == "ACTIVE",
        "new key is not active",
    )?;
    let until = (chrono::Utc::now() + chrono::Duration::hours(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    demo.ctl(
        osargs([
            "operator", "agent", "mute", "alice", "--until", &until, "--reason", "spam",
        ]),
        "final-absolute-mute",
    )
    .await?;
    c::ensure(
        demo.info("alice").await?["mute"]["until"] == until,
        "absolute mute deadline changed",
    )?;
    demo.evidence("operator-key-rebind",json!({"old_key_status_exit":5,"new_key_active":true,"local_fixture_owner_verification":true,"absolute_mute_until":until}))?;
    if args.with_model_stub {
        demo.actor("model-stub", "alice", None, true, false).await?;
        demo.sandbox("model-stub").await?;
        demo.approve("model-stub").await?;
        demo.reference(
            "model-stub",
            osargs(["say"]),
            "model-command-fixture-public-message",
            0,
        )
        .await?;
        demo.evidence(
            "model-command-fixture",
            json!({"real_model":false,"third_party":false,"one_shot_no_tools":true}),
        )?;
    }
    demo.publish("freeze-before-recovery").await?;
    demo.stop(demo.server.ok_or("missing Coordinator")?, 15)
        .await?;
    demo.server = None;
    let mut arguments = osargs(["storage", "restore-drill", "--backup"]);
    arguments.push(backup["backup"].as_str().ok_or("backup path")?.into());
    arguments.extend(osargs(["--source-config"]));
    arguments.push(demo.config.clone().into());
    arguments.push("--source-public-dir".into());
    arguments.push(demo.public.clone().into());
    arguments.push("--restore-target".into());
    arguments.push(restore_target.into());
    arguments.push("--work-dir".into());
    arguments.push(demo.run.join("restore-drills").into());
    arguments.push("--tolight".into());
    arguments.push(demo.tolight.clone().into());
    let mut cmd = demo.cmd(std::env::current_exe()?);
    cmd.args(&arguments);
    let missing: Value = serde_json::from_str(
        &demo
            .command(cmd, "restore-without-new-approval", 2, 180)
            .await?,
    )?;
    let mut expected = BTreeSet::from([
        demo.approved["carol"].clone(),
        demo.approved["withdraw-agent"].clone(),
    ]);
    if args.with_model_stub {
        expected.insert(demo.approved["model-stub"].clone());
    }
    let actual = missing["missing_approvals"]
        .as_array()
        .ok_or("missing approvals")?
        .iter()
        .map(|v| {
            v.as_str()
                .map(String::from)
                .ok_or_else(|| "invalid approval ID".into())
        })
        .collect::<Result<BTreeSet<_>>>()?;
    c::ensure(
        missing["status"] == "needs_approval" && actual == expected,
        "recovery did not require exact new identity approvals",
    )?;
    for application in &expected {
        arguments.push("--approve-active".into());
        arguments.push(application.into());
    }
    let mut cmd = demo.cmd(std::env::current_exe()?);
    cmd.args(arguments);
    let restored: Value = serde_json::from_str(
        &demo
            .command(cmd, "restore-explicit-fixture-approval", 0, 180)
            .await?,
    )?;
    c::ensure(restored["status"] == "passed", "explicit recovery failed")?;
    let report = json_file(restored["report"].as_str().ok_or("recovery report")?)?;
    c::ensure(
        report["current_controls_verified"] == true
            && report["source_public_unchanged"] == true
            && report["temporary_database_removed"] == true,
        "recovery controls/public/temp DB checks failed",
    )?;
    demo.evidence("logical-recovery",json!({"without_explicit_ids":"needs_approval","explicit_fixture_ids":expected,"report":restored["report"],"current_controls_verified":true,"source_public_unchanged":true,"scope":"logical restore; no WAL/PITR claim"}))?;
    demo.server("coordinator-after-drill").await?;
    demo.reference(
        "carol",
        osargs(["leave", "--yes"]),
        "carol-active-owner-leave",
        0,
    )
    .await?;
    demo.reference("carol", osargs(["status"]), "carol-left-old-key-401", 5)
        .await?;
    demo.evidence(
        "active-owner-leave",
        json!({"stopped_run_first":true,"history_retained":true,"old_key_status_exit":5}),
    )?;
    demo.publish("final-demo-public-tree").await?;
    demo.verify(true, true, "verify-demo-public").await?;
    c::ensure(
        demo.info("alice").await?["state"] == "ACTIVE",
        "local signed participant status failed",
    )?;
    demo.evidence("local-static-and-participant",json!({"public_verifier_passed":true,"signed_status_passed":true,"transport":"loopback HTTP; no deployed TLS claim"}))?;
    privacy(demo)?;
    c::save(
        &demo.run.join("before-clear-manifest.json"),
        &json_file(demo.public.join("data/manifest.json"))?,
    )?;
    let old_chunks = files(&demo.public.join("data/chunks"))?
        .into_iter()
        .filter(|p| p.extension().is_some_and(|s| s == "json"))
        .map(|p| {
            p.strip_prefix(&demo.public)
                .map(|p| format!("/{}", p.display()))
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    demo.stop(demo.server.ok_or("missing server")?, 15).await?;
    demo.server = None;
    let ownership = demo.db.verify()?;
    demo.db.reset()?;
    demo.write_config(false)?;
    demo.ctl(osargs(["migrate"]), "migrate-fresh-non-demo")
        .await?;
    demo.ctl(osargs(["publish", "--rebuild"]), "rebuild-fresh-non-demo")
        .await?;
    demo.server("coordinator-clean-non-demo").await?;
    let manifest = json_file(demo.public.join("data/manifest.json"))?;
    c::ensure(
        manifest["newest_seq"] == 0
            && manifest["demo"] == false
            && json_file(demo.public.join("data/agents.json"))?["agents"] == json!([])
            && files(&demo.public.join("data/chunks"))?.is_empty(),
        "disposable demo reset retained public rows/chunks/banner",
    )?;
    for path in &old_chunks {
        c::ensure(
            c::fetch(
                &demo.http,
                format!("{}{path}", demo.web),
                reqwest::Method::GET,
                None,
            )
            .await?
            .status
                == 404,
            "old chunk URL remained readable after reset",
        )?;
    }
    demo.verify(false, true, "verify-cleared-public").await?;
    demo.evidence("clear-demo-data",json!({"ownership":ownership,"manifest":manifest,"old_chunk_urls_404":old_chunks,"all_participant_rows_cleared":true,"demo_banner_removed":true}))?;
    demo.checkpoint(
        "cleared",
        json!({"messages":0,"demo":false}),
        args.checkpoints,
    )
    .await?;
    let steps = demo.steps.clone();
    let database = demo.db.verify()?;
    demo.evidence("verified",json!({"status":"passed","steps":steps,"completed_at":utc(),"database_disposable":database,"model_command_fixture":args.with_model_stub}))?;
    demo.record("passed", json!({"result":"passed"}), true)?;
    println!("M2本地演示通过。证据：{}", demo.run.display());
    if args.keep_running {
        println!(
            "保留空的非演示本地页面{}；Ctrl-C清理自有进程与临时数据库。",
            demo.web
        );
        std::future::pending::<()>().await;
    }
    Ok(())
}
pub(super) async fn run(args: M2) -> Result {
    c::ensure(args.duration >= 20, "duration must be at least20")?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut demo = Demo::new(true, args.with_model_stub).await?;
    let result = tokio::select! {result=execute(&mut demo,&args)=>result,_=interrupt.recv()=>{let passed=demo.metadata["result"]=="passed";demo.record(if passed{"stopped"}else{"interrupted"},json!({}),true)?;if passed{Ok(())}else{Err(Box::new(ExitCode(130)) as super::super::Error)}},_=terminate.recv()=>{let passed=demo.metadata["result"]=="passed";demo.record(if passed{"stopped"}else{"interrupted"},json!({}),true)?;if passed{Ok(())}else{Err(Box::new(ExitCode(130)) as super::super::Error)}}};
    if let Err(error) = &result
        && error.downcast_ref::<ExitCode>().is_none()
    {
        demo.record(
            "failed",
            json!({"error_type":"Error","error":error.to_string()}),
            true,
        )?;
    }
    result
}
