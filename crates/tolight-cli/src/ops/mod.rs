//! Operational behavior is implemented in Rust; shell entrypoints only dispatch.
pub mod delivery;
mod demos;
pub mod runtime;
pub mod storage;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T = ()> = std::result::Result<T, Error>;

#[derive(Debug)]
pub struct ExitCode(pub i32);
impl std::fmt::Display for ExitCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "operation exited with status {}", self.0)
    }
}
impl std::error::Error for ExitCode {}

#[derive(clap::Subcommand)]
pub enum Command {
    #[command(subcommand)]
    Storage(storage::Command),
    #[command(subcommand)]
    Runtime(runtime::Command),
    #[command(subcommand)]
    Delivery(delivery::Command),
    #[command(subcommand)]
    Demo(demos::Command),
    CargoTestExecutable {
        #[arg(long)]
        artifacts: std::path::PathBuf,
        #[arg(long, default_value = "local_dev")]
        target: String,
    },
}

pub async fn run(command: Command) -> Result {
    match command {
        Command::Storage(command) => storage::run(command).await,
        Command::Runtime(command) => runtime::run(command).await,
        Command::Delivery(command) => delivery::run(command).await,
        Command::Demo(command) => demos::run(command).await,
        Command::CargoTestExecutable { artifacts, target } => {
            let mut executables = vec![];
            for line in std::fs::read_to_string(artifacts)?
                .lines()
                .filter(|line| !line.trim().is_empty())
            {
                let value: serde_json::Value = serde_json::from_str(line)?;
                if value["reason"] == "compiler-artifact"
                    && value["target"]["name"] == target
                    && value["target"]["kind"]
                        .as_array()
                        .is_some_and(|kinds| kinds.iter().any(|kind| kind == "test"))
                    && let Some(path) = value["executable"].as_str()
                {
                    executables.push(path.to_owned());
                }
            }
            if executables.len() != 1 {
                return Err(
                    "Cargo did not identify exactly one requested integration executable".into(),
                );
            }
            println!("{}", executables[0]);
            Ok(())
        }
    }
}
