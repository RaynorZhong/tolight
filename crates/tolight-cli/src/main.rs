use clap::{Parser, Subcommand};
use std::{
    io::{self, IsTerminal, Write},
    net::SocketAddr,
    path::PathBuf,
};
use tolight_coordinator::{operator, publisher, service};
use tolight_coordinator_store::{SendLimits, Store};
use tolight_core::{config::Config, models::ApplicationInput};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Parser)]
#[command(
    name = "tolight",
    version,
    about = "公开 AI Agent 聊天室；本地 Coordinator、发布与运维"
)]
struct Cli {
    #[arg(long, global = true, default_value = "config.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// 离线校验安全报告资料，不读取配置或连接数据库
    CheckSecurity {
        #[arg(long, default_value = "public")]
        source: PathBuf,
    },
    /// 启动本地 Coordinator（不自动迁移）
    Serve,
    /// 只在 loopback 预览静态公共目录
    ServePublic {
        #[arg(long, default_value = "public-dist")]
        dir: PathBuf,
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: SocketAddr,
    },
    /// 显式执行幂等数据库迁移
    Migrate,
    /// 发布静态公共树；安装资源不触碰 data/
    Publish {
        #[arg(long)]
        rebuild: bool,
        #[arg(long)]
        install_assets: Option<PathBuf>,
        #[arg(long,requires_all=["examples","install_assets"])]
        docs: Option<PathBuf>,
        #[arg(long,requires_all=["docs","install_assets"])]
        examples: Option<PathBuf>,
    },
    /// 本机 Operator 控制
    Operator {
        #[command(subcommand)]
        command: Operator,
    },
}
#[derive(Subcommand)]
enum Operator {
    Pause {
        #[arg(long, default_value = "other")]
        reason: String,
    },
    Resume {
        #[arg(long, default_value = "other")]
        reason: String,
    },
    Attempts {
        agent_id: String,
        #[arg(long)]
        since: Option<String>,
    },
    /// 在线只读查询私有操作审计；JSON 明确报告截断，不触发申请清理
    Audit {
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        agent_id: Option<String>,
        #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u32).range(1..=10_000))]
        limit: u32,
    },
    Retract {
        event_id: String,
        #[arg(long, default_value = "other", help = "此理由码会随撤回事件永久公开")]
        reason: String,
        #[arg(long)]
        yes: bool,
    },
    ExportControls {
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        restore_target: String,
    },
    RestoreControls {
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long)]
        dry_run: bool,
        #[arg(long = "approve-active")]
        approve_active: Vec<uuid::Uuid>,
        #[arg(long)]
        yes: bool,
    },
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    Applications {
        #[command(subcommand)]
        command: ApplicationsCommand,
    },
}
#[derive(Subcommand)]
enum AgentCommand {
    Mute {
        agent_id: String,
        #[arg(long = "for", conflicts_with = "until")]
        for_duration: Option<String>,
        #[arg(long)]
        until: Option<String>,
        #[arg(long, default_value = "other")]
        reason: String,
    },
    Unmute {
        agent_id: String,
    },
    Kick {
        agent_id: String,
        #[arg(long, default_value = "other")]
        reason: String,
        #[arg(long)]
        yes: bool,
    },
    List {
        #[arg(long)]
        pending: bool,
    },
    Approve {
        agent_id: String,
        #[arg(long)]
        yes: bool,
    },
    Reject {
        agent_id: String,
        #[arg(long, default_value = "other")]
        reason: String,
    },
    Add {
        agent_id: String,
        #[arg(long)]
        display_name: String,
        #[arg(long)]
        bio: String,
        #[arg(long)]
        owner_contact: String,
        #[arg(long, default_value = "")]
        framework_label: String,
        #[arg(long)]
        public_key: String,
    },
    SetKey {
        agent_id: String,
        #[arg(long, default_value = "other")]
        reason: String,
        #[arg(long)]
        public_key: String,
        #[arg(long)]
        yes: bool,
    },
}
#[derive(Subcommand)]
enum ApplicationsCommand {
    Open,
    Close,
}

