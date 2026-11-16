//! Native subprocess fixture for model I/O, output bounds, and supervisor tests.
use std::io::{Read, Write};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let configured = std::env::var("REFERENCE_FIXTURE_MODE").ok();
    if configured.is_none() && (args.len() == 1 || matches!(args[1].as_str(), "--help" | "-h")) {
        println!("Native integration-test subprocess fixture; not a participant client.");
        return Ok(());
    }
    let mut mode = configured.as_deref().unwrap_or_else(|| &args[1]).to_owned();
    if mode == "control" {
        mode = std::fs::read_to_string(&args[2])?;
    }
    if let Some(code) = mode.strip_prefix("exit:") {
        std::process::exit(code.parse()?);
    }
    if mode == "flush" {
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            std::fs::write(std::env::var("REFERENCE_FIXTURE_READY")?, "ready")?;
            term.recv().await;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let mut file = std::fs::File::create(std::env::var("REFERENCE_FIXTURE_FLUSHED")?)?;
            file.write_all(b"durable")?;
            file.sync_all()?;
            return Ok(());
        }
        #[cfg(not(unix))]
        return Err("signal fixture requires Unix".into());
    }
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    match mode.as_str() {
        "capture" => {
            let request: serde_json::Value = serde_json::from_str(&input)?;
            let record = serde_json::json!({
                "request": request,
                "env_names": std::env::vars_os().map(|(key, _)| key.to_string_lossy().into_owned()).collect::<Vec<_>>(),
                "credential_length": std::env::var("TOLIGHT_MODEL_CREDENTIAL").unwrap_or_default().len(),
            });
            std::fs::write(&args[2], serde_json::to_vec(&record)?)?;
            println!("模型生成的公开文本");
        }
        "conformance" => {
            let request: serde_json::Value = serde_json::from_str(&input)?;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&args[2])?;
            writeln!(file, "{}", serde_json::to_string(&request)?)?;
            print!("模型桩给出的简短回应。");
        }
        "timeout" => tokio::time::sleep(std::time::Duration::from_secs(5)).await,
        "failure" => {
            eprint!("local-test-credential");
            print!("local-test-credential");
            std::process::exit(2);
        }
        "oversize" => std::io::stdout().write_all(&vec![b'x'; 1024 * 1024 + 1])?,
        "silent" => print!("[SILENT]"),
        "noop" => (),
        "descendant" => {
            let directory = std::path::Path::new(&args[2]);
            let behavior = &args[3];
            let mut child = std::process::Command::new(std::env::current_exe()?);
            child
                .args(["descendant-child", &args[2]])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            if behavior == "exit-closed" {
                child.stdout(std::process::Stdio::null());
            }
            let child = child.spawn()?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while !directory.join("ready").exists() {
                if std::time::Instant::now() >= deadline {
                    return Err("descendant startup deadline".into());
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            // Deliberately do not wait: these modes exercise orphan cleanup.
            std::fs::write(directory.join("child-pid"), child.id().to_string())?;
            if behavior == "wait" {
                std::future::pending::<()>().await;
            }
            println!("parent output");
        }
        "descendant-child" => {
            let directory = std::path::Path::new(&args[2]);
            std::fs::write(directory.join("ready"), "ready")?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !directory.join("release").exists() {
                if std::time::Instant::now() >= deadline {
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            std::fs::write(
                directory.join("late-effect"),
                "unexpected descendant effect",
            )?;
        }
        _ => return Err(format!("unknown fixture mode: {mode}").into()),
    }
    Ok(())
}
