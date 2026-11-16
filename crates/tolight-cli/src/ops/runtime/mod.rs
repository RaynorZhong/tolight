//! Native operational lifecycle, monitoring and deployment verification.
mod alert;
mod check;
pub(crate) mod common;
mod lan;
mod local;
mod probe;
mod verify;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T = ()> = std::result::Result<T, Error>;

#[derive(clap::Subcommand)]
pub enum Command {
    LocalDev(local::Args),
    AlertCheck(alert::Args),
    Probe(probe::Args),
    VerifyPublic(verify::Args),
}

pub async fn run(command: Command) -> Result {
    let command = match command {
        Command::LocalDev(args) => return local::run(args).await,
        command => command,
    };
    common::COMMAND_CANCELLED.store(false, std::sync::atomic::Ordering::Release);
    #[cfg(unix)]
    let mut signal = {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        tokio::spawn(async move {
            tokio::select! { _=terminate.recv()=>(), _=interrupt.recv()=>() }
            common::COMMAND_CANCELLED.store(true, std::sync::atomic::Ordering::Release);
        })
    };
    #[cfg(not(unix))]
    let mut signal = tokio::spawn(async {
        let _ = tokio::signal::ctrl_c().await;
        common::COMMAND_CANCELLED.store(true, std::sync::atomic::Ordering::Release);
    });
    let operation = async {
        match command {
            Command::LocalDev(_) => unreachable!(),
            Command::AlertCheck(args) => alert::run(args).await,
            Command::Probe(args) => probe::run(args).await,
            Command::VerifyPublic(args) => verify::run(args).await,
        }
    };
    let result = tokio::select! {
        result=operation=>result,
        _=&mut signal=>Err("operational command interrupted".into()),
    };
    signal.abort();
    result
}