fn confirm(yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        return Err("非交互执行必须显式传 --yes；尚未执行操作".into());
    }
    print!("确认以上当前快照？输入 y：");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if answer.trim() != "y" {
        return Err("已取消，尚未执行操作".into());
    }
    Ok(())
}
async fn operate(store: &Store, config: &Config, command: Operator) -> Result<()> {
    store.check_schema().await?;
    match command {
        Operator::Pause { reason } => {
            operator::pause(store, config, true, &reason).await?;
            println!("聊天室已暂停");
        }
        Operator::Resume { reason } => {
            operator::pause(store, config, false, &reason).await?;
            println!("聊天室已恢复");
        }
        Operator::Attempts { agent_id, since } => {
            let since = since.as_deref().map(parse_utc).transpose()?;
            for attempt in operator::attempts_since(store, config, &agent_id, since).await? {
                let escape = tolight_core::validation::escape_terminal;
                println!(
                    "client_message_id={} at={} outcome={} reason={} detail={} event_seq={:?}",
                    escape(&attempt.client_message_id),
                    tolight_core::datetime::rfc3339(attempt.created_at),
                    escape(&attempt.outcome),
                    escape(attempt.reason_code.as_deref().unwrap_or("-")),
                    escape(attempt.reason_detail.as_deref().unwrap_or("-")),
                    attempt.event_seq
                );
            }
        }
        Operator::Audit {
            since,
            agent_id,
            limit,
        } => {
            let since = since.as_deref().map(parse_utc).transpose()?;
            let page = operator::audit(store, since, agent_id.as_deref(), limit as usize).await?;
            println!("{}", terminal_json(&page)?);
        }
        Operator::Retract {
            event_id,
            reason,
            yes,
        } => {
            let event = operator::review_event(store, config, &event_id).await?;
            let preview = event["text"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(100)
                .collect::<String>();
            println!(
                "撤回 seq={} 发言者={} 时间={} 正文开头={}",
                event["seq"],
                tolight_core::validation::escape_terminal(
                    event["agent"]["display_name"].as_str().unwrap_or("")
                ),
                event["created_at"],
                tolight_core::validation::escape_terminal(&preview)
            );
            confirm(yes)?;
            operator::retract(store, config, &event_id, &reason).await?;
            println!(
                "已撤回 seq={} event_id={} reason={}（此理由码会公开）",
                event["seq"],
                tolight_core::validation::escape_terminal(&event_id),
                reason
            );
        }
        Operator::ExportControls {
            output,
            restore_target,
        } => {
            let snapshot = store
                .export_controls(&output, &config.public.dir, parse_utc(&restore_target)?)
                .await?;
            println!(
                "已导出私有控制快照 export_id={} identities={}",
                snapshot.export_id,
                snapshot.agents.len()
            );
        }
        Operator::RestoreControls {
            snapshot,
            dry_run,
            approve_active,
            yes,
        } => {
            let path = tolight_coordinator_store::controls::validate_private_path(
                &snapshot,
                &config.public.dir,
            )?;
            let bytes = std::fs::read(path)?;
            let prepared = store
                .prepare_restore_controls(&bytes, tolight_core::CONFORMANCE_SUITE_VERSION)
                .await?;
            println!("{}", serde_json::to_string_pretty(prepared.plan())?);
            if !dry_run {
                confirm(yes)?;
                println!(
                    "{}",
                    serde_json::to_string(&prepared.commit(&approve_active).await?)?
                );
            }
        }
        Operator::Agent { command } => match command {
            AgentCommand::Mute {
                agent_id,
                for_duration,
                until,
                reason,
            } => {
                let previous = operator::review(store, config, &agent_id).await?;
                let until = match (for_duration, until) {
                    (Some(duration), None) => Some(
                        time::OffsetDateTime::now_utc()
                            .checked_add(parse_duration(&duration)?)
                            .ok_or("禁言期限超出范围")?,
                    ),
                    (None, Some(until)) => Some(parse_utc(&until)?),
                    (None, None) => None,
                    _ => return Err("--for 与 --until 不能同时指定".into()),
                };
                let agent = operator::mute(store, config, &agent_id, until, &reason).await?;
                println!(
                    "原期限：{}；新期限：{}；{}",
                    mute_label(&previous),
                    mute_label(&agent),
                    operator::render_agent(&agent)
                );
            }
            AgentCommand::Unmute { agent_id } => {
                println!(
                    "已解除禁言：{}",
                    operator::render_agent(&operator::unmute(store, config, &agent_id).await?)
                );
            }
            AgentCommand::Kick {
                agent_id,
                reason,
                yes,
            } => {
                let target = operator::review(store, config, &agent_id).await?;
                if target.state == "PENDING" {
                    return Err("待审申请请改用 operator agent reject".into());
                }
                println!("{}", operator::render_agent(&target));
                confirm(yes)?;
                println!(
                    "已移出：{}",
                    operator::render_agent(
                        &operator::kick(store, config, &agent_id, &reason).await?
                    )
                );
            }
            AgentCommand::List { pending } => {
                for agent in operator::list(store, config, pending).await? {
                    let expires = agent.registered_at
                        + time::Duration::seconds(config.application_ttl as i64);
                    if pending {
                        println!(
                            "{} expires_at={}",
                            operator::render_agent(&agent),
                            tolight_core::datetime::rfc3339(expires)
                        );
                        let data = tolight_coordinator_store::conformance::ConformanceData {
                            agent: agent.clone(),
                            runs: store.conformance_runs(&agent.agent_id).await?,
                        };
                        println!(
                            "conformance={}",
                            tolight_coordinator::conformance_session::self_info(
                                &data,
                                time::OffsetDateTime::now_utc(),
                                config
                            )
                        );
                    } else {
                        let limits = SendLimits {
                            messages_per_minute: config.messages_per_minute,
                            messages_per_day: config.messages_per_day,
                            max_mentions_per_message: config.max_mentions_per_message,
                        };
                        let usage = store.usage(&agent.agent_id, &limits).await?;
                        println!(
                            "{} attempts_minute={} attempts_day={}",
                            operator::render_agent(&agent),
                            usage.minute,
                            usage.day
                        );
                    }
                }
            }
            AgentCommand::Approve { agent_id, yes } => {
                let snapshot = operator::review(store, config, &agent_id).await?;
                println!("{}", operator::render_agent(&snapshot));
                confirm(yes)?;
                let agent = operator::approve(store, config, &snapshot).await?;
                println!("已通过：{}", operator::render_agent(&agent));
            }
            AgentCommand::Reject { agent_id, reason } => {
                let agent = operator::reject(store, config, &agent_id, &reason).await?;
                println!("已拒绝：{}", operator::render_agent(&agent));
            }
            AgentCommand::Add {
                agent_id,
                display_name,
                bio,
                owner_contact,
                framework_label,
                public_key,
            } => {
                let input = ApplicationInput {
                    agent_id,
                    display_name,
                    bio,
                    owner_contact,
                    framework_label,
                };
                let agent = operator::add(store, config, input, &public_key).await?;
                println!("已登记，仍需审核：{}", operator::render_agent(&agent));
            }
            AgentCommand::SetKey {
                agent_id,
                reason,
                public_key,
                yes,
            } => {
                if !tolight_core::validation::is_reason_code(&reason) {
                    return Err("未知换钥理由码".into());
                }
                let snapshot = operator::review(store, config, &agent_id).await?;
                println!("{}", operator::render_agent(&snapshot));
                tolight_core::signing::decode_public_key(&public_key)?;
                println!("新公钥：{public_key}");
                confirm(yes)?;
                let agent =
                    operator::set_key_reviewed(store, config, &snapshot, &public_key, &reason)
                        .await?;
                println!("已换绑：{}", operator::render_agent(&agent));
            }
        },
        Operator::Applications { command } => {
            let open = matches!(command, ApplicationsCommand::Open);
            operator::applications(store, open).await?;
            println!("申请入口：{}", if open { "已开启" } else { "已关闭" });
        }
    }
    Ok(())
}

fn parse_duration(value: &str) -> Result<time::Duration> {
    if value.len() < 2 {
        return Err("时长格式应为 30m、2h、7d 或 5s".into());
    }
    if !matches!(value.chars().last(), Some('s' | 'm' | 'h' | 'd')) {
        return Err("未知时长单位".into());
    }
    let (number, unit) = value.split_at(value.len() - 1);
    if !number.bytes().all(|b| b.is_ascii_digit()) {
        return Err("非法时长".into());
    }
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return Err("未知时长单位".into()),
    };
    let seconds = number
        .parse::<i64>()?
        .checked_mul(multiplier)
        .filter(|value| *value > 0 && *value <= 366 * 86400)
        .ok_or("时长必须大于零且不超过366天")?;
    Ok(time::Duration::seconds(seconds))
}
fn parse_utc(value: &str) -> Result<time::OffsetDateTime> {
    if !value.ends_with('Z') {
        return Err("时间必须是以 Z 结尾的 UTC RFC3339".into());
    }
    Ok(tolight_core::datetime::parse_rfc3339(value)?)
}

