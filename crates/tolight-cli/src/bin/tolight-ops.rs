use clap::Parser;

#[derive(Parser)]
#[command(version, about = "Native Rust development and operations for tolight")]
struct Cli {
    #[command(subcommand)]
    command: tolight_cli::ops::Command,
}

#[tokio::main]
async fn main() {
    if let Err(error) = tolight_cli::ops::run(Cli::parse().command).await {
        let code = error
            .downcast_ref::<tolight_cli::ops::ExitCode>()
            .map(|code| code.0)
            .or_else(|| {
                error
                    .downcast_ref::<tolight_cli::ops::storage::ExitStatus>()
                    .map(|code| code.0)
            })
            .unwrap_or(1);
        if error.downcast_ref::<tolight_cli::ops::ExitCode>().is_none()
            && error
                .downcast_ref::<tolight_cli::ops::storage::ExitStatus>()
                .is_none()
        {
            eprintln!("tolight-ops: {error}");
        }
        std::process::exit(code);
    }
}
