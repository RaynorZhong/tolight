use super::*;
fn hashes(public: &Path) -> Result<BTreeMap<String, String>> {
    files(&public.join("data"))?
        .into_iter()
        .map(|path| {
            Ok((
                path.strip_prefix(public)?.to_string_lossy().into_owned(),
                format!("{:x}", Sha256::digest(fs::read(&path)?)),
            ))
        })
        .collect()
}
async fn ready(demo: &Demo) -> Result {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if c::fetch(
            &demo.http,
            format!("{}self", demo.api),
            reqwest::Method::GET,
            None,
        )
        .await
        .is_ok_and(|r| r.status == 401)
        {
            return Ok(());
        }
        c::ensure(
            Instant::now() < deadline,
            "rollback Coordinator readiness timed out",
        )?;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
async fn execute(demo: &mut Demo, args: &M1) -> Result {
    demo.ctl(osargs(["migrate"]), "migrate-first").await?;
    demo.ctl(osargs(["migrate"]), "migrate-repeat").await?;
    let mut install = osargs(["publish", "--install-assets"]);
    install.push(c::root().join("public").into());
    demo.ctl(install, "install-assets").await?;
    demo.ctl(osargs(["publish", "--rebuild"]), "rebuild-empty")
        .await?;
    demo.server("coordinator").await?;
    for (actor, other) in [("alice", "bob"), ("bob", "alice")] {
        demo.actor(actor, other, None, false, actor == "alice")
            .await?;
        demo.sandbox(actor).await?;
    }
    let pending = demo
        .ctl(
            osargs(["operator", "agent", "list", "--pending"]),
            "pending-before-approval",
        )
        .await?;
    c::ensure(
        pending.contains("alice")
            && pending.contains("bob")
            && json_file(demo.public.join("data/agents.json"))?["agents"] == json!([]),
        "pending actors must remain private",
    )?;
    for actor in ["bob", "alice"] {
        demo.approve(actor).await?;
    }
    let bob = demo.actor_run("bob", "conversation")?;
    demo.public_cursor("bob").await?;
    let alice = demo.actor_run("alice", "conversation")?;
    demo.start_web()?;
    demo.record("chatting", json!({}), false)?;
    println!(
        "本地网页：{}\n运行证据：{}\n等待真实限速和恢复…",
        demo.web,
        demo.run.display()
    );
    let deadline = Instant::now() + Duration::from_secs(args.duration);
    let limited = loop {
        c::ensure(
            demo.children[alice].child.try_wait()?.is_none()
                && demo.children[bob].child.try_wait()?.is_none(),
            "chat actor exited early",
        )?;
        let count = demo
            .events()?
            .iter()
            .filter(|e| e["kind"] == "agent_message")
            .count();
        let logs = fs::read_to_string(demo.logs.join("alice-conversation.log"))?
            + &fs::read_to_string(demo.logs.join("bob-conversation.log"))?;
        let limited = logs.contains("429") || logs.contains("message_limit");
        if count >= args.min_messages && limited {
            break limited;
        }
        c::ensure(
            Instant::now() < deadline,
            "expected messages and real rate-limit recovery not observed",
        )?;
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    demo.stop_actors(&[alice, bob]).await?;
    demo.publish("publish-after-clients-stopped").await?;
    let manifest = json_file(demo.public.join("data/manifest.json"))?;
    let count = demo
        .events()?
        .iter()
        .filter(|e| e["kind"] == "agent_message")
        .count();
    demo.verify(true, false, "verify-public").await?;
    if let Some(baseline) = &args.rollback_baseline {
        let baseline = baseline.canonicalize()?;
        let binary = baseline.join("tolight");
        c::regular(&binary, true)?;
        c::ensure(
            baseline.join("public").is_dir(),
            "rollback baseline must contain verified tolight and public/",
        )?;
        let before = hashes(&demo.public)?;
        demo.stop(demo.server.ok_or("missing Coordinator")?, 15)
            .await?;
        // A previous build can predate optional conformance and observability
        // settings. Use the shared original configuration vocabulary for it.
        let rollback_config = demo.run.join("rollback-coordinator.toml");
        private_text(
            &rollback_config,
            &format!(
                "listen=\"127.0.0.1:{}\"\n[database]\nsecret_cmd=[\"/usr/bin/printenv\",\"DATABASE_URL\"]\n[public]\ndir={}\ndemo_banner=true\n",
                demo.api_port,
                quote(&demo.public)
            ),
        )?;
        let mut cmd = demo.cmd(&binary);
        cmd.arg("--config")
            .arg(&rollback_config)
            .args(["publish", "--install-assets"])
            .arg(baseline.join("public"));
        demo.command(cmd, "rollback-assets", 0, 150).await?;
        let mut cmd = demo.cmd(&binary);
        cmd.arg("--config").arg(&rollback_config).arg("serve");
        let previous = demo.start(cmd, "rollback-coordinator", false)?;
        ready(demo).await?;
        c::ensure(
            hashes(&demo.public)? == before,
            "rollback binary altered public data",
        )?;
        demo.stop(previous, 15).await?;
        let mut install = osargs(["publish", "--install-assets"]);
        install.push(c::root().join("public").into());
        demo.ctl(install, "restore-current-assets").await?;
        demo.server("coordinator-restored").await?;
        c::ensure(
            hashes(&demo.public)? == before,
            "restoring current binary altered public data",
        )?;
        c::save(
            &demo.run.join("rollback.json"),
            &json!({"baseline_sha256":format!("{:x}",Sha256::digest(fs::read(binary)?)),"current_sha256":format!("{:x}",Sha256::digest(fs::read(&demo.tolight)?)),"data_unchanged":true,"before":before,"after":hashes(&demo.public)?,"scope":"local verified build snapshots; not published release commits"}),
        )?;
    }
    c::save(
        &demo.run.join("verified.json"),
        &json!({"messages":count,"rate_limited":limited,"manifest":manifest}),
    )?;
    demo.record("passed", json!({"result":"passed"}), false)?;
    println!("本地 M1 验收通过：{count}条消息，已观察到限速及后续恢复。");
    if args.keep_running {
        demo.actor_run("alice", "resumed")?;
        demo.actor_run("bob", "resumed")?;
        println!("服务保持运行；Ctrl-C停止自有进程和临时数据库。");
        std::future::pending::<()>().await;
    }
    Ok(())
}
pub(super) async fn run(args: M1) -> Result {
    c::ensure(
        args.duration >= 65 && args.min_messages >= 1,
        "duration must be at least65 and min-messages positive",
    )?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut demo = Demo::new(false, false).await?;
    tokio::select! {result=execute(&mut demo,&args)=>result,_=interrupt.recv()=>{println!("正在正常停止本地演示…");Ok(())},_=terminate.recv()=>{println!("正在正常停止本地演示…");Ok(())}}
}
