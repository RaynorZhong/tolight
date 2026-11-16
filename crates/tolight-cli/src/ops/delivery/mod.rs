//! Source release, rehearsal and acceptance evidence operations.
use clap::{Args, Subcommand};
use std::{error::Error, path::PathBuf};
mod acceptance;
mod common;
mod release;
mod source;

pub type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

pub fn repository() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[derive(Debug, Subcommand)]
pub enum Command {
    ReleaseCheck(ReleaseArgs),
    SourceRehearsal(SourceArgs),
    AcceptanceCheck(AcceptanceArgs),
}

#[derive(Debug, Args)]
pub struct ReleaseArgs {
    #[arg(long, default_value_os_t = repository())]
    pub source: PathBuf,
    #[arg(long)]
    pub tag: Option<String>,
    #[arg(long)]
    pub production: bool,
    #[arg(long)]
    pub tolight: Option<PathBuf>,
    #[arg(long)]
    pub report: Option<PathBuf>,
    #[arg(long)]
    pub notes: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct SourceArgs {
    #[arg(long, default_value_os_t = repository())]
    pub source: PathBuf,
    #[arg(long, default_value_os_t = repository().join(".local"))]
    pub work_dir: PathBuf,
    #[arg(long)]
    pub prepare_only: bool,
}

#[derive(Debug, Args)]
pub struct AcceptanceArgs {
    #[arg(long, default_value_os_t = repository())]
    pub source: PathBuf,
    #[arg(long)]
    pub ledger: Option<PathBuf>,
    #[arg(long = "final")]
    pub final_check: bool,
    #[arg(long)]
    pub render: bool,
}

pub async fn run(command: Command) -> Result<()> {
    use std::sync::atomic::Ordering;
    common::CANCELLED.store(false, Ordering::SeqCst);
    #[cfg(unix)]
    let (mut terminate, mut interrupt) = (
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
    );
    let mut task = tokio::task::spawn_blocking(move || match command {
        Command::ReleaseCheck(args) => release::run(args),
        Command::SourceRehearsal(args) => source::run(args),
        Command::AcceptanceCheck(args) => acceptance::run(args),
    });
    let interrupted = async {
        #[cfg(unix)]
        {
            tokio::select! {_=interrupt.recv()=>{},_=terminate.recv()=>{}}
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
    };
    tokio::select! {
        result=&mut task=>result?,
        _=interrupted=>{common::CANCELLED.store(true,Ordering::SeqCst);task.await?}
    }
}