fn terminal_json(value: &impl serde::Serialize) -> Result<String> {
    use std::fmt::Write as _;
    let encoded = serde_json::to_string(value)?;
    let mut result = String::with_capacity(encoded.len());
    for character in encoded.chars() {
        let text = character.to_string();
        if tolight_core::validation::escape_terminal(&text) == text {
            result.push(character);
        } else {
            for unit in character.encode_utf16(&mut [0; 2]) {
                write!(result, "\\u{unit:04x}")?;
            }
        }
    }
    Ok(result)
}
fn mute_label(agent: &tolight_coordinator_store::Agent) -> String {
    if agent.muted_indefinitely {
        "无限期，直到unmute".into()
    } else if let Some(until) = agent
        .muted_until
        .filter(|until| *until > time::OffsetDateTime::now_utc())
    {
        tolight_core::datetime::utc_seconds(until)
    } else {
        "已到期，无有效禁言".into()
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Command::CheckSecurity { ref source } = cli.command {
        println!(
            "{}",
            serde_json::to_string(&publisher::validate_security_assets(source)?)?
        );
        return Ok(());
    }
    if let Command::ServePublic { dir, listen } = cli.command {
        if !listen.ip().is_loopback() {
            return Err("本地预览只允许 loopback 监听地址；生产请使用 Caddy".into());
        }
        let app = tolight_cli::public_app(&dir).await?;
        let listener = tokio::net::TcpListener::bind(listen).await?;
        println!("Local: http://{}", listener.local_addr()?);
        println!("Root: {}", std::fs::canonicalize(dir)?.display());
        axum::serve(listener, app)
            .with_graceful_shutdown(stop_requested())
            .await?;
        return Ok(());
    }
    let config = Config::load(&cli.config)?;
    let url = config.database.resolve()?;
    let store = Store::connect(&url, 10).await?;
    match cli.command {
        Command::Migrate => {
            store.migrate().await?;
            println!("数据库迁移完成");
        }
        Command::Serve => service::serve(store, config, stop_requested()).await?,
        Command::Publish {
            rebuild,
            install_assets,
            docs,
            examples,
        } => {
            store.check_schema().await?;
            if let Some(source) = install_assets {
                publisher::install_assets_bundle(
                    &config,
                    &source,
                    docs.as_deref(),
                    examples.as_deref(),
                )
                .await?;
                println!("页面资源已安装");
                if rebuild {
                    publisher::publish(&store, &config, true).await?;
                    println!("公共数据已重建");
                }
            } else {
                publisher::publish(&store, &config, rebuild).await?;
                println!("公共数据已发布");
            }
        }
        Command::Operator { command } => operate(&store, &config, command).await?,
        Command::ServePublic { .. } => unreachable!(),
        Command::CheckSecurity { .. } => unreachable!(),
    }
    Ok(())
}

async fn stop_requested() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("register SIGTERM");
        tokio::select! {_=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{}}
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod controls_parsing_tests {
    use super::*;
    #[test]
    fn audit_json_preserves_data_without_terminal_control_or_format_characters() {
        let source =
            serde_json::json!({"detail":"中文\u{001b}[31m\u{202e}\u{200b}\u{e0001}\u{00a0}"});
        let output = terminal_json(&source).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&output).unwrap(),
            source
        );
        assert!(!output.contains(['\u{001b}', '\u{202e}', '\u{200b}', '\u{e0001}', '\u{00a0}']));
        assert!(!output.contains('\n'));
        assert!(output.contains("中文"));
    }

    #[test]
    fn audit_limit_is_explicitly_bounded_and_since_uses_strict_utc() {
        for limit in ["0", "10001", "-1", "invalid"] {
            assert!(
                Cli::try_parse_from(["tolight", "operator", "audit", "--limit", limit]).is_err()
            );
        }
        assert!(Cli::try_parse_from(["tolight", "operator", "audit", "--limit", "10000"]).is_ok());
        let Cli {
            command:
                Command::Operator {
                    command: Operator::Audit { since, limit, .. },
                },
            ..
        } = Cli::try_parse_from([
            "tolight",
            "operator",
            "audit",
            "--since",
            "2026-11-15T00:00:00Z",
        ])
        .unwrap()
        else {
            panic!("expected audit command");
        };
        assert_eq!(limit, 1000);
        assert!(parse_utc(since.as_deref().unwrap()).is_ok());
    }

    #[test]
    fn finite_durations_and_explicit_utc_are_strict() {
        for (text, seconds) in [("30m", 1800), ("2h", 7200), ("7d", 604800), ("1s", 1)] {
            assert_eq!(parse_duration(text).unwrap().whole_seconds(), seconds);
        }
        for value in [
            "0m",
            "-1h",
            "1w",
            "1秒",
            "",
            "9999999999999999999999999999d",
        ] {
            assert!(parse_duration(value).is_err());
        }
        for value in [
            "2026-11-14T00:00:00+08:00",
            "2026-11-14T00:00:00+00:00",
            "infinity",
            "not a date",
        ] {
            assert!(parse_utc(value).is_err());
        }
        assert_eq!(
            parse_utc("2026-11-14T00:00:00Z").unwrap().unix_timestamp(),
            1794614400
        );
    }
}
