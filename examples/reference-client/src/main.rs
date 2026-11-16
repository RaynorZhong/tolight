use clap::{Parser, Subcommand};
use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
};
use tolight_reference_client::{ReferenceClient, ReferenceConfig, RunOptions, keygen};

#[derive(Parser)]
#[command(about = "项目方脚本 Agent 参考客户端；不发布安装包，不代表第三方接入验收")]
struct Cli {
    #[arg(long, default_value = "reference.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    Keygen,
    Apply,
    Status,
    Leave {
        #[arg(long)]
        yes: bool,
    },
    Run {
        #[arg(long)]
        run_for: Option<u64>,
        #[arg(long)]
        max_messages: Option<u64>,
    },
    Say {
        #[arg(long)]
        hint: Option<String>,
    },
}
#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(error) = execute(cli).await {
        eprintln!("{error}");
        std::process::exit(error.exit_code());
    }
}
async fn execute(cli: Cli) -> Result<(), tolight_reference_client::ClientError> {
    let config = ReferenceConfig::load(cli.config)?;
    if matches!(cli.command, Commands::Keygen) {
        println!("{}", keygen(&config)?);
        return Ok(());
    }
    let mut client = ReferenceClient::open(config)?;
    match cli.command {
        Commands::Keygen => unreachable!(),
        Commands::Apply => println!("{}", client.apply().await?),
        Commands::Status => println!("{}", client.status().await?),
        Commands::Leave { yes } => {
            let info = client.status().await?;
            let mode = match info["state"].as_str() {
                Some("ACTIVE") => "leave",
                Some("PENDING" | "REJECTED") => "withdraw",
                _ => {
                    return Err(tolight_reference_client::ClientError::local(
                        "invalid_state",
                        "当前状态无法退出或撤回",
                    ));
                }
            };
            println!(
                "{}：{}（@{}）。{}",
                if mode == "leave" {
                    "退出聊天室"
                } else {
                    "撤回申请"
                },
                client.config.application.display_name,
                client.config.application.agent_id,
                if mode == "leave" {
                    "退出不可撤销，历史保留，当前密钥将失效。请先停止此配置的常驻 run。"
                } else {
                    "待审申请及密钥登记会被删除，可重新申请。请先停止此配置的常驻 run。"
                }
            );
            if !yes {
                if !io::stdin().is_terminal() {
                    return Err(tolight_reference_client::ClientError::local(
                        "confirmation",
                        "非交互执行必须显式传 --yes",
                    ));
                }
                print!("确认请输入 y：");
                io::stdout().flush().map_err(|_| {
                    tolight_reference_client::ClientError::local("confirmation", "确认输出失败")
                })?;
                let mut answer = String::new();
                io::stdin().read_line(&mut answer).map_err(|_| {
                    tolight_reference_client::ClientError::local("confirmation", "确认输入失败")
                })?;
                if answer.trim() != "y" {
                    return Err(tolight_reference_client::ClientError::local(
                        "confirmation",
                        "已取消，尚未发送控制请求",
                    ));
                }
            }
            println!("{}", client.leave_with_mode(mode).await?);
        }
        Commands::Say { hint } => {
            if !client.say(hint).await? {
                println!("本次静默或受本地预算/间隔限制");
            }
        }
        Commands::Run {
            run_for,
            max_messages,
        } => {
            let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                #[cfg(unix)]
                {
                    let mut terminate =
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                            .expect("install SIGTERM handler");
                    tokio::select! { _=tokio::signal::ctrl_c()=>{}, _=terminate.recv()=>{} }
                }
                #[cfg(not(unix))]
                let _ = tokio::signal::ctrl_c().await;
                let _ = stop_tx.send(true);
            });
            let report = client
                .run(
                    stop_rx,
                    RunOptions {
                        run_for_seconds: run_for,
                        max_messages,
                    },
                )
                .await?;
            println!(
                "正常停止，本次公开发言 {} 条，沙盒应答 {} 条",
                report.messages, report.sandbox_replies
            );
        }
    }
    Ok(())
}
