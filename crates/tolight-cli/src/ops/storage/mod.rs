//! Private PostgreSQL backup, WAL and recovery tools. No business SQL lives here.
use clap::{Args, Subcommand};
use std::{error::Error, fmt, path::PathBuf};

pub mod backup;
mod common;
mod physical_recovery;
pub mod public_tree;
mod recovery;
mod wal;

pub type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
#[derive(Debug)]
pub struct ExitStatus(pub i32);
impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "operation exited {}", self.0)
    }
}
impl Error for ExitStatus {}
pub fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

#[derive(Subcommand, Debug)]
pub enum Command {
    Backup(Backup),
    PhysicalBackup(PhysicalBackup),
    WalArchive(WalArchive),
    RestoreDrill(RestoreDrill),
    PitrDrill(PitrDrill),
    PitrInspect(PitrInspect),
}
#[derive(Args, Debug)]
pub struct Backup {
    #[arg(long)]
    pub config: Option<PathBuf>,
    #[arg(long, default_value_os_t=root().join(".local/backups"))]
    pub output_dir: PathBuf,
    #[arg(long)]
    pub public_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 14)]
    pub retention_days: i64,
}
#[derive(Args, Debug)]
pub struct PhysicalBackup {
    #[arg(long)]
    pub config: PathBuf,
    #[arg(long, default_value_os_t=root().join(".local/base-backups"))]
    pub output_dir: PathBuf,
    #[arg(long)]
    pub archive_dir: PathBuf,
}
#[derive(Args, Debug)]
pub struct WalArchive {
    #[arg(value_parser=["archive","restore"])]
    pub mode: String,
    #[arg(long)]
    pub archive_dir: PathBuf,
    #[arg(long)]
    pub name: String,
    #[arg(long)]
    pub source: Option<PathBuf>,
    #[arg(long)]
    pub destination: Option<PathBuf>,
    #[arg(long)]
    pub public_dir: Option<PathBuf>,
}
#[derive(Args, Debug)]
pub struct RestoreDrill {
    #[arg(long)]
    pub backup: PathBuf,
    #[arg(long)]
    pub source_config: PathBuf,
    #[arg(long)]
    pub source_public_dir: PathBuf,
    #[arg(long)]
    pub restore_target: String,
    #[arg(long, default_value_os_t=root().join(".local/restore-drills"))]
    pub work_dir: PathBuf,
    #[arg(long)]
    pub approve_active: Vec<uuid::Uuid>,
    #[arg(long, default_value_os_t=root().join("target/debug/tolight"))]
    pub tolight: PathBuf,
    #[arg(long, default_value_t = 15)]
    pub startup_timeout_seconds: u64,
}
#[derive(Args, Debug)]
pub struct PitrDrill {
    #[arg(long)]
    pub base_backup: PathBuf,
    #[arg(long)]
    pub archive_dir: PathBuf,
    #[arg(long)]
    pub source_config: PathBuf,
    #[arg(long)]
    pub source_public_dir: PathBuf,
    #[arg(long)]
    pub restore_target: String,
    #[arg(long, default_value_os_t=root().join(".local/pitr-drills"))]
    pub work_dir: PathBuf,
    #[arg(long)]
    pub approve_active: Vec<uuid::Uuid>,
    #[arg(long, default_value_os_t=root().join("target/debug/tolight"))]
    pub tolight: PathBuf,
    #[arg(long, default_value_t = 15)]
    pub startup_timeout_seconds: u64,
    #[arg(long, default_value_t = 300)]
    pub rto_budget_seconds: u64,
    #[arg(long, default_value_t = 60)]
    pub rpo_budget_seconds: u64,
}
#[derive(Args, Debug)]
pub struct PitrInspect {
    #[arg(long)]
    pub base_backup: PathBuf,
    #[arg(long)]
    pub archive_dir: PathBuf,
    #[arg(long, default_value_os_t=root().join(".local/inspections"))]
    pub work_dir: PathBuf,
    #[arg(long, default_value_os_t=root().join("public-dist"))]
    pub public_dir: PathBuf,
    #[arg(long, default_value_os_t=root().join("target/debug/tolight"))]
    pub tolight: PathBuf,
    #[arg(long)]
    pub database_user: Option<String>,
    #[arg(long, default_value_t = 15)]
    pub startup_timeout_seconds: u64,
}
pub fn request_cancel() {
    common::CANCELLED.store(true, std::sync::atomic::Ordering::Relaxed);
}
pub(crate) fn reset_cancel() {
    common::CANCELLED.store(false, std::sync::atomic::Ordering::Relaxed);
}
pub async fn run(command: Command) -> Result<()> {
    reset_cancel();
    #[cfg(unix)]
    let signal = {
        // Install both handlers before the worker can spawn a native process.
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        tokio::spawn(async move {
            tokio::select! {_=term.recv()=>(),_=interrupt.recv()=>()}
            request_cancel();
        })
    };
    #[cfg(not(unix))]
    let signal = tokio::spawn(async {
        let _ = tokio::signal::ctrl_c().await;
        request_cancel();
    });
    let result = tokio::task::spawn_blocking(move || match command {
        Command::Backup(args) => backup::logical(args),
        Command::PhysicalBackup(args) => backup::physical(args),
        Command::WalArchive(args) => wal::run(args),
        Command::RestoreDrill(args) => recovery::logical(args),
        Command::PitrDrill(args) => recovery::pitr(args),
        Command::PitrInspect(args) => recovery::inspect(args),
    })
    .await;
    signal.abort();
    result?
}
